//! The host's services: the persistent `kv`, outbound `fetch`, and a client
//! going away. As in `host.rs`, nothing asserts a duration; a test waits for
//! the host's stats, or the test upstream's own record, to say what happened.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use common::*;

// ------------------------------------------------------------------ kv

#[test]
fn an_apps_keys_are_its_own() {
    let apps = apps(&[
        sample("notes"),
        AppSpec {
            name: "other",
            from: samples().join("notes"),
            config: None,
        },
    ]);
    let host = start(&apps, 2);
    let addr = host.addr;
    assert_eq!(
        send(addr, "PUT", "/notes/secret", "for notes only").status,
        201
    );
    assert_eq!(get(addr, "/notes/secret").body, "for notes only");
    // The same key, in the other app: not there, not listed, and writing it
    // does not touch the first app's.
    assert_eq!(get(addr, "/other/secret").status, 404);
    assert_eq!(get(addr, "/other/").body, "");
    assert_eq!(send(addr, "PUT", "/other/secret", "other's").status, 201);
    assert_eq!(get(addr, "/notes/secret").body, "for notes only");
    assert_eq!(get(addr, "/other/secret").body, "other's");
    // One store per app, under its own name.
    assert!(apps.data.join("notes/kv.sqlite3").is_file());
    assert!(apps.data.join("other/kv.sqlite3").is_file());
}

#[test]
fn the_store_survives_a_restart() {
    let apps = apps(&[sample("notes")]);
    {
        let host = start(&apps, 1);
        assert_eq!(
            send(host.addr, "PUT", "/notes/kept", "across restarts").status,
            201
        );
        assert_eq!(send(host.addr, "PUT", "/notes/gone", "deleted").status, 201);
        assert_eq!(send(host.addr, "DELETE", "/notes/gone", "").status, 200);
    }
    let host = start(&apps, 1);
    assert_eq!(get(host.addr, "/notes/kept").body, "across restarts");
    assert_eq!(get(host.addr, "/notes/gone").status, 404);
    assert_eq!(get(host.addr, "/notes/").body, "kept\n");
}

#[test]
fn a_quota_is_the_apps_err() {
    let apps = apps(&[sample_with(
        "notes",
        "grant = [\"kv\", \"log\"]\n[kv]\nmax_value_bytes = 8\nmax_keys = 2\n",
    )]);
    let host = start(&apps, 1);
    let addr = host.addr;
    let big = send(addr, "PUT", "/notes/a", "nine bytes");
    assert_eq!(big.status, 413);
    assert!(
        big.body.contains("above this app's max_value_bytes of 8"),
        "{}",
        big.body
    );
    assert_eq!(send(addr, "PUT", "/notes/a", "1").status, 201);
    assert_eq!(send(addr, "PUT", "/notes/b", "2").status, 201);
    let third = send(addr, "PUT", "/notes/c", "3");
    assert_eq!(third.status, 413);
    assert!(third.body.contains("max_keys"), "{}", third.body);
    // Replacing a key is not a new key.
    assert_eq!(send(addr, "PUT", "/notes/a", "11").status, 201);
    assert_eq!(get(addr, "/notes/").body, "a\nb\n");
}

#[test]
fn notes_list_in_pages_both_ways() {
    let apps = apps(&[sample("notes")]);
    let host = start(&apps, 1);
    let addr = host.addr;
    for name in ["e1", "e2", "e3", "x1"] {
        assert_eq!(
            send(addr, "PUT", &format!("/notes/{name}"), name).status,
            201
        );
    }
    assert_eq!(get(addr, "/notes/?prefix=e&limit=2").body, "e1\ne2\n");
    assert_eq!(get(addr, "/notes/?prefix=e&after=e2").body, "e3\n");
    assert_eq!(
        get(addr, "/notes/?prefix=e&order=desc&limit=2").body,
        "e3\ne2\n"
    );
}

// --------------------------------------------------------------- fetch

/// What the test upstream does with a request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Answers 200, echoing the method, the `x-test` header and the body.
    Echo,
    /// Reads the request and never answers; notes when the connection is
    /// closed on it.
    Hang,
    /// Echo's answer, gzipped, with `content-encoding: gzip`.
    Gzip,
}

