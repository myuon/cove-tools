//! `apps/<name>/app.toml`: what an app is granted and what bounds it.
//!
//! ```toml
//! entry = "hello.handle"          # optional; `<name>.handle` by default
//! grant = ["log", "timer"]        # capabilities; nothing by default
//!
//! [limits]
//! fuel = 2000000                  # per request
//! max_host_calls = 100            # per request
//! deadline = "5s"                 # per request, parked time included ("ms" or "s")
//! max_call_depth = 512            # per request
//! max_heap_words = 1048576        # per request, see `AppLimits::max_heap_words`
//! max_in_flight = 16              # runs started and not answered, this app
//! max_queued = 64                 # requests waiting to start, this app
//! max_request_bytes = 1048576     # request body
//! max_response_bytes = 1048576    # response body
//!
//! [route]
//! hosts = ["admin.example"]       # reached by these hostnames only, not /<name>/
//!
//! [access]                        # Cloudflare Access, for `auth.identity`
//! team = { env = "ACCESS_TEAM_DOMAIN" }       # `<team>.cloudflareaccess.com`
//! aud = { env = "COVTOOLS_ACCESS_AUD" }       # the Access application's AUD tag(s)
//! emails = { env = "ACCESS_ALLOWED_EMAILS" }  # optional: only these
//! token = "admin"                 # a `[secrets]` name: the token way in
//! fallback = "none"               # or "token": the token too when Access is on
//! ```
//!
//! `[access]` is read by [`crate::access`]; its values are deployment
//! facts, so they come from the environment (or a file) like a secret's, and
//! an unset `team` or `aud` turns Access off for the app rather than
//! refusing it.
//!
//! What `app.toml` says can be changed at run time by the admin app
//! ([`AppOverride`], kept in the data directory by [`crate::overrides`]): the
//! grant, the fetch allowlist and the limits. An override is applied to the
//! file as written before anything is validated, so an overridden config is
//! held to every rule an `app.toml` is.
//!
//! Every key is optional and an unknown one is refused: a misspelt limit would
//! otherwise be a limit silently not applied. A refusal is the app's alone —
//! the host starts every other app.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use cove_runtime::Limits;
use serde::{Deserialize, Serialize};

/// The capability of the `admin` host module: listing the apps and changing
/// their configuration.
pub const ADMIN_CAPABILITY: &str = "admin";

/// The one app that may be granted [`ADMIN_CAPABILITY`]. Granting it to any
/// other app — in its `app.toml` or through an override — refuses that app.
pub const ADMIN_APP: &str = "admin";

/// The runtime's fixed per-run heap, in words.
///
/// `OwnedVm::new` builds every run over `DEFAULT_HEAP_WORDS` (four mebiwords,
/// 32 MiB) and offers no way to choose another (`cove-runtime`'s
/// `vm/parked.rs`); `Vm::with_heap_words` exists but `OwnedVm` does not
/// forward it. So this is the hard ceiling a run hits — "this run has no
/// memory left" — whatever `max_heap_words` says, and `max_heap_words` is
/// refused above it.
pub const HEAP_CEILING_WORDS: u64 = 1 << 22;

/// `app.toml` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppFile {
    /// `module.function`; `<app>.handle` when absent.
    pub entry: Option<String>,
    /// The capabilities this app is granted, and nothing else.
    #[serde(default)]
    pub grant: Vec<String>,
    #[serde(default)]
    pub limits: LimitsFile,
    #[serde(default)]
    pub kv: KvFile,
    #[serde(default)]
    pub fetch: FetchFile,
    /// Named secrets the app may check a presented credential against
    /// (`auth.check`), never read.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretFile>,
    #[serde(default)]
    pub route: RouteFile,
    /// Cloudflare Access, for `auth.identity` ([`crate::access`]).
    #[serde(default)]
    pub access: AccessFile,
}

/// `[access]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessFile {
    /// The Access team domain, `<team>.cloudflareaccess.com` (or a URL).
    pub team: Option<SecretFile>,
    /// The Access application's AUD tag; several, separated by commas or
    /// spaces, for an app behind more than one Access application.
    pub aud: Option<SecretFile>,
    /// Only these emails, separated by commas or spaces; any Access let
    /// through when unset.
    pub emails: Option<SecretFile>,
    /// The `[secrets]` entry `auth.identity` accepts as a token.
    pub token: Option<String>,
    /// `"none"` (the default): with Access on, only an Access token gets in.
    /// `"token"`: the `token` secret as well.
    pub fallback: Option<String>,
}

