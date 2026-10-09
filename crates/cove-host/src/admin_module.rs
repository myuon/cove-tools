//! The `host` module, capability `admin`: what the admin app (`apps/admin`)
//! sees of the host, and the only way it changes anything.
//!
//! The module is `host` rather than `admin` because the admin app's own main
//! module is `admin` — an app's main module is named after its directory —
//! and Cove refuses a package module that shadows a host module
//! (`cove::resolve::module_shadows_host`). What is granted, and what every
//! listing shows, is the capability: `admin`.
//!
//! | operation | answers |
//! | --- | --- |
//! | `host.apps()` | `Array<host.App>`: every app the host has, in load order — its state (`serving`, `disabled`, `refused`, `removed`) and why, version, tier, hostnames, what its entry requires and what it is granted (and which of those grants the admin changed), its fetch allowlist, its limits, its counters and its recent errors |
//! | `host.capabilities()` | `Array<String>`: every capability the host's modules declare, the ones a grant may name |
//! | `host.history(limit)` | `Array<host.Change>`: the newest `limit` changes, newest first |
//! | `host.setEnabled(app, enabled, who)` | `Result<String, Error>`: routes to the app again, or stops; nothing is reloaded |
//! | `host.configure(app, settings, who)` | `Result<String, Error>`: the app's grant, allowlist and limits become `settings`, if the app loads with them |
//! | `host.reset(app, who)` | `Result<String, Error>`: drops the admin's changes, back to `app.toml` |
//! | `host.secrets()` | `Array<host.Secret>`: every secret stored or used — `name`, `set`, `updatedMs` (0 when unset), `apps` that use it — never a value |
//! | `host.setSecret(name, value, who)` | `Result<String, Error>`: stores the value ([`crate::secrets`]) and reloads the apps that use it; the message says what each reload came to |
//! | `host.deleteSecret(name, force, who)` | `Result<String, Error>`: refused while an app uses it, unless `force` |
//!
//! The five that change something are answered **pending**: the run parks
//! while the app is reloaded on a blocking thread — parsed, checked,
//! admitted, lowered, prepared and compiled, as an update is — and holds no
//! worker. What they may do, and the persistence, are
//! [`crate::manage`]'s; `who` is what the admin app says of its user, and
//! goes into the change history as it is.
//!
//! The capability is `admin`, and only the app named `admin` may be granted
//! it ([`crate::config::ADMIN_APP`]); the module's instance in any other
//! app has no handle on the host at all, so even a grant that slipped
//! through would reach nothing.

use std::collections::BTreeSet;
use std::sync::Arc;

use cove_runtime::{
    Effect, FieldSchema, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry,
    RuntimeError, Transfer, TypeSchema, Value,
};

use crate::config::{LimitsFile, ADMIN_CAPABILITY};
use crate::hosts::{AppContext, HostModule, PendingWork};
use crate::manage::{AppInfo, SecretInfo, Settings, Via};
use crate::overrides::{Change, Control};

const STR: HostType = HostType::String;
const INT: HostType = HostType::Int;
const STRINGS: HostType = HostType::Array(&HostType::String);
const CHANGED: HostType = HostType::Result(&HostType::String, &HostType::Error);

const fn op(
    name: &'static str,
    params: &'static [HostType],
    result: HostType,
    effect: Effect,
) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability: ADMIN_CAPABILITY,
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const fn field(name: &'static str, ty: HostType) -> FieldSchema {
    FieldSchema { name, ty }
}

const LIMITS_FIELDS: &[FieldSchema] = &[
    field("maxHostCalls", INT),
    field("deadlineMs", INT),
    field("maxHeapWords", INT),
    field("maxInFlight", INT),
    field("maxQueued", INT),
    field("maxRequestBytes", INT),
    field("maxResponseBytes", INT),
];

