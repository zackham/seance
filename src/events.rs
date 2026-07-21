//! The event log: seance's flight recorder.
//!
//! Every meaningful action — human UI actions and agent control-plane calls
//! alike — appends one attributed line to `~/.local/share/seance/events.jsonl`.
//! This is the substrate the council converged on: provenance first, then
//! timelines, replays, and summaries compound on top.
//!
//! Actors: `"human"` (UI actions), `"agent:<pane-slug>"` (ctl calls that carry
//! a `from` pane), `"cli"` (ctl calls from outside seance), `"system"`.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    /// Unix millis.
    pub ts: u64,
    /// "human" | "agent:<slug>" | "cli" | "system"
    pub actor: String,
    pub workspace: Option<String>,
    /// The pane acted UPON (not the actor's own pane).
    pub pane: Option<String>,
    /// Short machine-readable kind: pane_spawned, pane_killed, pane_renamed,
    /// pane_moved, pane_tiled, pane_shelved, pane_popped, pane_returned,
    /// focus, workspace_selected, workspace_created, ctl_send, ctl_send_raw,
    /// ctl_read, ctl_status, ctl_kill, ctl_new, status_set, ask, ask_answered.
    pub kind: String,
    /// Human-readable one-liner.
    pub detail: String,
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
    PathBuf::from(shellexpand::tilde("~/.local/share/seance").into_owned())
        .join("events.jsonl")
}

/// Append one event. Best-effort: a logging failure never breaks the app.
pub fn log(actor: &str, workspace: Option<&str>, pane: Option<&str>, kind: &str, detail: String) {
    let event = Event {
        ts: now_ms(),
        actor: actor.to_string(),
        workspace: workspace.map(str::to_string),
        pane: pane.map(str::to_string),
        kind: kind.to_string(),
        detail,
    };
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&event) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{json}");
        }
    }
}

/// Read events, newest last, with optional filters. `since_ms` of 0 = all.
pub fn read(
    since_ms: u64,
    workspace: Option<&str>,
    pane: Option<&str>,
    actor: Option<&str>,
    limit: usize,
) -> Vec<Event> {
    let Ok(content) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };
    let mut events: Vec<Event> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .filter(|e| e.ts >= since_ms)
        .filter(|e| workspace.is_none_or(|w| e.workspace.as_deref() == Some(w)))
        .filter(|e| pane.is_none_or(|p| e.pane.as_deref() == Some(p)))
        .filter(|e| actor.is_none_or(|a| e.actor == a || e.actor.starts_with(&format!("{a}:"))))
        .collect();
    let skip = events.len().saturating_sub(limit);
    events.drain(..skip);
    events
}

/// Render a timestamp as local wall-clock HH:MM:SS.
pub fn fmt_time(ts_ms: u64) -> String {
    // Cheap local-time formatting without a chrono dependency: shell out is
    // overkill, so use the libc-free approach of computing from the offset the
    // system reports for *now*. Good enough for a same-day activity feed.
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
    // Parse `date +%z` once per process.
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
