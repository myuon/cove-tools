//! `kv`: each app's persistent key-value store.
//!
//! | operation | answers |
//! | --- | --- |
//! | `kv.get(key)` | `Option<String>` |
//! | `kv.put(key, value)` | `Result<Unit, Error>`: an `Err` past a quota |
//! | `kv.delete(key)` | `Bool`, whether the key was there |
//! | `kv.increment(key, by)` | `Result<Int, Error>`: adds `by` to the decimal integer stored at `key` (0 when there is none) and answers the new value, **atomically** — two runs incrementing at once both count, which a `get` then a `put` would not promise. An `Err` when the stored value is not an integer, the sum overflows, or a quota is reached |
//! | `kv.list(prefix, after, limit)` | `Array<kv.Entry>`: keys starting with `prefix` and greater than `after` (`""` for the first page), ascending, at most `limit` (1–1000) |
//! | `kv.listDesc(prefix, before, limit)` | the same, descending, keys less than `before` (`""` for the last page) |
//!
//! `kv.Entry` is `{ key: String, value: String }`. Paging is by key: the last
//! key of one page is the `after` (or `before`) of the next, so an app that
//! writes `event:<zero-padded time>` lists its newest events with
//! `listDesc("event:", "", 50)`.
//!
//! # Storage
//!
//! SQLite (bundled, through `rusqlite`), one database file per app at
//! `<data>/<app>/kv.sqlite3`. One file per app is the namespace: an app's
//! module instance holds its own connection and nothing else, so no key it
//! can write names another app's data. SQLite is the boring choice — one
//! file, a format that outlives this program, crash-safe by design. The
//! database runs in WAL mode with `synchronous = NORMAL`: a committed write
//! survives the host process crashing; a power loss may lose the last
//! transactions but never corrupts the file.
//!
//! # Answered at once, on the worker
//!
//! Every call answers `Ready`, on the worker thread that made it — never on
//! the async runtime's threads, so HTTP is never blocked behind a write.
//! Parking would cost more than the call: a get or put on a local SQLite in
//! WAL mode is a few microseconds (`kv_call_cost`, an ignored test that
//! prints them), against a park, a hand-off to the I/O runtime, a resume job
//! and a resume. A store on the network would answer pending instead; the
//! scheduler supports both.
//!
//! # Quotas
//!
//! `[kv]` in `app.toml`: `max_key_bytes`, `max_value_bytes`, `max_keys` and
//! `max_bytes` (keys and values summed). A `put` past one is the app's `Err`
//! to handle, naming the quota; the store is unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cove_runtime::{
    Effect, FieldSchema, HostApi, HostType, ModuleSchema, OperationSchema, RuntimeError,
    TypeSchema, Value,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::config::KvLimits;
use crate::hosts::{AppContext, HostModule};

/// The most entries one `list` answers.
pub const MAX_LIST: i64 = 1000;

const fn op(
    name: &'static str,
    params: &'static [HostType],
    result: HostType,
    effect: Effect,
) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability: "kv",
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const STR: HostType = HostType::String;
const ENTRIES: HostType = HostType::Array(&HostType::Named("kv.Entry"));

/// The `kv` module.
pub const KV: ModuleSchema = ModuleSchema {
    name: "kv",
    capability: "kv",
    operations: &[
        op("get", &[STR], HostType::Option(&STR), Effect::Read),
        op(
            "put",
            &[STR, STR],
            HostType::Result(&HostType::Unit, &HostType::Error),
            Effect::ReversibleWrite,
        ),
        op("delete", &[STR], HostType::Bool, Effect::ReversibleWrite),
        op(
            "increment",
            &[STR, HostType::Int],
            HostType::Result(&HostType::Int, &HostType::Error),
            Effect::ReversibleWrite,
        ),
        op("list", &[STR, STR, HostType::Int], ENTRIES, Effect::Read),
        op(
            "listDesc",
            &[STR, STR, HostType::Int],
            ENTRIES,
            Effect::Read,
        ),
    ],
    types: &[TypeSchema {
        name: "Entry",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "key",
                ty: HostType::String,
            },
            FieldSchema {
                name: "value",
                ty: HostType::String,
            },
        ],
    }],
    resources: &[],
};

