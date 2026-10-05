//! The host modules an app may name, and how a module is registered.
//!
//! A module is a [`HostModule`]: one [`ModuleSchema`], read twice — the
//! checker is handed every schema, so a call into a module is checked at its
//! call site and a function reaching one requires its capability; and each
//! app's [`HostRegistry`] holds one instance per module, answering the same
//! schema at the boundary. [`HostModules::standard`] is what this host ships:
//!
//! | module | capability | what it is |
//! | --- | --- | --- |
//! | `web` | — | the `Request` and `Response` types; no operations, so building a `Response` needs no grant (cove#579) |
//! | `log` | `log` | `info`, `warn` and `error`: a line on standard output, prefixed with the app's name |
//! | `timer` | `timer` | `sleep(millis)`, answered **pending**: the run parks and holds no worker while it waits |
//!
//! A module that answers pending hands the scheduler a [`PendingWork`]: a
//! future that produces the answer. The scheduler runs it on the I/O runtime,
//! races it against the run's deadline, and resumes the run on any worker
//! when it finishes; a run cancelled at its deadline drops the future, which
//! is how outbound work is withdrawn. That is the seam the persistent KV and
//! outbound HTTP modules plug into: a module with a schema, an instance per
//! app, and futures for the slow operations.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use cove_runtime::{
    Effect, FieldSchema, Grants, HostAnswer, HostApi, HostRegistry, HostType, ModuleSchema,
    OperationSchema, Reentry, RuntimeError, Transfer, TypeSchema, Value,
};

use crate::stats::AppCounters;

/// What a module instance knows about the app it serves.
#[derive(Clone)]
pub struct AppContext {
    /// The app's name, which `log` prefixes every line with.
    pub app: String,
    /// Whether `log` prints nothing.
    pub quiet: bool,
    /// The app's counters, for what a module wants to count (a call that had
    /// to block instead of park, for one).
    pub counters: Arc<AppCounters>,
}

/// One host module: its schema, and an instance of it per app.
pub trait HostModule: Send + Sync {
    /// What the module declares. The checker and the boundary read this
    /// same value.
    fn schema(&self) -> ModuleSchema;
    /// The module's implementation for one app. Called once per app, when
    /// the app is loaded, and shared by every request of that app.
    fn instantiate(&self, app: &AppContext) -> Box<dyn HostApi>;
}

/// The modules this host registers, in order.
#[derive(Clone)]
pub struct HostModules {
    modules: Vec<Arc<dyn HostModule>>,
}

impl HostModules {
    /// `web`, `log` and `timer`.
    pub fn standard() -> HostModules {
        HostModules {
            modules: vec![Arc::new(Web), Arc::new(LogModule), Arc::new(TimerModule)],
        }
    }

    /// These modules and `module`.
    pub fn with(mut self, module: Arc<dyn HostModule>) -> HostModules {
        self.modules.push(module);
        self
    }

    /// Every module's schema, which the checker and the lowering are given.
    pub fn schemas(&self) -> Vec<ModuleSchema> {
        self.modules.iter().map(|m| m.schema()).collect()
    }

    /// An app's registry: its grants, and an instance of every module.
    pub fn registry<'a>(
        &self,
        granted: impl IntoIterator<Item = &'a String>,
        app: &AppContext,
    ) -> HostRegistry {
        let mut hosts = HostRegistry::new(Grants::new(granted.into_iter().cloned()));
        for module in &self.modules {
            hosts.register(module.instantiate(app));
        }
        hosts
    }
}

/// The answer a host computes off the worker, as the scheduler runs it.
pub type AnswerFuture = Pin<Box<dyn Future<Output = Result<Transfer, RuntimeError>> + Send>>;

/// What a host answers [`HostAnswer::Pending`] with.
///
/// The scheduler downcasts a parked run's request to this. Anything else is
/// a host this scheduler does not know, and the run is resumed with an error.
pub struct PendingWork {
    /// `module.op`, for the logs and the stats.
    pub op: &'static str,
    /// Produces the answer. Spawned on the I/O runtime; dropped unfinished
    /// when the run's deadline comes first.
    pub answer: AnswerFuture,
}

impl PendingWork {
    /// A pending answer for `op`, produced by `answer`.
    pub fn new(
        op: &'static str,
        answer: impl Future<Output = Result<Transfer, RuntimeError>> + Send + 'static,
    ) -> PendingWork {
        PendingWork {
            op,
            answer: Box::pin(answer),
        }
    }

    /// As the [`HostAnswer`] a host returns.
    pub fn answer(self) -> HostAnswer {
        HostAnswer::Pending(Box::new(self))
    }
}

/// `value` as the [`Transfer`] a parked run is resumed with.
///
/// Built and converted in one synchronous step: a [`Value`] is `Rc`-based and
/// must not be held across an `.await` of a future that has to be `Send`.
pub fn transfer(value: &Value) -> Result<Transfer, RuntimeError> {
    Transfer::of(value).map_err(|unsafe_value| {
        RuntimeError::new(format!("{} is not task-safe", unsafe_value.type_name))
    })
}

// ------------------------------------------------------------------- web

const STRING_MAP: HostType = HostType::Map(&HostType::String, &HostType::String);

