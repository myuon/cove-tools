//! The host's secret store (issue #32): `[secrets] x = { store = "…" }`,
//! set, replaced and deleted at run time through the admin listener, the
//! admin app and `cove-host secret`, with the apps that use a secret
//! reloaded — and the value in none of what any of them answers, logs or
//! records.
//!
//! The app under test is the `keyed` fixture: `/check` is `auth.check`
//! against its secret `key`, `/fetch?url=` a fetch that the host gives a
//! `[fetch.headers]` header built from the same secret.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use common::*;
use cove_host::Host;

/// The store name the fixture takes its secret from.
const STORED: &str = "keyed-key";
/// Values no test output may contain.
const FIRST: &str = "sk-first-0123456789abcdef";
const SECOND: &str = "sk-second-fedcba9876543210";

/// The fixture's app.toml, with `upstream` allowed and given the secret as
/// `x-api-key`.
fn keyed_toml(upstream: Option<SocketAddr>) -> String {
    let mut toml =
        format!("grant = [\"auth\", \"fetch\"]\n\n[secrets]\nkey = {{ store = \"{STORED}\" }}\n");
    if let Some(addr) = upstream {
        let origin = format!("http://127.0.0.1:{}", addr.port());
        toml.push_str(&format!(
            "\n[fetch]\nallow = [\"{origin}\"]\n\n[fetch.headers.\"{origin}\"]\n\
             x-api-key = {{ secret = \"key\", prefix = \"Key \" }}\n"
        ));
    }
    toml
}

fn keyed(config: &str) -> AppSpec<'_> {
    AppSpec {
        name: "keyed",
        from: fixtures().join("keyed"),
        config: Some(config),
    }
}

/// A request to the admin listener, with the token.
fn listener(host: &Host, method: &str, path: &str, body: &str) -> Answer {
    send_raw(
        host.admin_addr.expect("an admin listener"),
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {ADMIN_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// `/keyed/check` presenting `token`.
fn check(host: &Host, token: &str) -> u16 {
    send_raw(
        host.addr,
        format!(
            "GET /keyed/check HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {token}\r\n\r\n"
        )
        .as_bytes(),
    )
    .status
}

/// Everything the host says about itself and the app, from both listeners
/// and the data directory: none of it may hold a value.
fn everything_said(host: &Host, data: &Path) -> Vec<String> {
    let mut said = vec![
        listener(host, "GET", "/secrets", "").body,
        listener(host, "GET", "/changes?n=100", "").body,
        listener(host, "GET", "/apps", "").body,
        get(host.addr, "/_host/stats").body,
        get(host.addr, "/_host/apps/keyed").body,
        get(host.addr, "/_host/apps/keyed/logs?n=500").body,
        get(host.addr, "/_host/ui").body,
        host.stats().to_string(),
        host.banner(),
    ];
    for file in [
        "_host/changes.jsonl",
        "_host/overrides.json",
        "keyed/log.txt",
        "admin/log.txt",
    ] {
        said.push(std::fs::read_to_string(data.join(file)).unwrap_or_default());
    }
    said
}

fn assert_never_said(said: &[String]) {
    for text in said {
        for value in [FIRST, SECOND] {
            assert!(!text.contains(value), "a value was said:\n{text}");
        }
    }
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_missing_secret_refuses_the_app_and_setting_it_reloads_the_app_with_it() {
    let config = keyed_toml(None);
    let apps = apps(&[keyed(&config), sample("hello")]);
    let host = start(&apps, 2);
    // Refused at load, naming the secret; every other app serves.
    assert_eq!(check(&host, FIRST), 503);
    let reason = app_stats(&host, "keyed")["refused"].to_string();
    assert!(reason.contains(&format!("has no `{STORED}`")), "{reason}");
    assert_eq!(get(host.addr, "/hello/").status, 200);
    // Listed as used and unset.
    let listed: serde_json::Value =
        serde_json::from_str(&listener(&host, "GET", "/secrets", "").body).unwrap();
    assert_eq!(
        listed,
        serde_json::json!([{ "name": STORED, "set": false, "updated_ms": null, "apps": ["keyed"] }])
    );

    // Set: stored, and the app reloaded with it.
    let set = listener(&host, "PUT", &format!("/secrets/{STORED}"), FIRST);
    assert_eq!(set.status, 200, "{}", set.body);
    let answer: serde_json::Value = serde_json::from_str(&set.body).unwrap();
    assert_eq!(answer["reloaded"][0]["app"], "keyed");
    assert_eq!(answer["reloaded"][0]["ok"], true, "{answer}");
    assert_eq!(check(&host, FIRST), 200);
    assert_eq!(check(&host, "sk-wrong"), 401);
    #[cfg(unix)]
    assert_eq!(mode_of(&apps.data.join("_host/secrets")), 0o600);

    // Replaced: the new value is the one checked, the old one no longer.
    let version = app_stats(&host, "keyed")["version"].clone();
    let replaced = listener(&host, "PUT", &format!("/secrets/{STORED}"), SECOND);
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    assert!(replaced.body.contains("replaced"), "{}", replaced.body);
    assert_ne!(app_stats(&host, "keyed")["version"], version);
    assert_eq!(check(&host, SECOND), 200);
    assert_eq!(check(&host, FIRST), 401);
    let listed: serde_json::Value =
        serde_json::from_str(&listener(&host, "GET", "/secrets", "").body).unwrap();
    assert_eq!(listed[0]["set"], true);
    assert!(listed[0]["updated_ms"].as_u64().unwrap() > 0);

    // The history has the name and the action.
    let changes = listener(&host, "GET", "/changes?n=10", "").body;
    assert!(changes.contains("secret set"), "{changes}");
    assert!(changes.contains(&format!("`{STORED}`")), "{changes}");
    assert_never_said(&everything_said(&host, &apps.data));
    drop(host);

    // A restart finds it.
    let host = start(&apps, 1);
    assert_eq!(check(&host, SECOND), 200);
    assert_never_said(&everything_said(&host, &apps.data));
}

/// A local server that answers 200 and keeps each request's head.
fn upstream() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let heads: Arc<Mutex<Vec<String>>> = Arc::default();
    let kept = Arc::clone(&heads);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            kept.lock().unwrap().push(head);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        }
    });
    (addr, heads)
}

