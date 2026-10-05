//! `fetch`: outbound HTTP, to the hosts an app's allowlist names.
//!
//! | operation | answers |
//! | --- | --- |
//! | `fetch.get(url)` | `Result<fetch.Response, Error>` |
//! | `fetch.request(method, url, headers, body)` | the same, for any method, with `Map<String, String>` headers and a `String` body |
//!
//! `fetch.Response` is `{ status: Int, headers: Map<String, String>, body:
//! String }`; header names are lowercased and a repeated header's values
//! joined with `, `; a body that is not UTF-8 is decoded lossily. Any
//! response is `Ok`, whatever its status: an `Err` is a fetch that did not
//! get one — refused, unreachable, too slow, too large.
//!
//! # Where it may go
//!
//! `[fetch] allow` in `app.toml` lists `scheme://host[:port|:*]`; the port
//! defaults to the scheme's. The `fetch` capability says an app may make
//! outbound calls at all, the allowlist says to where, and it is held here,
//! at the boundary: a URL off the list is answered `Err` at once, **before
//! anything is sent**, and without parking. Redirects are not followed — a
//! redirect is answered as the response it is — so an allowed host cannot
//! send the request somewhere else. Hosts are matched by name as written;
//! what that name resolves to is the resolver's (no DNS-rebinding defence).
//!
//! # Waiting
//!
//! A fetch answers **pending**: the run parks, and the request runs on the
//! I/O runtime as a [`PendingWork`] future (reqwest over hyper). The
//! scheduler races it against the run's deadline and the client's
//! connection; whichever ends first drops the future, which closes the
//! outbound connection — the upstream sees the request abandoned. Each fetch
//! is also bounded by `[fetch] timeout` and `max_response_bytes`, and its body
//! by `max_request_bytes`.
//!
//! # https
//!
//! Supported, with rustls and the Mozilla root set (`webpki-roots`), so
//! there is no dependency on the system's OpenSSL. Inbound TLS stays the
//! reverse proxy's; outbound has no proxy to do it, and an app talking to
//! an API needs it.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use cove_runtime::value::MapKey;
use cove_runtime::{
    Effect, FieldSchema, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry,
    RuntimeError, Transfer, TypeSchema, Value,
};

use crate::config::FetchPolicy;
use crate::hosts::{AppContext, HostModule, PendingWork};
use crate::stats::AppCounters;

const STRING_MAP: HostType = HostType::Map(&HostType::String, &HostType::String);
const ANSWER: HostType = HostType::Result(&HostType::Named("fetch.Response"), &HostType::Error);

const fn op(name: &'static str, params: &'static [HostType], effect: Effect) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result: ANSWER,
        capability: "fetch",
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

/// The `fetch` module.
pub const FETCH: ModuleSchema = ModuleSchema {
    name: "fetch",
    capability: "fetch",
    operations: &[
        op("get", &[HostType::String], Effect::Read),
        op(
            "request",
            &[
                HostType::String,
                HostType::String,
                STRING_MAP,
                HostType::String,
            ],
            Effect::IrreversibleWrite,
        ),
    ],
    types: &[TypeSchema {
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
    }],
    resources: &[],
};

pub(crate) struct FetchModule;

impl HostModule for FetchModule {
    fn schema(&self) -> ModuleSchema {
        FETCH
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .connect_timeout(app.fetch.timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|e| format!("cannot build its fetch client: {e}"))?;
        Ok(Box::new(FetchHost {
            app: app.app.clone(),
            client,
            policy: Arc::new(app.fetch.clone()),
            counters: Arc::clone(&app.counters),
            io: app.io.clone(),
        }))
    }
}

struct FetchHost {
    app: String,
    client: reqwest::Client,
    policy: Arc<FetchPolicy>,
    counters: Arc<AppCounters>,
    io: tokio::runtime::Handle,
}