/// `[route]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteFile {
    /// Hostnames that reach this app with the whole path. An app that has
    /// any is reached by them only: `/<name>/` on another hostname is not
    /// it.
    #[serde(default)]
    pub hosts: Vec<String>,
}

/// One `[secrets]` entry: where its value comes from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretFile {
    /// An environment variable of the host process.
    pub env: Option<String>,
    /// A file, relative to the app's directory unless absolute; trailing
    /// whitespace is trimmed.
    pub file: Option<String>,
    /// The value itself, for tests and demos; prefer `env` or `file`.
    pub value: Option<String>,
}

/// An app's secrets, by name, resolved. `Debug` prints the names only.
#[derive(Clone, Default)]
pub struct Secrets(pub BTreeMap<String, String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

/// Resolves every secret, or says which could not be.
fn resolve_secrets(
    entries: BTreeMap<String, SecretFile>,
    dir: Option<&Path>,
) -> Result<Secrets, String> {
    let mut secrets = BTreeMap::new();
    for (name, entry) in entries {
        let value = match (entry.env, entry.file, entry.value) {
            (Some(var), None, None) => std::env::var(&var).map_err(|_| {
                format!("secret `{name}`: the environment variable `{var}` is not set")
            })?,
            (None, Some(file), None) => {
                let path = match dir {
                    Some(dir) if !Path::new(&file).is_absolute() => dir.join(&file),
                    _ => std::path::PathBuf::from(&file),
                };
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("secret `{name}`: cannot read `{}`: {e}", path.display()))?
                    .trim_end()
                    .to_string()
            }
            (None, None, Some(value)) => value,
            _ => {
                return Err(format!(
                    "secret `{name}` must have exactly one of `env`, `file` or `value`"
                ))
            }
        };
        if value.is_empty() {
            return Err(format!("secret `{name}` is empty"));
        }
        secrets.insert(name, value);
    }
    Ok(Secrets(secrets))
}

/// A non-secret setting from the same three places as a secret: `None` when
/// its environment variable is unset or empty, or it is empty.
fn resolve_setting(
    key: &str,
    entry: Option<SecretFile>,
    dir: Option<&Path>,
) -> Result<Option<String>, String> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    let value = match (entry.env, entry.file, entry.value) {
        (Some(var), None, None) => std::env::var(&var).unwrap_or_default(),
        (None, Some(file), None) => {
            let path = match dir {
                Some(dir) if !Path::new(&file).is_absolute() => dir.join(&file),
                _ => std::path::PathBuf::from(&file),
            };
            std::fs::read_to_string(&path)
                .map_err(|e| format!("`{key}`: cannot read `{}`: {e}", path.display()))?
        }
        (None, None, Some(value)) => value,
        _ => {
            return Err(format!(
                "`{key}` must have exactly one of `env`, `file` or `value`"
            ))
        }
    };
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_string()))
}

/// A list setting: split on commas and whitespace.
fn words(text: Option<String>) -> Vec<String> {
    text.unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolves `[access]` against the app's secrets.
fn resolve_access(
    file: AccessFile,
    secrets: &Secrets,
    dir: Option<&Path>,
) -> Result<crate::access::Access, String> {
    use crate::access::{Access, AccessPolicy};
    if let Some(token) = &file.token {
        if !secrets.0.contains_key(token) {
            return Err(format!(
                "`access.token = \"{token}\"` names no `[secrets]` entry"
            ));
        }
    }
    let fallback = match file.fallback.as_deref() {
        None | Some("none") => false,
        Some("token") => true,
        Some(other) => {
            return Err(format!(
                "`access.fallback = \"{other}\"` is not \"none\" or \"token\""
            ))
        }
    };
    if fallback && file.token.is_none() {
        return Err("`access.fallback = \"token\"` needs `access.token`".to_string());
    }
    let declared = file.team.is_some() || file.aud.is_some();
    let team = resolve_setting("access.team", file.team, dir)?;
    let audiences = words(resolve_setting("access.aud", file.aud, dir)?);
    let emails = words(resolve_setting("access.emails", file.emails, dir)?)
        .into_iter()
        .map(|email| email.to_ascii_lowercase())
        .collect();
    let (jwt, off) = match (team, audiences.is_empty()) {
        (Some(team), false) => (Some(AccessPolicy::new(&team, audiences, emails)?), None),
        _ if declared => (
            None,
            Some("`[access]` has no team or no aud here (unset in the environment?)".to_string()),
        ),
        _ => (None, None),
    };
    Ok(Access {
        jwt,
        token: file.token,
        fallback,
        off,
    })
}

/// `[kv]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KvFile {
    pub max_key_bytes: Option<usize>,
    pub max_value_bytes: Option<usize>,
    pub max_keys: Option<u64>,
    pub max_bytes: Option<u64>,
}

