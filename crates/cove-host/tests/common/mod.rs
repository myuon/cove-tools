//! What the integration tests share: an apps directory assembled per test, a
//! host started in-process on a free port, and a small HTTP client.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cove_host::{Host, ServeOptions};
use serde_json::Value as Json;

/// The sample apps, at the repository's root.
pub fn samples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps")
}

/// The apps only the tests use.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/apps")
}

/// A temporary apps directory, removed when dropped.
pub struct Apps {
    pub root: PathBuf,
}

impl Drop for Apps {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// One app of a test's apps directory: `name`, whose `.cove` files are
/// copied from `from`, and whose `app.toml` is `config` — or the copied
/// one's, when `None`.
pub struct AppSpec<'a> {
    pub name: &'a str,
    pub from: PathBuf,
    pub config: Option<&'a str>,
}

/// A sample app, as it ships.
pub fn sample(name: &str) -> AppSpec<'_> {
    AppSpec {
        name,
        from: samples().join(name),
        config: None,
    }
}

/// A sample app under a config of the test's own.
pub fn sample_with<'a>(name: &'a str, config: &'a str) -> AppSpec<'a> {
    AppSpec {
        name,
        from: samples().join(name),
        config: Some(config),
    }
}

/// A test-only app, as it ships.
pub fn fixture(name: &str) -> AppSpec<'_> {
    AppSpec {
        name,
        from: fixtures().join(name),
        config: None,
    }
}

/// Builds an apps directory holding `specs`.
pub fn apps(specs: &[AppSpec]) -> Apps {
    let root = std::env::temp_dir().join(format!(
        "cove-host-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    for spec in specs {
        let dir = root.join(spec.name);
        std::fs::create_dir_all(&dir).unwrap();
        for entry in std::fs::read_dir(&spec.from).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "cove") {
                // Renamed to the app's module where the app is a copy under
                // another name: the main module is the directory's.
                let file = path.file_name().unwrap().to_str().unwrap();
                let source_module = spec.from.file_name().unwrap().to_str().unwrap();
                let file = file.replacen(source_module, spec.name, 1);
                let text = std::fs::read_to_string(&path).unwrap();
                std::fs::write(dir.join(file), text).unwrap();
            }
        }
        let config = match spec.config {
            Some(config) => config.to_string(),
            None => std::fs::read_to_string(spec.from.join("app.toml")).unwrap(),
        };
        std::fs::write(dir.join("app.toml"), config).unwrap();
    }
    Apps { root }
}

/// The options a test host starts with: a free port, `workers` workers, the
/// default slice, quiet.
pub fn options(apps: &Apps, workers: usize) -> ServeOptions {
    let mut options = ServeOptions::new(&apps.root);
    options.addr = "127.0.0.1:0".to_string();
    options.workers = workers;
    options.quiet = true;
    // `COVE_HOST_TEST_BACKEND=vm` runs the suite on the encoded VM where the
    // native tier would otherwise be chosen.
    if let Ok(backend) = std::env::var("COVE_HOST_TEST_BACKEND") {
        options.backend = backend.parse().expect("a backend: auto, vm or native");
    }
    options
}

/// Starts a host over `apps`.
pub fn start(apps: &Apps, workers: usize) -> Host {
    Host::start(options(apps, workers)).expect("the host starts")
}

/// An HTTP response, as the tests read it.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends `head` (with `Connection: close` added) and `body`, and reads the
/// whole answer.
pub fn send_raw(addr: SocketAddr, raw: &[u8]) -> Answer {
    let mut stream = TcpStream::connect(addr).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    stream.write_all(raw).unwrap();
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Answer {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("not an HTTP response: {text:?}"));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {head:?}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Answer {
        status,
        headers,
        body: body.to_string(),
    }
}

/// `GET path`.
pub fn get(addr: SocketAddr, path: &str) -> Answer {
    send_raw(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

/// `POST path` with `body`.
pub fn post(addr: SocketAddr, path: &str, body: &str) -> Answer {
    send_raw(
        addr,
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// One app's entry in the host's stats.
pub fn app_stats(host: &Host, app: &str) -> Json {
    host.stats()["apps"][app].clone()
}

/// A counter of one app's stats, by its JSON path (`"errors.fuel"`).
pub fn count(host: &Host, app: &str, path: &str) -> u64 {
    let mut value = app_stats(host, app);
    for key in path.split('.') {
        value = value[key].clone();
    }
    value
        .as_u64()
        .unwrap_or_else(|| panic!("`{app}.{path}` is not a count: {value}"))
}

/// Waits until `condition` holds, polling; panics with `what` after a
/// generous bound. Not a timing assertion: the bound is there so that a
/// broken host fails the test instead of hanging it.
pub fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < until, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
