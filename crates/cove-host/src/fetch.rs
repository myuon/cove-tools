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
//! get one — refused, unreachable, too slow, too large, undecodable.
//!
//! # Content encoding
//!
//! A request is sent with `accept-encoding: gzip, deflate, br` unless the app
//! set its own `accept-encoding`. A response whose `content-encoding` is
//! `gzip`, `deflate` or `br` is decoded — whatever the request asked for,
//! since some servers compress regardless — and the app sees the decoded
//! body, with `content-encoding` and `content-length` (which describe the
//! encoded one) taken out of its headers. Decoding streams, and
//! `max_response_bytes` bounds the *decoded* body: a fetch stops as soon as
//! it is passed, so a compression bomb is refused without being inflated. A
//! body that does not decode is an `Err`, not text. Any other encoding is
//! handed over as it came, header and all.
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
//! # Secret headers
//!
//! `[fetch.headers."<origin>"]` binds a header to a `[secrets]` entry, with
//! an optional literal prefix (`authorization = { secret = "openai", prefix
//! = "Bearer " }`). The origin must be an entry of `[fetch] allow` as
//! `app.toml` writes it, or the app is refused at load. The header is added
//! here, after the app's own headers, to a request whose URL has exactly
//! that origin — scheme, host and effective port — and replaces an
//! app-supplied header of the same name. The app never holds the value: it
//! is not in a `fetch.Response`, an `Err`, a log line or the host's stats.
//! Since redirects are not followed, it cannot be carried to another origin
//! by one; and an admin override of the allowlist cannot give it a new
//! origin — taking the origin off the list refuses requests there, as for
//! any origin off the list.
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
/// The `accept-encoding` sent when the app sets none: what the client
/// decodes (reqwest's `gzip`, `deflate` and `brotli` features).
const ACCEPT_ENCODING: &str = "gzip, deflate, br";
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
        let client = client(&app.fetch)?;
        Ok(Box::new(FetchHost {
            app: app.app.clone(),
            client,
            policy: Arc::new(app.fetch.clone()),
            counters: Arc::clone(&app.counters),
            io: app.io.clone(),
        }))
    }
}

