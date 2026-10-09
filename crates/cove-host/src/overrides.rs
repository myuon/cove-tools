//! What the admin app changed, kept where a release cannot overwrite it.
//!
//! The admin app ([`crate::admin_module`]) enables and disables apps and
//! changes their grants, fetch allowlists and limits. Those changes are not
//! written into `apps/<app>/app.toml`: a release's `install.sh` replaces
//! `apps/` wholesale, and the deployed unit has it read-only besides
//! (`ProtectHome=read-only`, with only the data directory writable). So they
//! are kept in the data directory, beside the apps' own state, which no
//! release touches:
//!
//! | file | what |
//! | --- | --- |
//! | `<data>/_host/overrides.json` | per app, an [`AppOverride`]: `enabled`, capabilities added and removed, allowlist entries added and removed, limits set — each relative to `app.toml` |
//! | `<data>/_host/changes.jsonl` | the change history: one JSON object per line, `unix_ms`, `who`, `app`, `action`, `detail`, `outcome`; appended, never rewritten |
//! | `<data>/_host/secrets` | the secret store ([`crate::secrets`]), mode 0600 |
//!
//! `_host` cannot be an app's directory (an app's name starts with a
//! letter). An override is applied every time the app is loaded — at start,
//! on `cove-host update`, on a change — so it survives a restart and a
//! release alike. `overrides.json` is the emergency exit as well: it is
//! plain JSON, and deleting an app's entry (or the file) and restarting
//! puts the app back to its `app.toml`; `cove-host reset <app>` does the
//! same on the running host.
//!
//! Without a data directory (the tests' in-memory hosts) both live in memory
//! only.
//!
//! An `overrides.json` written by a host from before Cove's ADR 0091 may set
//! `limits.fuel`, which no longer exists. It is the operator's data, not a
//! file being put forward, so it is not refused: the key is dropped when the
//! file is read, with a warning naming the app, and the next change written
//! leaves it out.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::AppOverride;
use crate::secrets::SecretStore;
use crate::server::Front;

/// How many changes are kept in memory for the admin app's history page.
/// The file keeps every one.
const HISTORY_IN_MEMORY: usize = 1000;

/// The admin's changes, and the host they act on: what the `host` module
/// of the admin app holds.
pub struct Control {
    pub overrides: Overrides,
    pub history: History,
    /// The secrets the admin sets ([`crate::secrets`]): `<data>/_host/secrets`.
    pub secrets: Arc<SecretStore>,
    front: OnceLock<Weak<Front>>,
}

impl Control {
    /// Reads `<data>/_host/`, or starts empty without a data directory. A
    /// file that does not read is an error, and the host does not start:
    /// running with the changes silently dropped would re-enable what was
    /// disabled and re-grant what was taken away.
    pub fn open(data: Option<&Path>) -> Result<Control, String> {
        let dir = data.map(|data| data.join("_host"));
        Ok(Control {
            overrides: Overrides::open(dir.as_deref())?,
            history: History::open(dir.as_deref())?,
            secrets: Arc::new(SecretStore::open(dir.as_deref())?),
            front: OnceLock::new(),
        })
    }

    /// Points the control at the host, once it exists.
    pub(crate) fn attach(&self, front: &Arc<Front>) {
        let _ = self.front.set(Arc::downgrade(front));
    }

    /// The host, while it runs.
    pub(crate) fn front(&self) -> Option<Arc<Front>> {
        self.front.get().and_then(Weak::upgrade)
    }
}

/// `overrides.json`.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OverridesFile {
    #[serde(default)]
    apps: BTreeMap<String, AppOverride>,
}

/// Every app's [`AppOverride`], by name.
pub struct Overrides {
    path: Option<PathBuf>,
    apps: Mutex<BTreeMap<String, AppOverride>>,
}