/// `[fetch]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchFile {
    #[serde(default)]
    pub allow: Vec<String>,
    pub timeout: Option<String>,
    pub max_request_bytes: Option<usize>,
    pub max_response_bytes: Option<usize>,
}

/// An app's key-value quotas.
#[derive(Clone, Debug)]
pub struct KvLimits {
    /// The longest key, in bytes.
    pub max_key_bytes: usize,
    /// The largest value, in bytes.
    pub max_value_bytes: usize,
    /// How many keys the app may hold.
    pub max_keys: u64,
    /// The keys' and values' bytes, summed, the app may hold.
    pub max_bytes: u64,
}

impl Default for KvLimits {
    fn default() -> KvLimits {
        KvLimits {
            max_key_bytes: 1024,
            max_value_bytes: 1 << 20,
            max_keys: 100_000,
            max_bytes: 64 << 20,
        }
    }
}

/// One `[fetch] allow` entry: `scheme://host`, `scheme://host:port` or
/// `scheme://host:*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowRule {
    pub scheme: String,
    pub host: String,
    /// `None` is any port.
    pub port: Option<u16>,
}

impl AllowRule {
    /// Parses one entry. The port defaults to the scheme's (80 or 443).
    pub fn parse(text: &str) -> Result<AllowRule, String> {
        let bad = || {
            format!(
                "`{text}` is not an allowlist entry like \"https://api.example.com\", \
                 \"http://127.0.0.1:8080\" or \"http://localhost:*\""
            )
        };
        let (scheme, rest) = text.split_once("://").ok_or_else(bad)?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return Err(bad()),
        };
        if rest.is_empty() || rest.contains('/') || rest.contains('@') {
            return Err(bad());
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, "*")) => (host, None),
            Some((host, port)) => (host, Some(port.parse().map_err(|_| bad())?)),
            None => (rest, Some(default_port)),
        };
        if host.is_empty() {
            return Err(bad());
        }
        Ok(AllowRule {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Whether a request to `scheme://host:port` is allowed by this rule.
    pub fn admits(&self, scheme: &str, host: &str, port: u16) -> bool {
        self.scheme == scheme
            && self.host.eq_ignore_ascii_case(host)
            && self.port.is_none_or(|allowed| allowed == port)
    }
}

impl std::fmt::Display for AllowRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}://{}:{port}", self.scheme, self.host),
            None => write!(f, "{}://{}:*", self.scheme, self.host),
        }
    }
}

/// Where an app's `fetch` may go, and how much it may move.
#[derive(Clone, Debug)]
pub struct FetchPolicy {
    /// Empty: nowhere.
    pub allow: Vec<AllowRule>,
    /// One fetch, connect to last byte. The run's deadline also bounds it.
    pub timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
}

impl Default for FetchPolicy {
    fn default() -> FetchPolicy {
        FetchPolicy {
            allow: Vec::new(),
            timeout: Duration::from_secs(10),
            max_request_bytes: 1 << 20,
            max_response_bytes: 4 << 20,
        }
    }
}

impl FetchPolicy {
    /// Whether `scheme://host:port` is on the allowlist.
    pub fn admits(&self, scheme: &str, host: &str, port: u16) -> bool {
        self.allow
            .iter()
            .any(|rule| rule.admits(scheme, host, port))
    }
}

/// `[limits]` as written; also what an override sets, key by key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_host_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_call_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_heap_words: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_queued: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_bytes: Option<usize>,
}

impl LimitsFile {
    /// Whether no key is set.
    pub fn is_empty(&self) -> bool {
        *self == LimitsFile::default()
    }