/// The `host` module, capability `admin`.
pub const ADMIN: ModuleSchema = ModuleSchema {
    name: "host",
    capability: ADMIN_CAPABILITY,
    operations: &[
        op(
            "apps",
            &[],
            HostType::Array(&HostType::Named("host.App")),
            Effect::Read,
        ),
        op("capabilities", &[], STRINGS, Effect::Read),
        op(
            "history",
            &[INT],
            HostType::Array(&HostType::Named("host.Change")),
            Effect::Read,
        ),
        op(
            "setEnabled",
            &[STR, HostType::Bool, STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op(
            "configure",
            &[STR, HostType::Named("host.Settings"), STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op("reset", &[STR, STR], CHANGED, Effect::IrreversibleWrite),
        op(
            "secrets",
            &[],
            HostType::Array(&HostType::Named("host.Secret")),
            Effect::Read,
        ),
        op(
            "setSecret",
            &[STR, STR, STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op(
            "deleteSecret",
            &[STR, HostType::Bool, STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
    ],
    types: &[
        // `maxHeapWords` 0 is "no limit but the runtime's"; every other
        // value is the limit itself.
        TypeSchema {
            name: "Limits",
            cases: &[],
            fields: LIMITS_FIELDS,
        },
        TypeSchema {
            name: "Failure",
            cases: &[],
            fields: &[
                field("atMs", INT),
                field("kind", STR),
                field("status", INT),
                field("version", STR),
                field("message", STR),
            ],
        },
        TypeSchema {
            name: "App",
            cases: &[],
            fields: &[
                field("name", STR),
                // `serving`, `disabled`, `refused` or `removed`.
                field("state", STR),
                // Why it was refused; "" otherwise.
                field("reason", STR),
                field("version", STR),
                // `native`, `vm`, or "" for an app not loaded.
                field("tier", STR),
                field("entry", STR),
                field("hosts", STRINGS),
                field("required", STRINGS),
                // Whether `required` is a lower bound.
                field("requiredOpen", HostType::Bool),
                field("granted", STRINGS),
                // What the admin added to and removed from `app.toml`'s grant.
                field("grantAdded", STRINGS),
                field("grantRemoved", STRINGS),
                field("fetchAllow", STRINGS),
                field("limits", HostType::Named("host.Limits")),
                // The `[limits]` keys the admin set.
                field("limitsChanged", STRINGS),
                // Whether this is the admin app itself.
                field("isAdmin", HostType::Bool),
                field("served", INT),
                field("ok", INT),
                field("errors", INT),
                field("rejected", INT),
                field("inFlight", INT),
                field("queued", INT),
                // -1 for an app with no store open.
                field("kvKeys", INT),
                field("kvBytes", INT),
                field(
                    "recentErrors",
                    HostType::Array(&HostType::Named("host.Failure")),
                ),
            ],
        },
        TypeSchema {
            name: "Settings",
            cases: &[],
            fields: &[
                field("grant", STRINGS),
                field("fetchAllow", STRINGS),
                field("limits", HostType::Named("host.Limits")),
            ],
        },
        TypeSchema {
            name: "Secret",
            cases: &[],
            fields: &[
                field("name", STR),
                // Whether the store has it.
                field("set", HostType::Bool),
                // When it was last set; 0 when it is not.
                field("updatedMs", INT),
                // The apps whose app.toml takes a secret from it.
                field("apps", STRINGS),
            ],
        },
        TypeSchema {
            name: "Change",
            cases: &[],
            fields: &[
                field("atMs", INT),
                field("who", STR),
                field("app", STR),
                field("action", STR),
                field("detail", STR),
                field("outcome", STR),
            ],
        },
    ],
    resources: &[],
};

pub(crate) struct AdminModule;

impl HostModule for AdminModule {
    fn schema(&self) -> ModuleSchema {
        ADMIN
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(AdminHost {
            control: app.control.clone(),
            io: app.io.clone(),
        }))
    }
}

struct AdminHost {
    /// The host; `None` in every app but the admin app, and in `cove-host
    /// test`.
    control: Option<Arc<Control>>,
    io: tokio::runtime::Handle,
}

fn strings(items: impl IntoIterator<Item = impl Into<String>>) -> Value {
    Value::array(items.into_iter().map(|item| Value::string(item.into())))
}

fn int(value: impl TryInto<i64>) -> Value {
    Value::int(value.try_into().unwrap_or(i64::MAX))
}

fn limits_value(limits: &LimitsFile) -> Value {
    let deadline_ms = limits
        .deadline
        .as_deref()
        .and_then(|text| crate::config::parse_duration(text).ok())
        .map_or(0, |deadline| deadline.as_millis() as i64);
    Value::structure(
        "host.Limits",
        vec![
            ("maxHostCalls", int(limits.max_host_calls.unwrap_or(0))),
            ("deadlineMs", Value::int(deadline_ms)),
            ("maxHeapWords", int(limits.max_heap_words.unwrap_or(0))),
            ("maxInFlight", int(limits.max_in_flight.unwrap_or(0))),
            ("maxQueued", int(limits.max_queued.unwrap_or(0))),
            (
                "maxRequestBytes",
                int(limits.max_request_bytes.unwrap_or(0)),
            ),
            (
                "maxResponseBytes",
                int(limits.max_response_bytes.unwrap_or(0)),
            ),
        ],
    )
}

fn app_value(info: AppInfo) -> Value {
    Value::structure(
        "host.App",
        vec![
            ("name", Value::string(info.name)),
            ("state", Value::string(info.state)),
            ("reason", Value::string(info.reason)),
            ("version", Value::string(info.version)),
            ("tier", Value::string(info.tier)),
            ("entry", Value::string(info.entry)),
            ("hosts", strings(info.hosts)),
            ("required", strings(info.required)),
            ("requiredOpen", Value::bool(info.required_open)),
            ("granted", strings(info.granted)),
            ("grantAdded", strings(info.grant_added)),
            ("grantRemoved", strings(info.grant_removed)),
            ("fetchAllow", strings(info.fetch_allow)),
            ("limits", limits_value(&info.limits)),
            ("limitsChanged", strings(info.limits_changed)),
            ("isAdmin", Value::bool(info.is_admin)),
            ("served", int(info.served)),
            ("ok", int(info.ok)),
            ("errors", int(info.errors)),
            ("rejected", int(info.rejected)),
            ("inFlight", int(info.in_flight)),
            ("queued", int(info.queued)),
            (
                "kvKeys",
                info.kv.map_or(Value::int(-1), |(keys, _)| int(keys)),
            ),
            (
                "kvBytes",
                info.kv.map_or(Value::int(-1), |(_, bytes)| int(bytes)),
            ),
            (
                "recentErrors",
                Value::array(info.recent_errors.into_iter().map(|failure| {
                    Value::structure(
                        "host.Failure",
                        vec![
                            ("atMs", int(failure.at_ms)),
                            ("kind", Value::string(failure.kind)),
                            ("status", int(failure.status)),
                            ("version", Value::string(failure.version)),
                            ("message", Value::string(failure.message)),
                        ],
                    )
                })),
            ),
        ],
    )
}

fn secret_value(info: SecretInfo) -> Value {
    Value::structure(
        "host.Secret",
        vec![
            ("name", Value::string(info.name)),
            ("set", Value::bool(info.set)),
            ("updatedMs", int(info.updated_ms.unwrap_or(0))),
            ("apps", strings(info.apps)),
        ],
    )
}

fn change_value(change: Change) -> Value {
    Value::structure(
        "host.Change",
        vec![
            ("atMs", int(change.unix_ms)),
            ("who", Value::string(change.who)),
            ("app", Value::string(change.app)),
            ("action", Value::string(change.action)),
            ("detail", Value::string(change.detail)),
            ("outcome", Value::string(change.outcome)),
        ],
    )
}

/// The strings of an `Array<String>` argument.
fn string_items(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::items)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A `host.Settings` argument as [`Settings`], or why it is not one: a
/// negative limit, or a zero where zero means nothing. The config's own
/// rules (at least one in flight, a heap under the ceiling, allowlist
/// entries that parse) are the reload's to apply.
fn settings_of(value: &Value) -> Result<Settings, String> {
    let limits = value.field("limits").ok_or("the settings have no limits")?;
    let get = |name: &str| -> Result<i64, String> {
        let n = limits
            .field(name)
            .and_then(Value::as_int)
            .ok_or_else(|| format!("`{name}` is missing"))?;
        if n < 0 {
            return Err(format!("`{name}` cannot be negative ({n})"));
        }
        Ok(n)
    };
    let positive = |name: &str| -> Result<u64, String> {
        match get(name)? {
            0 => Err(format!("`{name}` must be at least 1")),
            n => Ok(n as u64),
        }
    };
    let heap = get("maxHeapWords")? as u64;
    let limits = LimitsFile {
        max_host_calls: Some(get("maxHostCalls")? as u64),
        deadline: Some(format!("{}ms", positive("deadlineMs")?)),
        max_call_depth: None,
        max_heap_words: (heap > 0).then_some(heap),
        max_in_flight: Some(positive("maxInFlight")? as usize),
        max_queued: Some(get("maxQueued")? as usize),
        max_request_bytes: Some(get("maxRequestBytes")? as usize),
        max_response_bytes: Some(get("maxResponseBytes")? as usize),
        ..LimitsFile::default()
    };
    let grant: BTreeSet<String> = string_items(value.field("grant"))
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let mut fetch_allow = Vec::new();
    for entry in string_items(value.field("fetchAllow")) {
        let entry = entry.trim().to_string();
        if !entry.is_empty() && !fetch_allow.contains(&entry) {
            fetch_allow.push(entry);
        }
    }
    Ok(Settings {
        grant,
        fetch_allow,
        limits,
    })
}

impl AdminHost {
    fn control(&self) -> Result<&Arc<Control>, RuntimeError> {
        self.control.as_ref().ok_or_else(|| {
            RuntimeError::new(
                "admin: this app has no host to administer (only the app `admin` of a running \
                 host does)",
            )
        })
    }

    /// The change `op` asks for, as the future that makes it.
    fn change(
        &self,
        op: &str,
        args: &[Value],
    ) -> Result<impl std::future::Future<Output = Result<Transfer, RuntimeError>>, RuntimeError>
    {
        let control = Arc::clone(self.control()?);
        let text = |at: usize| {
            args.get(at)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let app = text(0);
        enum Asked {
            Enable(bool),
            Configure(Result<Settings, String>),
            Reset,
            SetSecret(String),
            DeleteSecret(bool),
        }
        let (asked, who) = match op {
            "setEnabled" => (
                Asked::Enable(args.get(1).and_then(Value::as_bool).unwrap_or(true)),
                text(2),
            ),
            "configure" => (
                Asked::Configure(args.get(1).map_or(Err("no settings".into()), settings_of)),
                text(2),
            ),
            "reset" => (Asked::Reset, text(1)),
            // The first argument is the secret's name; the value goes to the
            // store and nowhere else.
            "setSecret" => (Asked::SetSecret(text(1)), text(2)),
            "deleteSecret" => (
                Asked::DeleteSecret(args.get(1).and_then(Value::as_bool).unwrap_or(false)),
                text(2),
            ),
            other => {
                return Err(RuntimeError::new(format!(
                    "`host` declares no operation `{other}`"
                )))
            }
        };
        Ok(async move {
            let Some(front) = control.front() else {
                return Err(RuntimeError::new("admin: the host is shutting down"));
            };
            let via = Via::AdminApp(who);
            let outcome = match asked {
                Asked::Enable(enabled) => front.set_enabled(&app, enabled, &via).await,
                Asked::Configure(Ok(settings)) => front.configure(&app, settings, &via).await,
                Asked::Configure(Err(why)) => {
                    front.record_refusal(&app, "configure", &via, &why);
                    Err(crate::manage::ChangeError::Refused(why))
                }
                Asked::Reset => front.reset(&app, &via).await,
                Asked::SetSecret(value) => front
                    .set_secret(&app, &value, &via)
                    .await
                    .map(|change| change.message),
                Asked::DeleteSecret(force) => front
                    .delete_secret(&app, force, &via)
                    .await
                    .map(|change| change.message),
            };
            Ok(match outcome {
                Ok(message) => Transfer::ok(Transfer::string(message)),
                Err(error) => Transfer::err(Transfer::error(error.to_string())),
            })
        })
    }
}

impl HostApi for AdminHost {
    fn module_schema(&self) -> ModuleSchema {
        ADMIN
    }

    /// The reads, at once; a change where the run cannot park, waited for
    /// on the worker.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let control = self.control()?;
        match op {
            "apps" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                Ok(Value::array(front.app_infos().into_iter().map(app_value)))
            }
            "capabilities" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                Ok(strings(front.capabilities()))
            }
            "secrets" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                Ok(Value::array(
                    front.secret_infos().into_iter().map(secret_value),
                ))
            }
            "history" => {
                let limit = args.first().and_then(Value::as_int).unwrap_or(50);
                let limit = limit.clamp(0, 1000) as usize;
                Ok(Value::array(
                    control.history.newest(limit).into_iter().map(change_value),
                ))
            }
            _ => {
                let change = self.change(op, &args)?;
                self.io.block_on(change).map(Transfer::into_value)
            }
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        match op {
            "setEnabled" | "configure" | "reset" | "setSecret" | "deleteSecret" => {
                match self.change(op, &args) {
                    Ok(change) => PendingWork::new("host.change", change).answer(),
                    Err(error) => HostAnswer::Ready(Err(error)),
                }
            }
            _ => HostAnswer::Ready(self.call(op, args)),
        }
    }
}
