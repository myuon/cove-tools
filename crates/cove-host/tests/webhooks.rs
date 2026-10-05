//! The webhook lab (`apps/webhooks`, issue #2) on a host started in-process:
//! receiving, storing, showing, limits, access control and escaping.

mod common;

use std::net::SocketAddr;

use common::*;
use serde_json::Value as Json;

const SECRET: &str = "test-admin-secret";

/// The sample's `app.toml` with the admin secret given literally, and
/// `extra` appended.
fn config(extra: &str) -> String {
    let shipped = std::fs::read_to_string(samples().join("webhooks/app.toml")).unwrap();
    let mut config = shipped.replace(
        "admin = { env = \"WEBHOOKS_ADMIN_TOKEN\" }",
        &format!("admin = {{ value = \"{SECRET}\" }}"),
    );
    assert!(
        config.contains(SECRET),
        "the shipped app.toml changed shape"
    );
    config.push_str(extra);
    config
}

fn lab(extra: &str) -> Apps {
    let config = config(extra);
    apps(&[
        AppSpec {
            name: "webhooks",
            from: samples().join("webhooks"),
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
