//! Persistent executed-command journal — the client-side half of the
//! "no duplicate command execution after reconnect" guarantee (§4).
//!
//! The Platform replays every `pending` command when the socket reconnects.
//! If the connector executed a command but died before reporting the result,
//! a RAM-only replay cache would execute it AGAIN on restart. This journal
//! appends every executed command id to a bounded JSONL file (FIFO
//! eviction), reloads the tail at boot, and answers `contains()` — so a
//! replayed command is answered with a recorded duplicate result instead of
//! a second execution.
//!
//! Ordering policy: the id is recorded BEFORE dispatch (at-most-once
//! execution). A crash mid-execution loses that one command rather than
//! duplicating it — the deliberate trade-off for a trading bridge; the
//! command expires server-side and the audit trail shows the record.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

const DEFAULT_CAPACITY: usize = 10_000;

/// Bounded, crash-tolerant record of executed command ids.
pub struct CommandJournal {
    file: Mutex<File>,
    inner: Mutex<JournalInner>,
    capacity: usize,
}

/// Id set + FIFO insertion order (eviction must be deterministic oldest-out).
#[derive(Default)]
struct JournalInner {
    seen: HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl CommandJournal {
    /// Opens (or creates) the journal at `path` and loads the most recent
    /// `capacity` records. A missing file is a fresh journal, not an error.
    ///
    /// # Errors
    /// Unreadable/unwritable journal file or malformed existing content.
    pub fn open(path: PathBuf) -> Result<Self, String> {
        Self::open_with_capacity(path, DEFAULT_CAPACITY)
    }

    /// # Errors
    /// Same as [`CommandJournal::open`].
    pub fn open_with_capacity(path: PathBuf, capacity: usize) -> Result<Self, String> {
        let capacity = capacity.max(1);
        let mut inner = JournalInner::default();
        if let Ok(file) = File::open(&path) {
            let reader = BufReader::new(file);
            // Keep only the tail — earlier records age out of the window.
            let mut lines: Vec<String> = Vec::new();
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        lines.push(l);
                        if lines.len() > capacity {
                            lines.remove(0);
                        }
                    }
                    Err(e) => return Err(format!("journal read failed: {e}")),
                }
            }
            for line in &lines {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(id) = v.get("command_id").and_then(|x| x.as_str())
                {
                    inner.seen.insert(id.to_string());
                    inner.order.push_back(id.to_string());
                }
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("journal open failed: {e}"))?;
        Ok(Self {
            file: Mutex::new(file),
            inner: Mutex::new(inner),
            capacity,
        })
    }

    /// True when this command id was already executed (per the loaded tail).
    pub fn contains(&self, command_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .contains(command_id)
    }

    /// Records an executed command id. The record hits the file (with
    /// flush) before this returns — a crash cannot silently drop it.
    ///
    /// # Errors
    /// File append/flush failure — callers must treat this as fatal for the
    /// command (refuse execution rather than run unrecorded).
    pub fn record(&self, command_id: &str, status: &str) -> Result<(), String> {
        let line = serde_json::json!({
            "command_id": command_id,
            "status": status,
            "recorded_at": crate::bridge::broker_time_now(),
        });
        {
            let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
            writeln!(file, "{line}")
                .and_then(|_| file.flush())
                .map_err(|e| format!("journal write failed: {e}"))?;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.seen.insert(command_id.to_string()) {
            inner.order.push_back(command_id.to_string());
        }
        // FIFO eviction mirrors the file tail.
        while inner.seen.len() > self.capacity {
            if let Some(oldest) = inner.order.pop_front() {
                inner.seen.remove(&oldest);
            } else {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("nse-journal-tests");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir.join(format!("{tag}-{}.jsonl", std::process::id()))
    }

    #[test]
    fn records_and_persists_across_reopen() {
        let path = temp_path("persist");
        let _ = std::fs::remove_file(&path);
        {
            let journal = CommandJournal::open(path.clone()).expect("open");
            assert!(!journal.contains("cmd-1"));
            journal.record("cmd-1", "executed").expect("record");
            assert!(journal.contains("cmd-1"));
        }
        // Reopen — the record survives (this is the restart case).
        let journal = CommandJournal::open(path.clone()).expect("reopen");
        assert!(journal.contains("cmd-1"));
        assert!(!journal.contains("cmd-2"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bounded_eviction_drops_oldest() {
        let path = temp_path("bounded");
        let _ = std::fs::remove_file(&path);
        let journal = CommandJournal::open_with_capacity(path.clone(), 3).expect("open");
        for i in 0..5 {
            journal
                .record(&format!("cmd-{i}"), "executed")
                .expect("record");
        }
        assert!(journal.contains("cmd-4"));
        assert!(journal.contains("cmd-2"));
        assert!(!journal.contains("cmd-0"), "oldest evicted from the window");
        assert!(!journal.contains("cmd-1"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_tail_lines_are_tolerated() {
        let path = temp_path("corrupt");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "not-json\n{\"command_id\":\"cmd-9\",\"status\":\"executed\"}\n\n",
        )
        .expect("seed");
        let journal = CommandJournal::open(path.clone()).expect("open tolerates junk");
        assert!(journal.contains("cmd-9"));
        assert!(!journal.contains("not-json"));
        let _ = std::fs::remove_file(&path);
    }
}
