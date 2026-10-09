//! The webhook lab (`examples/webhooks`, issue #2) on a host started in-process:
//! receiving, storing, showing, limits, access control and escaping.

mod common;

use std::net::SocketAddr;

use common::*;
use serde_json::Value as Json;

const SECRET: &str = "test-admin-secret";

/// The sample's `app.toml` with the admin secret given literally, and
/// `extra` appended.
fn config(extra: &str) -> String {
    let shipped = std::fs::read_to_string(examples().join("webhooks/app.toml")).unwrap();
    let mut config = shipped.replace(
        "admin = { env = \"WEBHOOKS_ADMIN_TOKEN\" }",
        &format!("admin = {{ value = \"{SECRET}\" }}"),
    );
    assert!(
        config.contains(SECRET),
        "the shipped app.toml changed shape"
    );
    // The tests' hosts listen on a free port, not 8080.
    let start = config
        .find("allow = [")
        .expect("the shipped app.toml has an allowlist");
    let end = start + config[start..].find(']').unwrap() + 1;
    config.replace_range(start..end, "allow = [\"http://127.0.0.1:*\"]");
    config.push_str(extra);
    config
}

fn lab(extra: &str) -> Apps {
    let config = config(extra);
    apps(&[
        AppSpec {
            name: "webhooks",
            from: examples().join("webhooks"),
            config: Some(Box::leak(config.into_boxed_str())),
        },
        sample("hello"),
    ])
}

/// A request with `headers` (each `Name: value`) and `body`.
fn request(addr: SocketAddr, method: &str, path: &str, headers: &[&str], body: &str) -> Answer {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: lab.test\r\nConnection: close\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    send_raw(addr, head.as_bytes())
}

fn bearer() -> String {
    format!("Authorization: Bearer {SECRET}")
}

/// Makes an endpoint with the form fields `form`; its id.
fn endpoint(addr: SocketAddr, form: &str) -> String {
    let made = request(
        addr,
        "POST",
        "/webhooks/admin/endpoints",
        &[&bearer(), "Accept: application/json"],
        form,
    );
    assert_eq!(made.status, 201, "{made:?}");
    let made: Json = serde_json::from_str(&made.body).unwrap();
    assert_eq!(
        made["url"],
        format!(
            "http://lab.test/webhooks/in/{}",
            made["id"].as_str().unwrap()
        )
        .as_str()
    );
    made["id"].as_str().unwrap().to_string()
}

/// The endpoint's history as JSON.
fn history(addr: SocketAddr, id: &str) -> Json {
    let page = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}?format=json"),
        &[&bearer()],
        "",
    );
    assert_eq!(page.status, 200, "{page:?}");
    serde_json::from_str(&page.body).unwrap()
}

/// One stored event as JSON.
fn event(addr: SocketAddr, id: &str, event: &str) -> Json {
    let view = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{event}?view=json"),
        &[&bearer()],
        "",
    );
    assert_eq!(view.status, 200, "{view:?}");
    serde_json::from_str(&view.body).unwrap()
}

