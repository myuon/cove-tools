//! The host's secret store: secrets the admin sets at run time (issue #32).
//!
//! An `app.toml` names a secret's source; `[secrets] gemini = { store =
//! "gemini" }` takes it from here, beside `env`, `file` and `value`. The
//! store is one file, `<data>/_host/secrets`, next to the admin's other
//! changes ([`crate::overrides`]): JSON, `{ "secrets": { "<name>": {
//! "value": "…", "updated_ms": … } } }`, mode 0600, written whole through a
//! temporary file that is synced and renamed, so a crash leaves the old
//! file or the new one and never a part of either.
//!
//! **Write-only.** Nothing here hands a value to anyone but the config
//! resolver (`SecretStore::get`): the listings carry names and times, the
//! `Debug` of the store prints names, and no error quotes a value. The admin
//! listener ([`crate::admin`]), the `host` module ([`crate::admin_module`])
//! and `cove-host secret` set, list and delete through [`crate::server`]'s
//! `Front::set_secret` and `Front::delete_secret`, which reload the apps
//! that use the secret.
//!
//! Without a data directory (the tests' in-memory hosts) the store lives in
//! memory only.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The file, under `<data>/_host/`.
pub const FILE: &str = "secrets";

/// The largest value the store takes: far more than any API key, small
/// enough that a mistaken paste of a file is refused.
pub const MAX_VALUE_BYTES: usize = 16 * 1024;

/// One stored secret.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    value: String,
    /// When it was last set, in milliseconds since the Unix epoch.
    updated_ms: u64,
}

/// The file as written.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    #[serde(default)]
    secrets: BTreeMap<String, Stored>,
}

/// What may be said of a stored secret: its name and when it was set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSecret {
    pub name: String,
    pub updated_ms: u64,
}

/// The store: the file, and what it holds.
pub struct SecretStore {
    path: Option<PathBuf>,
    secrets: Mutex<BTreeMap<String, Stored>>,
}

impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set()
            .entries(self.secrets.lock().unwrap().keys())
            .finish()
    }
}

impl SecretStore {
    /// An empty store in memory.
    pub fn in_memory() -> SecretStore {
        SecretStore {
            path: None,
            secrets: Mutex::new(BTreeMap::new()),
        }
    }

    /// Reads `<dir>/secrets` (`dir` is `<data>/_host`), or starts empty
    /// without one. A file that does not read is an error, saying so without
    /// quoting it.
    pub fn open(dir: Option<&Path>) -> Result<SecretStore, String> {
        let path = dir.map(|dir| dir.join(FILE));
        let secrets = match &path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => {
                    serde_json::from_str::<StoreFile>(&text)
                        .map_err(|e| {
                            // serde_json's message names a line and column,
                            // never the text there.
                            format!(
                                "the secret store `{}` does not read (line {}, column {}); \
                                 fix it, or delete it and set the secrets again",
                                path.display(),
                                e.line(),
                                e.column()
                            )
                        })?
                        .secrets
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(e) => return Err(format!("cannot read `{}`: {e}", path.display())),
            },
            None => BTreeMap::new(),
        };
        Ok(SecretStore {
            path,
            secrets: Mutex::new(secrets),
        })
    }

    /// The store of the data directory `data`: `<data>/_host/secrets`.
    pub fn open_data(data: &Path) -> Result<SecretStore, String> {
        SecretStore::open(Some(&data.join("_host")))
    }

    /// Where it is kept, if anywhere.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// `name`'s value: for resolving an app's `{ store = "…" }`, and nothing
    /// else.
    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.secrets
            .lock()
            .unwrap()
            .get(name)
            .map(|stored| stored.value.clone())
    }

    /// Whether `name` is set.
    pub fn contains(&self, name: &str) -> bool {
        self.secrets.lock().unwrap().contains_key(name)
    }

    /// Every stored secret's name and time, by name.
    pub fn list(&self) -> Vec<StoredSecret> {
        self.secrets
            .lock()
            .unwrap()
            .iter()
            .map(|(name, stored)| StoredSecret {
                name: name.clone(),
                updated_ms: stored.updated_ms,
            })
            .collect()
    }

    /// Sets `name` to `value` — written to disk first, so a value the host
    /// uses is one a restart finds.
    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        valid_name(name)?;
        valid_value(name, value)?;
        let mut secrets = self.secrets.lock().unwrap();
        let mut next = secrets.clone();
        next.insert(
            name.to_string(),
            Stored {
                value: value.to_string(),
                updated_ms: now_ms(),
            },
        );
        self.write(&next)?;
        *secrets = next;
        Ok(())
    }

    /// Removes `name`; whether it was there.
    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let mut secrets = self.secrets.lock().unwrap();
        if !secrets.contains_key(name) {
            return Ok(false);
        }
        let mut next = secrets.clone();
        next.remove(name);
        self.write(&next)?;
        *secrets = next;
        Ok(true)
    }

    fn write(&self, secrets: &BTreeMap<String, Stored>) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut text = serde_json::to_string_pretty(&StoreFile {
            secrets: secrets.clone(),
        })
        .map_err(|e| e.to_string())?;
        text.push('\n');
        write_private_atomically(path, text.as_bytes())
            .map_err(|e| format!("cannot write the secret store `{}`: {e}", path.display()))
    }
}

