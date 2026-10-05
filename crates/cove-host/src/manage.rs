//! Changing a running app's configuration: what the admin app does through
//! the `host` module ([`crate::admin_module`]), and what `cove-host
//! enable`/`disable`/`reset` do through the admin listener.
//!
//! **A change is a re-check, never a revocation.** Cove's capabilities are
//! decided before a program runs — the checker derives what the entry
//! requires and the host refuses what is not granted — so there is no taking
//! a capability away from a run in progress. A change to an app's grant,
//! allowlist or limits loads the app again with the change applied, exactly
//! as `cove-host update` loads a new version (parse, check, admit, lower,
//! prepare, compile, off the request path), and routes to that version:
//! requests already admitted finish on the version they were admitted to.
//!
//! What comes of it:
//!
//! | the reload | the change | the app |
//! | --- | --- | --- |
//! | loads | kept, written to `overrides.json` | serves the new version |
//! | does not admit: its entry requires a capability the change took away | kept | **refused**, answering 503 with the reason; every other app serves |
//! | anything else — a limit out of range, an allowlist entry or capability that does not parse, an unknown capability, `admin` for an app that is not `admin`, a check that fails | **not kept** | serves as before; the reasons are the answer |
//!
//! Taking a capability away from an app that needs it is a decision the
//! admin may make — that is how an app is stopped from using something — so
//! it is applied; a change that is merely wrong is refused. The admin app is
//! the exception to the first row: a change that would leave *it* refused,
//! or without `admin`, is refused, as is disabling it — through the admin app.
//! The admin listener (`cove-host disable admin`, `cove-host reset admin`)
//! can still do either: it is the emergency exit, and it does not depend on
//! the admin app working.
//!
//! **Disabling** stops routing to the app and nothing else: it keeps its
//! version loaded, its queue, counters, log and store. New requests are
//! answered 503 (`app … is disabled by the administrator`, counted as
//! `rejected.disabled`); those already admitted finish. Enabling routes to it
//! again, with its data as it was.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value as Json;

use crate::apps::{load_with, AppState, Lineage};
use crate::config::{read_app_with, AppOverride, LimitsFile, ADMIN_APP, ADMIN_CAPABILITY};
use crate::server::Front;

/// Who the change history says made a change through the admin listener.
pub const LISTENER: &str = "admin listener";

/// Where a change came from.
pub enum Via {
    /// The admin app, with what it says of its user.
    AdminApp(String),
    /// The admin listener: the CLI, on the machine, with the admin token.
    Listener,
}

impl Via {
    fn who(&self) -> String {
        match self {
            Via::AdminApp(who) if who.trim().is_empty() => "admin app".to_string(),
            Via::AdminApp(who) => format!("admin app: {}", who.trim()),
            Via::Listener => LISTENER.to_string(),
        }
    }

    fn is_admin_app(&self) -> bool {
        matches!(self, Via::AdminApp(_))
    }
}

/// Why a change was not made.
#[derive(Debug)]
pub enum ChangeError {
    /// No app by that name.
    NotFound(String),
    /// The change was refused; the configuration is as it was.
    Refused(String),
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeError::NotFound(why) | ChangeError::Refused(why) => f.write_str(why),
        }
    }
}

/// What an app's configuration is to become: the grant, the allowlist, and
/// the limits (each key set to its effective value).
#[derive(Clone, Debug)]
pub struct Settings {
    pub grant: BTreeSet<String>,
    pub fetch_allow: Vec<String>,
    pub limits: LimitsFile,
}

/// One recent error of an app.
pub struct Failure {
    pub at_ms: u64,
    pub kind: String,
    pub status: u64,
    pub version: String,
    pub message: String,
}

/// One app as the admin app lists it.
pub struct AppInfo {
    pub name: String,
    /// `serving`, `disabled`, `refused` or `removed`.
    pub state: String,
    pub reason: String,
    pub version: String,
    pub tier: String,
    pub entry: String,
    pub hosts: Vec<String>,
    pub required: Vec<String>,
    pub required_open: bool,
    pub granted: Vec<String>,
    pub grant_added: Vec<String>,
    pub grant_removed: Vec<String>,
    pub fetch_allow: Vec<String>,
    pub limits: LimitsFile,
    pub limits_changed: Vec<String>,
    pub is_admin: bool,
    pub served: u64,
    pub ok: u64,
    pub errors: u64,
    pub rejected: u64,
    pub in_flight: u64,
    pub queued: u64,
    /// `(keys, bytes)` in its store, when one is open.
    pub kv: Option<(u64, u64)>,
    pub recent_errors: Vec<Failure>,
}

