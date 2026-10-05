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

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
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
use serde_json::{json, Map, Value as Json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, Semaphore};

use crate::apps::{list, load_all, App, AppState, Backend, LoadOptions};
use crate::convert::{AppRequest, Reply};
use crate::hosts::HostModules;
use crate::router::{PathPrefix, Router};
use crate::sched::{Engine, Flight, Start};
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
        }
    }
}

/// What the connection tasks share.
struct Front {
    engine: Arc<Engine>,
    router: Box<dyn Router>,
    counters: ServerCounters,
    connections: Arc<Semaphore>,
    started: Instant,
    options: ServeOptions,
}

/// A running host.
pub struct Host {
    /// Where it listens.
    pub addr: SocketAddr,
    front: Arc<Front>,
    runtime: Option<tokio::runtime::Runtime>,
    _workers: Vec<JoinHandle<()>>,
}

impl Host {
    /// Loads the apps, binds, and starts the threads. Returns once the host
    /// is accepting. An app that is refused is reported and not served; the
    /// error is only for a host that cannot start at all.
    pub fn start(options: ServeOptions) -> Result<Host, String> {
        let load = LoadOptions {
            backend: options.backend,
            quiet: options.quiet,
            modules: options.modules.clone(),
        };
        let apps = load_all(&options.apps, &load)?;
        Host::with_apps(apps, options)
    }

    /// Starts a host over apps already loaded.
    pub fn with_apps(apps: Vec<App>, options: ServeOptions) -> Result<Host, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(options.io_threads.max(1))
            .thread_name("cove-host-io")
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the I/O runtime: {e}"))?;
        let listener = std::net::TcpListener::bind(&options.addr)
            .map_err(|e| format!("cannot listen on `{}`: {e}", options.addr))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let router = PathPrefix::new(apps.iter().map(|app| app.name.as_str()));
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
        });
        let listener = {
            let _entered = runtime.enter();
            tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?
        };
        runtime.spawn(Arc::clone(&front).accept(listener));
        Ok(Host {
            addr,
            front,
            runtime: Some(runtime),
            _workers: workers,
        })
    }

    /// Every app, loaded or refused.
    pub fn apps(&self) -> &[App] {
        &self.front.engine.apps
    }

    /// What `GET /_host/stats` answers.
    pub fn stats(&self) -> Json {
        self.front.stats()
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
        out.push_str(&format!(
            "  stats: curl -s http://{}/_host/stats\n",
            self.addr
        ));
        out
    }

    /// Blocks until Ctrl-C.
    pub fn wait_for_ctrl_c(&self) {
        if let Some(runtime) = &self.runtime {
            let _ = runtime.block_on(tokio::signal::ctrl_c());
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
        let path = request.uri().path().to_string();
        if path == "/_host/stats" {
            let mut body = serde_json::to_string_pretty(&self.stats()).unwrap_or_default();
            body.push('\n');
            return Reply {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            };
        }
        if path == "/" {
            return Reply::text(200, self.index());
        }
        if path == "/_host" || path.starts_with("/_host/") {
            return Reply::text(404, format!("cove-host has nothing at `{path}`\n"));
        }
        let host = request
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(|host| host.split(':').next().unwrap_or(host).to_string());
        let Some(route) = self.router.route(host.as_deref(), &path) else {
            self.counters.not_found.fetch_add(1, Ordering::Relaxed);
            let name = self.router.asked_for(host.as_deref(), &path);
            return Reply::text(404, format!("no app named `{name}`\n"));
        };
        let app = &self.engine.apps[route.app];
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
                app: route.app,
                accepted: Instant::now(),
                reply,
            },
        });
        if let Err(why) = self.engine.queue.admit(route.app, start) {
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
        match answer.await {
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
        for app in &self.engine.apps {
            out.push_str(&format!("  /{}/  {}\n", app.name, app.describe()));
        }
        out.push_str("\n  /_host/stats  per-app counters, queues and work, as JSON\n");
        out
    }

    fn stats(&self) -> Json {
        let engine = &self.engine;
        let mut apps = Map::new();
        let mut totals: BTreeMap<&str, f64> = BTreeMap::new();
        for (at, app) in engine.apps.iter().enumerate() {
            let (in_flight, queued) = engine.queue.gauges(at);
            let mut entry = app.counters.to_json(in_flight, queued);
            for key in [
                "served",
                "ok",
                "in_flight",
                "queued",
                "parked",
                "parks",
                "yields",
                "yield_requests",
                "yields_declined",
                "overdue_yields",
                "blocking_host_calls",
                "instructions",
                "fuel",
                "worker_ms",
            ] {
                *totals.entry(key).or_default() += entry[key].as_f64().unwrap_or_default();
            }
            for group in ["errors", "rejected"] {
                let sum: f64 = entry[group]
                    .as_object()
                    .map(|counts| counts.values().filter_map(Json::as_f64).sum())
                    .unwrap_or_default();
                *totals.entry(group).or_default() += sum;
            }
            let object = entry.as_object_mut().expect("an object");
            let (state, tier, reason) = match &app.state {
                AppState::Ready(ready) => ("ready", Json::from(ready.tier), Json::Null),
                AppState::Refused(why) => ("refused", Json::Null, Json::from(why.as_str())),
            };
            object.insert("state".into(), json!(state));
            object.insert("tier".into(), tier);
            object.insert("refused".into(), reason);
            object.insert("required".into(), json!(list(&app.required)));
            object.insert("granted".into(), json!(list(&app.granted)));
            apps.insert(app.name.clone(), entry);
        }
        let totals: Map<String, Json> = totals
            .into_iter()
            .map(|(key, value)| {
                let value = if key == "worker_ms" {
                    json!(value)
                } else {
                    json!(value as u64)
                };
                (key.to_string(), value)
            })
            .collect();
        let read = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
        json!({
            "uptime_s": self.started.elapsed().as_secs_f64(),
            "workers": engine.workers(),
            "slice_ms": engine.slice.map(|slice| slice.as_secs_f64() * 1e3),
            "connections": read(&self.counters.connections),
            "rejected_connections": read(&self.counters.rejected_connections),
            "not_found": read(&self.counters.not_found),
            "admitted": engine.queue.admitted(),
            "max_in_flight": self.options.max_in_flight,
            "totals": totals,
            "apps": apps,
        })
    }
}

/// A reply as the hyper response.
fn to_response(reply: Reply) -> Response<Full<Bytes>> {
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
