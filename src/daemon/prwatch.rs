//! Daemon-side PR watcher ingest.
//!
//! Config seam, quicklaunch-style: an **external** poller (vita's `gh` loop)
//! owns judgment about a PR's state and writes it atomically to
//! `<state_dir>/pr_watch.json`:
//!
//! ```json
//! { "https://github.com/o/r/pull/1": {
//!     "state": "open", "attention": "needs",
//!     "label": "CI ✗", "updated_ms": 1754000000000 } }
//! ```
//!
//! This thread re-reads the file only when its mtime moves (same idiom as the
//! host-widget poller in `fsbridge.rs`), merges the verdicts onto links the
//! daemon already scraped, and pushes state to every window when something
//! actually changed. Nothing here decides attention — that keeps the poller
//! swappable and the daemon dumb.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::runtime::protocol::PrStatus;

use super::SharedEngine;

/// Poll cadence. The file changes at the external poller's rate (~180s), so
/// this is only about how fast a fresh verdict reaches the sidebar.
const POLL: Duration = Duration::from_secs(2);

/// `<state_dir>/pr_watch.json` — beside state.json, honors `SEANCE_STATE_DIR`.
pub fn watch_file_path() -> PathBuf {
    match crate::state::state_dir() {
        Ok(dir) => dir.join("pr_watch.json"),
        Err(_) => {
            PathBuf::from(shellexpand::tilde("~/.local/share/seance/pr_watch.json").into_owned())
        }
    }
}

/// Parse the watch file body. Unknown/malformed entries are dropped rather
/// than poisoning the whole map — a poller bug must not blank the chips.
pub fn parse_watch(bytes: &[u8]) -> HashMap<String, PrStatus> {
    serde_json::from_slice::<HashMap<String, serde_json::Value>>(bytes)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(url, v)| {
            serde_json::from_value::<PrStatus>(v)
                .ok()
                .map(|st| (url, st))
        })
        .collect()
}

fn mtime_ns(path: &std::path::Path) -> Option<u128> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

/// Start the mtime-poll thread.
pub fn start_pr_watch_poller(engine: SharedEngine) {
    std::thread::Builder::new()
        .name("seance-pr-watch".into())
        .spawn(move || {
            let path = watch_file_path();
            let mut last: Option<u128> = None;
            loop {
                let now = mtime_ns(&path);
                if now != last {
                    last = now;
                    let watch = std::fs::read(&path)
                        .map(|b| parse_watch(&b))
                        .unwrap_or_default();
                    if let Ok(mut eng) = engine.lock() {
                        if eng.ingest_pr_watch(&watch) {
                            eng.persist();
                            eng.push_state_to_all();
                        }
                    }
                }
                std::thread::sleep(POLL);
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_poller_shape_and_skips_junk() {
        let body = r#"{
            "https://github.com/o/r/pull/1": {"state":"open","attention":"needs","label":"CI ✗","updated_ms":7},
            "https://github.com/o/r/pull/2": 12,
            "https://github.com/o/r/pull/3": {"state":"merged"}
        }"#;
        let m = parse_watch(body.as_bytes());
        assert_eq!(m.len(), 2);
        let one = &m["https://github.com/o/r/pull/1"];
        assert_eq!(one.attention.as_deref(), Some("needs"));
        assert_eq!(one.label, "CI ✗");
        let three = &m["https://github.com/o/r/pull/3"];
        assert_eq!(three.state, "merged");
        assert!(three.attention.is_none());
    }

    #[test]
    fn parses_new_richer_fields() {
        let body = r#"{
            "https://github.com/o/r/pull/1": {
                "state":"open","attention":"needs","label":"CI ✗","updated_ms":7,
                "is_draft":true,"ci":"fail","review":"changes",
                "opened_ms":100,"last_review_ms":200,"last_comment_ms":300
            }
        }"#;
        let m = parse_watch(body.as_bytes());
        let one = &m["https://github.com/o/r/pull/1"];
        assert!(one.is_draft);
        assert_eq!(one.ci.as_deref(), Some("fail"));
        assert_eq!(one.review.as_deref(), Some("changes"));
        assert_eq!(one.opened_ms, 100);
        assert_eq!(one.last_review_ms, 200);
        assert_eq!(one.last_comment_ms, 300);
    }

    #[test]
    fn old_shape_without_new_keys_still_parses() {
        let body = r#"{"https://github.com/o/r/pull/1":
            {"state":"open","attention":"needs","label":"CI ✗","updated_ms":7}}"#;
        let one = &parse_watch(body.as_bytes())["https://github.com/o/r/pull/1"];
        assert_eq!(one.label, "CI ✗");
        assert!(!one.is_draft);
        assert!(one.ci.is_none());
        assert!(one.review.is_none());
        assert_eq!(
            (one.opened_ms, one.last_review_ms, one.last_comment_ms),
            (0, 0, 0)
        );
    }

    #[test]
    fn corrupt_body_is_empty_not_panic() {
        assert!(parse_watch(b"not json").is_empty());
    }
}