    /// `self`, with every key `over` sets replaced.
    fn overlaid(self, over: &LimitsFile) -> LimitsFile {
        LimitsFile {
            fuel: over.fuel.or(self.fuel),
            max_host_calls: over.max_host_calls.or(self.max_host_calls),
            deadline: over.deadline.clone().or(self.deadline),
            max_call_depth: over.max_call_depth.or(self.max_call_depth),
            max_heap_words: over.max_heap_words.or(self.max_heap_words),
            max_in_flight: over.max_in_flight.or(self.max_in_flight),
            max_queued: over.max_queued.or(self.max_queued),
            max_request_bytes: over.max_request_bytes.or(self.max_request_bytes),
            max_response_bytes: over.max_response_bytes.or(self.max_response_bytes),
        }
    }
}

/// What the admin app changed of an app's `app.toml`, kept in the data
/// directory rather than in the file: a release replaces `apps/` wholesale,
/// and the change has to outlive it.
///
/// Sets are kept as what was added and removed relative to the file, not as
/// the whole set, so that a release whose `app.toml` grants something new
/// still grants it — unless that very capability was removed here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppOverride {
    /// `false`: the app is loaded but not routed to.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch_allow_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch_allow_remove: Vec<String>,
    #[serde(default, skip_serializing_if = "LimitsFile::is_empty")]
    pub limits: LimitsFile,
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl AppOverride {
    /// The default: enabled, nothing changed.
    pub fn none() -> AppOverride {
        AppOverride {
            enabled: true,
            ..AppOverride::default()
        }
    }

    /// Whether this changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == AppOverride::none()
    }

    /// Whether it changes the configuration (anything but `enabled`).
    pub fn changes_config(&self) -> bool {
        AppOverride {
            enabled: true,
            ..self.clone()
        } != AppOverride::none()
    }

    /// The override that turns the app's own configuration — `file`, as
    /// its `app.toml` reads with no override — into `grant`, `allow` and
    /// `limits`; `enabled` as given. A limit equal to what the file already
    /// comes to is no change and is not kept, so that a later release's
    /// `app.toml` is not masked by a value nobody changed.
    pub fn between(
        file: &AppConfig,
        grant: &BTreeSet<String>,
        allow: &[String],
        limits: &LimitsFile,
        enabled: bool,
    ) -> AppOverride {
        let file_allow: BTreeSet<String> = file.file_allow.iter().cloned().collect();
        let allow: BTreeSet<String> = allow.iter().cloned().collect();
        let theirs = file.limits.as_file();
        let differs = |mine: &Option<u64>, theirs: &Option<u64>| mine.filter(|_| mine != theirs);
        let differs_usize =
            |mine: &Option<usize>, theirs: &Option<usize>| mine.filter(|_| mine != theirs);
        let limits = LimitsFile {
            fuel: differs(&limits.fuel, &theirs.fuel),
            max_host_calls: differs(&limits.max_host_calls, &theirs.max_host_calls),
            deadline: limits.deadline.clone().filter(|mine| {
                let parsed = |text: &Option<String>| {
                    text.as_deref().and_then(|text| parse_duration(text).ok())
                };
                parsed(&Some(mine.clone())) != parsed(&theirs.deadline)
            }),
            max_call_depth: differs_usize(&limits.max_call_depth, &theirs.max_call_depth),
            max_heap_words: differs(&limits.max_heap_words, &theirs.max_heap_words),
            max_in_flight: differs_usize(&limits.max_in_flight, &theirs.max_in_flight),
            max_queued: differs_usize(&limits.max_queued, &theirs.max_queued),
            max_request_bytes: differs_usize(&limits.max_request_bytes, &theirs.max_request_bytes),
            max_response_bytes: differs_usize(
                &limits.max_response_bytes,
                &theirs.max_response_bytes,
            ),
        };
        AppOverride {
            enabled,
            grant_add: grant.difference(&file.granted).cloned().collect(),
            grant_remove: file.granted.difference(grant).cloned().collect(),
            fetch_allow_add: allow.difference(&file_allow).cloned().collect(),
            fetch_allow_remove: file_allow.difference(&allow).cloned().collect(),
            limits,
        }
    }

    /// `file` with this override applied.
    fn apply(&self, mut file: AppFile) -> AppFile {
        let mut grant: BTreeSet<String> = file.grant.into_iter().collect();
        grant.extend(self.grant_add.iter().cloned());
        for gone in &self.grant_remove {
            grant.remove(gone);
        }
        file.grant = grant.into_iter().collect();
        let mut allow = file.fetch.allow;
        for added in &self.fetch_allow_add {
            if !allow.contains(added) {
                allow.push(added.clone());
            }
        }
        allow.retain(|entry| !self.fetch_allow_remove.contains(entry));
        file.fetch.allow = allow;
        file.limits = std::mem::take(&mut file.limits).overlaid(&self.limits);
        file
    }
}

