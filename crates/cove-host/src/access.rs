//! Cloudflare Access: who is asking, verified (issue #23).
//!
//! A request that came through a Cloudflare Access application carries the
//! user's Access token, a JWT signed by the team's keys, in the
//! `Cf-Access-Jwt-Assertion` header (and in the `CF_Authorization` cookie,
//! which is read when the header is missing). `auth.identity(headers)`
//! verifies it and answers the email inside, so that an app knows who its
//! user is without a login of its own, and a request that did not come
//! through Access — straight to the host's port, say — has no identity.
//!
//! | operation | answers |
//! | --- | --- |
//! | `auth.identity(headers)` | `Result<auth.Identity, Error>`: `{ email, via }`, `via` being `"access"` or `"token"`; `Err` says why there is none |
//! | `auth.usesAccess()` | `Bool`: whether Access is on for this app — whether a refusal should prompt for a token (`WWW-Authenticate`) or not |
//!
//! # What is checked
//!
//! The token's signature (RS256, by a key of the team's JWKS,
//! `https://<team>/cdn-cgi/access/certs`, chosen by the token's `kid`), its
//! issuer (`https://<team>`), its audience (one of the app's `aud` tags:
//! the Access application it came through), `exp` and `nbf` with a minute's
//! leeway, and an `email` claim — so an Access *service* token, which has
//! none, is not an identity. `[access] emails`, when set, narrows it further.
//!
//! # The keys
//!
//! Fetched with the first token, then kept. A token signed by a key not held
//! fetches them again (Cloudflare rotates them), at most once a second
//! whatever the tokens say; and keys an hour old are fetched again when next
//! needed, and still used, if that fetch fails, until they are six hours
//! old. Past that, or with no keys at all, every token is refused and the
//! failure logged: **closed, never open**. A fetch is answered pending — the
//! run parks — and a token whose key is held is verified at once.
//!
//! # The token way in
//!
//! `[access] token` names one of the app's `[secrets]`, presented as
//! `Authorization: Bearer <secret>` or as a Basic password. With Access off
//! for the app (no team or no aud: local runs, tests), that is the only way
//! in, and `auth.usesAccess()` is `false` so the app can prompt for it. With
//! Access on, it is accepted only under `fallback = "token"`; under
//! `"none"`, the default, only an Access token is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_runtime::value::MapKey;
use cove_runtime::Value;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::config::Secrets;

/// The header Cloudflare Access adds to a request it let through.
pub const ASSERTION_HEADER: &str = "cf-access-jwt-assertion";
/// The cookie Access sets in the browser, holding the same token.
pub const ASSERTION_COOKIE: &str = "CF_Authorization";

/// Keys this old are fetched again when next needed.
pub const REFRESH_AFTER: Duration = Duration::from_secs(60 * 60);
/// Keys this old are not used, whether or not a fresh fetch succeeded.
pub const STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
/// At most one fetch of the keys in this long, whatever tokens arrive.
pub const MIN_REFRESH_GAP: Duration = Duration::from_secs(1);
/// The skew allowed on `exp` and `nbf`, in seconds.
pub const LEEWAY_SECS: u64 = 60;
/// One fetch of the keys, connect to last byte.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// An app's `[access]`, resolved.
#[derive(Clone, Debug, Default)]
pub struct Access {
    /// Access is on: the tokens to accept. `None` is off.
    pub jwt: Option<AccessPolicy>,
    /// The `[secrets]` entry accepted as a token.
    pub token: Option<String>,
    /// With Access on, whether the token is accepted too.
    pub fallback: bool,
    /// Why `[access]` is written but off, to log when the app loads.
    pub off: Option<String>,
}

/// The Access tokens one app accepts.
#[derive(Clone, Debug)]
pub struct AccessPolicy {
    /// `https://<team>`: the `iss` a token must have.
    pub issuer: String,
    /// The team's JWKS.
    pub certs: String,
    /// The AUD tags of the Access applications in front of the app; a
    /// token's `aud` must hold one.
    pub audiences: Vec<String>,
    /// Lower-cased; empty is any.
    pub emails: Vec<String>,
}

