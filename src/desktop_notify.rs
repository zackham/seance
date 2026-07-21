//! Desktop notifications for attention routing (needs-human / ask).
//!
//! Fire-and-forget `notify-send` on Linux. Silent no-op if the binary is missing.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Dedup window so a thrashing agent can't spam the notification daemon.
const MIN_GAP: Duration = Duration::from_secs(4);

pub fn notify(summary: &str, body: &str) {
    let now = Instant::now();
    let now_ms = now.elapsed().as_millis() as u64; // wrong baseline — use wall clock
    let _ = now_ms;
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST_MS.load(Ordering::Relaxed);
    if wall.saturating_sub(prev) < MIN_GAP.as_millis() as u64 {
        return;
    }
    LAST_MS.store(wall, Ordering::Relaxed);

    let summary = summary.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = Command::new("notify-send")
            .args([
                "--app-name=seance",
                "--urgency=normal",
                "--expire-time=12000",
                &summary,
                &body,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

pub fn needs_human(pane: &str, note: Option<&str>) {
    let body = match note {
        Some(n) if !n.is_empty() => format!("{pane}: {n}"),
        _ => format!("{pane} needs you"),
    };
    notify("seance · needs human", &body);
}

pub fn ask(from: &str, question: &str) {
    let q = if question.len() > 160 {
        format!("{}…", &question[..160])
    } else {
        question.to_string()
    };
    notify("seance · agent asks", &format!("{from}: {q}"));
}