/// The `[limits]` keys `limits` sets.
fn limit_keys(limits: &LimitsFile) -> Vec<String> {
    let json = serde_json::to_value(limits).unwrap_or_default();
    json.as_object()
        .map(|keys| keys.keys().cloned().collect())
        .unwrap_or_default()
}

/// A JSON group of counters, summed.
fn sum(group: &Json) -> u64 {
    group
        .as_object()
        .map(|counts| counts.values().filter_map(Json::as_u64).sum())
        .unwrap_or_default()
}

impl Front {
    /// Every app the host has, in load order.
    pub(crate) fn app_infos(&self) -> Vec<AppInfo> {
        let context = self.ops_context();
        self.engine
            .slots()
            .iter()
            .map(|slot| {
                let json = crate::ops::app_json(&context, slot);
                let over = self.control.overrides.get(&slot.name);
                let mut info = AppInfo {
                    name: slot.name.clone(),
                    state: "removed".to_string(),
                    reason: String::new(),
                    version: String::new(),
                    tier: String::new(),
                    entry: String::new(),
                    hosts: Vec::new(),
                    required: Vec::new(),
                    required_open: false,
                    granted: Vec::new(),
                    grant_added: over
                        .as_ref()
                        .map(|o| o.grant_add.clone())
                        .unwrap_or_default(),
                    grant_removed: over
                        .as_ref()
                        .map(|o| o.grant_remove.clone())
                        .unwrap_or_default(),
                    fetch_allow: Vec::new(),
                    limits: LimitsFile::default(),
                    limits_changed: over
                        .as_ref()
                        .map(|o| limit_keys(&o.limits))
                        .unwrap_or_default(),
                    is_admin: slot.name == ADMIN_APP,
                    served: json["served"].as_u64().unwrap_or_default(),
                    ok: json["ok"].as_u64().unwrap_or_default(),
                    errors: sum(&json["errors"]),
                    rejected: sum(&json["rejected"]),
                    in_flight: json["in_flight"].as_u64().unwrap_or_default(),
                    queued: json["queued"].as_u64().unwrap_or_default(),
                    kv: json["kv"]["keys"]
                        .as_u64()
                        .zip(json["kv"]["bytes"].as_u64()),
                    recent_errors: slot
                        .counters
                        .recent_errors()
                        .iter()
                        .rev()
                        .take(10)
                        .map(|error| Failure {
                            at_ms: error["unix_ms"].as_u64().unwrap_or_default(),
                            kind: error["kind"].as_str().unwrap_or_default().to_string(),
                            status: error["status"].as_u64().unwrap_or_default(),
                            version: error["version"].as_str().unwrap_or_default().to_string(),
                            message: error["message"].as_str().unwrap_or_default().to_string(),
                        })
                        .collect(),
                };
                if let Some(app) = slot.current() {
                    let (state, reason, tier) = match &app.state {
                        AppState::Ready(ready) if slot.enabled() => {
                            ("serving", String::new(), ready.tier)
                        }
                        AppState::Ready(ready) => ("disabled", String::new(), ready.tier),
                        AppState::Refused(why) if slot.enabled() => ("refused", why.clone(), ""),
                        AppState::Refused(why) => ("disabled", why.clone(), ""),
                    };
                    info.state = state.to_string();
                    info.reason = reason;
                    info.tier = tier.to_string();
                    info.version = app.version.clone();
                    info.entry = app.entry.clone();
                    info.hosts = app.hosts.clone();
                    info.required = app.required.iter().cloned().collect();
                    info.required_open = app.open;
                    info.granted = app.granted.iter().cloned().collect();
                    info.fetch_allow = app.fetch_allow.clone();
                    info.limits = app.limits.as_file();
                }
                info
            })
            .collect()
    }