/// The `x-api-key` the upstream's newest request carried.
fn newest_key(heads: &Mutex<Vec<String>>) -> Option<String> {
    let heads = heads.lock().unwrap();
    heads.last()?.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-api-key")
            .then(|| value.trim().to_string())
    })
}

#[test]
fn a_new_value_is_the_header_the_next_fetch_sends() {
    let (addr, heads) = upstream();
    let config = keyed_toml(Some(addr));
    let apps = apps(&[keyed(&config)]);
    let host = start(&apps, 1);
    let fetch = || {
        get(
            host.addr,
            &format!("/keyed/fetch?url=http://127.0.0.1:{}/v1", addr.port()),
        )
    };
    assert_eq!(fetch().status, 503, "refused until the secret is set");

    assert_eq!(
        listener(&host, "PUT", &format!("/secrets/{STORED}"), FIRST).status,
        200
    );
    let answer = fetch();
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(newest_key(&heads), Some(format!("Key {FIRST}")));

    assert_eq!(
        listener(&host, "PUT", &format!("/secrets/{STORED}"), SECOND).status,
        200
    );
    assert_eq!(fetch().status, 200);
    assert_eq!(newest_key(&heads), Some(format!("Key {SECOND}")));
    assert_never_said(&everything_said(&host, &apps.data));
}