#[test]
fn a_request_is_stored_whole_and_survives_a_restart() {
    let apps = lab("");
    let id;
    let sent;
    {
        let host = start(&apps, 2);
        id = endpoint(host.addr, "name=github");
        let received = request(
            host.addr,
            "POST",
            &format!("/webhooks/in/{id}/hooks/push?a=1&b=%3Cx%3E"),
            &["Content-Type: application/json", "X-GitHub-Event: push"],
            r#"{"ref":"refs/heads/main","u":"é😀"}"#,
        );
        assert_eq!(received.status, 200, "{received:?}");
        let receipt: Json = serde_json::from_str(&received.body).unwrap();
        sent = receipt["event"].as_str().unwrap().to_string();
    }
    // A new host over the same data directory.
    let host = start(&apps, 1);
    let events = history(host.addr, &id)["events"].clone();
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["id"], sent.as_str());
    let stored = event(host.addr, &id, &sent);
    assert_eq!(stored["method"], "POST");
    assert_eq!(stored["path"], "/hooks/push");
    assert_eq!(stored["query"]["a"], "1");
    assert_eq!(stored["query"]["b"], "<x>");
    assert_eq!(stored["headers"]["x-github-event"], "push");
    assert_eq!(stored["headers"]["content-type"], "application/json");
    assert!(stored["headers"].get("x-forwarded-prefix").is_none());
    assert_eq!(stored["body"], r#"{"ref":"refs/heads/main","u":"é😀"}"#);
    assert!(stored["receivedAt"].as_str().unwrap().ends_with('Z'));
    // The raw view is the body alone, as plain text.
    let raw = request(
        host.addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{sent}?view=raw"),
        &[&bearer()],
        "",
    );
    assert_eq!(raw.body, r#"{"ref":"refs/heads/main","u":"é😀"}"#);
    assert_eq!(
        raw.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(raw.header("x-content-type-options"), Some("nosniff"));
    // The page shows it, pretty-printed too.
    let page = request(
        host.addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{sent}"),
        &[&bearer()],
        "",
    );
    assert_eq!(page.status, 200);
    assert!(
        page.body
            .contains("&quot;ref&quot;: &quot;refs/heads/main&quot;"),
        "{}",
        page.body
    );
    assert!(page.body.contains("Copy body"));
}

#[test]
fn the_admin_pages_need_the_secret_and_the_receive_urls_do_not() {
    let apps = lab("");
    let host = start(&apps, 1);
    let addr = host.addr;
    for header in [
        None,
        Some("Authorization: Bearer wrong"),
        Some("Authorization: Basic YWRtaW46d3Jvbmc="),
    ] {
        let headers: Vec<&str> = header.into_iter().collect();
        let refused = request(addr, "GET", "/webhooks/admin", &headers, "");
        assert_eq!(refused.status, 401, "{header:?}");
        assert_eq!(
            refused.header("www-authenticate"),
            Some("Basic realm=\"webhook lab\"")
        );
        let made = request(
            addr,
            "POST",
            "/webhooks/admin/endpoints",
            &headers,
            "name=x",
        );
        assert_eq!(made.status, 401);
    }
    // A browser's login: any user, the secret as the password.
    let basic = format!(
        "Authorization: Basic {}",
        base64(&format!("someone:{SECRET}"))
    );
    assert_eq!(
        request(addr, "GET", "/webhooks/admin", &[&basic], "").status,
        200
    );
    let id = endpoint(addr, "name=open");
    // Receiving needs nothing but the URL.
    assert_eq!(
        request(addr, "POST", &format!("/webhooks/in/{id}"), &[], "hi").status,
        200
    );
    assert_eq!(
        request(addr, "POST", "/webhooks/in/not-an-endpoint", &[], "hi").status,
        404
    );
    // A form posted from another site is refused even with the login.
    for cross in ["Origin: https://evil.example", "Sec-Fetch-Site: cross-site"] {
        let forged = request(
            addr,
            "POST",
            &format!("/webhooks/admin/e/{id}/delete"),
            &[&bearer(), cross],
            "",
        );
        assert_eq!(forged.status, 403, "{cross}");
    }
    assert_eq!(history(addr, &id)["events"].as_array().unwrap().len(), 1);
    // From its own page it goes through.
    let deleted = request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/delete"),
        &[
            &bearer(),
            "Origin: http://lab.test",
            "Sec-Fetch-Site: same-origin",
        ],
        "",
    );
    assert_eq!(deleted.status, 303);
    assert_eq!(
        request(addr, "POST", &format!("/webhooks/in/{id}"), &[], "hi").status,
        404
    );
}

#[test]
fn retention_and_the_body_limit_bound_what_is_kept() {
    let apps = lab("");
    let host = start(&apps, 1);
    let addr = host.addr;
    let id = endpoint(addr, "name=small&retention=2&maxBody=10");
    let mut sent = Vec::new();
    for n in 0..4 {
        let answer = request(
            addr,
            "POST",
            &format!("/webhooks/in/{id}"),
            &[],
            &format!("body number {n}"),
        );
        let receipt: Json = serde_json::from_str(&answer.body).unwrap();
        sent.push(receipt["event"].as_str().unwrap().to_string());
    }
    let events = history(addr, &id)["events"].clone();
    let kept: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    // The newest two, newest first.
    assert_eq!(kept, [sent[3].as_str(), sent[2].as_str()]);
    let gone = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{}", sent[0]),
        &[&bearer()],
        "",
    );
    assert_eq!(gone.status, 404);
    let stored = event(addr, &id, &sent[3]);
    assert_eq!(stored["body"], "body numbe");
    assert_eq!(stored["bytes"], 13);
    assert_eq!(stored["truncated"], true);

    // Clearing the history leaves the endpoint.
    let cleared = request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/clear"),
        &[&bearer()],
        "",
    );
    assert_eq!(cleared.status, 303);
    assert_eq!(history(addr, &id)["events"].as_array().unwrap().len(), 0);
}