impl Overrides {
    fn open(dir: Option<&Path>) -> Result<Overrides, String> {
        let path = dir.map(|dir| dir.join("overrides.json"));
        let mut apps = match &path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => {
                    serde_json::from_str::<OverridesFile>(&text)
                        .map_err(|e| {
                            format!(
                                "`{}` does not read ({e}); fix it, or delete it to put every \
                                 app back to its app.toml",
                                path.display()
                            )
                        })?
                        .apps
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(e) => return Err(format!("cannot read `{}`: {e}", path.display())),
            },
            None => BTreeMap::new(),
        };
        for warning in drop_removed_keys(&mut apps) {
            eprintln!("cove-host: warning: {warning}");
        }
        Ok(Overrides {
            path,
            apps: Mutex::new(apps),
        })
    }

    /// `name`'s override, if it has one.
    pub fn get(&self, name: &str) -> Option<AppOverride> {
        self.apps.lock().unwrap().get(name).cloned()
    }

    /// Whether `name` is enabled: true unless an override says otherwise.
    pub fn enabled(&self, name: &str) -> bool {
        self.get(name).is_none_or(|over| over.enabled)
    }

    /// Keeps `over` as `name`'s override — written to disk first, so that a
    /// change the host acts on is one a restart will find. An empty one
    /// removes the entry.
    pub fn set(&self, name: &str, over: AppOverride) -> Result<(), String> {
        let mut apps = self.apps.lock().unwrap();
        let mut next = apps.clone();
        if over.is_empty() {
            next.remove(name);
        } else {
            next.insert(name.to_string(), over);
        }
        if let Some(path) = &self.path {
            write_atomically(path, &OverridesFile { apps: next.clone() })?;
        }
        *apps = next;
        Ok(())
    }
}

/// Takes the keys Cove has removed out of every override read — the
/// `limits.fuel` of ADR 0091 — and says what was taken, one line per app.
/// An override left with nothing in it is dropped as well.
fn drop_removed_keys(apps: &mut BTreeMap<String, AppOverride>) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, over) in apps.iter_mut() {
        if over.limits.fuel.take().is_some() {
            warnings.push(format!(
                "`overrides.json`: the admin's `limits.fuel` for `{name}` is dropped: Cove ADR \
                 0091 removed the fuel allowance, and the app's requests are bounded by \
                 `limits.deadline`"
            ));
        }
    }
    apps.retain(|_, over| !over.is_empty());
    warnings
}

/// Writes `value` as pretty JSON to `path` through a temporary file and a
/// rename, so a crash leaves the old file or the new one, never half.
fn write_atomically(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create `{}`: {e}", dir.display()))?;
    let temporary = path.with_extension("json.tmp");
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    let written = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    written.map_err(|e| format!("cannot write `{}`: {e}", path.display()))
}

/// One change, as the history records it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Change {
    pub unix_ms: u64,
    /// Who asked: what the admin app says of its user (the Cloudflare Access
    /// email, when it has one), or `admin listener` for `cove-host
    /// update`/`remove`/`enable`/`disable`/`reset`.
    pub who: String,
    /// The app; empty for a change to the secret store.
    pub app: String,
    /// `enable`, `disable`, `configure`, `reset`, `update`, `remove`,
    /// `deploy`, `rollback`, `secret set`, `secret delete`.
    pub action: String,
    /// What was asked for.
    pub detail: String,
    /// What came of it: `applied: …`, `refused: …`.
    pub outcome: String,
}

/// The change history: `changes.jsonl`, and its newest entries in memory.
pub struct History {
    path: Option<PathBuf>,
    recent: Mutex<Vec<Change>>,
}

impl History {
    fn open(dir: Option<&Path>) -> Result<History, String> {
        let path = dir.map(|dir| dir.join("changes.jsonl"));
        let mut recent = Vec::new();
        if let Some(path) = &path {
            match std::fs::read_to_string(path) {
                // A line that does not read (a write cut short) is skipped:
                // the history is a record, not configuration.
                Ok(text) => recent.extend(
                    text.lines()
                        .filter_map(|line| serde_json::from_str::<Change>(line).ok()),
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot read `{}`: {e}", path.display())),
            }
        }
        let excess = recent.len().saturating_sub(HISTORY_IN_MEMORY);
        recent.drain(..excess);
        Ok(History {
            path,
            recent: Mutex::new(recent),
        })
    }

    /// Records a change. A history that cannot be written is said on
    /// stderr; the change itself stands.
    pub fn record(&self, who: &str, app: &str, action: &str, detail: &str, outcome: &str) {
        let change = Change {
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            who: who.to_string(),
            app: app.to_string(),
            action: action.to_string(),
            detail: detail.to_string(),
            outcome: outcome.to_string(),
        };
        let mut recent = self.recent.lock().unwrap();
        if let Some(path) = &self.path {
            let appended = (|| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                let mut line = serde_json::to_string(&change).map_err(std::io::Error::other)?;
                line.push('\n');
                file.write_all(line.as_bytes())
            })();
            if let Err(e) = appended {
                eprintln!(
                    "cove-host: cannot append to `{}`: {e}; the change stands",
                    path.display()
                );
            }
        }
        recent.push(change);
        if recent.len() > HISTORY_IN_MEMORY {
            recent.remove(0);
        }
    }

