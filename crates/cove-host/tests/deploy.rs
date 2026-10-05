//! What a deployment behind a reverse proxy relies on: the operations views
//! off the public listener (`--ops-listener admin`), and a host that drains
//! on shutdown.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use cove_host::{Host, OpsListener};

fn get_as(addr: SocketAddr, path: &str, host: &str) -> Answer {
    send_raw(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

const OPS: [&str; 6] = [
    "/_host",
    "/_host/",
    "/_host/stats",
    "/_host/ui",
    "/_host/apps/hello",
    "/_host/apps/hello/logs?n=5",
];

#[test]
fn the_ops_views_can_live_on_the_admin_listener_only() {
    let apps = apps(&[sample("hello")]);
    let mut options = options(&apps, 1);
    options.ops_listener = OpsListener::Admin;
    let host = Host::start(options).expect("the host starts");
    let admin = host.admin_addr.unwrap();

    for path in OPS {
        let public = get(host.addr, path);
        assert_eq!(public.status, 404, "{path} on the public listener");
        assert!(!public.body.contains("\"apps\""), "{path}: {}", public.body);
    }
    let index = get(host.addr, "/");
    assert_eq!(index.status, 200);
    assert!(!index.body.contains("/_host"), "{}", index.body);
    // The apps are still served.
    assert_eq!(get(host.addr, "/hello/").status, 200);

    // On the admin listener, without the token, read-only.
    let stats = get_as(admin, "/_host/stats", "127.0.0.1");
    assert_eq!(stats.status, 200, "{stats:?}");
    assert!(stats.body.contains("\"hello\""), "{}", stats.body);
    assert_eq!(get_as(admin, "/_host/ui", "localhost:8791").status, 200);
    assert_eq!(get_as(admin, "/_host/apps/hello", "[::1]:8791").status, 200);
    assert_eq!(
        get_as(admin, "/_host/apps/hello/logs", "localhost").status,
        200
    );
    // A page on another site rebinding its name to 127.0.0.1 is refused.
    assert_eq!(
        get_as(admin, "/_host/stats", "evil.example:8791").status,
        403
    );
    // And nothing changes without the token.
    assert_eq!(post(admin, "/_host/stats", "").status, 405);
    assert_eq!(post(admin, "/apps/hello/update", "").status, 401);
    assert_eq!(get(admin, "/apps").status, 401);
}

#[test]
fn by_default_the_ops_views_stay_on_the_public_listener() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    assert_eq!(get(host.addr, "/_host/stats").status, 200);
    // The admin listener does not serve them without its token.
    assert_eq!(get(host.admin_addr.unwrap(), "/_host/stats").status, 401);
}

#[test]
fn the_ops_views_on_the_admin_listener_need_one() {
    let apps = apps(&[sample("hello")]);
    let mut options = options(&apps, 1);
    options.ops_listener = OpsListener::Admin;
    options.admin = None;
    let refused = Host::start(options).err().expect("refused");
    assert!(refused.contains("--ops-listener admin"), "{refused}");
}

#[test]
fn a_host_shutting_down_admits_nothing_new() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(host.shutdown(Duration::from_secs(5)), 0);
    let refused = get(host.addr, "/hello/");
    assert_eq!(refused.status, 503);
    assert_eq!(refused.header("retry-after"), Some("1"));
}