/// What bounds one app, with the defaults filled in.
#[derive(Clone, Debug)]
pub struct AppLimits {
    /// Each request's run: fuel, deadline, host calls, call depth, tasks.
    pub run: Limits,
    /// A run whose heap is larger than this when it answers is answered 500
    /// instead (`heap` in `/_host/stats`). Checked when the run answers,
    /// because that is the one point the runtime lets an embedder read a
    /// run's heap (`OwnedVm::heap_words`; a parked or yielded run does not
    /// expose it); the hard ceiling is [`HEAP_CEILING_WORDS`].
    pub max_heap_words: Option<u64>,
    /// Runs of this app started and not yet answered — running, parked or
    /// waiting to continue. A request beyond it waits in the queue.
    pub max_in_flight: usize,
    /// Requests of this app waiting to start. A request beyond it is
    /// rejected with 429.
    pub max_queued: usize,
    /// The largest request body accepted; larger is 413.
    pub max_request_bytes: usize,
    /// The largest response body an app may answer; larger is 500.
    pub max_response_bytes: usize,
}

impl Default for AppLimits {
    fn default() -> AppLimits {
        AppLimits {
            run: Limits {
                fuel: Some(50_000_000),
                deadline: Some(Duration::from_secs(10)),
                max_host_calls: Some(1_000),
                max_call_depth: None,
                // `spawn` is refused when the app is loaded (see
                // `apps::refuse_spawn`); this is the runtime's own backstop
                // for it, stopping a run whose first `spawn` got past that.
                max_tasks: Some(0),
            },
            max_heap_words: None,
            max_in_flight: 64,
            max_queued: 256,
            max_request_bytes: 1 << 20,
            max_response_bytes: 4 << 20,
        }
    }
}

impl AppLimits {
    /// These limits as `[limits]` would write them, every key set but a
    /// heap limit that is not.
    pub fn as_file(&self) -> LimitsFile {
        LimitsFile {
            fuel: self.run.fuel,
            max_host_calls: self.run.max_host_calls,
            deadline: self
                .run
                .deadline
                .map(|deadline| format!("{}ms", deadline.as_millis())),
            max_call_depth: self.run.max_call_depth,
            max_heap_words: self.max_heap_words,
            max_in_flight: Some(self.max_in_flight),
            max_queued: Some(self.max_queued),
            max_request_bytes: Some(self.max_request_bytes),
            max_response_bytes: Some(self.max_response_bytes),
        }
    }
}

/// An app's configuration, read and validated.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub entry: String,
    pub granted: BTreeSet<String>,
    pub limits: AppLimits,
    pub kv: KvLimits,
    pub fetch: FetchPolicy,
    pub secrets: Secrets,
    /// `[route] hosts`, lower-cased.
    pub hosts: Vec<String>,
    /// `[fetch] allow` as written, after any override: the entries the
    /// admin app edits.
    pub file_allow: Vec<String>,
    /// `[access]`, resolved.
    pub access: crate::access::Access,
}

/// Reads `dir/app.toml` for the app `name`.
pub fn read_app(dir: &Path, name: &str) -> Result<AppConfig, String> {
    read_app_with(dir, name, None)
}

/// [`read_app`], with `over` applied to the file before it is validated.
pub fn read_app_with(
    dir: &Path,
    name: &str,
    over: Option<&AppOverride>,
) -> Result<AppConfig, String> {
    let path = dir.join("app.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    parse_app_with(&text, name, Some(dir), over).map_err(|e| match over {
        Some(over) if over.changes_config() => {
            format!("`{}` with the admin's changes: {e}", path.display())
        }
        _ => format!("`{}`: {e}", path.display()),
    })
}

/// Parses an `app.toml` for the app `name`.
pub fn parse_app(text: &str, name: &str) -> Result<AppConfig, String> {
    parse_app_in(text, name, None)
}

/// [`parse_app`], with `file` secrets read relative to `dir`.
pub fn parse_app_in(text: &str, name: &str, dir: Option<&Path>) -> Result<AppConfig, String> {
    parse_app_with(text, name, dir, None)
}

