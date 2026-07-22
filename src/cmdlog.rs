//! The **command log**: structured command boundaries for shell panes.
//!
//! Seance shell panes source an rc file (`assets/seance.bash`) whose hooks
//! report every command's *start* and *end* over the control plane. This module
//! is the app-side store those reports land in — a per-pane ring buffer of
//! [`CommandRecord`]s so agents can read structured facts ("what ran, where,
//! exit code, how long") instead of screen-scraping, and the human gets
//! "jump to the last failed command" affordances.
//!
//! # Design
//!
//! - **Pure rust, gpui-free.** Like [`crate::control`], this compiles on its own
//!   and is trivially unit-testable. The app owns one `CommandLog`, mutates it
//!   on the gpui loop when `CmdBegin`/`CmdEnd` ops arrive, and reads it for the
//!   `Commands`/`LastCommand` query ops. Nothing here touches a terminal.
//! - **Per-pane ring buffer, cap [`CAP_PER_PANE`].** Long-lived shells run
//!   thousands of commands; we keep only the most recent 500 per pane so memory
//!   stays bounded. Oldest records drop off the front as new ones arrive.
//! - **Monotonic `seq` per pane.** Each pane numbers its records 1, 2, 3, … so a
//!   record has a stable identity even after older records age out of the ring.
//!   `seq` also lets `end` unambiguously address "the record `begin` just made"
//!   without the shell round-tripping an id back to us.
//!
//! # The begin/end contract
//!
//! The shell fires them in order per pane: `begin(cmd)` before a command runs,
//! `end(exit)` from the prompt hook after it finishes. [`end`] closes the
//! **most recent still-open** record for the pane (the one with no `ended_ms`).
//! This tolerates the messy realities of shell hooks — see the module's caveats
//! and `docs/SHELL-INTEGRATION.md`:
//!
//! - **A stray `end` with no open record is a no-op** (e.g. the prompt fires at
//!   an interactive shell's first prompt before any command ran, or after we
//!   already closed the record).
//! - **A second `begin` before an `end`** (a hook missed the end, or a pipeline
//!   quirk) just opens a new record; the previous one stays open and simply
//!   never gets an exit — honest "unknown", never a fabricated code.

use std::collections::{HashMap, VecDeque};

/// Maximum records retained per pane. Oldest drop off the front past this.
pub const CAP_PER_PANE: usize = 500;

/// One command's lifecycle in a shell pane: what ran, where, and how it ended.
///
/// Times are unix epoch millis. `ended_ms`/`exit` are `None` while the command
/// is still running (or if its end report was lost) — always render an open
/// record as running/unknown, never invent a duration or an exit code.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommandRecord {
    /// Monotonic per-pane sequence number, starting at 1. Stable across ring
    /// eviction — identifies a record even after it (or older ones) age out.
    pub seq: u64,
    /// The command line as the shell captured it (the `DEBUG` trap's
    /// `BASH_COMMAND`). May be multi-line for a multi-line command; for a
    /// pipeline it's the whole pipeline text. See the doc's caveats.
    pub command: String,
    /// Working directory the command started in (the shell's `$PWD` at begin).
    pub cwd: String,
    /// When the command started (epoch millis), stamped app-side on `begin`.
    pub started_ms: u64,
    /// When the command finished (epoch millis), stamped app-side on `end`.
    /// `None` while running or if the end report never arrived.
    pub ended_ms: Option<u64>,
    /// The command's exit status (`$?`). `None` while running / on a lost end.
    pub exit: Option<i32>,
}

impl CommandRecord {
    /// Whether the command has finished (received an `end`).
    #[allow(dead_code)] // exercised by cmdlog tests; live done-checks read `ended_ms` directly
    pub fn is_done(&self) -> bool {
        self.ended_ms.is_some()
    }

    /// Whether the command finished with a non-zero exit status. An open record
    /// (still running / lost end) is **not** failed — `false`.
    pub fn is_failed(&self) -> bool {
        matches!(self.exit, Some(code) if code != 0)
    }

    /// Wall-clock duration in millis, if the record is closed.
    pub fn duration_ms(&self) -> Option<u64> {
        self.ended_ms.map(|end| end.saturating_sub(self.started_ms))
    }
}

/// Per-pane command ring buffers.
///
/// Keyed by pane slug (the same id used throughout the control plane and event
/// log). Each pane gets an independent `VecDeque` capped at [`CAP_PER_PANE`] and
/// its own monotonic `seq` counter (kept implicitly via the largest `seq` seen,
/// so it never rewinds even after eviction empties the ring).
///
/// Serde shape is used for daemon handoff + cold `state.json` so shell
/// command history survives `seance upgrade` (0.9.11+).
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CommandLog {
    #[serde(default)]
    panes: HashMap<String, PaneLog>,
}

/// One pane's ring plus its next-seq counter.
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PaneLog {
    #[serde(default)]
    records: VecDeque<CommandRecord>,
    /// The last `seq` we handed out. Next `begin` uses `next_seq + 1`. Kept
    /// separate from the ring so eviction can't rewind the counter.
    #[serde(default)]
    next_seq: u64,
}