/// A secret's name: a letter or digit, then letters, digits, `_`, `-` or
/// `.`, at most 64 bytes — something that reads in a URL path, a form and a
/// TOML string as it is.
pub fn valid_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a secret name: a letter or digit, then letters, digits, `_`, `-` \
             or `.`, at most 64"
        ))
    }
}

/// A value the store takes: not empty, not over [`MAX_VALUE_BYTES`].
/// Said of by its name, never quoted.
fn valid_value(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("secret `{name}`: the value is empty"));
    }
    if value.len() > MAX_VALUE_BYTES {
        return Err(format!(
            "secret `{name}`: the value is {} bytes, over the store's {MAX_VALUE_BYTES}",
            value.len()
        ));
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Writes `bytes` to `path` with mode 0600 through a temporary file beside
/// it: written, synced, renamed over `path`, and the directory synced, so
/// that a crash leaves the old file or the new one.
fn write_private_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let temporary = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(FILE)
    ));
    let written = (|| {
        let _ = std::fs::remove_file(&temporary);
        let mut file = create_private(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        // The rename itself, made durable. Not every platform can open a
        // directory to sync it; where it cannot, the rename stands as is.
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cove-host-secrets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_secret_survives_reopening_and_a_delete_removes_it() {
        let dir = scratch("reopen");
        let store = SecretStore::open(Some(&dir)).unwrap();
        assert!(store.list().is_empty());
        store.set("gemini", "sk-one").unwrap();
        store.set("openai", "sk-two").unwrap();
        store.set("gemini", "sk-three").unwrap();
        drop(store);
        let again = SecretStore::open(Some(&dir)).unwrap();
        assert_eq!(again.get("gemini").as_deref(), Some("sk-three"));
        assert_eq!(again.get("openai").as_deref(), Some("sk-two"));
        let names: Vec<String> = again.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["gemini", "openai"]);
        assert!(again.list().iter().all(|s| s.updated_ms > 0));
        assert!(again.delete("openai").unwrap());
        assert!(!again.delete("openai").unwrap());
        drop(again);
        let third = SecretStore::open(Some(&dir)).unwrap();
        assert!(!third.contains("openai"));
        assert!(third.contains("gemini"));
        // Names only, wherever it is printed.
        let printed = format!("{third:?}");
        assert_eq!(printed, "{\"gemini\"}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_the_owners_alone_and_leaves_no_temporary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let store = SecretStore::open(Some(&dir)).unwrap();
        store.set("gemini", "sk-mode").unwrap();
        store.set("gemini", "sk-mode-again").unwrap();
        let mode = std::fs::metadata(dir.join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, [FILE]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_fails_keeps_the_old_file_and_the_old_value() {
        let dir = scratch("atomic");
        let store = SecretStore::open(Some(&dir)).unwrap();
        store.set("gemini", "sk-before").unwrap();
        let before = std::fs::read(dir.join(FILE)).unwrap();
        // A directory where the temporary file goes: the write fails before
        // the rename, so the file is never touched.
        std::fs::create_dir_all(dir.join(format!(".{FILE}.tmp")).join("x")).unwrap();
        let error = store.set("gemini", "sk-after-never-written").unwrap_err();
        assert!(!error.contains("sk-after"), "{error}");
        assert_eq!(std::fs::read(dir.join(FILE)).unwrap(), before);
        assert_eq!(store.get("gemini").as_deref(), Some("sk-before"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_and_values_are_held_to_their_rules_without_quoting_a_value() {
        let store = SecretStore::in_memory();
        for name in ["", "-x", "a b", "a/b", "ü", &"x".repeat(65)] {
            assert!(store.set(name, "v").is_err(), "{name}");
        }
        for name in ["a", "GEMINI_API_KEY", "open-ai.v2", "0x"] {
            store.set(name, "v").unwrap();
        }
        assert!(store.set("empty", "").is_err());
        let huge = "s".repeat(MAX_VALUE_BYTES + 1);
        let error = store.set("huge", &huge).unwrap_err();
        assert!(!error.contains("sss"), "{error}");
    }

    #[test]
    fn a_broken_file_is_refused_without_quoting_it() {
        let dir = scratch("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), "{ \"secrets\": sk-leaked-if-quoted").unwrap();
        let error = SecretStore::open(Some(&dir)).unwrap_err();
        assert!(error.contains("does not read"), "{error}");
        assert!(!error.contains("sk-leaked"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