/// [`parse_app_in`], with `over` applied to the file before anything is
/// validated.
pub fn parse_app_with(
    text: &str,
    name: &str,
    dir: Option<&Path>,
    over: Option<&AppOverride>,
) -> Result<AppConfig, String> {
    let file: AppFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
    let file = match over {
        Some(over) => over.apply(file),
        None => file,
    };
    let defaults = AppLimits::default();
    let l = file.limits;
    let deadline = match l.deadline {
        Some(text) => Some(parse_duration(&text)?),
        None => defaults.run.deadline,
    };
    if let Some(words) = l.max_heap_words {
        if words > HEAP_CEILING_WORDS {
            return Err(format!(
                "`limits.max_heap_words = {words}` is above the runtime's fixed per-run heap of \
                 {HEAP_CEILING_WORDS} words, which `OwnedVm` cannot raise"
            ));
        }
    }
    let at_least_one = |key: &str, value: Option<usize>, default: usize| match value {
        Some(0) => Err(format!("`limits.{key}` must be at least 1")),
        Some(n) => Ok(n),
        None => Ok(default),
    };
    let limits = AppLimits {
        run: Limits {
            fuel: l.fuel.or(defaults.run.fuel),
            deadline,
            max_host_calls: l.max_host_calls.or(defaults.run.max_host_calls),
            max_call_depth: l.max_call_depth.or(defaults.run.max_call_depth),
            max_tasks: defaults.run.max_tasks,
        },
        max_heap_words: l.max_heap_words,
        max_in_flight: at_least_one("max_in_flight", l.max_in_flight, defaults.max_in_flight)?,
        max_queued: l.max_queued.unwrap_or(defaults.max_queued),
        max_request_bytes: l.max_request_bytes.unwrap_or(defaults.max_request_bytes),
        max_response_bytes: l.max_response_bytes.unwrap_or(defaults.max_response_bytes),
    };
    if limits
        .run
        .deadline
        .is_some_and(|deadline| deadline.is_zero())
    {
        return Err("`limits.deadline` must be longer than 0 ms".to_string());
    }
    if limits.run.fuel == Some(0) {
        return Err("`limits.fuel` must be at least 1".to_string());
    }
    for capability in &file.grant {
        if capability.is_empty()
            || !capability
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("`grant`: `{capability}` is not a capability name"));
        }
    }
    if file.grant.iter().any(|c| c == ADMIN_CAPABILITY) && name != ADMIN_APP {
        return Err(format!(
            "`grant` names `{ADMIN_CAPABILITY}`, which only the app `{ADMIN_APP}` may be \
             granted: it can change every app's configuration"
        ));
    }
    let mut hosts = Vec::new();
    for host in &file.route.hosts {
        let host = host.to_ascii_lowercase();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !valid {
            return Err(format!(
                "`route.hosts`: `{host}` is not a hostname (no scheme, port or path)"
            ));
        }
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    let entry = file.entry.unwrap_or_else(|| format!("{name}.handle"));
    if entry.split_once('.').is_none() {
        return Err(format!("`entry = \"{entry}\"` is not `module.function`"));
    }
    let kv_defaults = KvLimits::default();
    let kv = KvLimits {
        max_key_bytes: file.kv.max_key_bytes.unwrap_or(kv_defaults.max_key_bytes),
        max_value_bytes: file
            .kv
            .max_value_bytes
            .unwrap_or(kv_defaults.max_value_bytes),
        max_keys: file.kv.max_keys.unwrap_or(kv_defaults.max_keys),
        max_bytes: file.kv.max_bytes.unwrap_or(kv_defaults.max_bytes),
    };
    let fetch_defaults = FetchPolicy::default();
    let fetch = FetchPolicy {
        allow: file
            .fetch
            .allow
            .iter()
            .map(|entry| AllowRule::parse(entry).map_err(|why| format!("`fetch.allow`: {why}")))
            .collect::<Result<_, _>>()?,
        timeout: match file.fetch.timeout {
            Some(text) => parse_duration(&text)?,
            None => fetch_defaults.timeout,
        },
        max_request_bytes: file
            .fetch
            .max_request_bytes
            .unwrap_or(fetch_defaults.max_request_bytes),
        max_response_bytes: file
            .fetch
            .max_response_bytes
            .unwrap_or(fetch_defaults.max_response_bytes),
    };
    let secrets = resolve_secrets(file.secrets, dir)?;
    let access = resolve_access(file.access, &secrets, dir)?;
    Ok(AppConfig {
        entry,
        granted: file.grant.into_iter().collect(),
        limits,
        kv,
        fetch,
        secrets,
        hosts,
        file_allow: file.fetch.allow,
        access,
    })
}