#[test]
fn a_body_past_the_apps_request_limit_is_refused_by_the_host() {
    // The app's own `max_request_bytes`, lowered.
    let apps_dir = lab("");
    let toml = apps_dir.root.join("webhooks/app.toml");
    let text = std::fs::read_to_string(&toml)
        .unwrap()
        .replace("max_request_bytes = 1048576", "max_request_bytes = 100");
    std::fs::write(&toml, text).unwrap();
    let host = start(&apps_dir, 1);
    let id = endpoint(host.addr, "name=x");
    let large = request(
        host.addr,
        "POST",
        &format!("/webhooks/in/{id}"),
        &[],
        &"x".repeat(101),
    );
    assert_eq!(large.status, 413);
    assert_eq!(
        history(host.addr, &id)["events"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn secret_headers_are_masked_unless_the_endpoint_says_not() {
    let apps = lab("");
    let host = start(&apps, 1);
    let addr = host.addr;
    let headers = [
        "Authorization: Bearer sender-token",
        "Cookie: session=abc",
        "X-Api-Key: k-123",
        "X-Other: visible",
    ];
    for (form, masked) in [("name=masked", true), ("name=open&mask=off", false)] {
        let id = endpoint(addr, form);
        let answer = request(addr, "POST", &format!("/webhooks/in/{id}"), &headers, "");
        let receipt: Json = serde_json::from_str(&answer.body).unwrap();
        let stored = event(addr, &id, receipt["event"].as_str().unwrap());
        if masked {
            assert_eq!(
                stored["headers"]["authorization"],
                "[masked: 19 characters]"
            );
            assert_eq!(stored["headers"]["cookie"], "[masked: 11 characters]");
            assert_eq!(stored["headers"]["x-api-key"], "[masked: 5 characters]");
            assert_eq!(stored["masked"].as_array().unwrap().len(), 3);
        } else {
            assert_eq!(stored["headers"]["authorization"], "Bearer sender-token");
            assert_eq!(stored["headers"]["cookie"], "session=abc");
        }
        assert_eq!(stored["headers"]["x-other"], "visible");
    }
    // The form's checkbox: present means on, absent means off.
    let id = endpoint(addr, "name=form&masking=present");
    let answer = request(addr, "POST", &format!("/webhooks/in/{id}"), &headers, "");
    let receipt: Json = serde_json::from_str(&answer.body).unwrap();
    let stored = event(addr, &id, receipt["event"].as_str().unwrap());
    assert_eq!(stored["headers"]["cookie"], "session=abc");
}

#[test]
fn everything_a_sender_controls_is_escaped_on_the_pages() {
    let apps = lab("");
    let host = start(&apps, 1);
    let addr = host.addr;
    let id = endpoint(addr, "name=%3Cscript%3Ealert(1)%3C%2Fscript%3E");
    let answer = request(
        addr,
        "POST",
        &format!("/webhooks/in/{id}/%3Cimg%20src=x%3E?q=%3Cscript%3Ealert(2)%3C/script%3E"),
        &[
            "Content-Type: text/html",
            "X-Evil: <script>alert(3)</script>",
        ],
        "<script>alert(4)</script><img src=x onerror=alert(5)>",
    );
    let receipt: Json = serde_json::from_str(&answer.body).unwrap();
    let sent = receipt["event"].as_str().unwrap();
    for path in [
        "/webhooks/admin".to_string(),
        format!("/webhooks/admin/e/{id}"),
        format!("/webhooks/admin/e/{id}/events/{sent}"),
    ] {
        let page = request(addr, "GET", &path, &[&bearer()], "");
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(
            page.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert!(page.header("content-security-policy").is_some());
        for n in 1..=5 {
            assert!(
                !page.body.contains(&format!("<script>alert({n})")),
                "{path}: {n}"
            );
        }
        assert!(!page.body.contains("<img src=x"), "{path}");
    }
    let detail = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{sent}"),
        &[&bearer()],
        "",
    );
    assert!(detail
        .body
        .contains("&lt;script&gt;alert(3)&lt;/script&gt;"));
    assert!(detail
        .body
        .contains("&lt;script&gt;alert(4)&lt;/script&gt;&lt;img src=x onerror=alert(5)&gt;"));
    assert!(detail
        .body
        .contains("&lt;script&gt;alert(2)&lt;/script&gt;"));
}

/// A stored request resent to `url`; the attempt as JSON.
fn resend(addr: SocketAddr, id: &str, event: &str, url: &str) -> Json {
    let answer = request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/events/{event}/resend"),
        &[
            &bearer(),
            "Accept: application/json",
            "Content-Type: application/x-www-form-urlencoded",
        ],
        &format!(
            "url={}",
            url.replace('%', "%25")
                .replace('&', "%26")
                .replace('?', "%3F")
        ),
    );
    assert_eq!(answer.status, 200, "{answer:?}");
    serde_json::from_str(&answer.body).unwrap()
}

/// The completion criterion of #2: curl a receive URL, restart the host,
/// the history is still there, resend it to another endpoint.
#[test]
fn receive_restart_and_resend_to_another_endpoint() {
    let apps = lab("");
    let (github, staging, sent);
    {
        let host = start(&apps, 2);
        github = endpoint(host.addr, "name=github");
        staging = endpoint(host.addr, "name=staging");
        let answer = request(
            host.addr,
            "POST",
            &format!("/webhooks/in/{github}/push?delivery=7"),
            &[
                "Content-Type: application/json",
                "X-GitHub-Event: push",
                "Authorization: Bearer ghs_sender",
            ],
            r#"{"ref":"refs/heads/main"}"#,
        );
        let receipt: Json = serde_json::from_str(&answer.body).unwrap();
        sent = receipt["event"].as_str().unwrap().to_string();
    }
    let host = start(&apps, 2);
    let addr = host.addr;
    assert_eq!(history(addr, &github)["events"][0]["id"], sent.as_str());

    let target = format!(
        "http://127.0.0.1:{}/webhooks/in/{staging}/replayed",
        addr.port()
    );
    let attempt = resend(addr, &github, &sent, &target);
    assert_eq!(attempt["ok"], true, "{attempt}");
    assert_eq!(attempt["status"], 200);
    assert_eq!(attempt["url"], target.as_str());
    let receipt: Json = serde_json::from_str(attempt["body"].as_str().unwrap()).unwrap();
    assert_eq!(receipt["endpoint"], staging.as_str());

    // The other endpoint received it as the first one had.
    let copy = event(addr, &staging, receipt["event"].as_str().unwrap());
    assert_eq!(copy["method"], "POST");
    assert_eq!(copy["path"], "/replayed");
    assert_eq!(copy["body"], r#"{"ref":"refs/heads/main"}"#);
    assert_eq!(copy["headers"]["x-github-event"], "push");
    assert_eq!(copy["headers"]["content-type"], "application/json");
    // The masked header's placeholder was not sent as if it were the value.
    assert!(copy["headers"].get("authorization").is_none(), "{copy}");

    // The attempt is kept with the request, and shown on its page.
    let resends = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{github}/events/{sent}?view=resends"),
        &[&bearer()],
        "",
    );
    let resends: Json = serde_json::from_str(&resends.body).unwrap();
    assert_eq!(resends.as_array().unwrap().len(), 1);
    let page = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{github}/events/{sent}"),
        &[&bearer()],
        "",
    );
    assert!(page.body.contains("<b>200</b>"), "{}", page.body);
    // The resend parked its run; no worker waited on it.
    assert!(count(&host, "webhooks", "parks") >= 1);
    assert_eq!(count(&host, "webhooks", "blocking_host_calls"), 0);
}

#[test]
fn a_resend_that_gets_no_response_is_stored_as_a_failure() {
    let apps = lab("");
    let host = start(&apps, 2);
    let addr = host.addr;
    let id = endpoint(addr, "name=x");
    let answer = request(
        addr,
        "POST",
        &format!("/webhooks/in/{id}"),
        &[],
        "<script>alert(9)</script>",
    );
    let receipt: Json = serde_json::from_str(&answer.body).unwrap();
    let sent = receipt["event"].as_str().unwrap();

    // Off the allowlist: refused before anything is sent.
    let refused = resend(addr, &id, sent, "http://example.org/hook");
    assert_eq!(refused["ok"], false);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("not on app `webhooks`'s fetch allowlist"),
        "{refused}"
    );
    // Allowed, but nothing listens there.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let unreachable = resend(addr, &id, sent, &format!("http://127.0.0.1:{port}/"));
    assert_eq!(unreachable["ok"], false);
    assert!(
        unreachable["error"]
            .as_str()
            .unwrap()
            .contains("could not connect"),
        "{unreachable}"
    );

    // A response whose body is markup is shown as text.
    let echoed = resend(
        addr,
        &id,
        sent,
        &format!("http://127.0.0.1:{}/hello/echo", addr.port()),
    );
    assert_eq!(echoed["status"], 200);
    assert_eq!(echoed["body"], "<script>alert(9)</script>");
    let page = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{sent}"),
        &[&bearer()],
        "",
    );
    assert!(!page.body.contains("<script>alert(9)"));
    assert!(page.body.contains("&lt;script&gt;alert(9)&lt;/script&gt;"));
    assert!(page.body.contains("could not connect"));

    // All three are kept, newest first; deleting the request takes them too.
    let listed = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}/events/{sent}?view=resends"),
        &[&bearer()],
        "",
    );
    let listed: Json = serde_json::from_str(&listed.body).unwrap();
    let urls: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls.len(), 3);
    assert!(urls[0].ends_with("/hello/echo"));
    assert_eq!(urls[2], "http://example.org/hook");
    assert_eq!(count(&host, "webhooks", "fetch.refused"), 1);
    let deleted = request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/events/{sent}/delete"),
        &[&bearer()],
        "",
    );
    assert_eq!(deleted.status, 303);
    // Only the endpoint and its request count are left in the store.
    let detail = host.app_detail("webhooks").unwrap();
    assert_eq!(detail["kv"]["keys"], 2, "{detail}");
}