    /// The newest `limit` changes, newest first.
    pub fn newest(&self, limit: usize) -> Vec<Change> {
        let recent = self.recent.lock().unwrap();
        recent.iter().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cove-host-overrides-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn overrides_and_history_survive_reopening() {
        let data = scratch("reopen");
        let control = Control::open(Some(&data)).unwrap();
        let over = AppOverride {
            enabled: false,
            grant_remove: vec!["kv".to_string()],
            ..AppOverride::none()
        };
        control.overrides.set("notes", over.clone()).unwrap();
        control
            .history
            .record("me", "notes", "disable", "", "applied");
        drop(control);
        let again = Control::open(Some(&data)).unwrap();
        assert_eq!(again.overrides.get("notes"), Some(over));
        assert!(!again.overrides.enabled("notes"));
        assert!(again.overrides.enabled("hello"));
        let history = again.history.newest(10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].who, "me");
        // An empty override is no entry at all.
        again.overrides.set("notes", AppOverride::none()).unwrap();
        let text = std::fs::read_to_string(data.join("_host/overrides.json")).unwrap();
        assert!(!text.contains("notes"), "{text}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_persisted_fuel_override_is_dropped_on_load_not_refused() {
        let data = scratch("fuel");
        std::fs::create_dir_all(data.join("_host")).unwrap();
        // What a host from before ADR 0091 wrote: one app with fuel and
        // another limit, one with fuel alone.
        std::fs::write(
            data.join("_host/overrides.json"),
            r#"{
  "apps": {
    "algo": { "limits": { "fuel": 900000000, "deadline": "20000ms" } },
    "ledger": { "limits": { "fuel": 500000000 } },
    "notes": { "enabled": false }
  }
}
"#,
        )
        .unwrap();
        let control = Control::open(Some(&data)).unwrap();
        let algo = control.overrides.get("algo").unwrap();
        assert_eq!(algo.limits.fuel, None);
        assert_eq!(algo.limits.deadline.as_deref(), Some("20000ms"));
        // Nothing left of it but the fuel: no override at all.
        assert_eq!(control.overrides.get("ledger"), None);
        assert!(!control.overrides.enabled("notes"));
        // The next write leaves it out.
        control
            .overrides
            .set("notes", control.overrides.get("notes").unwrap())
            .unwrap();
        let text = std::fs::read_to_string(data.join("_host/overrides.json")).unwrap();
        assert!(!text.contains("fuel"), "{text}");
        assert!(text.contains("20000ms"), "{text}");
        let _ = std::fs::remove_dir_all(&data);

        let mut apps = BTreeMap::new();
        apps.insert(
            "ledger".to_string(),
            serde_json::from_str::<AppOverride>(r#"{ "limits": { "fuel": 1 } }"#).unwrap(),
        );
        let warnings = drop_removed_keys(&mut apps);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`ledger`"), "{}", warnings[0]);
        assert!(warnings[0].contains("ADR 0091"), "{}", warnings[0]);
    }

    #[test]
    fn a_broken_overrides_file_stops_the_start() {
        let data = scratch("broken");
        std::fs::create_dir_all(data.join("_host")).unwrap();
        std::fs::write(data.join("_host/overrides.json"), "{ nope").unwrap();
        let error = Control::open(Some(&data)).err().unwrap();
        assert!(error.contains("delete it"), "{error}");
        let _ = std::fs::remove_dir_all(&data);
    }
}