/// A local HTTP server the tests fetch from, recording what reached it.
struct Upstream {
    addr: SocketAddr,
    connections: Arc<AtomicU64>,
    /// Requests read in full.
    requests: Arc<AtomicU64>,
    /// Connections the other side closed while this one held its answer.
    abandoned: Arc<AtomicU64>,
    seen: Arc<Mutex<Vec<String>>>,
    /// Each request's head, request line and headers, as it arrived.
    heads: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    fn start(mode: Mode) -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = Upstream {
            addr: listener.local_addr().unwrap(),
            connections: Arc::default(),
            requests: Arc::default(),
            abandoned: Arc::default(),
            seen: Arc::default(),
            heads: Arc::default(),
        };
        let (connections, requests, abandoned, seen, heads) = (
            Arc::clone(&upstream.connections),
            Arc::clone(&upstream.requests),
            Arc::clone(&upstream.abandoned),
            Arc::clone(&upstream.seen),
            Arc::clone(&upstream.heads),
        );
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                connections.fetch_add(1, Ordering::SeqCst);
                let (requests, abandoned, seen, heads) = (
                    Arc::clone(&requests),
                    Arc::clone(&abandoned),
                    Arc::clone(&seen),
                    Arc::clone(&heads),
                );
                thread::spawn(move || serve(stream, mode, &requests, &abandoned, &seen, &heads));
            }
        });
        upstream
    }

    fn count(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::SeqCst)
    }
}