pub(crate) struct KvModule;

impl HostModule for KvModule {
    fn schema(&self) -> ModuleSchema {
        KV
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        // An app not granted `kv` can never reach the store, so it gets
        // none: no file is created for it.
        let store = if !app.granted.contains("kv") {
            None
        } else {
            Some(match &app.data {
                Some(dir) => shared(&dir.join("kv.sqlite3"), &app.kv)?,
                None => Arc::new(Mutex::new(
                    Store::in_memory(app.kv.clone())
                        .map_err(|why| format!("cannot open its kv store: {why}"))?,
                )),
            })
        };
        Ok(Box::new(KvHost { store }))
    }
}

/// One app's store and what it holds now.
pub struct Store {
    conn: Connection,
    limits: KvLimits,
    keys: u64,
    bytes: u64,
}

/// Why a `put` was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum PutError {
    /// Past a quota: the app's `Err`.
    Quota(String),
    /// The database failed: the host's error.
    Storage(String),
}

impl Store {
    /// The store at `path`, created if it is not there.
    pub fn open(path: &Path, limits: KvLimits) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create `{}`: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("`{}`: {e}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        Store::with(conn, limits)
    }

    /// A store that lives as long as the process.
    pub fn in_memory(limits: KvLimits) -> Result<Store, String> {
        Store::with(
            Connection::open_in_memory().map_err(|e| e.to_string())?,
            limits,
        )
    }