/// Saves endpoint `id`'s response settings from `form`; the status.
fn settings(addr: SocketAddr, id: &str, form: &str) -> Answer {
    request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/settings"),
        &[&bearer(), "Content-Type: application/x-www-form-urlencoded"],
        form,
    )
}

#[test]
fn an_endpoint_answers_as_configured_and_fails_on_schedule() {
    let apps = lab("");
    let host = start(&apps, 2);
    let addr = host.addr;
    let id = endpoint(addr, "name=flaky");
    // Before any settings: 200 and the receipt.
    let plain = request(addr, "POST", &format!("/webhooks/in/{id}"), &[], "x");
    assert_eq!(plain.status, 200);
    assert!(plain.body.contains("\"ok\":true"));

    let saved = settings(
        addr,
        &id,
        "status=202&headers=Content-Type%3A+text%2Fplain%0D%0AX-Lab%3A+yes&body=accepted&errorEvery=3&errorStatus=503&errorBody=try+later",
    );
    assert_eq!(saved.status, 303, "{saved:?}");
    // The plain request was the first; the schedule starts again with the
    // history, so clear it.
    request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/clear"),
        &[&bearer()],
        "",
    );
    let mut statuses = Vec::new();
    for n in 0..6 {
        let answer = request(
            addr,
            "POST",
            &format!("/webhooks/in/{id}"),
            &[],
            &format!("{n}"),
        );
        statuses.push(answer.status);
        if answer.status == 202 {
            assert_eq!(answer.body, "accepted");
            assert_eq!(answer.header("x-lab"), Some("yes"));
            assert_eq!(answer.header("content-type"), Some("text/plain"));
        } else {
            assert_eq!(answer.body, "try later");
        }
        assert!(answer.header("x-webhook-event").is_some());
    }
    assert_eq!(statuses, [202, 202, 503, 202, 202, 503]);
    // The history says what each was answered.
    let answered: Vec<u64> = history(addr, &id)["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["answered"].as_u64().unwrap())
        .rev()
        .collect();
    assert_eq!(answered, [202, 202, 503, 202, 202, 503]);
    assert_eq!(history(addr, &id)["reply"]["errorEvery"], 3);
}

