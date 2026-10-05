//! The HTTP front: hyper on tokio, handing every request to the scheduler.
//!
//! hyper owns the protocol — HTTP/1.1 keep-alive, pipelining, chunked
//! request bodies, header limits and timeouts — and tokio owns the sockets,
//! so an idle connection costs a task and no thread, and waiting on many of
//! them is epoll/kqueue rather than a `poll(2)` over every socket (the edge
//! sample's cove#590). No Cove code runs on tokio's threads: a request is
//! admitted into its app's queue ([`crate::sched`]) and the connection's task
//! awaits a oneshot that a worker answers.
//!
//! What is refused here, before anything runs:
//!
//! | condition | status |
//! | --- | --- |
//! | `--max-connections` open | 503, `Retry-After: 1`, connection closed |
//! | no app by that name | 404 |
//! | the app was refused at load | 503, with the reason (no `Retry-After`) |
//! | body larger than the app's `max_request_bytes` | 413 |
//! | body that is not UTF-8 | 400 |
//! | the app has `max_queued` requests waiting | 429, `Retry-After: 1` |
//! | the server has `--max-in-flight` requests admitted | 503, `Retry-After: 1` |
//!
//! A second listener, the **admin** one ([`crate::admin`]), takes updates.
//! An update loads the new version off the request path and switches the
//! app's route to it only if it loaded; see [`Host::update`].

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, Semaphore};

use crate::apps::{load_all, load_as, App, AppState, Backend, Lineage, LoadOptions};
use crate::convert::{AppRequest, Reply};
use crate::hosts::HostModules;
use crate::ops::{self, OpsContext, OpsListener};
use crate::proxy::Forwarding;
use crate::router::{PathPrefix, Router};
use crate::sched::{Cancel, Engine, Flight, Installed, Start};
use crate::stats::{Rejection, ServerCounters};

/// How to start a host.
#[derive(Clone)]
pub struct ServeOptions {
    /// The directory holding one directory per app.
    pub apps: PathBuf,
    /// Where to listen; port 0 picks a free one.
    pub addr: String,
    /// Threads that run Cove.
    pub workers: usize,
    /// Threads that run the HTTP front and pending host work.
    pub io_threads: usize,
    /// How long a run may hold a worker while others wait; `None` never asks.
    pub slice: Option<Duration>,
    /// Connections open at once; one more is answered 503 and closed.
    pub max_connections: usize,
    /// Requests admitted (queued, running or parked) over every app.
    pub max_in_flight: usize,
    pub backend: Backend,
    /// Whether `log` prints nothing.
    pub quiet: bool,
    pub modules: HostModules,
    /// Where apps keep their state, one directory per app; `None` keeps it
    /// in memory, gone at exit.
    pub data: Option<PathBuf>,
    /// Where the admin listener listens; `None` has none (no updates but a
    /// restart).
    pub admin: Option<String>,
    /// The admin token. `None` reads `<data>/admin.token`, writing a fresh
    /// random one there first if there is none.
    pub admin_token: Option<String>,
    /// Which listener serves `/_host/` (`--ops-listener`).
    pub ops_listener: OpsListener,
    /// What apps are told of the client's scheme and host
    /// (`--public-origin`, `--trust-proxy`).
    pub forwarding: Forwarding,
}

impl ServeOptions {
    /// The defaults `cove-host serve` uses, for `apps`.
    pub fn new(apps: impl Into<PathBuf>) -> ServeOptions {
        ServeOptions {
            apps: apps.into(),
            addr: "127.0.0.1:8080".to_string(),
            workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
            io_threads: 2,
            slice: Some(Duration::from_millis(2)),
            max_connections: 10_000,
            max_in_flight: 10_000,
            backend: Backend::Auto,
            quiet: false,
            modules: HostModules::standard(),
            data: Some(PathBuf::from("data")),
            admin: Some("127.0.0.1:8081".to_string()),
            admin_token: None,
            ops_listener: OpsListener::Public,
            forwarding: Forwarding::default(),
        }
    }
}

