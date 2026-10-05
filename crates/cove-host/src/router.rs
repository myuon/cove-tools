//! Which app a request is for.
//!
//! A [`Router`] is asked with the request's `Host` header and its path, and
//! answers the app's name and the path the app sees; the host then looks the
//! name up among the apps it serves now, so an app added or removed by an
//! update is routed to, or not, without the router knowing. [`PathPrefix`] is
//! the one this host uses: `/<app>/rest` reaches `<app>` with `/rest`.
//! Routing by hostname is another implementation of the same trait —
//! `Host: hello.example` to `hello` with the whole path.

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// The app's name.
    pub app: String,
    /// The path the app sees; starts with `/`.
    pub path: String,
}

/// Chooses the app for a request.
pub trait Router: Send + Sync {
    /// The app `path` (and `host`, the `Host` header without its port, when
    /// the request had one) is for, or `None` for no app.
    fn route(&self, host: Option<&str>, path: &str) -> Option<Route>;
}

/// `/<app>/rest` reaches `<app>` with `/rest`; `/<app>` reaches it with `/`.
pub struct PathPrefix;

impl Router for PathPrefix {
    fn route(&self, _host: Option<&str>, path: &str) -> Option<Route> {
        let path = path.strip_prefix('/')?;
        let (name, rest) = match path.split_once('/') {
            Some((name, rest)) => (name, format!("/{rest}")),
            None => (path, "/".to_string()),
        };
        (!name.is_empty()).then(|| Route {
            app: name.to_string(),
            path: rest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_routes_to_its_app_with_the_rest_of_the_path() {
        let route = |path| PathPrefix.route(None, path);
        assert_eq!(
            route("/crunch/a/b"),
            Some(Route {
                app: "crunch".to_string(),
                path: "/a/b".to_string()
            })
        );
        assert_eq!(route("/hello").unwrap().path, "/");
        assert_eq!(route("/hello/").unwrap().path, "/");
        assert_eq!(route("/"), None);
    }
}
