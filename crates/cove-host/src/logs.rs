//! Each app's recent log lines, kept in memory.
//!
//! `log.*` lines and the host's own lines about the app (a request that
//! failed, a run that would not yield) go into a ring of the last
//! [`LOG_LINES`], which `GET /_host/apps/<app>/logs` answers. Standard output
//! still gets the app's `log.*` lines unless the host runs `--quiet`. Nothing
//! here is persisted: a durable, queryable log is an operations feature.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many lines each app's ring keeps.
pub const LOG_LINES: usize = 1000;

/// The longest line kept; longer lines are cut, saying so.
const MAX_LINE: usize = 4096;

/// One app's recent lines.
#[derive(Default)]
pub struct LogRing {
    lines: Mutex<VecDeque<String>>,
}

impl LogRing {
    /// Appends `line` at `level` (`info`, `warn`, `error`, or `host` for the
    /// host's own), stamped with the wall clock.
    pub fn push(&self, level: &str, line: &str) {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis());
        let mut text = if line.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}… ({} bytes)", &line[..end], line.len())
        } else {
            line.to_string()
        };
        text = text.replace('\n', "\\n");
        let entry = format!("{}.{:03} {level}: {text}", millis / 1000, millis % 1000);
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(entry);
    }

    /// The last `n` lines, oldest first, one per line.
    pub fn tail(&self, n: usize) -> String {
        let lines = self.lines.lock().unwrap();
        let skip = lines.len().saturating_sub(n);
        let mut out = String::new();
        for line in lines.iter().skip(skip) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_last_lines() {
        let ring = LogRing::default();
        for i in 0..LOG_LINES + 5 {
            ring.push("info", &format!("line {i}"));
        }
        let tail = ring.tail(2);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with(&format!("info: line {}", LOG_LINES + 4)));
        assert_eq!(ring.tail(usize::MAX).lines().count(), LOG_LINES);
    }
}