/// What the connection tasks share.
pub(crate) struct Front {
    pub(crate) engine: Arc<Engine>,
    router: Box<dyn Router>,
    counters: ServerCounters,
    connections: Arc<Semaphore>,
    started: Instant,
    pub(crate) options: ServeOptions,
    /// How an update loads a version: the options the first load had.
    load: LoadOptions,
    /// One update at a time, so that two cannot race for a version number.
    updating: tokio::sync::Mutex<()>,
    pub(crate) admin_token: String,
    /// Set by [`Host::shutdown`]: new requests are answered 503.
    draining: AtomicBool,
}

/// Why an update did not happen.
#[derive(Debug)]
pub enum UpdateError {
    /// No directory with an `app.toml` by that name.
    NotFound(String),
    /// The new version did not load; the current one still serves. The
    /// reason, with the diagnostics.
    Refused {
        current: Option<String>,
        why: String,
    },
}

/// A running host.
pub struct Host {
    /// Where it listens.
    pub addr: SocketAddr,
    /// Where the admin listener listens, if there is one.
    pub admin_addr: Option<SocketAddr>,
    front: Arc<Front>,
    runtime: Option<tokio::runtime::Runtime>,
    _workers: Vec<JoinHandle<()>>,
}

impl Host {
    /// Loads the apps, binds, and starts the threads. Returns once the host
    /// is accepting. An app that is refused is reported and not served; the
    /// error is only for a host that cannot start at all.
    pub fn start(options: ServeOptions) -> Result<Host, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(options.io_threads.max(1))
            .thread_name("cove-host-io")
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the I/O runtime: {e}"))?;
        let load = LoadOptions {
            backend: options.backend,
            quiet: options.quiet,
            modules: options.modules.clone(),
            data: options.data.clone(),
            io: runtime.handle().clone(),
        };
        let apps = load_all(&options.apps, &load)?;
        Host::launch(runtime, apps, options, load)
    }

    /// Starts a host over apps already loaded on `runtime`.
    fn launch(
        runtime: tokio::runtime::Runtime,
        apps: Vec<App>,
        options: ServeOptions,
        load: LoadOptions,
    ) -> Result<Host, String> {
        if options.ops_listener == OpsListener::Admin && options.admin.is_none() {
            return Err(
                "--ops-listener admin needs the admin listener; drop --no-admin".to_string(),
            );
        }
        let listener = std::net::TcpListener::bind(&options.addr)
            .map_err(|e| format!("cannot listen on `{}`: {e}", options.addr))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let admin = match &options.admin {
            Some(at) => {
                let admin = std::net::TcpListener::bind(at)
                    .map_err(|e| format!("cannot listen on `{at}` for the admin: {e}"))?;
                admin.set_nonblocking(true).map_err(|e| e.to_string())?;
                Some(admin)
            }
            None => None,
        };
        let admin_addr = admin
            .as_ref()
            .map(|admin| admin.local_addr().map_err(|e| e.to_string()))
            .transpose()?;
        let admin_token = match (&options.admin_token, &options.admin) {
            (Some(token), _) => token.clone(),
            (None, None) => String::new(),
            (None, Some(_)) => crate::admin::token_file(options.data.as_deref())?,
        };
        let router = PathPrefix;
        let engine = Arc::new(Engine::new(
            apps,
            options.workers,
            options.max_in_flight,
            options.slice,
            runtime.handle().clone(),
        ));
        let workers = engine.start_workers()?;
        runtime.spawn(Arc::clone(&engine).monitor());
        let front = Arc::new(Front {
            engine,
            router: Box::new(router),
            counters: ServerCounters::default(),
            connections: Arc::new(Semaphore::new(options.max_connections.max(1))),
            started: Instant::now(),
            options,
            load,
            updating: tokio::sync::Mutex::new(()),
            admin_token,
            draining: AtomicBool::new(false),
        });
        let (listener, admin) = {
            let _entered = runtime.enter();
            (
                tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?,
                admin
                    .map(tokio::net::TcpListener::from_std)
                    .transpose()
                    .map_err(|e| e.to_string())?,
            )
        };
        runtime.spawn(Arc::clone(&front).accept(listener));
        if let Some(admin) = admin {
            runtime.spawn(crate::admin::serve(Arc::clone(&front), admin));
        }
        Ok(Host {
            addr,
            admin_addr,
            front,
            runtime: Some(runtime),
            _workers: workers,
        })
    }

    /// The version each app routes to now, loaded or refused; removed apps
    /// are left out.
    pub fn apps(&self) -> Vec<Arc<App>> {
        self.front
            .engine
            .slots()
            .iter()
            .filter_map(|slot| slot.current())
            .collect()
    }

    /// What `GET /_host/stats` answers.
    pub fn stats(&self) -> Json {
        ops::stats(&self.front.ops())
    }

    /// What `GET /_host/apps/<app>` answers.
    pub fn app_detail(&self, name: &str) -> Option<Json> {
        let slot = self.front.engine.slot_named(name)?;
        Some(ops::app_detail(&self.front.ops(), &slot))
    }

    /// Loads the app `name` again from its directory and, if it loads,
    /// routes to the new version: what `POST /apps/<name>/update` on the
    /// admin listener does.
    pub fn update(&self, name: &str) -> Result<Installed, UpdateError> {
        let runtime = self.runtime.as_ref().expect("running");
        runtime.block_on(self.front.update(name))
    }

    /// The admin token.
    pub fn admin_token(&self) -> &str {
        &self.front.admin_token
    }

    /// The startup banner: one line per app, and where to look.
    pub fn banner(&self) -> String {
        let mut out = String::new();
        for app in self.apps() {
            out.push_str(&format!("  {}\n", app.describe()));
        }
        let options = &self.front.options;
        out.push_str(&format!(
            "\nlistening on http://{} — {} worker thread(s), {}, a fresh isolate per request, \
             apps served round robin\n",
            self.addr,
            self.front.engine.workers(),
            match options.slice {
                Some(slice) => format!(
                    "a run yields after {:.1} ms while others wait",
                    slice.as_secs_f64() * 1e3
                ),
                None => "no time slice".to_string(),
            },
        ));
        let ops_at = match (options.ops_listener, self.admin_addr) {
            (OpsListener::Admin, Some(admin)) => admin,
            _ => self.addr,
        };
        out.push_str(&format!(
            "  stats: curl -s http://{ops_at}/_host/stats   ops page: http://{ops_at}/_host/ui{}\n",
            match options.ops_listener {
                OpsListener::Admin => "  (admin listener only)",
                OpsListener::Public => "",
            }
        ));
        if let Some(origin) = &options.forwarding.public_origin {
            out.push_str(&format!("  public origin: {origin}\n"));
        } else if options.forwarding.trust_proxy {
            out.push_str("  trusting X-Forwarded-Proto and X-Forwarded-Host\n");
        }
        if let Some(admin) = self.admin_addr {
            out.push_str(&format!(
                "  admin: http://{admin} (token in {}); update with `cove-host update <app>`\n",
                match (&self.front.options.admin_token, &self.front.options.data) {
                    (Some(_), _) => "--admin-token".to_string(),
                    (None, Some(data)) => data.join("admin.token").display().to_string(),
                    (None, None) => "admin.token".to_string(),
                }
            ));
        }
        out
    }

    /// Blocks until Ctrl-C (SIGINT) or, on Unix, SIGTERM — what
    /// `systemctl stop` sends.
    pub fn wait_for_signal(&self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        runtime.block_on(async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut term) = signal(SignalKind::terminate()) {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                    return;
                }
            }
            let _ = tokio::signal::ctrl_c().await;
        });
    }

    /// Stops admitting requests — each new one is answered 503 with
    /// `Connection: close` — and waits up to `grace` for those admitted to
    /// be answered. Returns how many were still unanswered.
    pub fn shutdown(&self, grace: Duration) -> usize {
        self.front.draining.store(true, Ordering::SeqCst);
        let until = Instant::now() + grace;
        loop {
            let left = self.front.engine.queue.admitted();
            if left == 0 || Instant::now() >= until {
                return left;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.front.engine.close();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// What a connection refused at the limit is told.
const TOO_MANY_CONNECTIONS: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
content-type: text/plain; charset=utf-8\r\nretry-after: 1\r\nconnection: close\r\n\
content-length: 37\r\n\r\ncove-host: too many open connections\n";

impl Front {
    async fn accept(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Out of file descriptors, most likely: back off rather
                    // than spin.
                    eprintln!("cove-host: accept: {error}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            self.counters.connections.fetch_add(1, Ordering::Relaxed);
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                self.counters
                    .rejected_connections
                    .fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _ = stream.write_all(TOO_MANY_CONNECTIONS).await;
                    let _ = stream.shutdown().await;
                });
                continue;
            };
            let front = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                let service = service_fn(move |request| {
                    let front = Arc::clone(&front);
                    async move { Ok::<_, Infallible>(front.handle(request).await) }
                });
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(10))
                    .max_buf_size(64 * 1024)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
                drop(permit);
            });
        }
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let reply = self.reply(request).await;
        to_response(reply)
    }

    async fn reply(&self, request: Request<Incoming>) -> Reply {
        if self.draining.load(Ordering::Relaxed) {
            return Reply::text(503, "cove-host is shutting down; try again shortly\n")
                .with_header("retry-after", "1")
                .with_header("connection", "close");
        }
        let path = request.uri().path().to_string();
        if is_ops_path(&path) {
            return match self.options.ops_listener {
                OpsListener::Public => self.ops_reply(&path, request.uri().query()),
                OpsListener::Admin => {
                    Reply::text(404, format!("cove-host has nothing at `{path}`\n"))
                }
            };
        }
        if path == "/" {
            return Reply::text(200, self.index());
        }
        let host = request
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(|host| host.split(':').next().unwrap_or(host).to_string());
        let Some(route) = self.router.route(host.as_deref(), &path) else {
            self.counters.not_found.fetch_add(1, Ordering::Relaxed);
            return Reply::text(404, "no app here; try /\n");
        };
        let found = self
            .engine
            .slot_named(&route.app)
            .and_then(|slot| Some((slot.index, slot.current()?)));
        let Some((index, app)) = found else {
            self.counters.not_found.fetch_add(1, Ordering::Relaxed);
            return Reply::text(404, format!("no app named `{}`\n", route.app));
        };
        if let AppState::Refused(why) = &app.state {
            return Reply::text(503, format!("app `{}` was not loaded: {why}\n", app.name));
        }

        let limit = app.limits.max_request_bytes;
        let too_large = || {
            app.counters.rejected(Rejection::TooLarge);
            Reply::text(
                413,
                format!(
                    "app `{}` accepts request bodies of at most {limit} bytes\n",
                    app.name
                ),
            )
        };
        let declared = request
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if declared.is_some_and(|length| length > limit as u64) {
            return too_large();
        }
        let method = request.method().as_str().to_string();
        let query: BTreeMap<String, String> = request
            .uri()
            .query()
            .map(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default();
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in request.headers() {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            headers
                .entry(name.as_str().to_string())
                .and_modify(|joined| {
                    joined.push_str(", ");
                    joined.push_str(&value);
                })
                .or_insert(value);
        }
        // How the client reached the host: see `crate::proxy`.
        self.options.forwarding.apply(&mut headers);
        // Where the app is mounted, so that it can write links to itself: the
        // host's, whatever the client sent. (Behind a proxy that mounts the
        // host under a prefix of its own, prepend it there.)
        let mount = path
            .strip_suffix(route.path.as_str())
            .unwrap_or(&path)
            .trim_end_matches('/')
            .to_string();
        headers.insert("x-forwarded-prefix".to_string(), mount);
        let body = match Limited::new(request.into_body(), limit).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => return too_large(),
            Err(error) => {
                app.counters.rejected(Rejection::BadRequest);
                return Reply::text(400, format!("cannot read the request body: {error}\n"));
            }
        };
        let Ok(body) = String::from_utf8(body.to_vec()) else {
            app.counters.rejected(Rejection::BadRequest);
            return Reply::text(400, "the request body is not UTF-8\n");
        };

        let (reply, answer) = oneshot::channel();
        let cancel = Cancel::new();
        let start = Box::new(Start {
            request: AppRequest {
                method,
                path: route.path,
                query: query.into_iter().collect(),
                headers: headers.into_iter().collect(),
                body,
            },
            flight: Flight {
                id: self.engine.next_id(),
                app: index,
                version: Arc::clone(&app),
                accepted: Instant::now(),
                reply,
                cancel: cancel.clone(),
                tally: Default::default(),
            },
        });
        if let Err(why) = self.engine.queue.admit(index, start) {
            app.counters.rejected(why);
            return match why {
                Rejection::QueueFull => Reply::text(
                    429,
                    format!(
                        "app `{}` has {} requests waiting already; try again later\n",
                        app.name, app.limits.max_queued
                    ),
                ),
                _ => Reply::text(
                    503,
                    format!(
                        "cove-host has {} requests in flight already; try again later\n",
                        self.options.max_in_flight
                    ),
                ),
            }
            .with_header("retry-after", "1");
        }
        // hyper drops this future when the client closes its connection
        // before the answer; the guard turns that drop into a cancellation.
        let guard = CancelOnDrop(Some(cancel));
        let answered = answer.await;
        guard.disarm();
        match answered {
            Ok(reply) => reply,
            Err(_) => Reply::text(
                500,
                format!(
                    "app `{}`: the request ended without an answer (the host is shutting down, \
                     or failed running it)\n",
                    app.name
                ),
            ),
        }
    }

    fn index(&self) -> String {
        let mut out = String::from("cove-host: Cove apps, one isolate per request\n\n");
        for app in self.engine.slots().iter().filter_map(|slot| slot.current()) {
            out.push_str(&format!("  /{}/  {}\n", app.name, app.describe()));
        }
        if self.options.ops_listener == OpsListener::Public {
            out.push_str("\n  /_host/ui  the operations page\n");
            out.push_str("  /_host/stats  per-app counters, queues and work, as JSON\n");
            out.push_str("  /_host/apps/<app>  one app, its versions and recent errors, as JSON\n");
            out.push_str("  /_host/apps/<app>/logs?n=200  an app's recent log lines\n");
        }
        out
    }

    /// Answers a `GET` under `/_host/` ([`crate::ops`]), on whichever
    /// listener serves them.
    pub(crate) fn ops_reply(&self, path: &str, query: Option<&str>) -> Reply {
        if path == "/_host/stats" {
            return json_reply(&ops::stats(&self.ops()));
        }
        if path == "/_host/ui" {
            return Reply {
                status: 200,
                headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
                body: ops::page(&self.ops()),
            };
        }
        if let Some(name) = path
            .strip_prefix("/_host/apps/")
            .filter(|n| !n.contains('/'))
        {
            return match self.engine.slot_named(name) {
                Some(slot) => json_reply(&ops::app_detail(&self.ops(), &slot)),
                None => Reply::text(404, format!("no app named `{name}`\n")),
            };
        }
        if let Some(name) = path
            .strip_prefix("/_host/apps/")
            .and_then(|rest| rest.strip_suffix("/logs"))
        {
            let Some(app) = self.engine.slot_named(name) else {
                return Reply::text(404, format!("no app named `{name}`\n"));
            };
            let lines = query
                .and_then(|query| {
                    form_urlencoded::parse(query.as_bytes())
                        .find(|(key, _)| key == "n")
                        .and_then(|(_, n)| n.parse().ok())
                })
                .unwrap_or(200);
            return Reply::text(200, app.logs.tail(lines));
        }
        Reply::text(404, format!("cove-host has nothing at `{path}`\n"))
    }

    pub(crate) fn ops_context(&self) -> OpsContext<'_> {
        self.ops()
    }

    fn ops(&self) -> OpsContext<'_> {
        OpsContext {
            engine: &self.engine,
            counters: &self.counters,
            started: self.started,
            max_in_flight: self.options.max_in_flight,
            data: self.options.data.as_deref(),
        }
    }

    /// Loads `name` from `<apps>/<name>` as the app's next version, off the
    /// request path (on a blocking thread, neither a worker nor the I/O
    /// runtime's), and routes to it only if it loaded. A new name is a new
    /// app. Requests already admitted — queued, running, yielded or parked —
    /// finish on the version they were admitted to.
    pub(crate) async fn update(&self, name: &str) -> Result<Installed, UpdateError> {
        let _one_at_a_time = self.updating.lock().await;
        let dir = self.options.apps.join(name);
        if !dir.join("app.toml").is_file() {
            return Err(UpdateError::NotFound(format!(
                "no app named `{name}` in `{}` (an app is a directory with an `app.toml`)",
                self.options.apps.display()
            )));
        }
        let slot = self.engine.slot_named(name);
        let current = slot
            .as_ref()
            .and_then(|slot| slot.current())
            .map(|app| app.version.clone());
        let lineage = match &slot {
            Some(slot) => Lineage {
                counters: Arc::clone(&slot.counters),
                logs: Arc::clone(&slot.logs),
                number: slot.next_version(),
            },
            None => Lineage::first(),
        };
        let load = self.load.clone();
        let owned = name.to_string();
        let app = tokio::task::spawn_blocking(move || load_as(&owned, &dir, &load, lineage))
            .await
            .map_err(|e| UpdateError::Refused {
                current: current.clone(),
                why: format!("the load failed: {e}"),
            })?;
        if let AppState::Refused(why) = &app.state {
            app.counters.updates_refused.fetch_add(1, Ordering::Relaxed);
            let line = format!(
                "update to {} refused, still serving {}: {}",
                app.version,
                current.as_deref().unwrap_or("nothing"),
                why.lines().next().unwrap_or_default()
            );
            app.logs.push("host", &line);
            eprintln!("cove-host: [{name}] {line}");
            return Err(UpdateError::Refused {
                current,
                why: why.clone(),
            });
        }
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        let installed = self.engine.install(app);
        counters.updates.fetch_add(1, Ordering::Relaxed);
        let line = format!(
            "updated: {} -> {}",
            installed.previous.as_deref().unwrap_or("nothing"),
            installed.version
        );
        logs.push("host", &line);
        eprintln!("cove-host: [{name}] {line}");
        Ok(installed)
    }

    /// Stops routing to `name`; what is in flight finishes.
    pub(crate) fn remove(&self, name: &str) -> Option<String> {
        let removed = self.engine.remove(name)?;
        if let Some(slot) = self.engine.slot_named(name) {
            slot.logs.push("host", &format!("removed (was {removed})"));
        }
        eprintln!("cove-host: [{name}] removed (was {removed})");
        Some(removed)
    }
}

/// Whether `path` is one of the host's own, under `/_host`.
pub(crate) fn is_ops_path(path: &str) -> bool {
    path == "/_host" || path.starts_with("/_host/")
}

/// A JSON reply.
pub(crate) fn json_reply(value: &Json) -> Reply {
    let mut body = serde_json::to_string_pretty(value).unwrap_or_default();
    body.push('\n');
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body,
    }
}

/// Cancels its request when dropped armed: when the connection's task drops
/// the request's future because the client went away.
struct CancelOnDrop(Option<Cancel>);

impl CancelOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}

/// A reply as the hyper response.
pub(crate) fn to_response(reply: Reply) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(reply.status);
    for (name, value) in &reply.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Full::new(Bytes::from(reply.body)))
        .unwrap_or_else(|error| {
            let mut response = Response::new(Full::new(Bytes::from(format!(
                "cove-host: cannot build the response: {error}\n"
            ))));
            *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}