impl CommandLog {
    /// A fresh, empty log.
    #[allow(dead_code)] // used by cmdlog + state tests; live construction uses Default
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new command record for `pane` and return its `seq`.
    ///
    /// `started_ms` is stamped by the caller-facing wrapper; here we take the
    /// caller's clock so tests are deterministic. The public app path uses
    /// [`CommandLog::begin`] which stamps `now`.
    pub fn begin_at(&mut self, pane: &str, command: String, cwd: String, started_ms: u64) -> u64 {
        let log = self.panes.entry(pane.to_string()).or_default();
        log.next_seq += 1;
        let seq = log.next_seq;
        log.records.push_back(CommandRecord {
            seq,
            command,
            cwd,
            started_ms,
            ended_ms: None,
            exit: None,
        });
        // Evict from the front to hold the cap.
        while log.records.len() > CAP_PER_PANE {
            log.records.pop_front();
        }
        seq
    }

    /// Open a new command record stamped with the current wall clock.
    /// Returns the record's `seq`.
    pub fn begin(&mut self, pane: &str, command: String, cwd: String) -> u64 {
        self.begin_at(pane, command, cwd, now_ms())
    }

    /// Close the most recent still-open record for `pane` at `ended_ms`,
    /// recording `exit`. Returns `true` if a record was closed; `false` if the
    /// pane is unknown or had no open record (stray end).
    pub fn end_at(&mut self, pane: &str, exit: i32, ended_ms: u64) -> bool {
        let Some(log) = self.panes.get_mut(pane) else {
            return false;
        };
        // Most recent open record = last record whose ended_ms is None. Scan
        // from the back; the common case hits on the first element.
        if let Some(rec) = log.records.iter_mut().rev().find(|r| r.ended_ms.is_none()) {
            rec.ended_ms = Some(ended_ms);
            rec.exit = Some(exit);
            true
        } else {
            // A stray end (no open record) is deliberately ignored — see module docs.
            false
        }
    }

    /// Close the most recent open record for `pane`, stamping the current clock.
    /// Returns whether a record was closed.
    pub fn end(&mut self, pane: &str, exit: i32) -> bool {
        self.end_at(pane, exit, now_ms())
    }

    /// The most recent `limit` records for `pane`, oldest-first (the natural
    /// reading order for a session). Empty vec for an unknown pane. `limit` of 0
    /// returns empty.
    pub fn list(&self, pane: &str, limit: usize) -> Vec<CommandRecord> {
        let Some(log) = self.panes.get(pane) else {
            return Vec::new();
        };
        let skip = log.records.len().saturating_sub(limit);
        log.records.iter().skip(skip).cloned().collect()
    }

    /// The most recent record for `pane`. With `failed_only`, the most recent
    /// record that finished non-zero (skips still-running and successful ones).
    /// `None` if nothing matches.
    pub fn last(&self, pane: &str, failed_only: bool) -> Option<CommandRecord> {
        let log = self.panes.get(pane)?;
        log.records
            .iter()
            .rev()
            .find(|r| !failed_only || r.is_failed())
            .cloned()
    }

    /// Forget a pane's entire log. Call when a pane is killed/closed so its
    /// records don't leak (the app already tears down other per-pane state on
    /// `pane_killed`). Idempotent.
    pub fn remove_pane(&mut self, pane: &str) {
        self.panes.remove(pane);
    }
}