fn serve(
    stream: TcpStream,
    mode: Mode,
    requests: &AtomicU64,
    abandoned: &AtomicU64,
    seen: &Mutex<Vec<String>>,
    heads: &Mutex<Vec<String>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let end = line == "\r\n";
        head.push_str(&line);
        if end {
            break;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body = String::from_utf8(body).unwrap();
    let method = head.split(' ').next().unwrap_or_default().to_string();
    let x_test = head
        .lines()
        .find_map(|line| line.strip_prefix("x-test: "))
        .unwrap_or("-")
        .to_string();
    heads.lock().unwrap().push(head.clone());
    seen.lock()
        .unwrap()
        .push(format!("{method} x-test={x_test} body={body}"));
    requests.fetch_add(1, Ordering::SeqCst);
    let mut stream = stream;
    match mode {
        Mode::Echo => {
            let answer = format!("{method} x-test={x_test} body={body}");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/x-echo\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
        Mode::Gzip => {
            let answer = format!("{method} x-test={x_test} body={body}");
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(answer.as_bytes()).unwrap();
            let answer = encoder.finish().unwrap();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/x-echo\r\ncontent-encoding: gzip\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            let _ = stream.write_all(&answer);
        }
        Mode::Hang => {
            // Nothing is ever sent; a read answers 0 once the fetch's
            // connection is closed.
            let mut byte = [0];
            if matches!(reader.read(&mut byte), Ok(0) | Err(_)) {
                abandoned.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

/// The proxy sample, allowed to reach `allow` only, under `limits`.
fn proxy_config(allow: &str, limits: &str) -> String {
    format!("grant = [\"fetch\", \"log\"]\n[limits]\n{limits}\n[fetch]\nallow = [\"{allow}\"]\n")
}

#[test]
fn fetch_reaches_an_allowed_upstream_with_get_and_post() {
    let upstream = Upstream::start(Mode::Echo);
    let config = proxy_config(&format!("http://127.0.0.1:{}", upstream.addr.port()), "");
    let apps = apps(&[sample_with("proxy", &config)]);
    let host = start(&apps, 2);
    let target = format!("http://127.0.0.1:{}/x", upstream.addr.port());

    let got = get(host.addr, &format!("/proxy/?url={target}"));
    assert_eq!(got.status, 200, "{got:?}");
    assert_eq!(got.body, "GET x-test=- body=");
    assert_eq!(got.header("content-type"), Some("text/x-echo"));

    let posted = send_raw(
        host.addr,
        format!(
            "POST /proxy/?url={target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             x-test: resent\r\nContent-Length: 11\r\n\r\n{{\"a\": true}}"
        )
        .as_bytes(),
    );
    assert_eq!(posted.status, 200, "{posted:?}");
    assert_eq!(posted.body, "POST x-test=resent body={\"a\": true}");
    assert_eq!(
        *upstream.seen.lock().unwrap(),
        [
            "GET x-test=- body=",
            "POST x-test=resent body={\"a\": true}"
        ]
    );
    assert_eq!(count(&host, "proxy", "parks"), 2);
    assert_eq!(count(&host, "proxy", "fetch.calls"), 2);
}

#[test]
fn a_gzipped_answer_reaches_the_app_decoded() {
    let upstream = Upstream::start(Mode::Gzip);
    let config = proxy_config(&format!("http://127.0.0.1:{}", upstream.addr.port()), "");
    let apps = apps(&[sample_with("proxy", &config)]);
    let host = start(&apps, 1);
    let target = format!("http://127.0.0.1:{}/x", upstream.addr.port());
    let got = get(host.addr, &format!("/proxy/?url={target}"));
    assert_eq!(got.status, 200, "{got:?}");
    assert_eq!(got.body, "GET x-test=- body=");
    let head = upstream.heads.lock().unwrap()[0].clone();
    assert_eq!(
        header_in(&head, "accept-encoding"),
        Some("gzip, deflate, br"),
        "{head}"
    );
}

#[test]
fn a_target_off_the_allowlist_is_refused_without_a_connection() {
    let upstream = Upstream::start(Mode::Echo);
    // Allowed: another port on the same host, and the same port over https.
    let config = format!(
        "grant = [\"fetch\", \"log\"]\n[fetch]\nallow = [\"http://127.0.0.1:{}\", \"https://127.0.0.1:{}\"]\n",
        upstream.addr.port() + 1,
        upstream.addr.port()
    );
    let apps = apps(&[sample_with("proxy", &config)]);
    let host = start(&apps, 1);
    for url in [
        format!("http://127.0.0.1:{}/", upstream.addr.port()),
        format!("http://localhost:{}/", upstream.addr.port()),
    ] {
        let refused = get(host.addr, &format!("/proxy/?url={url}"));
        assert_eq!(refused.status, 502);
        assert!(
            refused
                .body
                .contains("is not on app `proxy`'s fetch allowlist"),
            "{}",
            refused.body
        );
    }
    assert_eq!(Upstream::count(&upstream.connections), 0);
    assert_eq!(count(&host, "proxy", "fetch.refused"), 2);
    // Refused at once: the run never parked.
    assert_eq!(count(&host, "proxy", "parks"), 0);
}

/// The value of `name` in a request head, if it was sent.
fn header_in<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

#[test]
fn a_secret_header_reaches_its_origin_only_and_never_the_app() {
    const KEY: &str = "sk-cove-host-test-0123456789";
    let keyed = Upstream::start(Mode::Echo);
    let other = Upstream::start(Mode::Echo);
    // An origin with the header that nothing listens on: the fetch fails.
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let origin = |addr: SocketAddr| format!("http://127.0.0.1:{}", addr.port());
    let config = format!(
        "grant = [\"fetch\", \"log\"]\n\
         [secrets]\nopenai = {{ value = \"{KEY}\" }}\n\
         [fetch]\nallow = [\"{}\", \"{}\", \"{}\"]\n\
         [fetch.headers.\"{}\"]\n\
         authorization = {{ secret = \"openai\", prefix = \"Bearer \" }}\n\
         x-goog-api-key = {{ secret = \"openai\" }}\n\
         [fetch.headers.\"{}\"]\nx-goog-api-key = {{ secret = \"openai\" }}\n",
        origin(keyed.addr),
        origin(other.addr),
        origin(closed),
        origin(keyed.addr),
        origin(closed),
    );
    let apps = apps(&[sample_with("proxy", &config)]);
    let host = start(&apps, 1);
    // The proxy forwards the client's `x-*` headers: the app supplies an
    // `x-goog-api-key` of its own.
    let through = |target: String| {
        send_raw(
            host.addr,
            format!(
                "GET /proxy/?url={target}/v1 HTTP/1.1\r\nHost: localhost\r\n\
                 Connection: close\r\nx-goog-api-key: from-the-app\r\n\r\n"
            )
            .as_bytes(),
        )
    };
    let mut answered = Vec::new();

    let keyed_answer = through(origin(keyed.addr));
    assert_eq!(keyed_answer.status, 200, "{keyed_answer:?}");
    let head = keyed.heads.lock().unwrap()[0].clone();
    assert_eq!(
        header_in(&head, "authorization"),
        Some(format!("Bearer {KEY}").as_str()),
        "{head}"
    );
    // Replaced, not sent beside the app's.
    assert_eq!(header_in(&head, "x-goog-api-key"), Some(KEY), "{head}");
    assert!(!head.contains("from-the-app"), "{head}");
    answered.push(format!("{keyed_answer:?}"));

    // Another allowed origin gets the app's header and none of the secret.
    let other_answer = through(origin(other.addr));
    assert_eq!(other_answer.status, 200, "{other_answer:?}");
    let head = other.heads.lock().unwrap()[0].clone();
    assert_eq!(header_in(&head, "authorization"), None, "{head}");
    assert_eq!(header_in(&head, "x-goog-api-key"), Some("from-the-app"));
    assert!(!head.contains(KEY), "{head}");
    answered.push(format!("{other_answer:?}"));

    // A fetch that fails with the header set: the `Err` does not carry it.
    let failed = through(origin(closed));
    assert_eq!(failed.status, 502, "{failed:?}");
    assert!(failed.body.contains("could not connect"), "{}", failed.body);
    answered.push(format!("{failed:?}"));

    // Nothing the app was answered, logged or counted holds the value.
    answered.push(get(host.addr, "/_host/apps/proxy/logs?n=50").body);
    answered.push(get(host.addr, "/_host/apps/proxy").body);
    answered.push(host.stats().to_string());
    for text in &answered {
        assert!(!text.contains(KEY), "{text}");
    }
}

#[test]
fn a_fetch_is_aborted_at_the_runs_deadline() {
    let upstream = Upstream::start(Mode::Hang);
    let config = proxy_config(
        &format!("http://127.0.0.1:{}", upstream.addr.port()),
        "deadline = \"300ms\"",
    );
    let apps = apps(&[sample_with("proxy", &config)]);
    let host = start(&apps, 1);
    let late = get(
        host.addr,
        &format!("/proxy/?url=http://127.0.0.1:{}/hang", upstream.addr.port()),
    );
    assert_eq!(late.status, 504);
    assert!(
        late.body.contains("deadline of 300ms exceeded"),
        "{}",
        late.body
    );
    // The upstream sees its connection closed, not held for the fetch's
    // own timeout.
    wait_until("the upstream to see the fetch abandoned", || {
        Upstream::count(&upstream.abandoned) == 1
    });
}

#[test]
fn a_fetch_is_aborted_when_the_client_goes_away() {
    let upstream = Upstream::start(Mode::Hang);
    let config = proxy_config(&format!("http://127.0.0.1:{}", upstream.addr.port()), "");
    let apps = apps(&[sample_with("proxy", &config)]);
    let host = start(&apps, 1);
    let client = open(
        host.addr,
        &format!("/proxy/?url=http://127.0.0.1:{}/hang", upstream.addr.port()),
    );
    wait_until("the fetch to reach the upstream", || {
        Upstream::count(&upstream.requests) == 1
    });
    drop(client);
    wait_until("the upstream to see the fetch abandoned", || {
        Upstream::count(&upstream.abandoned) == 1
    });
    wait_until("the run to be counted cancelled", || {
        count(&host, "proxy", "errors.cancelled") == 1
    });
    assert_eq!(count(&host, "proxy", "in_flight"), 0);
    assert_eq!(count(&host, "proxy", "parked"), 0);
}

// ---------------------------------------------------------- disconnect

#[test]
fn a_client_going_away_cancels_its_run_wherever_it_is() {
    // `/spin` under a budget it will not exhaust for a long while: only a
    // cancellation ends it. One worker, so with two spinning one runs
    // (yielding every slice, since the other waits) and one waits.
    let apps = apps(&[
        sample_with(
            "hello",
            "[limits]\nfuel = 100000000000\ndeadline = \"120s\"\n",
        ),
        sample("slow"),
    ]);
    let host = start(&apps, 1);
    let first = open(host.addr, "/hello/spin");
    let second = open(host.addr, "/hello/spin");
    wait_until("both spinners admitted, and the first sliced", || {
        count(&host, "hello", "in_flight") + count(&host, "hello", "queued") == 2
            && count(&host, "hello", "yields") > 0
    });
    let parked = open(host.addr, "/slow/?ms=60000");
    wait_until("the slow run to park", || {
        count(&host, "slow", "parked") == 1
    });

    drop(first);
    drop(second);
    drop(parked);
    wait_until("every run to be cancelled", || {
        count(&host, "hello", "errors.cancelled") == 2
            && count(&host, "slow", "errors.cancelled") == 1
    });
    assert_eq!(count(&host, "hello", "in_flight"), 0);
    assert_eq!(count(&host, "hello", "queued"), 0);
    assert_eq!(count(&host, "slow", "parked"), 0);
    // The worker is free again.
    assert_eq!(get(host.addr, "/hello/?name=after").status, 200);
    assert!(get(host.addr, "/_host/apps/hello/logs")
        .body
        .contains("499 cancelled"));
}

#[test]
fn a_request_cancelled_while_queued_never_runs() {
    let apps = apps(&[sample_with(
        "hello",
        "[limits]\nfuel = 100000000000\ndeadline = \"120s\"\nmax_in_flight = 1\n",
    )]);
    let host = start(&apps, 1);
    let running = open(host.addr, "/hello/spin");
    wait_until("the first to run", || {
        count(&host, "hello", "in_flight") == 1
    });
    // Held back by max_in_flight, so it is certainly still queued.
    let queued = open(host.addr, "/hello/spin");
    wait_until("the second to queue", || {
        count(&host, "hello", "queued") == 1
    });
    drop(queued);
    drop(running);
    wait_until("both to be cancelled", || {
        count(&host, "hello", "errors.cancelled") == 2
    });
    // The queued one was dropped where it stood: it spent no fuel.
    assert_eq!(count(&host, "hello", "queued"), 0);
    assert_eq!(count(&host, "hello", "in_flight"), 0);
}

// ------------------------------------------------------------- helpers

/// `method path` with `body`.
fn send(addr: SocketAddr, method: &str, path: &str, body: &str) -> Answer {
    send_raw(
        addr,
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// A `GET path` whose answer is never read: dropping the stream is the
/// client going away.
fn open(addr: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    stream
}