impl AccessPolicy {
    /// From the team domain (`<team>.cloudflareaccess.com`, or a URL such as
    /// `https://<team>.cloudflareaccess.com` — or a test's `http://127.0.0.1:<port>`).
    pub fn new(
        team: &str,
        audiences: Vec<String>,
        emails: Vec<String>,
    ) -> Result<AccessPolicy, String> {
        let team = team.trim().trim_end_matches('/');
        let issuer = if team.contains("://") {
            team.to_string()
        } else {
            format!("https://{team}")
        };
        let url = reqwest::Url::parse(&issuer)
            .map_err(|e| format!("`access.team`: `{team}` is not a team domain: {e}"))?;
        if !matches!(url.scheme(), "https" | "http") || url.path() != "/" || url.query().is_some() {
            return Err(format!(
                "`access.team`: `{team}` is not a team domain like \"<team>.cloudflareaccess.com\""
            ));
        }
        Ok(AccessPolicy {
            certs: format!("{issuer}/cdn-cgi/access/certs"),
            issuer,
            audiences,
            emails,
        })
    }
}

/// Who is asking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The verified email; empty for a token, which names nobody.
    pub email: String,
    /// `"access"` or `"token"`.
    pub via: &'static str,
}

impl Identity {
    /// As the `auth.Identity` value.
    pub fn value(&self) -> Value {
        Value::structure(
            "auth.Identity",
            vec![
                ("email", Value::string(self.email.clone())),
                ("via", Value::string(self.via)),
            ],
        )
    }
}

/// What a request presented: its Access token, if any, and its
/// `Authorization` header.
#[derive(Clone, Debug, Default)]
pub struct Presented {
    pub assertion: Option<String>,
    pub authorization: String,
}

impl Presented {
    /// From the request's headers (names lower-cased, as the host gives
    /// them), as the app passed them to `auth.identity`.
    pub fn from_headers(headers: &Value) -> Presented {
        let mut presented = Presented::default();
        let mut cookie = None;
        for (name, value) in headers.entries().into_iter().flatten() {
            let MapKey::Str(name) = name else { continue };
            let value = value.as_str().unwrap_or_default();
            match name.to_ascii_lowercase().as_str() {
                ASSERTION_HEADER => presented.assertion = Some(value.trim().to_string()),
                "authorization" => presented.authorization = value.to_string(),
                "cookie" => cookie = Some(value.to_string()),
                _ => {}
            }
        }
        if presented.assertion.as_deref().is_none_or(str::is_empty) {
            presented.assertion = cookie.as_deref().and_then(cookie_value);
        }
        presented.assertion = presented.assertion.filter(|token| !token.is_empty());
        presented
    }
}

/// `CF_Authorization` in a `Cookie` header.
fn cookie_value(header: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == ASSERTION_COOKIE && !value.is_empty()).then(|| value.to_string())
    })
}

/// Where `auth.identity` got to without waiting.
pub enum Step {
    /// The answer.
    Done(Result<Identity, String>),
    /// The token needs keys not held: verify it with `verifier`, then
    /// [`Gate::finish`].
    Verify(Arc<Verifier>, String),
}

/// One app's way of deciding who is asking: its `[access]`, its secrets and
/// its verifier.
pub struct Gate {
    pub access: Access,
    pub secrets: Secrets,
    pub verifier: Option<Arc<Verifier>>,
}

impl Gate {
    /// Whether the request presents the `[access] token` secret.
    fn token_presented(&self, presented: &Presented) -> bool {
        match self
            .access
            .token
            .as_ref()
            .and_then(|name| self.secrets.0.get(name))
        {
            Some(secret) => crate::sys::presents(&presented.authorization, secret),
            None => false,
        }
    }

    fn token_identity() -> Identity {
        Identity {
            email: String::new(),
            via: "token",
        }
    }

    /// As far as it can get without fetching keys.
    pub fn begin(&self, presented: &Presented) -> Step {
        let token_ok = self.token_presented(presented);
        let Some(verifier) = &self.verifier else {
            return Step::Done(if token_ok {
                Ok(Gate::token_identity())
            } else if self.access.token.is_some() {
                Err("no valid token: send `Authorization: Bearer <token>`".to_string())
            } else {
                Err(
                    "this app has no way in: `[access]` has no team and aud, and no token"
                        .to_string(),
                )
            });
        };
        let Some(assertion) = presented.assertion.clone() else {
            return Step::Done(self.finish_with(
                token_ok,
                Err(format!(
                    "no Cloudflare Access token (`Cf-Access-Jwt-Assertion`): this app is \
                     reached through Access ({})",
                    verifier.policy.issuer
                )),
            ));
        };
        match verifier.verify_cached(&assertion) {
            Some(verified) => Step::Done(self.finish_with(token_ok, verified)),
            None => Step::Verify(Arc::clone(verifier), assertion),
        }
    }