/// `"300ms"` or `"5s"`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, unit) = match text.strip_suffix("ms") {
        Some(number) => (number, 1),
        None => match text.strip_suffix('s') {
            Some(number) => (number, 1000),
            None => {
                return Err(format!(
                    "`{text}` is not a duration like \"300ms\" or \"5s\""
                ))
            }
        },
    };
    let n: u64 = number
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` is not a duration like \"300ms\" or \"5s\""))?;
    Ok(Duration::from_millis(n * unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_every_default() {
        let config = parse_app("", "hello").unwrap();
        assert_eq!(config.entry, "hello.handle");
        assert!(config.granted.is_empty());
        assert_eq!(config.limits.run.fuel, Some(50_000_000));
        assert_eq!(config.limits.run.max_tasks, Some(0));
    }

    #[test]
    fn limits_are_read() {
        let config = parse_app(
            "grant = [\"log\"]\n[limits]\nfuel = 10\ndeadline = \"300ms\"\nmax_queued = 0\n",
            "a",
        )
        .unwrap();
        assert_eq!(config.limits.run.fuel, Some(10));
        assert_eq!(config.limits.run.deadline, Some(Duration::from_millis(300)));
        assert_eq!(config.limits.max_queued, 0);
        assert!(config.granted.contains("log"));
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let error = parse_app("[limits]\nfuell = 10\n", "a").unwrap_err();
        assert!(error.contains("fuell"), "{error}");
    }

    #[test]
    fn a_heap_above_the_ceiling_is_refused() {
        let error = parse_app("[limits]\nmax_heap_words = 99999999\n", "a").unwrap_err();
        assert!(error.contains("fixed per-run heap"), "{error}");
    }

    #[test]
    fn allowlist_entries() {
        let rule = AllowRule::parse("https://API.example.com").unwrap();
        assert!(rule.admits("https", "api.example.com", 443));
        assert!(!rule.admits("http", "api.example.com", 443));
        assert!(!rule.admits("https", "api.example.com", 8443));
        let any = AllowRule::parse("http://127.0.0.1:*").unwrap();
        assert!(any.admits("http", "127.0.0.1", 9999));
        assert!(!any.admits("http", "127.0.0.2", 9999));
        assert!(AllowRule::parse("ftp://x").is_err());
        assert!(AllowRule::parse("http://x/path").is_err());
        assert!(AllowRule::parse("example.com").is_err());
        let config = parse_app("[fetch]\nallow = [\"http://localhost:8080\"]\n", "a").unwrap();
        assert!(config.fetch.admits("http", "localhost", 8080));
    }

    #[test]
    fn secrets_resolve_and_never_print() {
        std::env::set_var("COVE_HOST_TEST_SECRET", "from-env");
        let config = parse_app(
            "[secrets]\na = { value = \"literal\" }\nb = { env = \"COVE_HOST_TEST_SECRET\" }\n",
            "x",
        )
        .unwrap();
        assert_eq!(config.secrets.0["a"], "literal");
        assert_eq!(config.secrets.0["b"], "from-env");
        assert_eq!(format!("{:?}", config.secrets), "{\"a\", \"b\"}");
        let missing = parse_app(
            "[secrets]\nc = { env = \"COVE_HOST_NOT_SET_ANYWHERE\" }\n",
            "x",
        );
        assert!(missing.unwrap_err().contains("is not set"));
        assert!(parse_app("[secrets]\nd = { value = \"v\", env = \"E\" }\n", "x").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("5m").is_err());
    }

    #[test]
    fn an_override_is_kept_as_what_changed_from_the_file() {
        let file = parse_app(
            "grant = [\"kv\", \"log\"]\n[limits]\nfuel = 10\n[fetch]\nallow = [\"https://a.example\"]\n",
            "notes",
        )
        .unwrap();
        let grant: BTreeSet<String> = ["log", "time"].iter().map(|s| s.to_string()).collect();
        let mut limits = file.limits.as_file();
        limits.fuel = Some(20);
        let over = AppOverride::between(
            &file,
            &grant,
            &["https://b.example".to_string()],
            &limits,
            true,
        );
        assert_eq!(over.grant_add, ["time"]);
        assert_eq!(over.grant_remove, ["kv"]);
        assert_eq!(over.fetch_allow_add, ["https://b.example"]);
        assert_eq!(over.fetch_allow_remove, ["https://a.example"]);
        // Only the limit that differs from what the file comes to.
        assert_eq!(
            over.limits,
            LimitsFile {
                fuel: Some(20),
                ..LimitsFile::default()
            }
        );
        // Applied to a later file that grants something new, the new grant
        // stays: only what was taken away is.
        let later = parse_app_with(
            "grant = [\"kv\", \"log\", \"random\"]\n",
            "notes",
            None,
            Some(&over),
        )
        .unwrap();
        let granted: Vec<&str> = later.granted.iter().map(String::as_str).collect();
        assert_eq!(granted, ["log", "random", "time"]);
        assert_eq!(later.limits.run.fuel, Some(20));
        assert_eq!(later.file_allow, ["https://b.example"]);
        // Nothing changed is no override.
        let same = AppOverride::between(
            &file,
            &file.granted,
            &file.file_allow,
            &file.limits.as_file(),
            true,
        );
        assert!(same.is_empty(), "{same:?}");
    }

    #[test]
    fn admin_and_hostnames_are_held_to_their_rules() {
        let error = parse_app("grant = [\"admin\"]\n", "notes").unwrap_err();
        assert!(error.contains("only the app `admin`"), "{error}");
        assert!(parse_app("grant = [\"admin\"]\n", "admin").is_ok());
        let config = parse_app("[route]\nhosts = [\"Admin.Example\"]\n", "admin").unwrap();
        assert_eq!(config.hosts, ["admin.example"]);
        for bad in ["https://a.example", "a.example:80", "a..example", ""] {
            let text = format!("[route]\nhosts = [\"{bad}\"]\n");
            assert!(parse_app(&text, "a").is_err(), "{bad}");
        }
    }

    #[test]
    fn access_is_on_with_a_team_and_an_aud_and_off_without() {
        let secret = "[secrets]\nadmin = { value = \"s\" }\n";
        let on = parse_app(
            &format!(
                "{secret}[access]\nteam = {{ value = \"t.cloudflareaccess.com\" }}\n\
                 aud = {{ value = \"a1, a2\" }}\nemails = {{ value = \"A@x.com b@y.com\" }}\n\
                 token = \"admin\"\n"
            ),
            "x",
        )
        .unwrap();
        let policy = on.access.jwt.unwrap();
        assert_eq!(policy.issuer, "https://t.cloudflareaccess.com");
        assert_eq!(policy.audiences, ["a1", "a2"]);
        assert_eq!(policy.emails, ["a@x.com", "b@y.com"]);
        assert!(!on.access.fallback);
        assert_eq!(on.access.token.as_deref(), Some("admin"));
        // An unset environment variable turns it off, and says so.
        let off = parse_app(
            &format!(
                "{secret}[access]\nteam = {{ env = \"COVE_HOST_NOT_SET_ANYWHERE\" }}\n\
                 aud = {{ value = \"a\" }}\ntoken = \"admin\"\n"
            ),
            "x",
        )
        .unwrap();
        assert!(off.access.jwt.is_none());
        assert!(off.access.off.is_some());
        // No `[access]` at all: off, and nothing to say.
        let none = parse_app(secret, "x").unwrap();
        assert!(none.access.jwt.is_none() && none.access.off.is_none());
    }

    #[test]
    fn access_is_held_to_its_rules() {
        let secret = "[secrets]\nadmin = { value = \"s\" }\n";
        for (access, wanted) in [
            ("token = \"nope\"\n", "names no `[secrets]` entry"),
            ("fallback = \"maybe\"\n", "is not \"none\" or \"token\""),
            ("fallback = \"token\"\n", "needs `access.token`"),
            (
                "team = { value = \"t.example/x\" }\naud = { value = \"a\" }\n",
                "team domain",
            ),
            ("aud = { value = \"a\", env = \"B\" }\n", "exactly one of"),
            ("audience = \"a\"\n", "audience"),
        ] {
            let error = parse_app(&format!("{secret}[access]\n{access}"), "x").unwrap_err();
            assert!(error.contains(wanted), "{access}: {error}");
        }
        let fallback = parse_app(
            &format!("{secret}[access]\ntoken = \"admin\"\nfallback = \"token\"\n"),
            "x",
        )
        .unwrap();
        assert!(fallback.access.fallback);
    }
}
