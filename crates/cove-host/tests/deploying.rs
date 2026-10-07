//! `cove-host deploy` and `cove-host rollback`: an app sent to the admin
//! listener as an archive, checked with the running host's config, written
//! into the apps directory only if it loads, and the version it replaced
//! kept. As in `updates.rs`, nothing asserts a duration.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use common::*;
use cove_host::deploy::{collect, pack, Limits};
use cove_host::Host;

/// A directory outside the apps directory holding the `versioned` fixture
/// with `VERSION` replaced by `label`, and `config` as its `app.toml` (or
/// the fixture's): the app as it lives in its own repository.
struct Source {
    dir: PathBuf,
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.dir.parent().unwrap());
    }
}

fn source(label: &str, config: Option<&str>) -> Source {
    let parent = std::env::temp_dir().join(format!(
        "cove-host-deploy-source-{}-{label}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let dir = parent.join("checkout");
    std::fs::create_dir_all(&dir).unwrap();
    let from = fixtures().join("versioned");
    let text = std::fs::read_to_string(from.join("versioned.cove")).unwrap();
    std::fs::write(dir.join("versioned.cove"), text.replace("VERSION", label)).unwrap();
    let config = match config {
        Some(config) => config.to_string(),
        None => std::fs::read_to_string(from.join("app.toml")).unwrap(),
    };
    std::fs::write(dir.join("app.toml"), config).unwrap();
    std::fs::write(dir.join("README.md"), "left out\n").unwrap();
    Source { dir }
}

fn archive_of(dir: &Path) -> Vec<u8> {
    pack(&collect(dir, Limits::standard()).unwrap()).unwrap()
}

/// `POST path` on the admin listener with `body`, as `token`.
fn admin_post(host: &Host, path: &str, body: &[u8], token: Option<&str>) -> Answer {
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let mut raw = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}\
         Content-Type: application/x-tar\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    send_raw(host.admin_addr.expect("an admin listener"), &raw)
}

fn deploy(host: &Host, name: &str, dir: &Path) -> Answer {
    admin_post(
        host,
        &format!("/apps/{name}/deploy"),
        &archive_of(dir),
        Some(ADMIN_TOKEN),
    )
}

fn rollback(host: &Host, name: &str) -> Answer {
    admin_post(
        host,
        &format!("/apps/{name}/rollback"),
        b"",
        Some(ADMIN_TOKEN),
    )
}

/// Every file under `dir`, by relative path, with its bytes.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).unwrap().display().to_string();
                out.push((relative, std::fs::read(&path).unwrap()));
            }
        }
    }
    if dir.exists() {
        walk(dir, dir, &mut out);
    }
    out.sort();
    out
}

fn version_of(answer: &Answer) -> String {
    answer.header("x-cove-app-version").unwrap().to_string()
}

#[test]
fn a_deploy_adds_a_new_app() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    let one = source("one", None);
    assert_eq!(get(host.addr, "/versioned/").status, 404);

    let deployed = deploy(&host, "versioned", &one.dir);
    assert_eq!(deployed.status, 200, "{}", deployed.body);
    let detail: serde_json::Value = serde_json::from_str(&deployed.body).unwrap();
    assert_eq!(detail["previous"], serde_json::Value::Null);
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
    // Its files, and only an app's files, are in the apps directory.
    let files: Vec<String> = snapshot(&apps.root.join("versioned"))
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(files, ["app.toml", "versioned.cove"]);
    assert!(!apps.root.join(".deploy/versioned").exists());
    assert!(!apps.root.join(".previous/versioned").exists());
    // Nothing to roll back to.
    assert_eq!(rollback(&host, "versioned").status, 404);
    // The other app was not touched.
    assert_eq!(get(host.addr, "/hello/").status, 200);
    let changes = host.stats();
    assert_eq!(changes["apps"]["versioned"]["updates"], 1);
}

#[test]
fn a_deploy_replaces_an_app_and_in_flight_requests_finish_on_the_old_version() {
    let apps = apps(&[fixture("versioned")]);
    let one = source("one", None);
    std::fs::copy(
        one.dir.join("versioned.cove"),
        apps.root.join("versioned/versioned.cove"),
    )
    .unwrap();
    let host = start(&apps, 1);
    let addr = host.addr;
    let v1 = version_of(&get(addr, "/versioned/"));
    let parked = thread::spawn(move || get(addr, "/versioned/sleep?ms=1500"));
    wait_until("the sleep to park", || {
        count(&host, "versioned", "parked") == 1
    });

    let two = source("two", None);
    let deployed = deploy(&host, "versioned", &two.dir);
    assert_eq!(deployed.status, 200, "{}", deployed.body);
    let detail: serde_json::Value = serde_json::from_str(&deployed.body).unwrap();
    assert_eq!(detail["previous"], v1.as_str());
    let v2 = detail["version"].as_str().unwrap().to_string();
    assert!(v2.starts_with("v2-"), "{v2}");

    let fresh = get(addr, "/versioned/");
    assert_eq!(fresh.body, "two 0\n");
    assert_eq!(version_of(&fresh), v2);
    let old = parked.join().unwrap();
    assert_eq!(old.status, 200, "{old:?}");
    assert_eq!(old.body, "one 0\n");
    assert_eq!(version_of(&old), v1);

    // The version it replaced is kept, beside the apps.
    let kept =
        std::fs::read_to_string(apps.root.join(".previous/versioned/versioned.cove")).unwrap();
    assert!(kept.contains("\"one {spun}"), "{kept}");
    assert!(!apps.root.join(".deploy/versioned").exists());
    assert!(!apps.root.join("versioned/README.md").exists());
}