/// An app's client: no redirects, rustls, and `gzip`, `deflate` and `br`
/// decoded (reqwest's defaults once those features are on).
fn client(policy: &FetchPolicy) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls()
        .connect_timeout(policy.timeout.min(Duration::from_secs(10)))
        .build()
        .map_err(|e| format!("cannot build its fetch client: {e}"))
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
        // The request URL's exact origin: scheme, host and effective port.
        let injected: Vec<_> = self
            .policy
            .headers
            .iter()
            .filter(|injected| injected.applies_to(scheme, host, port))
            .collect();
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
        let mut request = request.build().map_err(|e| e.to_string())?;
        // What the client decodes. reqwest would add its own spelling of it;
        // the app's own `accept-encoding` is sent as written.
        if let reqwest::header::Entry::Vacant(entry) = request
            .headers_mut()
            .entry(reqwest::header::ACCEPT_ENCODING)
        {
            entry.insert(reqwest::header::HeaderValue::from_static(ACCEPT_ENCODING));
        }
        // `[fetch.headers]`: added last, so a header of the same name the app
        // supplied is replaced, not sent beside it. Nothing the app is
        // answered with is built from these values.
        for injected in injected {
            request
                .headers_mut()
                .insert(injected.name.clone(), injected.value.clone());
        }
        Ok(request)
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
    // The client decodes `content-encoding` as the body streams, and takes
    // that header and `content-length` out of the response: the chunks below
    // are the decoded body, so the limit holds on what the app would get.
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
    } else if error.is_decode() {
        "could not decode the response"
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;
    use crate::config::AllowRule;

    const TEXT: &str = "The decoded body: ünïcödé, and long enough to compress. \
                        The decoded body: ünïcödé, and long enough to compress.";

    /// A one-shot upstream on 127.0.0.1: reads one request, sends its head
    /// (request line and headers) back on the channel, and answers with
    /// `headers` (each `name: value\r\n`), a `content-length` and `body`.
    fn upstream(headers: &str, body: Vec<u8>) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let headers = headers.to_string();
        let (heads, received) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let _ = heads.send(head);
            let mut stream = stream;
            // A client that stopped reading closes the connection: a failed
            // write is that, not a test failure.
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(&body);
        });
        (port, received)
    }

    /// A host for an app allowed to reach any port on 127.0.0.1.
    fn host(io: &tokio::runtime::Runtime, max_response_bytes: usize) -> FetchHost {
        let policy = FetchPolicy {
            allow: vec![AllowRule::parse("http://127.0.0.1:*").unwrap()],
            max_response_bytes,
            ..FetchPolicy::default()
        };
        FetchHost {
            app: "test".to_string(),
            client: client(&policy).unwrap(),
            policy: Arc::new(policy),
            counters: Arc::default(),
            io: io.handle().clone(),
        }
    }

    /// `fetch.request("GET", url, headers, "")`, as far as the response the
    /// host holds, and the request head the upstream received.
    fn get(
        port: u16,
        received: &mpsc::Receiver<String>,
        headers: &[(&str, &str)],
        max_response_bytes: usize,
    ) -> (Result<Fetched, String>, String) {
        let io = tokio::runtime::Runtime::new().unwrap();
        let host = host(&io, max_response_bytes);
        let headers = Value::map(
            headers
                .iter()
                .map(|(name, value)| (MapKey::Str(name.to_string()), Value::string(*value))),
        );
        let args = [
            Value::string("GET"),
            Value::string(format!("http://127.0.0.1:{port}/")),
            headers,
            Value::string(""),
        ];
        let request = host.admit("request", &args).unwrap();
        let fetched = io.block_on(fetch(&host.client, request, &host.policy));
        let head = received.recv().unwrap_or_default();
        (fetched, head)
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn deflate(bytes: &[u8]) -> Vec<u8> {
        // HTTP's `deflate` is the zlib format (RFC 9110 8.4.1.2).
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
            encoder.write_all(bytes).unwrap();
        }
        out
    }

    #[test]
    fn an_encoded_body_is_decoded_and_its_headers_dropped() {
        for (encoding, body) in [
            ("gzip", gzip(TEXT.as_bytes())),
            ("deflate", deflate(TEXT.as_bytes())),
            ("br", brotli(TEXT.as_bytes())),
        ] {
            let (port, received) = upstream(
                &format!("content-type: text/plain\r\ncontent-encoding: {encoding}\r\n"),
                body,
            );
            let (fetched, head) = get(port, &received, &[], 1 << 20);
            let fetched = fetched.unwrap_or_else(|e| panic!("{encoding}: {e}"));
            assert_eq!(fetched.body, TEXT, "{encoding}");
            let names: Vec<&str> = fetched.headers.iter().map(|(n, _)| n.as_str()).collect();
            assert!(
                !names.contains(&"content-encoding"),
                "{encoding}: {names:?}"
            );
            assert!(!names.contains(&"content-length"), "{encoding}: {names:?}");
            assert!(names.contains(&"content-type"), "{encoding}: {names:?}");
            assert_eq!(
                header(&head, "accept-encoding"),
                Some(ACCEPT_ENCODING),
                "{head}"
            );
        }
    }

    #[test]
    fn an_identity_body_is_handed_over_as_it_came() {
        let (port, received) = upstream("content-type: text/plain\r\n", TEXT.as_bytes().to_vec());
        let (fetched, head) = get(port, &received, &[], 1 << 20);
        let fetched = fetched.unwrap();
        assert_eq!(fetched.body, TEXT);
        let length = TEXT.len().to_string();
        assert!(
            fetched
                .headers
                .iter()
                .any(|(n, v)| n == "content-length" && *v == length),
            "{:?}",
            fetched.headers
        );
        assert_eq!(
            header(&head, "accept-encoding"),
            Some(ACCEPT_ENCODING),
            "{head}"
        );
    }

    #[test]
    fn the_apps_own_accept_encoding_is_sent_as_written() {
        let (port, received) = upstream("", TEXT.as_bytes().to_vec());
        let (fetched, head) = get(port, &received, &[("accept-encoding", "identity")], 1 << 20);
        assert_eq!(fetched.unwrap().body, TEXT);
        assert_eq!(header(&head, "accept-encoding"), Some("identity"), "{head}");

        // A server that compresses anyway is still decoded.
        let (port, received) = upstream("content-encoding: gzip\r\n", gzip(TEXT.as_bytes()));
        let (fetched, head) = get(port, &received, &[("accept-encoding", "identity")], 1 << 20);
        assert_eq!(fetched.unwrap().body, TEXT);
        assert_eq!(header(&head, "accept-encoding"), Some("identity"), "{head}");
    }

    #[test]
    fn a_body_that_does_not_decode_is_an_error() {
        for encoding in ["gzip", "deflate", "br"] {
            let (port, received) = upstream(
                &format!("content-encoding: {encoding}\r\n"),
                b"this is not compressed at all, whatever the header says".to_vec(),
            );
            let (fetched, _) = get(port, &received, &[], 1 << 20);
            let error = match fetched {
                Ok(fetched) => panic!("{encoding}: answered {:?}", fetched.body),
                Err(error) => error,
            };
            assert!(
                error.contains("could not decode the response"),
                "{encoding}: {error}"
            );
        }
    }

    /// The process's peak resident set, in bytes.
    #[cfg(unix)]
    fn peak_rss() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: `getrusage` fills the struct it is given.
        let usage = unsafe {
            libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
            usage.assume_init()
        };
        let peak = u64::try_from(usage.ru_maxrss).unwrap_or(0);
        // Bytes on macOS, KiB elsewhere.
        if cfg!(target_os = "macos") {
            peak
        } else {
            peak * 1024
        }
    }

    #[test]
    fn a_compression_bomb_is_refused_without_being_inflated() {
        // 1 GiB of zeros in about 1 MiB of gzip, one member (the decoder
        // refuses bytes after the first).
        let zeros = vec![0; 1 << 20];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        for _ in 0..1024 {
            encoder.write_all(&zeros).unwrap();
        }
        let bomb = encoder.finish().unwrap();
        assert!(bomb.len() < 8 << 20, "{}", bomb.len());
        let (port, received) = upstream("content-encoding: gzip\r\n", bomb);

        #[cfg(unix)]
        let before = peak_rss();
        let started = Instant::now();
        let (fetched, _) = get(port, &received, &[], 1 << 20);
        let took = started.elapsed();
        let error = match fetched {
            Ok(fetched) => panic!("answered {} bytes", fetched.body.len()),
            Err(error) => error,
        };
        assert!(error.contains("max_response_bytes of 1048576"), "{error}");
        // Inflating it would hold 1 GiB and take seconds; stopping at the
        // limit holds about 1 MiB and takes milliseconds.
        assert!(took < Duration::from_secs(5), "{took:?}");
        #[cfg(unix)]
        {
            let grew = peak_rss().saturating_sub(before);
            assert!(grew < 128 << 20, "peak resident set grew {grew} bytes");
        }
    }
}
