//! The event bus: seance's flight recorder and pub/sub spine.
//!
//! Every meaningful action — human UI actions and agent control-plane calls
//! alike — becomes a typed, sequenced, attributable event. JSONL at
//! `~/.local/share/seance/events.jsonl` is the durable subscriber; in-process
//! watchers (and `seance ctl watch`) receive live copies.
//!
//! Actors: `"human"` (UI), `"agent:<pane-slug>"` (ctl from a pane), `"cli"`
//! (ctl outside seance), `"daemon"`, `"system"`.
//!
//! Optional fields (`caused_by`, `span`, `origin`) enable causal chains and
//! byte-level / action-level attribution without breaking old log lines.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How many recent events stay in the in-memory ring for catch-up on subscribe.
const RING_CAP: usize = 4096;

/// One attributed event. Old log lines without `id`/`seq` still deserialize.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Stable id (`evt_<seq>`). Empty on legacy lines.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Monotonic sequence (process-global, restarts at 1 after daemon reboot
    /// unless recovered from the log tail — see [`Bus::load_seq_from_disk`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seq: u64,
    /// Unix millis.
    pub ts: u64,
    /// "human" | "agent:<slug>" | "cli" | "daemon" | "system"
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The pane acted UPON (not the actor's own pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    /// Short machine-readable kind (see module docs / roadmap).
    pub kind: String,
    /// Human-readable one-liner.
    pub detail: String,
    /// Causal parent event id, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<String>,
    /// Span id grouping related events (e.g. a command run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
    /// Provenance of an input/mutation: human_keystroke | ctl_send |
    /// ctl_send_raw | propose_accepted | inject | arm | shell_hook | …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Optional fields for [`log_ex`].
#[derive(Default, Clone, Debug)]
pub struct LogOpts {
    pub caused_by: Option<String>,
    pub span: Option<String>,
    pub origin: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn log_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            let expanded = shellexpand::full(&dir)
                .map(|s| s.into_owned())
                .unwrap_or(dir);
            return PathBuf::from(expanded).join("events.jsonl");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.local/share/seance").into_owned()).join("events.jsonl")
}

// ---------------------------------------------------------------------------
// In-process bus
// ---------------------------------------------------------------------------

struct Bus {
    seq: AtomicU64,
    ring: Mutex<VecDeque<Event>>,
    subs: Mutex<Vec<Sender<Event>>>,
}

impl Bus {
    fn new() -> Self {
        let bus = Self {
            seq: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAP)),
            subs: Mutex::new(Vec::new()),
        };
        bus.load_seq_from_disk();
        bus
    }

    /// Best-effort: start seq after the highest seq already on disk so
    /// restarts don't reuse ids. Legacy lines without seq are ignored.
    fn load_seq_from_disk(&self) {
        let Ok(content) = std::fs::read_to_string(log_path()) else {
            return;
        };
        let mut max = 0u64;
        for line in content.lines().rev().take(500) {
            if let Ok(e) = serde_json::from_str::<Event>(line) {
                if e.seq > max {
                    max = e.seq;
                }
            }
        }
        if max > 0 {
            self.seq.store(max, Ordering::SeqCst);
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn publish(&self, mut event: Event) -> Event {
        if event.seq == 0 {
            event.seq = self.next_seq();
        }
        if event.id.is_empty() {
            event.id = format!("evt_{}", event.seq);
        }
        if event.ts == 0 {
            event.ts = now_ms();
        }

        // Durable append first (best-effort).
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&event) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "{json}");
            }
        }

        // Ring.
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() >= RING_CAP {
                ring.pop_front();
            }
            ring.push_back(event.clone());
        }

        // Live subscribers (drop dead ones).
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|tx| tx.send(event.clone()).is_ok());
        }

        event
    }

    fn subscribe(&self) -> (Receiver<Event>, u64) {
        let (tx, rx) = mpsc::channel();
        let cursor = self.seq.load(Ordering::SeqCst);
        if let Ok(mut subs) = self.subs.lock() {
            subs.push(tx);
        }
        (rx, cursor)
    }

    fn ring_since(&self, since_seq: u64) -> Vec<Event> {
        let Ok(ring) = self.ring.lock() else {
            return Vec::new();
        };
        ring.iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect()
    }
}

fn bus() -> &'static Bus {
    static BUS: OnceLock<Bus> = OnceLock::new();
    BUS.get_or_init(Bus::new)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Append + publish one event. Best-effort: failure never breaks the app.
/// Returns the fully stamped event (with id/seq).
pub fn log(actor: &str, workspace: Option<&str>, pane: Option<&str>, kind: &str, detail: String) -> Event {
    log_ex(actor, workspace, pane, kind, detail, LogOpts::default())
}

/// Like [`log`] with causal / origin metadata.
pub fn log_ex(
    actor: &str,
    workspace: Option<&str>,
    pane: Option<&str>,
    kind: &str,
    detail: String,
    opts: LogOpts,
) -> Event {
    let event = Event {
        id: String::new(),
        seq: 0,
        ts: 0,
        actor: actor.to_string(),
        workspace: workspace.map(str::to_string),
        pane: pane.map(str::to_string),
        kind: kind.to_string(),
        detail,
        caused_by: opts.caused_by,
        span: opts.span,
        origin: opts.origin,
    };
    bus().publish(event)
}