/// A response, as the host holds it until it becomes a `fetch.Response`.
struct Fetched {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl FetchHost {
    /// The request the call asks for, if the allowlist and the limits admit
    /// it; otherwise the `Err` message the app is answered with.
    fn admit(&self, op: &str, args: &[Value]) -> Result<reqwest::Request, String> {
        let text = |at: usize| args.get(at).and_then(Value::as_str).unwrap_or_default();
        let (method, url, body) = match op {
            "get" => ("GET", text(0), ""),
            _ => (text(0), text(1), text(3)),
        };
        let url = reqwest::Url::parse(url).map_err(|e| format!("`{url}` is not a URL: {e}"))?;
        let scheme = url.scheme();
        let host = url.host_str().unwrap_or_default();
        let port = url.port_or_known_default().unwrap_or(0);
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!("`{url}`: credentials in a URL are not sent"));
        }
        if !self.policy.admits(scheme, host, port) {
            let allowed: Vec<String> = self.policy.allow.iter().map(ToString::to_string).collect();
            return Err(format!(
                "`{scheme}://{host}:{port}` is not on app `{}`'s fetch allowlist ({})",
                self.app,
                if allowed.is_empty() {
                    "it has none: see `[fetch] allow` in app.toml".to_string()
                } else {
                    allowed.join(", ")
                }
            ));
        }
        if body.len() > self.policy.max_request_bytes {
            return Err(format!(
                "a request body of {} bytes is above app `{}`'s fetch max_request_bytes of {}",
                body.len(),
                self.app,
                self.policy.max_request_bytes
            ));
        }
        let method = reqwest::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
            .map_err(|_| format!("`{method}` is not an HTTP method"))?;
        let mut request = self
            .client
            .request(method, url)
            .timeout(self.policy.timeout)
            .body(body.to_string());
        if let Some(headers) = args
            .get(2)
            .filter(|_| op == "request")
            .and_then(Value::entries)
        {
            for (name, value) in headers {
                let MapKey::Str(name) = name else {
                    return Err("a header name is not a String".to_string());
                };
                let value = value.as_str().unwrap_or_default();
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| format!("`{name}` is not a header name"))?;
                let value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| format!("the value of header `{name}` cannot be sent"))?;
                request = request.header(name, value);
            }
        }
        request.build().map_err(|e| e.to_string())
    }

    /// The fetch itself: runs on the I/O runtime.
    fn perform(
        &self,
        request: reqwest::Request,
    ) -> impl std::future::Future<Output = Result<Transfer, RuntimeError>> + Send + 'static {
        let client = self.client.clone();
        let policy = Arc::clone(&self.policy);
        let counters = Arc::clone(&self.counters);
        let url = request.url().to_string();
        async move {
            let fetched = fetch(&client, request, &policy).await;
            if fetched.is_err() {
                counters.fetch_errors.fetch_add(1, Ordering::Relaxed);
            }
            Ok(answer(&url, fetched))
        }
    }
}

async fn fetch(
    client: &reqwest::Client,
    request: reqwest::Request,
    policy: &FetchPolicy,
) -> Result<Fetched, String> {
    let mut response = client.execute(request).await.map_err(describe)?;
    let status = response.status().as_u16();
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in response.headers() {
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        match headers.iter_mut().find(|(n, _)| n == name.as_str()) {
            Some((_, joined)) => {
                joined.push_str(", ");
                joined.push_str(&value);
            }
            None => headers.push((name.as_str().to_string(), value)),
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(describe)? {
        if body.len() + chunk.len() > policy.max_response_bytes {
            return Err(format!(
                "the response is larger than this app's fetch max_response_bytes of {}",
                policy.max_response_bytes
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Fetched {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// A reqwest error as the app reads it: what happened, and the cause.
fn describe(error: reqwest::Error) -> String {
    let what = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "failed"
    };
    let mut text = format!("fetch {what}: {error}");
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}

/// A fetch's outcome as the `Result<fetch.Response, Error>` a parked run
/// is resumed with, built as a [`Transfer`] where it is made — on the I/O
/// runtime, across `.await`s, where an `Rc`-based [`Value`] cannot be held.
fn answer(url: &str, fetched: Result<Fetched, String>) -> Transfer {
    match fetched {
        Ok(fetched) => Transfer::ok(Transfer::structure(
            "fetch.Response",
            [
                ("status", Transfer::Int(i64::from(fetched.status))),
                (
                    "headers",
                    Transfer::Map(
                        fetched
                            .headers
                            .into_iter()
                            .map(|(k, v)| (MapKey::Str(k), Transfer::string(v)))
                            .collect(),
                    ),
                ),
                ("body", Transfer::string(fetched.body)),
            ],
        )),
        Err(why) => Transfer::err(Transfer::error(format!("{url}: {why}"))),
    }
}

impl HostApi for FetchHost {
    fn module_schema(&self) -> ModuleSchema {
        FETCH
    }

    /// The blocking answer, for a call made where the run cannot park: the
    /// same fetch, waited for on the worker (counted). `cove-host test`
    /// answers every call this way.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.counters.fetches.fetch_add(1, Ordering::Relaxed);
        let request = match self.admit(op, &args) {
            Ok(request) => request,
            Err(why) => {
                self.counters.fetch_refused.fetch_add(1, Ordering::Relaxed);
                return Ok(Value::err(Value::error(why)));
            }
        };
        self.counters
            .blocking_host_calls
            .fetch_add(1, Ordering::Relaxed);
        self.io
            .block_on(self.perform(request))
            .map(Transfer::into_value)
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        self.counters.fetches.fetch_add(1, Ordering::Relaxed);
        match self.admit(op, &args) {
            Ok(request) => PendingWork::new("fetch", self.perform(request)).answer(),
            // Refused before anything is sent, and answered at once.
            Err(why) => {
                self.counters.fetch_refused.fetch_add(1, Ordering::Relaxed);
                HostAnswer::Ready(Ok(Value::err(Value::error(why))))
            }
        }
    }
}