#[test]
fn a_secret_an_app_uses_is_deleted_only_when_forced() {
    let config = keyed_toml(None);
    let apps = apps(&[keyed(&config), sample("hello")]);
    let host = start(&apps, 1);
    assert_eq!(listener(&host, "PUT", "/secrets/unused", FIRST).status, 200);
    assert_eq!(
        listener(&host, "PUT", &format!("/secrets/{STORED}"), SECOND).status,
        200
    );
    assert_eq!(check(&host, SECOND), 200);

    // Used: refused, and nothing changes.
    let refused = listener(&host, "DELETE", &format!("/secrets/{STORED}"), "");
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(refused.body.contains("used by keyed"), "{}", refused.body);
    assert_eq!(check(&host, SECOND), 200);

    // Not used: deleted.
    let deleted = listener(&host, "DELETE", "/secrets/unused", "");
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    assert!(deleted.body.contains("no app uses it"), "{}", deleted.body);
    assert_eq!(listener(&host, "DELETE", "/secrets/unused", "").status, 404);

    // Forced: deleted, and the app that used it is refused at once.
    let forced = listener(&host, "DELETE", &format!("/secrets/{STORED}?force=1"), "");
    assert_eq!(forced.status, 200, "{}", forced.body);
    assert!(forced.body.contains("now refused"), "{}", forced.body);
    assert_eq!(check(&host, SECOND), 503);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    let store = std::fs::read_to_string(apps.data.join("_host/secrets")).unwrap();
    assert!(
        !store.contains(STORED) && !store.contains(SECOND),
        "{store}"
    );

    // What the store refuses: a name that is not one, an empty value.
    assert_eq!(listener(&host, "PUT", "/secrets/-x", FIRST).status, 422);
    assert_eq!(listener(&host, "PUT", "/secrets/empty", "").status, 422);
    // And all of it needs the token.
    let anonymous = send_raw(
        host.admin_addr.unwrap(),
        format!(
            "PUT /secrets/{STORED} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{FIRST}",
            FIRST.len()
        )
        .as_bytes(),
    );
    assert_eq!(anonymous.status, 401);
    assert!(
        !apps.data.join("_host/secrets").exists() || {
            let store = std::fs::read_to_string(apps.data.join("_host/secrets")).unwrap();
            !store.contains(FIRST)
        }
    );
    assert_never_said(&everything_said(&host, &apps.data));
}

#[test]
fn a_reload_that_fails_keeps_the_value_and_the_serving_version() {
    let config = keyed_toml(None);
    let apps = apps(&[keyed(&config)]);
    let host = start(&apps, 1);
    assert_eq!(
        listener(&host, "PUT", &format!("/secrets/{STORED}"), FIRST).status,
        200
    );
    let serving = app_stats(&host, "keyed")["version"].clone();
    // The app's code breaks on disk; the next reload is refused.
    std::fs::write(apps.root.join("keyed/keyed.cove"), "export fn handle( {\n").unwrap();
    let set = listener(&host, "PUT", &format!("/secrets/{STORED}"), SECOND);
    assert_eq!(set.status, 200, "{}", set.body);
    let answer: serde_json::Value = serde_json::from_str(&set.body).unwrap();
    assert_eq!(answer["reloaded"][0]["ok"], false, "{answer}");
    assert!(
        answer["reloaded"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("still serving"),
        "{answer}"
    );
    // Still serving the version loaded with the old value; the new value is
    // stored, and the next load that works uses it.
    assert_eq!(app_stats(&host, "keyed")["version"], serving);
    assert_eq!(check(&host, FIRST), 200);
    let store = std::fs::read_to_string(apps.data.join("_host/secrets")).unwrap();
    assert!(store.contains(SECOND));
    std::fs::copy(
        fixtures().join("keyed/keyed.cove"),
        apps.root.join("keyed/keyed.cove"),
    )
    .unwrap();
    assert_eq!(
        listener(&host, "POST", "/apps/keyed/update", "").status,
        200
    );
    assert_eq!(check(&host, SECOND), 200);
}

#[test]
fn check_and_test_read_the_store_of_the_data_directory_they_are_given() {
    let config = keyed_toml(None);
    let apps = apps(&[keyed(&config)]);
    let modules = cove_host::HostModules::standard();
    let only = ["keyed".to_string()];
    // No store: refused by check, saying how to give it one.
    let report = cove_host::toolchain::check(&apps.root, &only, &modules).unwrap();
    assert!(!report.ok);
    assert!(report.out.contains("pass `--data`"), "{}", report.out);
    // A store without the secret: refused, naming it.
    let store = cove_host::secrets::SecretStore::open_data(&apps.data).unwrap();
    let report =
        cove_host::toolchain::check_with(&apps.root, &only, &modules, Some(&apps.data)).unwrap();
    assert!(!report.ok);
    assert!(
        report.out.contains(&format!("has no `{STORED}`")),
        "{}",
        report.out
    );
    // With it: ok.
    store.set(STORED, FIRST).unwrap();
    let report =
        cove_host::toolchain::check_with(&apps.root, &only, &modules, Some(&apps.data)).unwrap();
    assert!(report.ok, "{}{}", report.out, report.err);
    assert!(!report.out.contains(FIRST) && !report.err.contains(FIRST));
    // `test` runs without it, on a placeholder, and says so.
    let report = cove_host::toolchain::test(&apps.root, &only, None, &modules).unwrap();
    assert!(report.ok, "{}{}", report.out, report.err);
    assert!(
        report.out.contains("aWrongTokenIsNotTheSecret"),
        "{}",
        report.out
    );
    assert!(
        report.err.contains("placeholder for secret(s) `key`"),
        "{}",
        report.err
    );
    // And with the store, uses it and says nothing.
    let report =
        cove_host::toolchain::test_with(&apps.root, &only, None, &modules, Some(&apps.data))
            .unwrap();
    assert!(report.ok, "{}{}", report.out, report.err);
    assert!(!report.err.contains("placeholder"), "{}", report.err);
}

// ------------------------------------------------------------ the admin app

/// `apps/admin`'s app.toml, its own secret a literal.
fn admin_app_toml() -> &'static str {
    let toml = std::fs::read_to_string(bundled().join("admin/app.toml"))
        .unwrap()
        .replace("{ env = \"ADMIN_UI_TOKEN\" }", "{ value = \"ui-secret\" }");
    Box::leak(toml.into_boxed_str())
}