#[test]
fn a_refused_deploy_changes_nothing_and_says_why() {
    let apps = apps(&[fixture("versioned")]);
    let one = source("one", None);
    std::fs::copy(
        one.dir.join("versioned.cove"),
        apps.root.join("versioned/versioned.cove"),
    )
    .unwrap();
    let host = start(&apps, 1);
    let v1 = version_of(&get(host.addr, "/versioned/"));
    // A kept previous version, which a refusal must not touch either.
    let two = source("two", None);
    assert_eq!(deploy(&host, "versioned", &two.dir).status, 200);
    let v2 = version_of(&get(host.addr, "/versioned/"));
    assert_ne!(v1, v2);
    let before = snapshot(&apps.root);

    let unresolved =
        "grant = [\"timer\"]\n[secrets]\nopenai = { env = \"COVE_HOST_TEST_NEVER_SET\" }\n";
    let logged = source("bad", None);
    let text = std::fs::read_to_string(logged.dir.join("versioned.cove")).unwrap();
    std::fs::write(
        logged.dir.join("versioned.cove"),
        text.replace("use timer\n", "use timer\nuse log\n").replace(
            "  var spun = 0\n",
            "  log.info(\"hello\")\n  var spun = 0\n",
        ),
    )
    .unwrap();
    let cases = [
        (
            source("bad", None),
            Some("export fn handle( {\n"),
            "does not parse",
        ),
        (
            logged,
            None,
            "requires `log`, which app.toml does not grant",
        ),
        (
            source("bad", Some(unresolved)),
            None,
            "COVE_HOST_TEST_NEVER_SET",
        ),
    ];
    for (n, (bad, replace_source, reason)) in cases.iter().enumerate() {
        if let Some(text) = replace_source {
            std::fs::write(bad.dir.join("versioned.cove"), text).unwrap();
        }
        let refused = deploy(&host, "versioned", &bad.dir);
        assert_eq!(refused.status, 422, "{reason}: {}", refused.body);
        assert!(
            refused.body.contains(&format!("still serving {v2}")),
            "{}",
            refused.body
        );
        assert!(refused.body.contains(reason), "{reason}: {}", refused.body);
        let still = get(host.addr, "/versioned/");
        assert_eq!(still.body, "two 0\n");
        assert_eq!(version_of(&still), v2);
        assert_eq!(snapshot(&apps.root), before, "{reason}: the files changed");
        assert_eq!(count(&host, "versioned", "updates_refused"), n as u64 + 1);
    }
    // A name that cannot be an app, and an archive that is refused.
    assert_eq!(deploy(&host, "_host", &two.dir).status, 422);
    let garbage = admin_post(
        &host,
        "/apps/versioned/deploy",
        b"not an archive",
        Some(ADMIN_TOKEN),
    );
    assert_eq!(garbage.status, 400, "{}", garbage.body);
    assert_eq!(snapshot(&apps.root), before);
    // The kept version is still the one to roll back to.
    let back = rollback(&host, "versioned");
    assert_eq!(back.status, 200, "{}", back.body);
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
}

#[test]
fn a_rollback_restores_the_kept_version_and_a_second_undoes_it() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    let one = source("one", None);
    let two = source("two", None);
    assert_eq!(deploy(&host, "versioned", &one.dir).status, 200);
    assert_eq!(deploy(&host, "versioned", &two.dir).status, 200);
    assert_eq!(get(host.addr, "/versioned/").body, "two 0\n");

    let back = rollback(&host, "versioned");
    assert_eq!(back.status, 200, "{}", back.body);
    let detail: serde_json::Value = serde_json::from_str(&back.body).unwrap();
    assert!(detail["version"].as_str().unwrap().starts_with("v3-"));
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
    let current = std::fs::read_to_string(apps.root.join("versioned/versioned.cove")).unwrap();
    assert!(current.contains("\"one {spun}"));
    let kept =
        std::fs::read_to_string(apps.root.join(".previous/versioned/versioned.cove")).unwrap();
    assert!(kept.contains("\"two {spun}"));

    assert_eq!(rollback(&host, "versioned").status, 200);
    assert_eq!(get(host.addr, "/versioned/").body, "two 0\n");
    assert_eq!(count(&host, "versioned", "updates"), 4);
    assert!(!apps.root.join(".deploy/versioned").exists());

    // The history says what was done.
    let changes = send_raw(
        host.admin_addr.unwrap(),
        format!(
            "GET /changes HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {ADMIN_TOKEN}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(changes.body.contains("\"rollback\""), "{}", changes.body);
    assert!(changes.body.contains("\"deploy\""), "{}", changes.body);
}

