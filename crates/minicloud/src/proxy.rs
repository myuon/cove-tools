//! What an app is told about how the client reached the host.
//!
//! The apps build their own origin — `x-forwarded-proto` and `host` — to
//! write absolute URLs (the webhook lab's receive URLs) and to refuse a form
//! posted from another site (the `Origin` check of the lab and the ledger).
//! Behind a TLS-terminating proxy such as cloudflared the host sees plain
//! HTTP from `127.0.0.1`, while the browser says `Origin:
//! https://covtools.example`; and without a proxy, a client could send an
//! `X-Forwarded-Proto` of its choosing. So the host decides those headers
//! itself before an app sees them, one of three ways:
//!
//! | option | `x-forwarded-proto` | `host` |
//! | --- | --- | --- |
//! | `--public-origin https://h.example` | `https` | `h.example` |
//! | `--trust-proxy` | the request's `X-Forwarded-Proto` (`http` or `https`; else `http`) | its `X-Forwarded-Host`, else its `Host` |
//! | neither (the default) | `http`, whatever the client sent | its `Host` |
//!
//! `x-forwarded-host` is removed in every case: `host` is the one answer.
//! `--public-origin` is the one to use behind a proxy; it does not depend on
//! what the proxy forwards.
//!
//! **An app reached by hostname** (`[route] hosts`, [`crate::router`]) has
//! that hostname as its origin: under `--public-origin
//! https://covtools.example`, a request routed by `Host:
//! admin.covtools.example` is told `https` and `admin.covtools.example` (the
//! public origin's scheme, and its port if it has one), so each hostname a
//! proxy serves is its own origin. The name is the one the route matched,
//! never a header the client chose.

use std::collections::BTreeMap;

/// The origin the host is reached at from outside: a scheme and an
/// authority, no path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicOrigin {
    /// `http` or `https`.
    pub scheme: String,
    /// `host` or `host:port`.
    pub authority: String,
}

impl std::str::FromStr for PublicOrigin {
    type Err = String;
    fn from_str(text: &str) -> Result<PublicOrigin, String> {
        let refuse = |why: &str| {
            Err(format!(
                "`{text}` is not a public origin ({why}); write `https://host` or `http://host:port`"
            ))
        };
        let Some((scheme, rest)) = text.split_once("://") else {
            return refuse("no scheme");
        };
        if scheme != "http" && scheme != "https" {
            return refuse("the scheme is neither http nor https");
        }
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty() {
            return refuse("no host");
        }
        if authority.contains(['/', '?', '#', '@', ' ']) {
            return refuse("an origin has no path, query, user or spaces");
        }
        Ok(PublicOrigin {
            scheme: scheme.to_string(),
            authority: authority.to_ascii_lowercase(),
        })
    }
}

impl PublicOrigin {
    /// The port, when the origin names one.
    pub fn port(&self) -> Option<&str> {
        let after_v6 = self.authority.rsplit(']').next().unwrap_or_default();
        after_v6.rsplit_once(':').map(|(_, port)| port)
    }
}

impl std::fmt::Display for PublicOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.scheme, self.authority)
    }
}

/// How the host decides the headers in the module's table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Forwarding {
    /// `--public-origin`: wins over everything the request says.
    pub public_origin: Option<PublicOrigin>,
    /// `--trust-proxy`: believe the request's `X-Forwarded-Proto` and
    /// `X-Forwarded-Host`.
    pub trust_proxy: bool,
}

impl Forwarding {
    /// Rewrites `headers` (lower-case names, repeated values joined with
    /// `, `) to what the app is told. `hostname` is the name the request was
    /// routed by, for an app reached by hostname.
    pub fn apply(&self, headers: &mut BTreeMap<String, String>, hostname: Option<&str>) {
        let forwarded_host = headers.remove("x-forwarded-host");
        let forwarded_proto = headers.remove("x-forwarded-proto");
        let (proto, host) = match &self.public_origin {
            Some(origin) => {
                let authority = match hostname {
                    Some(name) => match origin.port() {
                        Some(port) => format!("{name}:{port}"),
                        None => name.to_string(),
                    },
                    None => origin.authority.clone(),
                };
                (origin.scheme.clone(), Some(authority))
            }
            None if self.trust_proxy => {
                let proto = forwarded_proto
                    .as_deref()
                    .map(first)
                    .filter(|proto| *proto == "http" || *proto == "https")
                    .unwrap_or("http")
                    .to_string();
                let host = forwarded_host
                    .as_deref()
                    .map(first)
                    .filter(|host| !host.is_empty())
                    .map(str::to_string);
                (proto, host)
            }
            None => ("http".to_string(), None),
        };
        headers.insert("x-forwarded-proto".to_string(), proto);
        if let Some(host) = host {
            headers.insert("host".to_string(), host);
        }
    }
}

