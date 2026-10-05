//! Which app a request is for.
//!
//! A [`Router`] is asked with the request's `Host` header and its path, and
//! answers the app and the path the app sees. [`PathPrefix`] is the one this
//! host uses: `/<app>/rest` reaches `<app>` with `/rest`. Routing by hostname
//! is another implementation of the same trait — `Host: hello.example` to
//! `hello` with the whole path — which the HTTP front does not have to know
//! about.

use std::collections::HashMap;

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// The app's index in the host's app list.
    pub app: usize,
    /// The path the app sees; starts with `/`.
    pub path: String,
}

/// Chooses the app for a request.
pub trait Router: Send + Sync {
    /// The app `path` (and `host`, the `Host` header without its port, when
    /// the request had one) reaches, or `None` for no app.
    fn route(&self, host: Option<&str>, path: &str) -> Option<Route>;
    /// The app name a request that matched no app asked for, for the 404.
    fn asked_for(&self, _host: Option<&str>, path: &str) -> String {
        path.trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

/// `/<app>/rest` reaches `<app>` with `/rest`; `/<app>` reaches it with `/`.
pub struct PathPrefix {
    by_name: HashMap<String, usize>,
}

impl PathPrefix {
    /// A router over app names, by their index.
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> PathPrefix {
        PathPrefix {
            by_name: names
                .into_iter()
                .enumerate()
                .map(|(at, name)| (name.to_string(), at))
                .collect(),
        }
    }
}

impl Router for PathPrefix {
    fn route(&self, _host: Option<&str>, path: &str) -> Option<Route> {
        let path = path.strip_prefix('/')?;
        let (name, rest) = match path.split_once('/') {
            Some((name, rest)) => (name, format!("/{rest}")),
            None => (path, "/".to_string()),
        };
        let app = *self.by_name.get(name)?;
        Some(Route { app, path: rest })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_routes_to_its_app_with_the_rest_of_the_path() {
        let router = PathPrefix::new(["hello", "crunch"]);
        assert_eq!(
            router.route(None, "/crunch/a/b"),
            Some(Route {
                app: 1,
                path: "/a/b".to_string()
            })
        );
        assert_eq!(router.route(None, "/hello").unwrap().path, "/");
        assert_eq!(router.route(None, "/hello/").unwrap().path, "/");
        assert_eq!(router.route(None, "/nobody/"), None);
        assert_eq!(router.asked_for(None, "/nobody/x"), "nobody");
    }
}