#[test]
fn deploy_and_rollback_need_the_admin_token() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    let one = source("one", None);
    let archive = archive_of(&one.dir);
    for token in [None, Some("wrong")] {
        let refused = admin_post(&host, "/apps/versioned/deploy", &archive, token);
        assert_eq!(refused.status, 401, "{token:?}");
        let refused = admin_post(&host, "/apps/hello/rollback", b"", token);
        assert_eq!(refused.status, 401, "{token:?}");
    }
    assert!(!apps.root.join("versioned").exists());
    assert!(!apps.root.join(".deploy").exists());
    // And the public listener has no way to it.
    assert_eq!(post(host.addr, "/apps/versioned/deploy", "x").status, 404);
}

/// The `cove-host` binary, run with `args` and `stdin`.
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
fn the_cli_deploys_a_directory_or_an_archive_on_stdin_and_rolls_back() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    let token = apps.root.with_extension("token");
    std::fs::write(&token, format!("{ADMIN_TOKEN}\n")).unwrap();
    let admin = host.admin_addr.unwrap().to_string();
    let flags = ["--admin", &admin, "--token-file", token.to_str().unwrap()];

    let one = source("one", None);
    let dir = one.dir.to_str().unwrap();
    let mut args = vec!["deploy", dir, "--name", "versioned"];
    args.extend(flags);
    let (ok, out, err) = cove_host(&args, b"");
    assert!(ok, "{out}{err}");
    assert!(err.contains("left out: README.md"), "{err}");
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");

    // `tar … | ssh host cove-host deploy - --name …`.
    let two = source("two", None);
    let mut args = vec!["deploy", "-", "--name", "versioned"];
    args.extend(flags);
    let (ok, out, err) = cove_host(&args, &archive_of(&two.dir));
    assert!(ok, "{out}{err}");
    assert_eq!(get(host.addr, "/versioned/").body, "two 0\n");

    // Refused: printed, non-zero.
    let bad = source("bad", None);
    std::fs::write(bad.dir.join("versioned.cove"), "export fn handle( {\n").unwrap();
    let mut args = vec!["deploy", bad.dir.to_str().unwrap(), "--name", "versioned"];
    args.extend(flags);
    let (ok, _, err) = cove_host(&args, b"");
    assert!(!ok);
    assert!(err.contains("422"), "{err}");
    assert!(err.contains("does not parse"), "{err}");
    // `-` without a name, and stdin that is not an archive.
    let mut args = vec!["deploy", "-"];
    args.extend(flags);
    assert!(!cove_host(&args, b"").0);

    let mut args = vec!["rollback", "versioned"];
    args.extend(flags);
    let (ok, out, err) = cove_host(&args, b"");
    assert!(ok, "{out}{err}");
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
    let _ = std::fs::remove_file(token);
}

#[test]
fn deploy_into_writes_an_apps_directory_without_a_host() {
    let apps = apps(&[sample("hello")]);
    let root = apps.root.to_str().unwrap();
    let hello_before = snapshot(&apps.root.join("hello"));

    let one = source("one", None);
    let (ok, out, err) = cove_host(
        &[
            "deploy",
            one.dir.to_str().unwrap(),
            "--name",
            "versioned",
            "--into",
            root,
        ],
        b"",
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("deployed `versioned`"), "{out}");
    let bad = source("bad", None);
    std::fs::write(bad.dir.join("versioned.cove"), "export fn handle( {\n").unwrap();
    let before = snapshot(&apps.root);
    let (ok, _, err) = cove_host(
        &[
            "deploy",
            bad.dir.to_str().unwrap(),
            "--name",
            "versioned",
            "--into",
            root,
        ],
        b"",
    );
    assert!(!ok);
    assert!(err.contains("refused; nothing changed"), "{err}");
    assert_eq!(snapshot(&apps.root), before);

    let two = source("two", None);
    let (ok, out, err) = cove_host(
        &[
            "deploy",
            two.dir.to_str().unwrap(),
            "--name",
            "versioned",
            "--into",
            root,
        ],
        b"",
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("is kept in"), "{out}");
    // Another app in the directory is left as it was, and the host loads
    // what was written.
    assert_eq!(snapshot(&apps.root.join("hello")), hello_before);
    let host = start(&apps, 1);
    assert_eq!(get(host.addr, "/versioned/").body, "two 0\n");
    assert_eq!(get(host.addr, "/hello/").status, 200);
    // The staged and kept copies are not apps.
    assert!(host.stats()["apps"].get(".previous").is_none());
    assert_eq!(rollback(&host, "versioned").status, 200);
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
}