/// The first of a comma-joined list: the value the nearest proxy was told
/// by the client is the leftmost one.
fn first(value: &str) -> &str {
    value.split(',').next().unwrap_or_default().trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_origin_is_a_scheme_and_a_host() {
        let origin: PublicOrigin = "https://Covtools.Example/".parse().unwrap();
        assert_eq!(origin.to_string(), "https://covtools.example");
        assert!("http://h.example:8790".parse::<PublicOrigin>().is_ok());
        for bad in ["covtools.example", "ftp://h", "https://", "https://h/x"] {
            assert!(bad.parse::<PublicOrigin>().is_err(), "{bad}");
        }
    }

    #[test]
    fn by_default_the_client_cannot_claim_https() {
        let mut h = headers(&[
            ("host", "localhost:8790"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "evil.example"),
        ]);
        Forwarding::default().apply(&mut h, None);
        assert_eq!(h["x-forwarded-proto"], "http");
        assert_eq!(h["host"], "localhost:8790");
        assert!(!h.contains_key("x-forwarded-host"));
    }

    #[test]
    fn a_trusted_proxy_is_believed() {
        let trust = Forwarding {
            public_origin: None,
            trust_proxy: true,
        };
        let mut h = headers(&[
            ("host", "localhost:8790"),
            ("x-forwarded-proto", "https, http"),
            ("x-forwarded-host", "covtools.example"),
        ]);
        trust.apply(&mut h, None);
        assert_eq!(h["x-forwarded-proto"], "https");
        assert_eq!(h["host"], "covtools.example");
        // Without the forwarded headers, the request's own.
        let mut h = headers(&[("host", "covtools.example")]);
        trust.apply(&mut h, None);
        assert_eq!(h["x-forwarded-proto"], "http");
        assert_eq!(h["host"], "covtools.example");
        // Nonsense is not believed.
        let mut h = headers(&[("host", "a"), ("x-forwarded-proto", "gopher")]);
        trust.apply(&mut h, None);
        assert_eq!(h["x-forwarded-proto"], "http");
    }

    #[test]
    fn a_public_origin_wins() {
        let fixed = Forwarding {
            public_origin: Some("https://covtools.example".parse().unwrap()),
            trust_proxy: true,
        };
        let mut h = headers(&[
            ("host", "localhost:8790"),
            ("x-forwarded-proto", "http"),
            ("x-forwarded-host", "evil.example"),
        ]);
        fixed.apply(&mut h, None);
        assert_eq!(h["x-forwarded-proto"], "https");
        assert_eq!(h["host"], "covtools.example");
        assert!(!h.contains_key("x-forwarded-host"));
    }

    #[test]
    fn a_hostname_route_is_its_own_origin() {
        let fixed = Forwarding {
            public_origin: Some("https://covtools.example".parse().unwrap()),
            trust_proxy: false,
        };
        let mut h = headers(&[("host", "admin.covtools.example")]);
        fixed.apply(&mut h, Some("admin.covtools.example"));
        assert_eq!(h["x-forwarded-proto"], "https");
        assert_eq!(h["host"], "admin.covtools.example");
        let with_port = Forwarding {
            public_origin: Some("http://h.example:8790".parse().unwrap()),
            trust_proxy: false,
        };
        let mut h = headers(&[("host", "localhost")]);
        with_port.apply(&mut h, Some("admin.localhost"));
        assert_eq!(h["host"], "admin.localhost:8790");
        assert_eq!(
            with_port.public_origin.as_ref().unwrap().port(),
            Some("8790")
        );
        assert_eq!(fixed.public_origin.as_ref().unwrap().port(), None);
    }
}