    /// The answer, given what verifying the Access token came to.
    pub fn finish(
        &self,
        presented: &Presented,
        verified: Result<String, String>,
    ) -> Result<Identity, String> {
        self.finish_with(self.token_presented(presented), verified)
    }

    fn finish_with(
        &self,
        token_ok: bool,
        verified: Result<String, String>,
    ) -> Result<Identity, String> {
        match verified {
            Ok(email) => Ok(Identity {
                email,
                via: "access",
            }),
            Err(_) if token_ok && self.access.fallback => Ok(Gate::token_identity()),
            Err(why) => Err(why),
        }
    }
}

/// The signing keys held, by `kid`.
#[derive(Default)]
struct Keys {
    by_kid: HashMap<String, DecodingKey>,
    /// When `by_kid` was fetched.
    fetched: Option<Instant>,
    /// When a fetch was last started, whatever came of it.
    attempted: Option<Instant>,
}

/// Verifies one app's Access tokens, holding the team's keys.
pub struct Verifier {
    pub policy: AccessPolicy,
    client: reqwest::Client,
    keys: Mutex<Keys>,
    /// One fetch at a time; the others wait for it and use what it got.
    fetching: tokio::sync::Mutex<()>,
    /// Where a failed fetch is reported.
    log: Box<dyn Fn(&str) + Send + Sync>,
}

/// The claims read from a token beyond those `jsonwebtoken` checks.
#[derive(Deserialize)]
struct Claims {
    #[serde(default)]
    email: Option<String>,
}

/// One key of a JWKS.
#[derive(Deserialize)]
struct Jwk {
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    kty: String,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

impl Verifier {
    /// A verifier for `policy`, reporting a failed fetch to `log`.
    pub fn new(
        policy: AccessPolicy,
        log: Box<dyn Fn(&str) + Send + Sync>,
    ) -> Result<Verifier, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot build the Access key client: {e}"))?;
        Ok(Verifier {
            policy,
            client,
            keys: Mutex::new(Keys::default()),
            fetching: tokio::sync::Mutex::new(()),
            log,
        })
    }