/// The handler contract: what an app's `handle` takes and answers.
///
/// Types and no operations, so naming the module requires no capability —
/// an app that only builds a `Response` is pure.
pub const WEB: ModuleSchema = ModuleSchema {
    name: "web",
    capability: "web",
    operations: &[],
    types: &[
        TypeSchema {
            name: "Request",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "method",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "path",
                    ty: HostType::String,
                },
                FieldSchema {
                    name: "query",
                    ty: STRING_MAP,
                },
                FieldSchema {
                    name: "headers",
                    ty: STRING_MAP,
                },
                FieldSchema {
                    name: "body",
                    ty: HostType::String,
                },
            ],
        },
        TypeSchema {
            name: "Response",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "status",
                    ty: HostType::Int,
                },
                FieldSchema {
                    name: "headers",
                    ty: STRING_MAP,
                },
                FieldSchema {
                    name: "body",
                    ty: HostType::String,
                },
            ],
        },
    ],
    resources: &[],
};

struct Web;

impl HostModule for Web {
    fn schema(&self) -> ModuleSchema {
        WEB
    }

    fn instantiate(&self, _app: &AppContext) -> Box<dyn HostApi> {
        Box::new(WebHost)
    }
}

struct WebHost;

impl HostApi for WebHost {
    fn module_schema(&self) -> ModuleSchema {
        WEB
    }

    fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::new(format!(
            "`web` declares no operation `{op}`"
        )))
    }
}

// ------------------------------------------------------------------- log

const fn log_op(name: &'static str) -> OperationSchema {
    OperationSchema {
        name,
        params: &[HostType::String],
        variadic: false,
        result: HostType::Unit,
        capability: "log",
        effect: Effect::IrreversibleWrite,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

/// A line on standard output, prefixed with the app's name and the level.
pub const LOG: ModuleSchema = ModuleSchema {
    name: "log",
    capability: "log",
    operations: &[log_op("info"), log_op("warn"), log_op("error")],
    types: &[],
    resources: &[],
};

struct LogModule;

impl HostModule for LogModule {
    fn schema(&self) -> ModuleSchema {
        LOG
    }

    fn instantiate(&self, app: &AppContext) -> Box<dyn HostApi> {
        Box::new(LogHost {
            app: app.app.clone(),
            quiet: app.quiet,
        })
    }
}

struct LogHost {
    app: String,
    quiet: bool,
}

impl HostApi for LogHost {
    fn module_schema(&self) -> ModuleSchema {
        LOG
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if !self.quiet {
            // The boundary held the arity and the argument to `LOG`.
            let line = args.first().and_then(Value::as_str).unwrap_or_default();
            println!("[{}] {op}: {line}", self.app);
        }
        Ok(Value::unit())
    }
}

// ----------------------------------------------------------------- timer

/// The longest one `timer.sleep` may ask for. A run's deadline is what
/// normally ends a long wait; this bounds the blocking fallback too.
pub const MAX_SLEEP: Duration = Duration::from_secs(60);

/// `timer.sleep(millis)`: waits, holding no worker.
pub const TIMER: ModuleSchema = ModuleSchema {
    name: "timer",
    capability: "timer",
    operations: &[OperationSchema {
        name: "sleep",
        params: &[HostType::Int],
        variadic: false,
        result: HostType::Unit,
        capability: "timer",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }],
    types: &[],
    resources: &[],
};

struct TimerModule;

impl HostModule for TimerModule {
    fn schema(&self) -> ModuleSchema {
        TIMER
    }

    fn instantiate(&self, app: &AppContext) -> Box<dyn HostApi> {
        Box::new(TimerHost {
            counters: Arc::clone(&app.counters),
        })
    }
}

struct TimerHost {
    counters: Arc<AppCounters>,
}

impl TimerHost {
    fn duration(args: &[Value]) -> Result<Duration, RuntimeError> {
        let millis = args.first().and_then(Value::as_int).unwrap_or_default();
        let millis = u64::try_from(millis).map_err(|_| {
            RuntimeError::new(format!(
                "timer.sleep({millis}): a duration cannot be negative"
            ))
        })?;
        let wanted = Duration::from_millis(millis);
        if wanted > MAX_SLEEP {
            return Err(RuntimeError::new(format!(
                "timer.sleep({millis}): longer than the host's {} s limit",
                MAX_SLEEP.as_secs()
            )));
        }
        Ok(wanted)
    }
}

impl HostApi for TimerHost {
    fn module_schema(&self) -> ModuleSchema {
        TIMER
    }

    /// The blocking answer, for a call made where the run cannot park — below
    /// an encoded function that compiled code called, for one (ADR 0085's
    /// open item). It sleeps on the worker, which is the cost parking exists
    /// to avoid, so every one is counted (`blocking_host_calls`).
    fn call(&self, _op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let wanted = Self::duration(&args)?;
        self.counters
            .blocking_host_calls
            .fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(wanted);
        Ok(Value::unit())
    }

    fn call_parkable(&self, _op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        let wanted = match Self::duration(&args) {
            Ok(wanted) => wanted,
            Err(error) => return HostAnswer::Ready(Err(error)),
        };
        PendingWork::new("timer.sleep", async move {
            tokio::time::sleep(wanted).await;
            transfer(&Value::unit())
        })
        .answer()
    }
}
