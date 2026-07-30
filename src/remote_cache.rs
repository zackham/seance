//! Non-blocking cache over the daemon fs bridge for RENDER-path reads.
//!
//! Render code must never block on the bridge, but several surfaces (pad
//! drawer sidecars, phone-bind chips, prompt library) want daemon-side file
//! contents while painting. The contract:
//!
//! - [`RemoteCache::get`] is render-safe: it marks the path as *wanted* and
//!   returns whatever was last fetched (`None` until the first refresh, or
//!   when the file is confirmed missing).
//! - [`RemoteCache::refresh`] BLOCKS on the bridge and must only run on a
//!   background thread/executor. The app runs it on a ~2s loop, matching the
//!   old local-disk poll cadence — data is at most one tick stale.
//! - [`RemoteCache::fetch_now`] is a blocking fetch-and-store for background
//!   event paths that need the fresh value immediately (e.g. right after
//!   `seance ctl phone` writes the bind file).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::gui_client::GuiClient;

/// Pure map state, factored out so cache behavior is unit-testable without a
/// live [`GuiClient`].
#[derive(Default)]
struct CacheMap {
    /// path → last-fetched contents (`None` = confirmed missing on the daemon).
    entries: HashMap<String, Option<String>>,
    /// Paths some render path has asked for; refreshed every tick.
    wanted: HashSet<String>,
}

impl CacheMap {
    /// Mark `path` wanted; return the cached contents if any fetch has landed.
    fn get(&mut self, path: &str) -> Option<String> {
        self.wanted.insert(path.to_string());
        self.entries.get(path).cloned().flatten()
    }

    fn wanted_paths(&self) -> Vec<String> {
        self.wanted.iter().cloned().collect()
    }

    /// Store one fetch result. Returns true when the visible value changed.
    fn store(&mut self, path: &str, value: Option<String>) -> bool {
        match self.entries.get(path) {
            Some(prev) if *prev == value => false,
            _ => {
                self.entries.insert(path.to_string(), value);
                true
            }
        }
    }
}

/// Shared render-safe view of daemon-side files. Clone the [`Arc`] freely.
pub struct RemoteCache {
    client: Arc<GuiClient>,
    map: Mutex<CacheMap>,
}

impl RemoteCache {
    pub fn new(client: Arc<GuiClient>) -> Self {
        Self {
            client,
            map: Mutex::new(CacheMap::default()),
        }
    }

    /// Render-safe read: marks `path` wanted, returns the last-fetched
    /// contents (`None` = not fetched yet OR confirmed missing).
    pub fn get(&self, path: &str) -> Option<String> {
        self.map.lock().unwrap().get(path)
    }

    /// Blocking: fetch every wanted path over the bridge and update the map.
    /// Background threads/executors only. Returns true if anything changed
    /// (callers use this to decide whether to `cx.notify()`).
    pub fn refresh(&self) -> bool {
        let wanted = self.map.lock().unwrap().wanted_paths();
        let mut changed = false;
        for path in wanted {
            // Read failure (missing file) → confirmed-missing. A bridge
            // outage also lands here; the next tick self-heals.
            let value = self.client.fs_read_string(&path).ok().map(|(s, _)| s);
            changed |= self.map.lock().unwrap().store(&path, value);
        }
        changed
    }

    /// Blocking fetch of ONE path, stored + returned. Background only.
    pub fn fetch_now(&self, path: &str) -> Option<String> {
        let value = self.client.fs_read_string(path).ok().map(|(s, _)| s);
        let mut map = self.map.lock().unwrap();
        map.wanted.insert(path.to_string());
        map.store(path, value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_marks_wanted_and_returns_none_until_fetched() {
        let mut m = CacheMap::default();
        assert_eq!(m.get("/a"), None);
        assert!(m.wanted_paths().contains(&"/a".to_string()));
        // A fetch landing makes the value visible; a missing-file fetch
        // stays None but is a *stored* miss (no change on repeat).
        assert!(m.store("/a", Some("hi".into())));
        assert_eq!(m.get("/a"), Some("hi".into()));
        assert!(m.store("/a", None));
        assert_eq!(m.get("/a"), None);
    }

    #[test]
    fn store_reports_change_only_on_new_value() {
        let mut m = CacheMap::default();
        assert!(m.store("/p", Some("v1".into()))); // first fetch
        assert!(!m.store("/p", Some("v1".into()))); // same contents
        assert!(m.store("/p", Some("v2".into()))); // edit
        assert!(m.store("/p", None)); // file removed
        assert!(!m.store("/p", None)); // still missing
    }
}