    /// Every capability the host's modules declare: what a grant may name.
    pub(crate) fn capabilities(&self) -> Vec<String> {
        let mut capabilities: Vec<String> = self
            .options
            .modules
            .schemas()
            .iter()
            .flat_map(|schema| {
                std::iter::once(schema.capability)
                    .chain(schema.operations.iter().map(|op| op.capability))
            })
            // `web` declares types only: nothing to grant.
            .filter(|capability| *capability != "web")
            .map(str::to_string)
            .collect();
        capabilities.sort();
        capabilities.dedup();
        capabilities
    }

    /// Records a change refused before anything was tried.
    pub(crate) fn record_refusal(&self, app: &str, action: &str, via: &Via, why: &str) {
        self.control
            .history
            .record(&via.who(), app, action, "", &format!("refused: {why}"));
    }

    /// Routes to `name` again, or stops routing to it.
    pub(crate) async fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
        via: &Via,
    ) -> Result<String, ChangeError> {
        let action = if enabled { "enable" } else { "disable" };
        let _one_at_a_time = self.updating.lock().await;
        let result = (|| {
            let Some(slot) = self.engine.slot_named(name) else {
                return Err(ChangeError::NotFound(format!("no app named `{name}`")));
            };
            if !enabled && name == ADMIN_APP && via.is_admin_app() {
                return Err(ChangeError::Refused(format!(
                    "the admin app cannot disable itself; on the machine, `cove-host disable \
                     {ADMIN_APP}` can"
                )));
            }
            let mut over = self
                .control
                .overrides
                .get(name)
                .unwrap_or_else(AppOverride::none);
            over.enabled = enabled;
            self.control
                .overrides
                .set(name, over)
                .map_err(ChangeError::Refused)?;
            let was = slot.enabled();
            slot.set_enabled(enabled);
            let line = match (was, enabled) {
                (true, true) => "enabled (it was)".to_string(),
                (false, false) => "disabled (it was)".to_string(),
                (_, true) => "enabled: routed to again".to_string(),
                (_, false) => "disabled: not routed to; in-flight requests finish".to_string(),
            };
            slot.logs.push("host", &format!("{line} ({})", via.who()));
            eprintln!("cove-host: [{name}] {line}");
            Ok(format!("`{name}` {line}"))
        })();
        self.record(name, action, "", via, &result);
        result
    }

    /// Makes `name`'s grant, allowlist and limits `settings`, if the app
    /// loads with them (or is refused only for a capability they take away).
    pub(crate) async fn configure(
        &self,
        name: &str,
        settings: Settings,
        via: &Via,
    ) -> Result<String, ChangeError> {
        let detail = describe_settings(&settings);
        let _one_at_a_time = self.updating.lock().await;
        let result = self.reconfigure(name, Some(settings), via).await;
        self.record(name, "configure", &detail, via, &result);
        result
    }

    /// Drops the admin's changes to `name`'s configuration: back to its
    /// `app.toml`. Whether it is enabled stays as it is.
    pub(crate) async fn reset(&self, name: &str, via: &Via) -> Result<String, ChangeError> {
        let _one_at_a_time = self.updating.lock().await;
        let result = self.reconfigure(name, None, via).await;
        self.record(name, "reset", "back to app.toml", via, &result);
        result
    }

    fn record(
        &self,
        name: &str,
        action: &str,
        detail: &str,
        via: &Via,
        result: &Result<String, ChangeError>,
    ) {
        let outcome = match result {
            Ok(message) => format!("applied: {message}"),
            Err(error) => format!("refused: {error}"),
        };
        self.control
            .history
            .record(&via.who(), name, action, detail, &outcome);
    }

    /// Loads `name` with `settings` (or with none of the admin's changes),
    /// and installs and keeps the result if it may be.
    async fn reconfigure(
        &self,
        name: &str,
        settings: Option<Settings>,
        via: &Via,
    ) -> Result<String, ChangeError> {
        let Some(slot) = self.engine.slot_named(name) else {
            return Err(ChangeError::NotFound(format!("no app named `{name}`")));
        };
        let dir = self.options.apps.join(name);
        // The app as its `app.toml` alone says: what the change is relative
        // to.
        let file = read_app_with(&dir, name, None).map_err(|why| {
            ChangeError::Refused(format!(
                "its app.toml does not read, so nothing is changed: {why}"
            ))
        })?;
        let current = self
            .control
            .overrides
            .get(name)
            .unwrap_or_else(AppOverride::none);
        let next = match &settings {
            Some(settings) => {
                let known = self.capabilities();
                let unknown: Vec<&String> = settings
                    .grant
                    .iter()
                    .filter(|capability| !known.contains(capability))
                    .collect();
                if let Some(capability) = unknown.first() {
                    return Err(ChangeError::Refused(format!(
                        "`{capability}` is not a capability this host has ({})",
                        known.join(", ")
                    )));
                }
                if name == ADMIN_APP && !settings.grant.contains(ADMIN_CAPABILITY) {
                    return Err(ChangeError::Refused(format!(
                        "the admin app keeps `{ADMIN_CAPABILITY}`: without it nothing could \
                         change the configuration but the CLI"
                    )));
                }
                AppOverride::between(
                    &file,
                    &settings.grant,
                    &settings.fetch_allow,
                    &settings.limits,
                    current.enabled,
                )
            }
            None => AppOverride {
                enabled: current.enabled,
                ..AppOverride::none()
            },
        };
        // The config's own rules first: they need no compiling.
        read_app_with(&dir, name, Some(&next)).map_err(ChangeError::Refused)?;

        let lineage = Lineage {
            counters: Arc::clone(&slot.counters),
            logs: Arc::clone(&slot.logs),
            number: slot.next_version(),
        };
        let load = self.load.clone();
        let owned = name.to_string();
        let candidate = next.clone();
        let mut app = tokio::task::spawn_blocking(move || {
            load_with(&owned, &dir, &load, lineage, Some(&candidate))
        })
        .await
        .map_err(|e| ChangeError::Refused(format!("the reload failed: {e}")))?;
        self.refuse_taken_hostnames(&mut app);
        let refused = match &app.state {
            AppState::Ready(_) => None,
            AppState::Refused(why) => Some(why.clone()),
        };
        if let Some(why) = &refused {
            // Refused for want of a capability the change took away: that
            // is the change doing what it says, for any app but the admin
            // app. Anything else is a change that does not work.
            let lacks = !app.required.is_empty() && !app.required.is_subset(&app.granted);
            if !lacks || (name == ADMIN_APP && via.is_admin_app()) {
                return Err(ChangeError::Refused(format!(
                    "`{name}` would not load with this change, so it was not made:\n{why}"
                )));
            }
        }
        // Kept first, then routed to: nothing between them can fail or be
        // cancelled, so the host never serves a change a restart would not
        // find.
        self.control
            .overrides
            .set(name, next)
            .map_err(ChangeError::Refused)?;
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        let installed = self.engine.install(app);
        counters.updates.fetch_add(1, Ordering::Relaxed);
        let line = match &refused {
            None => format!(
                "reconfigured ({}): {} -> {}",
                via.who(),
                installed.previous.as_deref().unwrap_or("nothing"),
                installed.version
            ),
            Some(why) => format!(
                "reconfigured ({}) and now refused: {}",
                via.who(),
                why.lines().next().unwrap_or_default()
            ),
        };
        logs.push("host", &line);
        eprintln!("cove-host: [{name}] {line}");
        Ok(match refused {
            None => format!(
                "`{name}` serves {} with the new configuration",
                installed.version
            ),
            Some(why) => format!(
                "`{name}` is now refused, answering 503 to every request: {}",
                why.lines().next().unwrap_or_default()
            ),
        })
    }
}

/// What a configure asked for, for the history.
fn describe_settings(settings: &Settings) -> String {
    let limits = serde_json::to_value(&settings.limits).unwrap_or_default();
    let limits = limits
        .as_object()
        .map(|keys| {
            keys.iter()
                .map(|(key, value)| match value {
                    Json::String(text) => format!("{key}={text}"),
                    other => format!("{key}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    format!(
        "grant [{}]; fetch.allow [{}]; {limits}",
        settings
            .grant
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        settings.fetch_allow.join(", "),
    )
}