const UI_HOST: &str = "admin.localhost";
/// `Basic base64("owner:ui-secret")`.
const UI_LOGIN: &str = "Basic b3duZXI6dWktc2VjcmV0";

/// A request to the admin app, from its own pages unless `extra` says
/// otherwise.
fn ui(host: &Host, method: &str, path: &str, body: &str) -> Answer {
    ui_with(host, method, path, body, "Sec-Fetch-Site: same-origin\r\n")
}

fn ui_with(host: &Host, method: &str, path: &str, body: &str, extra: &str) -> Answer {
    send_raw(
        host.addr,
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n\
             Authorization: {UI_LOGIN}\r\n{extra}\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

#[test]
fn the_admin_app_sets_replaces_and_deletes_a_secret_and_never_shows_it() {
    let config = keyed_toml(None);
    let apps = apps(&[sample_with("admin", admin_app_toml()), keyed(&config)]);
    let host = start(&apps, 2);
    let page = ui(&host, "GET", "/secrets", "");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains(&format!("<code>{STORED}</code>")),
        "{}",
        page.body
    );
    assert!(page.body.contains(">unset<"), "{}", page.body);
    assert!(page.body.contains("type=password"), "{}", page.body);
    // The overview says why the app is refused.
    let overview = ui(&host, "GET", "/", "").body;
    assert!(
        overview.contains(&format!("has no `{STORED}`")),
        "{overview}"
    );

    // From another site: refused, nothing stored.
    let cross = ui_with(
        &host,
        "POST",
        "/secrets/set",
        &format!("name={STORED}&value={FIRST}"),
        "Sec-Fetch-Site: cross-site\r\n",
    );
    assert_eq!(cross.status, 403);
    assert_eq!(check(&host, FIRST), 503);

    // Set: the page says what the reload did; the value is not on it.
    let set = ui(
        &host,
        "POST",
        "/secrets/set",
        &format!("name={STORED}&value={FIRST}"),
    );
    assert_eq!(set.status, 200, "{}", set.body);
    assert!(set.body.contains("`keyed` serving"), "{}", set.body);
    assert_eq!(check(&host, FIRST), 200);
    // Replaced from the row's own form.
    let replaced = ui(
        &host,
        "POST",
        "/secrets/set",
        &format!("name={STORED}&value={SECOND}"),
    );
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    assert_eq!(check(&host, SECOND), 200);
    assert_eq!(check(&host, FIRST), 401);
    let listing = ui(&host, "GET", "/secrets", "").body;
    assert!(listing.contains(">set<"), "{listing}");

    // A set the host refuses comes back with the name and an empty value.
    let bad = ui(
        &host,
        "POST",
        "/secrets/set",
        &format!("name=-bad&value={FIRST}"),
    );
    assert_eq!(bad.status, 422, "{}", bad.body);
    assert!(bad.body.contains("not a secret name"), "{}", bad.body);
    assert!(bad.body.contains("value=\"-bad\""), "{}", bad.body);

    // Delete: needs `confirm`; used, it needs `force` as well.
    let unconfirmed = ui(&host, "POST", "/secrets/delete", &format!("name={STORED}"));
    assert_eq!(unconfirmed.status, 422);
    let used = ui(
        &host,
        "POST",
        "/secrets/delete",
        &format!("name={STORED}&confirm=on"),
    );
    assert_eq!(used.status, 422, "{}", used.body);
    assert!(used.body.contains("used by keyed"), "{}", used.body);
    assert_eq!(check(&host, SECOND), 200);
    let forced = ui(
        &host,
        "POST",
        "/secrets/delete",
        &format!("name={STORED}&confirm=on&force=on"),
    );
    assert_eq!(forced.status, 200, "{}", forced.body);
    assert_eq!(check(&host, SECOND), 503);

    // The history: who, the name and the action.
    let history = ui(&host, "GET", "/history", "").body;
    assert!(history.contains("secret set"), "{history}");
    assert!(history.contains("secret delete"), "{history}");
    assert!(history.contains("admin app: token"), "{history}");

    let mut said = everything_said(&host, &apps.data);
    said.extend([
        set.body,
        replaced.body,
        listing,
        bad.body,
        used.body,
        forced.body,
        history,
    ]);
    said.push(ui(&host, "GET", "/apps/keyed", "").body);
    said.push(get(host.addr, "/_host/apps/admin/logs?n=500").body);
    assert_never_said(&said);
}

// ------------------------------------------------------------------- the CLI

fn cove_host(args: &[&str], stdin: &[u8]) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cove-host"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_cli_sets_lists_and_deletes_through_the_admin_listener() {
    let config = keyed_toml(None);
    let apps = apps(&[keyed(&config)]);
    let host = start(&apps, 1);
    let token = apps.root.with_extension("token");
    std::fs::write(&token, ADMIN_TOKEN).unwrap();
    let admin = host.admin_addr.unwrap().to_string();
    let flags = [
        "--admin",
        admin.as_str(),
        "--token-file",
        token.to_str().unwrap(),
    ];
    let run = |args: &[&str], stdin: &[u8]| {
        let mut all: Vec<&str> = args.to_vec();
        all.extend(flags);
        cove_host(&all, stdin)
    };

    // One trailing newline is not part of the value.
    let (ok, out, err) = run(&["secret", "set", STORED], format!("{FIRST}\n").as_bytes());
    assert!(ok, "{out}{err}");
    assert!(out.contains("\"ok\": true"), "{out}");
    assert_eq!(check(&host, FIRST), 200);

    let (ok, out, err) = run(&["secret", "list"], b"");
    assert!(ok, "{out}{err}");
    assert!(out.contains(STORED) && out.contains("keyed"), "{out}");

    let (ok, _, err) = run(&["secret", "delete", STORED], b"");
    assert!(!ok);
    assert!(
        err.contains("409") && err.contains("used by keyed"),
        "{err}"
    );
    let (ok, out, err) = run(&["secret", "delete", STORED, "--force"], b"");
    assert!(ok, "{out}{err}");
    assert_eq!(check(&host, FIRST), 503);

    // Nothing on stdin, or a name that is not one: refused before sending.
    assert!(!run(&["secret", "set", STORED], b"").0);
    let (ok, _, err) = run(&["secret", "set", "a b"], FIRST.as_bytes());
    assert!(!ok);
    assert!(err.contains("not a secret name"), "{err}");
    assert_never_said(&everything_said(&host, &apps.data));
    let _ = std::fs::remove_file(token);
}