#[test]
fn the_error_schedule_counts_every_request_even_at_once() {
    let apps = lab("");
    let host = start(&apps, 4);
    let addr = host.addr;
    let id = endpoint(addr, "name=busy");
    assert_eq!(
        settings(addr, &id, "errorEvery=2&errorStatus=500").status,
        303
    );
    let senders: Vec<_> = (0..20)
        .map(|n| {
            let id = id.clone();
            std::thread::spawn(move || {
                request(
                    addr,
                    "POST",
                    &format!("/webhooks/in/{id}"),
                    &[],
                    &format!("{n}"),
                )
                .status
            })
        })
        .collect();
    let statuses: Vec<u16> = senders.into_iter().map(|s| s.join().unwrap()).collect();
    // `kv.increment` is atomic: exactly every second request fails, however
    // they interleave.
    assert_eq!(
        statuses.iter().filter(|s| **s == 500).count(),
        10,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        10,
        "{statuses:?}"
    );
}

#[test]
fn a_delayed_endpoint_parks_while_other_apps_answer() {
    let apps = lab("");
    // One worker: if the wait held it, nothing else could run.
    let host = start(&apps, 1);
    let addr = host.addr;
    let id = endpoint(addr, "name=slow");
    assert_eq!(settings(addr, &id, "status=201&delayMs=3000").status, 303);
    let sender = std::thread::spawn(move || {
        request(
            addr,
            "POST",
            &format!("/webhooks/in/{id}/late"),
            &[],
            "waiting",
        )
    });
    wait_until("the delayed request to park", || {
        count(&host, "webhooks", "parked") == 1
    });
    // Another app answers on the one worker while it waits ...
    assert_eq!(get(addr, "/hello/?name=meanwhile").status, 200);
    // ... and so does the lab itself: the request is already on its page.
    let id = history_endpoint(addr);
    let events = history(addr, &id)["events"].clone();
    assert_eq!(events[0]["path"], "/late");
    assert_eq!(count(&host, "webhooks", "parked"), 1);

    let answer = sender.join().unwrap();
    assert_eq!(answer.status, 201);
    assert!(answer.body.contains("\"ok\":true"));
    assert_eq!(count(&host, "webhooks", "parks"), 1);
    assert_eq!(count(&host, "webhooks", "blocking_host_calls"), 0);
}