    /// The token's `kid`, if it is an RS256 JWT at all.
    fn kid(token: &str) -> Result<String, String> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| format!("the Access token is not a JWT: {e}"))?;
        if header.alg != Algorithm::RS256 {
            return Err(format!(
                "the Access token is signed with {:?}, not RS256",
                header.alg
            ));
        }
        header
            .kid
            .ok_or_else(|| "the Access token names no key (`kid`)".to_string())
    }

    /// The verdict, if the keys held decide it: `None` when they have to be
    /// fetched first.
    pub fn verify_cached(&self, token: &str) -> Option<Result<String, String>> {
        let kid = match Verifier::kid(token) {
            Ok(kid) => kid,
            Err(why) => return Some(Err(why)),
        };
        let key = {
            let keys = self.keys.lock().unwrap();
            match (keys.by_kid.get(&kid), keys.fetched) {
                (Some(key), Some(at)) if at.elapsed() < REFRESH_AFTER => key.clone(),
                _ => return None,
            }
        };
        Some(self.check(token, &key))
    }

    /// The verified email, fetching the keys if they have to be.
    pub async fn verify(&self, token: &str) -> Result<String, String> {
        if let Some(verdict) = self.verify_cached(token) {
            return verdict;
        }
        let kid = Verifier::kid(token)?;
        self.refresh().await;
        let key = {
            let keys = self.keys.lock().unwrap();
            match (keys.by_kid.get(&kid), keys.fetched) {
                (Some(key), Some(at)) if at.elapsed() < STALE_AFTER => key.clone(),
                (None, Some(at)) if at.elapsed() < STALE_AFTER => {
                    return Err(format!(
                        "the Access token is signed by a key (`{kid}`) not among the team's"
                    ))
                }
                _ => {
                    return Err(format!(
                        "Cloudflare Access's signing keys could not be fetched from {}, so no \
                         token is accepted",
                        self.policy.certs
                    ))
                }
            }
        };
        self.check(token, &key)
    }

    /// Fetches the keys, unless another fetch started within
    /// [`MIN_REFRESH_GAP`] (one that just finished included).
    async fn refresh(&self) {
        let _one = self.fetching.lock().await;
        {
            let mut keys = self.keys.lock().unwrap();
            if keys
                .attempted
                .is_some_and(|at| at.elapsed() < MIN_REFRESH_GAP)
            {
                return;
            }
            keys.attempted = Some(Instant::now());
        }
        match fetch_keys(&self.client, &self.policy.certs).await {
            Ok(fetched) => {
                let mut keys = self.keys.lock().unwrap();
                keys.by_kid = fetched;
                keys.fetched = Some(Instant::now());
            }
            Err(why) => (self.log)(&format!(
                "cannot fetch Cloudflare Access's signing keys from {}: {why}; a token whose \
                 key is not held (or is over {} hours old) is refused",
                self.policy.certs,
                STALE_AFTER.as_secs() / 3600
            )),
        }
    }

    /// Checks `token` against `key` and the policy: the email, or why not.
    fn check(&self, token: &str, key: &DecodingKey) -> Result<String, String> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.policy.issuer]);
        validation.set_audience(&self.policy.audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        let data = jsonwebtoken::decode::<Claims>(token, key, &validation).map_err(|e| {
            let what = match e.kind() {
                ErrorKind::InvalidSignature => "its signature is not the team's".to_string(),
                ErrorKind::ExpiredSignature => "it has expired".to_string(),
                ErrorKind::ImmatureSignature => "it is not valid yet (`nbf`)".to_string(),
                ErrorKind::InvalidAudience => {
                    "it is for another Access application (`aud`)".to_string()
                }
                ErrorKind::InvalidIssuer => "it is from another Access team (`iss`)".to_string(),
                ErrorKind::MissingRequiredClaim(claim) => format!("it has no `{claim}`"),
                _ => e.to_string(),
            };
            format!("the Access token is refused: {what}")
        })?;
        let email = data
            .claims
            .email
            .map(|email| email.trim().to_string())
            .filter(|email| !email.is_empty())
            .ok_or_else(|| {
                "the Access token names no user (`email`): a service token is not an identity"
                    .to_string()
            })?;
        if !self.policy.emails.is_empty()
            && !self.policy.emails.contains(&email.to_ascii_lowercase())
        {
            return Err(format!("{email} is not one of this app's allowed users"));
        }
        Ok(email)
    }
}

/// The RSA keys of the JWKS at `url`, by `kid`.
async fn fetch_keys(
    client: &reqwest::Client,
    url: &str,
) -> Result<HashMap<String, DecodingKey>, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("answered {}", response.status()));
    }
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    let jwks: Jwks = serde_json::from_slice(&body).map_err(|e| format!("not a JWKS: {e}"))?;
    let mut keys = HashMap::new();
    for jwk in jwks.keys {
        if jwk.kty != "RSA" {
            continue;
        }
        let (Some(kid), Some(n), Some(e)) = (jwk.kid, jwk.n, jwk.e) else {
            continue;
        };
        let key = DecodingKey::from_rsa_components(&n, &e)
            .map_err(|err| format!("key `{kid}`: {err}"))?;
        keys.insert(kid, key);
    }
    if keys.is_empty() {
        return Err("it holds no RSA key".to_string());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_team_domain_is_its_issuer_and_certs() {
        let policy =
            AccessPolicy::new("ioijoi.cloudflareaccess.com", vec!["a".into()], vec![]).unwrap();
        assert_eq!(policy.issuer, "https://ioijoi.cloudflareaccess.com");
        assert_eq!(
            policy.certs,
            "https://ioijoi.cloudflareaccess.com/cdn-cgi/access/certs"
        );
        let url = AccessPolicy::new("http://127.0.0.1:9/", vec![], vec![]).unwrap();
        assert_eq!(url.issuer, "http://127.0.0.1:9");
        assert!(AccessPolicy::new("team.example/path", vec![], vec![]).is_err());
    }

    #[test]
    fn the_cookie_is_read_when_the_header_is_missing() {
        assert_eq!(
            cookie_value("a=b; CF_Authorization=tok.en.x; c=d").as_deref(),
            Some("tok.en.x")
        );
        assert_eq!(cookie_value("CF_Authorization="), None);
        assert_eq!(cookie_value("x=1"), None);
    }
}
