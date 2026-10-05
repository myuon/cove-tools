//! The admin listener: updates, on an address of its own, behind a token.
//!
//! A second HTTP listener (`--admin`, default `127.0.0.1:8081`), so that the
//! public listener — the one a reverse proxy forwards — has no way to change
//! what runs, and the admin address can be kept on localhost whatever the
//! public one is. Every request needs `Authorization: Bearer <token>`; the
//! token is `--admin-token`, or the contents of `<data>/admin.token`, which
//! the host writes with a fresh random token (mode 0600) the first time it
//! starts without one. `cove-host update` reads the same file.
//!
//! | request | does |
//! | --- | --- |
//! | `POST /apps/<app>/update` | loads `<apps>/<app>` as the app's next version and routes to it if it loads; a new name adds the app. 200 with the old and new versions; 422 with the diagnostics, the current version still serving; 404 for no such directory |
//! | `DELETE /apps/<app>` | stops routing to the app; what is in flight finishes |
//! | `GET /apps` | the stats, as `GET /_host/stats` |
//!
//! Anything without the token is 401, and changes nothing.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;

use crate::convert::Reply;
use crate::ops;
use crate::server::{json_reply, to_response, Front, UpdateError};

/// Serves the admin listener until the runtime stops.
pub(crate) async fn serve(front: Arc<Front>, listener: tokio::net::TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let front = Arc::clone(&front);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let front = Arc::clone(&front);
                async move { Ok::<_, Infallible>(handle(&front, request).await) }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(front: &Front, request: Request<Incoming>) -> Response<Full<Bytes>> {
    to_response(reply(front, request).await)
}

async fn reply(front: &Front, request: Request<Incoming>) -> Reply {
    let presented = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if front.admin_token.is_empty() || !same(presented.trim(), &front.admin_token) {
        return Reply::text(
            401,
            "cove-host admin: a valid `Authorization: Bearer <token>` is required\n",
        )
        .with_header("www-authenticate", "Bearer");
    }
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let name = path
        .strip_prefix("/apps/")
        .map(|rest| rest.trim_end_matches("/update"))
        .unwrap_or_default()
        .to_string();
    match (method, path.as_str()) {
        (Method::GET, "/apps") => json_reply(&ops::stats(&front_ops(front))),
        (Method::POST, _) if path.ends_with("/update") && !name.is_empty() => {
            match front.update(&name).await {
                Ok(installed) => {
                    let mut reply = json_reply(&json!({
                        "app": installed.app,
                        "version": installed.version,
                        "previous": installed.previous,
                    }));
                    reply.status = 200;
                    reply
                }
                Err(UpdateError::NotFound(why)) => Reply::text(404, format!("{why}\n")),
                Err(UpdateError::Refused { current, why }) => Reply::text(
                    422,
                    format!(
                        "update of `{name}` refused; still serving {}:\n{why}\n",
                        current.as_deref().unwrap_or("nothing")
                    ),
                ),
            }
        }
        (Method::DELETE, _) if !name.is_empty() && !name.contains('/') => {
            match front.remove(&name) {
                Some(version) => Reply::text(200, format!("removed `{name}` (was {version})\n")),
                None => Reply::text(404, format!("no app named `{name}` is routed to\n")),
            }
        }
        _ => Reply::text(
            404,
            "cove-host admin: POST /apps/<app>/update, DELETE /apps/<app>, GET /apps\n",
        ),
    }
}

fn front_ops(front: &Front) -> ops::OpsContext<'_> {
    front.ops_context()
}

/// Compares two tokens in time that depends on their lengths only.
fn same(presented: &str, token: &str) -> bool {
    presented.len() == token.len()
        && presented
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// The token in `<data>/admin.token`, writing a fresh one there first if
/// there is none.
pub fn token_file(data: Option<&Path>) -> Result<String, String> {
    let Some(data) = data else {
        return Err(
            "the admin listener needs --admin-token or a data directory for admin.token"
                .to_string(),
        );
    };
    let path = data.join("admin.token");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    std::fs::create_dir_all(data)
        .map_err(|e| format!("cannot create `{}`: {e}", data.display()))?;
    let token = fresh_token()?;
    write_private(&path, &token).map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    Ok(token)
}

/// 32 random bytes, as hex, from the operating system.
fn fresh_token() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| {
            format!("cannot read /dev/urandom for an admin token ({e}); pass --admin-token")
        })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{token}")
}

#[cfg(not(unix))]
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{token}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abd", "abc"));
        assert!(!same("ab", "abc"));
        assert!(!same("", "abc"));
    }

    #[test]
    fn a_token_file_is_made_once_and_read_after() {
        let dir = std::env::temp_dir().join(format!("cove-host-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = token_file(Some(&dir)).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(token_file(Some(&dir)).unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