/// The one endpoint's id, from the endpoint list's JSON-free page.
fn history_endpoint(addr: SocketAddr) -> String {
    let page = request(addr, "GET", "/webhooks/admin", &[&bearer()], "");
    let at = page.body.find("/webhooks/admin/e/").unwrap() + "/webhooks/admin/e/".len();
    page.body[at..at + 16].to_string()
}

#[test]
fn bad_settings_are_refused_and_good_ones_are_shown_escaped() {
    let apps = lab("");
    let host = start(&apps, 1);
    let addr = host.addr;
    let id = endpoint(addr, "name=x");
    for (form, problem) in [
        ("status=99", "status must be 100 to 599"),
        ("delayMs=9000", "delay must be 0 to 8000"),
        ("headers=not+a+header", "is not a `Name: value` header"),
        ("headers=Bad+Name%3A+x", "is not a `Name: value` header"),
        ("errorEvery=lots", "error every must be a whole number"),
    ] {
        let refused = settings(addr, &id, form);
        assert_eq!(refused.status, 400, "{form}");
        assert!(refused.body.contains(problem), "{form}: {}", refused.body);
    }
    assert_eq!(history(addr, &id)["reply"]["status"], 200);

    let markup = "%3C%2Ftextarea%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E";
    assert_eq!(
        settings(addr, &id, &format!("body={markup}&errorBody={markup}")).status,
        303
    );
    let page = request(
        addr,
        "GET",
        &format!("/webhooks/admin/e/{id}"),
        &[&bearer()],
        "",
    );
    assert!(!page.body.contains("<script>alert(1)"));
    assert!(page
        .body
        .contains("&lt;/textarea&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
    // What a sender gets is the body as configured, as text.
    let answer = request(addr, "POST", &format!("/webhooks/in/{id}"), &[], "");
    assert_eq!(answer.body, "</textarea><script>alert(1)</script>");
}

/// Standard base64, for a Basic login.
fn base64(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for at in 0..4 {
            if at <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * at) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A lab host told how clients reach it.
fn behind(apps: &Apps, forwarding: minicloud::Forwarding) -> minicloud::Host {
    let mut options = options(apps, 1);
    options.forwarding = forwarding;
    minicloud::Host::start(options).expect("the host starts")
}

/// Makes an endpoint, as the given headers say; its id and receive URL.
fn endpoint_as(addr: SocketAddr, headers: &[&str]) -> (String, String) {
    let mut all = vec![bearer(), "Accept: application/json".to_string()];
    all.extend(headers.iter().map(|h| h.to_string()));
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    let made = request(addr, "POST", "/webhooks/admin/endpoints", &all, "name=x");
    assert_eq!(made.status, 201, "{made:?}");
    let made: Json = serde_json::from_str(&made.body).unwrap();
    (
        made["id"].as_str().unwrap().to_string(),
        made["url"].as_str().unwrap().to_string(),
    )
}

/// Deletes endpoint `id` as a browser on `origin` would; the status.
fn delete_from(addr: SocketAddr, id: &str, origin: &str) -> u16 {
    let origin = format!("Origin: {origin}");
    request(
        addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/delete"),
        &[&bearer(), &origin],
        "",
    )
    .status
}

#[test]
fn behind_a_tls_proxy_the_public_origin_is_the_one_the_lab_sees() {
    let apps = lab("");
    let host = behind(
        &apps,
        minicloud::Forwarding {
            public_origin: Some("https://covtools.example".parse().unwrap()),
            trust_proxy: false,
        },
    );
    // cloudflared reaches the host on localhost, over plain HTTP.
    let (id, url) = endpoint_as(host.addr, &["X-Forwarded-Proto: http"]);
    assert_eq!(url, format!("https://covtools.example/webhooks/in/{id}"));
    let page = request(host.addr, "GET", "/webhooks/admin", &[&bearer()], "");
    assert!(page.body.contains(&url), "{}", page.body);
    // The browser's form says the public origin, and that is this site.
    assert_eq!(delete_from(host.addr, &id, "http://lab.test"), 403);
    assert_eq!(delete_from(host.addr, &id, "http://localhost:8790"), 403);
    assert_eq!(delete_from(host.addr, &id, "https://covtools.example"), 303);
}

#[test]
fn forwarded_headers_count_only_from_a_trusted_proxy() {
    let apps = lab("");
    let forged = [
        "X-Forwarded-Proto: https",
        "X-Forwarded-Host: covtools.example",
    ];
    {
        // By default a client cannot claim to have come over TLS.
        let host = start(&apps, 1);
        let (id, url) = endpoint_as(host.addr, &forged);
        assert_eq!(url, format!("http://lab.test/webhooks/in/{id}"));
        assert_eq!(delete_from(host.addr, &id, "https://covtools.example"), 403);
        assert_eq!(delete_from(host.addr, &id, "http://lab.test"), 303);
    }
    let host = behind(
        &apps,
        minicloud::Forwarding {
            public_origin: None,
            trust_proxy: true,
        },
    );
    let (id, url) = endpoint_as(host.addr, &forged);
    assert_eq!(url, format!("https://covtools.example/webhooks/in/{id}"));
    let origin = "Origin: https://covtools.example";
    let deleted = request(
        host.addr,
        "POST",
        &format!("/webhooks/admin/e/{id}/delete"),
        &[&bearer(), origin, forged[0], forged[1]],
        "",
    );
    assert_eq!(deleted.status, 303);
}