/// Current wall clock in unix epoch millis (0 on the impossible clock error).
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_returns_monotonic_seq_per_pane() {
        let mut log = CommandLog::new();
        assert_eq!(log.begin("p", "ls".into(), "/tmp".into()), 1);
        assert_eq!(log.begin("p", "pwd".into(), "/tmp".into()), 2);
        // Independent counter per pane.
        assert_eq!(log.begin("q", "whoami".into(), "/".into()), 1);
        assert_eq!(log.begin("p", "date".into(), "/tmp".into()), 3);
    }

    #[test]
    fn end_closes_most_recent_open_record() {
        let mut log = CommandLog::new();
        log.begin_at("p", "make".into(), "/w".into(), 1_000);
        assert!(log.end_at("p", 0, 1_500));

        let rec = log.last("p", false).unwrap();
        assert_eq!(rec.exit, Some(0));
        assert_eq!(rec.ended_ms, Some(1_500));
        assert_eq!(rec.duration_ms(), Some(500));
        assert!(rec.is_done());
        assert!(!rec.is_failed());
    }

    #[test]
    fn end_records_nonzero_exit_as_failed() {
        let mut log = CommandLog::new();
        log.begin("p", "false".into(), "/w".into());
        assert!(log.end("p", 1));
        let rec = log.last("p", false).unwrap();
        assert_eq!(rec.exit, Some(1));
        assert!(rec.is_failed());
    }

    #[test]
    fn end_with_no_open_record_is_noop() {
        let mut log = CommandLog::new();
        // Prompt hook fired before any command ran: no panic, no phantom record.
        assert!(!log.end("p", 0));
        assert!(log.last("p", false).is_none());
        assert!(log.list("p", 10).is_empty());
    }

    #[test]
    fn second_begin_before_end_leaves_first_open() {
        let mut log = CommandLog::new();
        log.begin_at("p", "first".into(), "/w".into(), 100);
        log.begin_at("p", "second".into(), "/w".into(), 200);
        // end closes the most-recent open one = "second".
        log.end_at("p", 0, 300);

        let all = log.list("p", 10);
        assert_eq!(all.len(), 2);
        // "first" stayed open — honest unknown, no fabricated exit.
        assert_eq!(all[0].command, "first");
        assert_eq!(all[0].exit, None);
        assert_eq!(all[0].ended_ms, None);
        assert!(!all[0].is_done());
        // "second" got closed.
        assert_eq!(all[1].command, "second");
        assert_eq!(all[1].exit, Some(0));
    }

    #[test]
    fn end_targets_most_recent_open_skipping_closed() {
        let mut log = CommandLog::new();
        log.begin_at("p", "a".into(), "/w".into(), 100);
        log.end_at("p", 0, 150); // a closed
        log.begin_at("p", "b".into(), "/w".into(), 200);
        log.end_at("p", 2, 250); // should close b, not re-close a

        let all = log.list("p", 10);
        assert_eq!(all[0].exit, Some(0)); // a untouched
        assert_eq!(all[1].exit, Some(2)); // b closed with its own code
    }

    #[test]
    fn list_returns_oldest_first_tail() {
        let mut log = CommandLog::new();
        for i in 0..5 {
            log.begin("p", format!("cmd{i}"), "/w".into());
        }
        let tail = log.list("p", 3);
        let names: Vec<_> = tail.iter().map(|r| r.command.clone()).collect();
        assert_eq!(names, vec!["cmd2", "cmd3", "cmd4"]);
    }

    #[test]
    fn list_limit_zero_is_empty() {
        let mut log = CommandLog::new();
        log.begin("p", "x".into(), "/w".into());
        assert!(log.list("p", 0).is_empty());
    }

    #[test]
    fn list_limit_larger_than_len_returns_all() {
        let mut log = CommandLog::new();
        log.begin("p", "x".into(), "/w".into());
        log.begin("p", "y".into(), "/w".into());
        assert_eq!(log.list("p", 100).len(), 2);
    }

    #[test]
    fn ring_buffer_caps_and_seq_keeps_climbing() {
        let mut log = CommandLog::new();
        for _ in 0..(CAP_PER_PANE + 50) {
            log.begin("p", "c".into(), "/w".into());
        }
        // Ring is capped.
        assert_eq!(log.list("p", usize::MAX).len(), CAP_PER_PANE);
        // But seq never rewound: the newest record's seq reflects total begins.
        let last = log.last("p", false).unwrap();
        assert_eq!(last.seq, (CAP_PER_PANE + 50) as u64);
        // ...and the oldest surviving record's seq is past 1 (front evicted).
        let oldest = log.list("p", usize::MAX).into_iter().next().unwrap();
        assert_eq!(oldest.seq, 51);
    }

    #[test]
    fn last_failed_only_skips_success_and_running() {
        let mut log = CommandLog::new();
        log.begin("p", "ok".into(), "/w".into());
        log.end("p", 0);
        log.begin("p", "boom".into(), "/w".into());
        log.end("p", 127);
        log.begin("p", "still-running".into(), "/w".into()); // open, not counted

        // Plain last = the most recent record (the open one).
        assert_eq!(log.last("p", false).unwrap().command, "still-running");
        // failed_only = most recent NON-ZERO closed record.
        let failed = log.last("p", true).unwrap();
        assert_eq!(failed.command, "boom");
        assert_eq!(failed.exit, Some(127));
    }

    #[test]
    fn last_failed_only_none_when_no_failures() {
        let mut log = CommandLog::new();
        log.begin("p", "ok".into(), "/w".into());
        log.end("p", 0);
        assert!(log.last("p", true).is_none());
    }

    #[test]
    fn unknown_pane_reads_empty() {
        let log = CommandLog::new();
        assert!(log.list("nope", 10).is_empty());
        assert!(log.last("nope", false).is_none());
        assert!(log.last("nope", true).is_none());
    }

    #[test]
    fn remove_pane_forgets_records_and_is_idempotent() {
        let mut log = CommandLog::new();
        log.begin("p", "x".into(), "/w".into());
        log.remove_pane("p");
        assert!(log.last("p", false).is_none());
        // Idempotent: removing again doesn't panic.
        log.remove_pane("p");
        // A fresh begin after removal restarts seq at 1 (pane state was dropped).
        assert_eq!(log.begin("p", "y".into(), "/w".into()), 1);
    }

    #[test]
    fn record_serde_roundtrips() {
        let rec = CommandRecord {
            seq: 7,
            command: "cargo test".into(),
            cwd: "/home/z/proj".into(),
            started_ms: 1_700_000_000_000,
            ended_ms: Some(1_700_000_002_500),
            exit: Some(0),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: CommandRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.exit, Some(0));
        assert_eq!(back.duration_ms(), Some(2_500));
    }
}
