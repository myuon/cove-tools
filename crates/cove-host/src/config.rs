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
//! ```
//!
//! Every key is optional and an unknown one is refused: a misspelt limit would
//! otherwise be a limit silently not applied. A refusal is the app's alone —
//! the host starts every other app.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use cove_runtime::Limits;
use serde::Deserialize;

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
}

/// `[limits]` as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsFile {
    pub fuel: Option<u64>,
    pub max_host_calls: Option<u64>,
    pub deadline: Option<String>,
    pub max_call_depth: Option<usize>,
    pub max_heap_words: Option<u64>,
    pub max_in_flight: Option<usize>,
    pub max_queued: Option<usize>,
    pub max_request_bytes: Option<usize>,
    pub max_response_bytes: Option<usize>,
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

/// An app's configuration, read and validated.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub entry: String,
    pub granted: BTreeSet<String>,
    pub limits: AppLimits,
}

/// Reads `dir/app.toml` for the app `name`.
pub fn read_app(dir: &Path, name: &str) -> Result<AppConfig, String> {
    let path = dir.join("app.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    parse_app(&text, name).map_err(|e| format!("`{}`: {e}", path.display()))
}

/// Parses an `app.toml` for the app `name`.
pub fn parse_app(text: &str, name: &str) -> Result<AppConfig, String> {
    let file: AppFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
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
    let entry = file.entry.unwrap_or_else(|| format!("{name}.handle"));
    if entry.split_once('.').is_none() {
        return Err(format!("`entry = \"{entry}\"` is not `module.function`"));
    }
    Ok(AppConfig {
        entry,
        granted: file.grant.into_iter().collect(),
        limits,
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
    fn durations() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("5m").is_err());
    }
}