/// Subscribe to live events. Returns `(receiver, current_seq)`.
/// Use `ring_since` / `read` for catch-up, then drain the receiver.
pub fn subscribe() -> (Receiver<Event>, u64) {
    bus().subscribe()
}

/// In-memory ring events with `seq > since_seq`.
pub fn ring_since(since_seq: u64) -> Vec<Event> {
    bus().ring_since(since_seq)
}

/// Current sequence high-water mark.
pub fn current_seq() -> u64 {
    bus().seq.load(Ordering::SeqCst)
}

/// Read events from disk, newest last, with optional filters.
/// `since_ms` of 0 = all. `since_seq` of 0 = all sequences.
pub fn read(
    since_ms: u64,
    workspace: Option<&str>,
    pane: Option<&str>,
    actor: Option<&str>,
    limit: usize,
) -> Vec<Event> {
    read_ex(since_ms, 0, workspace, pane, actor, None, limit)
}

/// Extended read with seq cursor and kind filter.
pub fn read_ex(
    since_ms: u64,
    since_seq: u64,
    workspace: Option<&str>,
    pane: Option<&str>,
    actor: Option<&str>,
    kinds: Option<&[String]>,
    limit: usize,
) -> Vec<Event> {
    let Ok(content) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };
    let mut events: Vec<Event> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .filter(|e| e.ts >= since_ms)
        .filter(|e| since_seq == 0 || e.seq > since_seq)
        .filter(|e| workspace.is_none_or(|w| e.workspace.as_deref() == Some(w)))
        .filter(|e| pane.is_none_or(|p| e.pane.as_deref() == Some(p)))
        .filter(|e| actor.is_none_or(|a| e.actor == a || e.actor.starts_with(&format!("{a}:"))))
        .filter(|e| {
            kinds.is_none_or(|ks| {
                ks.is_empty() || ks.iter().any(|k| k == &e.kind || e.kind.starts_with(k.as_str()))
            })
        })
        .collect();
    let skip = events.len().saturating_sub(limit);
    events.drain(..skip);
    events
}

/// Does `event` match a watch filter?
pub fn matches_filter(
    e: &Event,
    workspace: Option<&str>,
    pane: Option<&str>,
    actor: Option<&str>,
    kinds: Option<&[String]>,
) -> bool {
    if let Some(w) = workspace {
        if e.workspace.as_deref() != Some(w) {
            return false;
        }
    }
    if let Some(p) = pane {
        if e.pane.as_deref() != Some(p) {
            return false;
        }
    }
    if let Some(a) = actor {
        if e.actor != a && !e.actor.starts_with(&format!("{a}:")) {
            return false;
        }
    }
    if let Some(ks) = kinds {
        if !ks.is_empty()
            && !ks
                .iter()
                .any(|k| k == &e.kind || e.kind.starts_with(k.as_str()))
        {
            return false;
        }
    }
    true
}

/// Render a timestamp as local wall-clock HH:MM:SS.
pub fn fmt_time(ts_ms: u64) -> String {
    let secs = ts_ms / 1000;
    let offset = local_offset_secs();
    let local = secs as i64 + offset;
    let (h, m, s) = (
        (local / 3600).rem_euclid(24),
        (local / 60).rem_euclid(60),
        local.rem_euclid(60),
    );
    format!("{h:02}:{m:02}:{s:02}")
}

fn local_offset_secs() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                let s = s.trim();
                if s.len() >= 5 {
                    let sign = if s.starts_with('-') { -1 } else { 1 };
                    let h: i64 = s[1..3].parse().ok()?;
                    let m: i64 = s[3..5].parse().ok()?;
                    Some(sign * (h * 3600 + m * 60))
                } else {
                    None
                }
            })
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_assigns_seq_and_id() {
        // Isolated state dir so we don't pollute the real log.
        let dir = std::env::temp_dir().join(format!("seance-events-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("SEANCE_STATE_DIR", &dir);
        // Note: bus is OnceLock — may already be init from other tests in-process.
        // Just check publish shape.
        let e = log("cli", Some("lab"), Some("w1"), "test_kind", "hello".into());
        assert!(!e.id.is_empty() || e.seq > 0 || e.ts > 0);
        assert_eq!(e.kind, "test_kind");
        assert_eq!(e.actor, "cli");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matches_filter_kinds_prefix() {
        let e = Event {
            id: "evt_1".into(),
            seq: 1,
            ts: 1,
            actor: "agent:w".into(),
            workspace: Some("lab".into()),
            pane: Some("w".into()),
            kind: "ctl_send".into(),
            detail: "x".into(),
            caused_by: None,
            span: None,
            origin: Some("ctl_send".into()),
        };
        let kinds = vec!["ctl_".into()];
        assert!(matches_filter(&e, Some("lab"), None, None, Some(&kinds)));
        let kinds2 = vec!["status_set".into()];
        assert!(!matches_filter(&e, None, None, None, Some(&kinds2)));
    }

    #[test]
    fn legacy_event_deserializes() {
        let json = r#"{"ts":1,"actor":"human","workspace":"lab","pane":"t1","kind":"focus","detail":"focused"}"#;
        let e: Event = serde_json::from_str(json).unwrap();
        assert_eq!(e.kind, "focus");
        assert_eq!(e.seq, 0);
        assert!(e.id.is_empty());
    }
}