    fn with(conn: Connection, limits: KvLimits) -> Result<Store, String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;",
        )
        .map_err(|e| e.to_string())?;
        let (keys, bytes): (i64, i64) = conn
            .query_row(
                "SELECT count(*), coalesce(sum(length(CAST(key AS BLOB)) + length(CAST(value AS BLOB))), 0) FROM kv",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        Ok(Store {
            conn,
            limits,
            keys: keys as u64,
            bytes: bytes as u64,
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn put(&mut self, key: &str, value: &str) -> Result<(), PutError> {
        let limits = &self.limits;
        if key.len() > limits.max_key_bytes {
            return Err(PutError::Quota(format!(
                "kv.put: a key of {} bytes is above this app's max_key_bytes of {}",
                key.len(),
                limits.max_key_bytes
            )));
        }
        if value.len() > limits.max_value_bytes {
            return Err(PutError::Quota(format!(
                "kv.put: a value of {} bytes is above this app's max_value_bytes of {}",
                value.len(),
                limits.max_value_bytes
            )));
        }
        let old: Option<i64> = self
            .conn
            .query_row(
                "SELECT length(CAST(value AS BLOB)) FROM kv WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| PutError::Storage(e.to_string()))?;
        let entry = (key.len() + value.len()) as u64;
        let (keys, bytes) = match old {
            Some(old) => (
                self.keys,
                self.bytes - (key.len() as u64 + old as u64) + entry,
            ),
            None => (self.keys + 1, self.bytes + entry),
        };
        if keys > limits.max_keys {
            return Err(PutError::Quota(format!(
                "kv.put: this app holds {} keys already, its max_keys",
                self.keys
            )));
        }
        if bytes > limits.max_bytes {
            return Err(PutError::Quota(format!(
                "kv.put: this write would hold {bytes} bytes, above this app's max_bytes of {}",
                limits.max_bytes
            )));
        }
        self.conn
            .execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|e| PutError::Storage(e.to_string()))?;
        self.keys = keys;
        self.bytes = bytes;
        Ok(())
    }

    /// Adds `by` to the integer at `key`; the new value.
    pub fn increment(&mut self, key: &str, by: i64) -> Result<i64, PutError> {
        let current = match self.get(key).map_err(PutError::Storage)? {
            None => 0,
            Some(text) => text.trim().parse::<i64>().map_err(|_| {
                PutError::Quota(format!(
                    "kv.increment: the value at `{key}` is not an integer"
                ))
            })?,
        };
        let next = current
            .checked_add(by)
            .ok_or_else(|| PutError::Quota(format!("kv.increment: `{key}` would overflow")))?;
        self.put(key, &next.to_string())?;
        Ok(next)
    }

    pub fn delete(&mut self, key: &str) -> Result<bool, String> {
        let old: Option<i64> = self
            .conn
            .query_row(
                "DELETE FROM kv WHERE key = ?1 RETURNING length(CAST(value AS BLOB))",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(old) = old {
            self.keys -= 1;
            self.bytes -= key.len() as u64 + old as u64;
        }
        Ok(old.is_some())
    }

    /// Keys starting with `prefix`, beyond `from` in the direction asked
    /// (`""` from the start), at most `limit`.
    pub fn list(
        &self,
        prefix: &str,
        from: &str,
        limit: i64,
        descending: bool,
    ) -> Result<Vec<(String, String)>, String> {
        // Every key starting with `prefix` is at least `prefix` and below
        // `upper`: UTF-8 orders bytewise as code points do, and SQLite's
        // BINARY collation compares bytes.
        let upper = successor(prefix);
        let mut sql = String::from("SELECT key, value FROM kv WHERE key >= ?1");
        if upper.is_some() {
            sql.push_str(" AND key < ?2");
        }
        if !from.is_empty() {
            sql.push_str(if descending {
                " AND key < ?3"
            } else {
                " AND key > ?3"
            });
        }
        sql.push_str(if descending {
            " ORDER BY key DESC LIMIT ?4"
        } else {
            " ORDER BY key LIMIT ?4"
        });
        let mut statement = self.conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
        // Unused numbered parameters are fine to bind; SQLite needs every
        // number up to the highest named.
        let rows = statement
            .query_map(
                params![prefix, upper.unwrap_or_default(), from, limit],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// `(keys, bytes)` held now.
    pub fn usage(&self) -> (u64, u64) {
        (self.keys, self.bytes)
    }
}

/// The least string greater than every string starting with `prefix`, or
/// `None` when there is none (an empty prefix, or one of only U+10FFFF).
fn successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let mut next = last as u32 + 1;
        if (0xD800..0xE000).contains(&next) {
            next = 0xE000;
        }
        if let Some(next) = char::from_u32(next) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

struct KvHost {
    /// `None` for an app not granted `kv`, whose calls the boundary refuses.
    store: Option<Arc<Mutex<Store>>>,
}

/// The stores open in this process, by file.
fn open_stores() -> &'static Mutex<HashMap<PathBuf, Weak<Mutex<Store>>>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<Store>>>>> = OnceLock::new();
    OPEN.get_or_init(Mutex::default)
}

/// The store at `path`: the one already open, if a version of the app has
/// it open, with the new version's quotas; or newly opened.
///
/// One `Store` per file however many versions of the app are alive during
/// an update, so that its quota accounting is one account.
fn shared(path: &Path, limits: &KvLimits) -> Result<Arc<Mutex<Store>>, String> {
    let mut open = open_stores().lock().unwrap();
    if let Some(store) = open.get(path).and_then(Weak::upgrade) {
        store.lock().unwrap().limits = limits.clone();
        return Ok(store);
    }
    let store = Arc::new(Mutex::new(
        Store::open(path, limits.clone())
            .map_err(|why| format!("cannot open its kv store: {why}"))?,
    ));
    open.insert(path.to_path_buf(), Arc::downgrade(&store));
    Ok(store)
}

/// `(keys, bytes)` the store at `path` holds, if it is open.
pub fn usage(path: &Path) -> Option<(u64, u64)> {
    let store = open_stores().lock().unwrap().get(path)?.upgrade()?;
    let usage = store.lock().unwrap().usage();
    Some(usage)
}

/// The longest value a listing carries; a longer one is cut and `bytes`
/// says how long the whole of it is.
pub const PEEK_VALUE_CHARS: usize = 512;

/// The longest value one key's read carries.
pub const READ_VALUE_CHARS: usize = 64 * 1024;

/// One key as the admin sees it: its value cut to what was asked for, and
/// how long the whole value is.
pub struct Peek {
    pub key: String,
    pub value: String,
    /// The whole value's length in bytes, as the quotas count it.
    pub bytes: u64,
    /// Whether `value` is shorter than the whole.
    pub cut: bool,
}

/// Keys of the store at `path` starting with `prefix`, beyond `from` (`""`
/// from the start), at most `limit`, oldest key first.
///
/// Reads only: the store the running app holds if one is open, else the file
/// opened read-only — never created, so an app with no store gets none from
/// being looked at. A store that is not there answers no keys.
pub fn peek(path: &Path, prefix: &str, from: &str, limit: i64) -> Result<Vec<Peek>, String> {
    let limit = limit.clamp(0, MAX_LIST);
    with_reader(path, Vec::new(), |conn| {
        // Every key starting with `prefix` is at least `prefix` and below
        // `upper`, as `Store::list` has it.
        let upper = successor(prefix);
        let mut sql = String::from(
            "SELECT key, substr(value, 1, ?5), length(value), length(CAST(value AS BLOB)) \
             FROM kv WHERE key >= ?1",
        );
        if upper.is_some() {
            sql.push_str(" AND key < ?2");
        }
        if !from.is_empty() {
            sql.push_str(" AND key > ?3");
        }
        sql.push_str(" ORDER BY key LIMIT ?4");
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                prefix,
                upper.unwrap_or_default(),
                from,
                limit,
                PEEK_VALUE_CHARS as i64
            ],
            |row| peek_row(row, PEEK_VALUE_CHARS),
        )?;
        rows.collect()
    })
}

/// One key of the store at `path`, or `None` when it has none.
///
/// The value is cut past [`READ_VALUE_CHARS`]; otherwise as [`peek`].
pub fn read(path: &Path, key: &str) -> Result<Option<Peek>, String> {
    with_reader(path, None, |conn| {
        conn.query_row(
            "SELECT key, substr(value, 1, ?2), length(value), length(CAST(value AS BLOB)) \
             FROM kv WHERE key = ?1",
            params![key, READ_VALUE_CHARS as i64],
            |row| peek_row(row, READ_VALUE_CHARS),
        )
        .optional()
    })
}

/// A row of `SELECT key, substr(..), length(value), length(CAST(..))`.
fn peek_row(row: &rusqlite::Row<'_>, chars: usize) -> rusqlite::Result<Peek> {
    let whole: i64 = row.get(2)?;
    let bytes: i64 = row.get(3)?;
    Ok(Peek {
        key: row.get(0)?,
        value: row.get(1)?,
        bytes: bytes.max(0) as u64,
        cut: whole > chars as i64,
    })
}

/// Runs `read` on the open store at `path`, or on the file opened
/// read-only; `empty` when there is no file, or no table in it.
fn with_reader<T>(
    path: &Path,
    empty: T,
    read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> Result<T, String> {
    let open = open_stores()
        .lock()
        .unwrap()
        .get(path)
        .and_then(Weak::upgrade);
    let outcome = if let Some(store) = open {
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        read(&store.conn)
    } else {
        if !path.is_file() {
            return Ok(empty);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("`{}`: {e}", path.display()))?;
        read(&conn)
    };
    match outcome {
        Ok(found) => Ok(found),
        Err(e) if e.to_string().contains("no such table") => Ok(empty),
        Err(e) => Err(e.to_string()),
    }
}

fn storage(why: String) -> RuntimeError {
    RuntimeError::new(format!("kv: the store failed: {why}"))
}

impl HostApi for KvHost {
    fn module_schema(&self) -> ModuleSchema {
        KV
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // The boundary held the arity and each argument's type to `KV`.
        let text = |at: usize| args[at].as_str().unwrap_or_default();
        let Some(store) = &self.store else {
            return Err(RuntimeError::new("kv: this app has no store"));
        };
        let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
        match op {
            "get" => Ok(match store.get(text(0)).map_err(storage)? {
                Some(value) => Value::some(Value::string(value)),
                None => Value::none(),
            }),
            "put" => match store.put(text(0), text(1)) {
                Ok(()) => Ok(Value::ok(Value::unit())),
                Err(PutError::Quota(why)) => Ok(Value::err(Value::error(why))),
                Err(PutError::Storage(why)) => Err(storage(why)),
            },
            "delete" => Ok(Value::bool(store.delete(text(0)).map_err(storage)?)),
            "increment" => {
                let by = args[1].as_int().unwrap_or_default();
                match store.increment(text(0), by) {
                    Ok(value) => Ok(Value::ok(Value::int(value))),
                    Err(PutError::Quota(why)) => Ok(Value::err(Value::error(why))),
                    Err(PutError::Storage(why)) => Err(storage(why)),
                }
            }
            "list" | "listDesc" => {
                let limit = args[2].as_int().unwrap_or_default();
                if !(1..=MAX_LIST).contains(&limit) {
                    return Err(RuntimeError::new(format!(
                        "kv.{op}: a limit of {limit} is not 1 to {MAX_LIST}"
                    )));
                }
                let entries = store
                    .list(text(0), text(1), limit, op == "listDesc")
                    .map_err(storage)?;
                Ok(Value::array(entries.into_iter().map(|(key, value)| {
                    Value::structure(
                        "kv.Entry",
                        vec![("key", Value::string(key)), ("value", Value::string(value))],
                    )
                })))
            }
            other => Err(RuntimeError::new(format!(
                "`kv` declares no operation `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(limits: KvLimits) -> Store {
        Store::in_memory(limits).unwrap()
    }

    #[test]
    fn put_get_delete() {
        let mut kv = store(KvLimits::default());
        assert_eq!(kv.get("a").unwrap(), None);
        kv.put("a", "1").unwrap();
        kv.put("a", "22").unwrap();
        assert_eq!(kv.get("a").unwrap().as_deref(), Some("22"));
        assert_eq!(kv.usage(), (1, 3));
        assert!(kv.delete("a").unwrap());
        assert!(!kv.delete("a").unwrap());
        assert_eq!(kv.usage(), (0, 0));
    }

    #[test]
    fn increments_count_from_nothing_and_refuse_text() {
        let mut kv = store(KvLimits::default());
        assert_eq!(kv.increment("n", 1).unwrap(), 1);
        assert_eq!(kv.increment("n", 2).unwrap(), 3);
        assert_eq!(kv.get("n").unwrap().as_deref(), Some("3"));
        kv.put("t", "text").unwrap();
        assert!(matches!(kv.increment("t", 1), Err(PutError::Quota(_))));
        kv.put("big", &i64::MAX.to_string()).unwrap();
        assert!(matches!(kv.increment("big", 1), Err(PutError::Quota(_))));
    }

    #[test]
    fn lists_by_prefix_in_pages_both_ways() {
        let mut kv = store(KvLimits::default());
        for key in ["e:1", "e:2", "e:3", "e;", "d:9", "e:"] {
            kv.put(key, key).unwrap();
        }
        let keys = |entries: Vec<(String, String)>| -> Vec<String> {
            entries.into_iter().map(|(k, _)| k).collect()
        };
        assert_eq!(
            keys(kv.list("e:", "", 10, false).unwrap()),
            ["e:", "e:1", "e:2", "e:3"]
        );
        assert_eq!(
            keys(kv.list("e:", "e:1", 2, false).unwrap()),
            ["e:2", "e:3"]
        );
        assert_eq!(keys(kv.list("e:", "", 2, true).unwrap()), ["e:3", "e:2"]);
        assert_eq!(keys(kv.list("e:", "e:2", 10, true).unwrap()), ["e:1", "e:"]);
        assert_eq!(kv.list("", "", 100, false).unwrap().len(), 6);
    }

    #[test]
    fn quotas_refuse_and_change_nothing() {
        let mut kv = store(KvLimits {
            max_key_bytes: 4,
            max_value_bytes: 4,
            max_keys: 2,
            max_bytes: 12,
        });
        let quota = |result| matches!(result, Err(PutError::Quota(_)));
        assert!(quota(kv.put("toolong", "v")));
        assert!(quota(kv.put("k", "toolong")));
        kv.put("a", "1234").unwrap();
        kv.put("b", "1234").unwrap();
        assert!(quota(kv.put("c", "1")));
        // Replacing a key is not a new key, but its bytes still count.
        assert!(quota(kv.put("a", "12345")));
        kv.put("a", "1").unwrap();
        assert_eq!(kv.usage(), (2, 7));
    }

    #[test]
    fn a_store_survives_reopening() {
        let dir = std::env::temp_dir().join(format!("minicloud-kv-{}", std::process::id()));
        let path = dir.join("kv.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut kv = Store::open(&path, KvLimits::default()).unwrap();
            kv.put("kept", "yes").unwrap();
        }
        let kv = Store::open(&path, KvLimits::default()).unwrap();
        assert_eq!(kv.get("kept").unwrap().as_deref(), Some("yes"));
        assert_eq!(kv.usage(), (1, 7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successor_bounds_a_prefix() {
        assert_eq!(successor("e:").as_deref(), Some("e;"));
        assert_eq!(successor(""), None);
        assert_eq!(successor("a\u{10FFFF}").as_deref(), Some("b"));
    }

    /// What a call costs, printed: `cargo t -- --ignored kv_call_cost --nocapture`.
    #[test]
    #[ignore]
    fn kv_call_cost() {
        let dir = std::env::temp_dir().join(format!("minicloud-kv-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut kv = Store::open(&dir.join("kv.sqlite3"), KvLimits::default()).unwrap();
        let n = 20_000;
        let started = std::time::Instant::now();
        for i in 0..n {
            kv.put(&format!("key:{i:08}"), "a value of some thirty bytes..")
                .unwrap();
        }
        let put = started.elapsed() / n;
        let started = std::time::Instant::now();
        for i in 0..n {
            kv.get(&format!("key:{i:08}")).unwrap().unwrap();
        }
        let get = started.elapsed() / n;
        let started = std::time::Instant::now();
        for i in 0..1000 {
            kv.list("key:", &format!("key:{:08}", i * 10), 50, false)
                .unwrap();
        }
        let list = started.elapsed() / 1000;
        println!("kv on disk: put {put:?}, get {get:?}, list of 50 {list:?} per call");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("minicloud-kv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn peek_lists_by_prefix_and_after() {
        let dir = scratch("peek");
        let path = dir.join("kv.sqlite3");
        {
            let mut kv = Store::open(&path, KvLimits::default()).unwrap();
            for key in ["e:1", "e:2", "e:3", "d:9"] {
                kv.put(key, &format!("v{key}")).unwrap();
            }
        }
        let keys = |found: Vec<Peek>| -> Vec<String> { found.into_iter().map(|p| p.key).collect() };
        assert_eq!(
            keys(peek(&path, "", "", 10).unwrap()),
            ["d:9", "e:1", "e:2", "e:3"]
        );
        assert_eq!(
            keys(peek(&path, "e:", "", 10).unwrap()),
            ["e:1", "e:2", "e:3"]
        );
        assert_eq!(keys(peek(&path, "e:", "e:1", 1).unwrap()), ["e:2"]);
        let one = read(&path, "e:2").unwrap().unwrap();
        assert_eq!((one.value.as_str(), one.bytes, one.cut), ("ve:2", 4, false));
        assert!(read(&path, "nope").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_cuts_long_values_and_says_how_long() {
        let dir = scratch("cut");
        let path = dir.join("kv.sqlite3");
        {
            let mut kv = Store::open(&path, KvLimits::default()).unwrap();
            kv.put("long", &"é".repeat(PEEK_VALUE_CHARS + 10)).unwrap();
            kv.put("short", "ok").unwrap();
        }
        let found = peek(&path, "", "", 10).unwrap();
        assert_eq!(found[0].key, "long");
        assert!(found[0].cut);
        assert_eq!(found[0].value.chars().count(), PEEK_VALUE_CHARS);
        assert_eq!(found[0].bytes, 2 * (PEEK_VALUE_CHARS as u64 + 10));
        assert!(!found[1].cut);
        let whole = read(&path, "long").unwrap().unwrap();
        assert!(!whole.cut);
        assert_eq!(whole.value.chars().count(), PEEK_VALUE_CHARS + 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_of_a_store_that_is_not_there_is_empty_and_creates_nothing() {
        let dir = scratch("absent");
        let path = dir.join("app").join("kv.sqlite3");
        assert!(peek(&path, "", "", 10).unwrap().is_empty());
        assert!(read(&path, "k").unwrap().is_none());
        assert!(!path.exists());
        assert!(!dir.exists());
        // A database with no `kv` table is empty too.
        std::fs::create_dir_all(&dir).unwrap();
        let bare = dir.join("bare.sqlite3");
        Connection::open(&bare)
            .unwrap()
            .execute_batch("CREATE TABLE other (x);")
            .unwrap();
        assert!(peek(&bare, "", "", 10).unwrap().is_empty());
        assert!(read(&bare, "k").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
