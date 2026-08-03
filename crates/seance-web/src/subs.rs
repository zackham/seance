//! Per-GUI subscription preferences: which circles are **active** (rendered as
//! today's sidebar rows, streamed by the daemon) and which are **parked** (the
//! collapsed group underneath).
//!
//! Active/parked is presentation state owned by *this client*, not the daemon —
//! the daemon only knows a subscription set, and the two are kept in sync:
//! every activate sends `Subscribe`, every park sends `Unsubscribe`, and the
//! `State` push reconciles (a daemon-side auto-subscribe — spawn, fork, create,
//! select — folds straight back into the active list).
//!
//! A subset of the active circles may additionally be **pinned** — rendered in
//! their own section at the top of the sidebar, above the normal active band.
//!
//! Persistence is `localStorage["seance_active"]` =
//! `{"active":[…],"seen":[…],"pinned":[…]}` behind the [`SubStore`] seam so the
//! logic stays testable off-wasm (`pinned` is `serde(default)`: blobs written
//! before pins existed still parse).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// localStorage key holding the serialized [`SubPrefs`].
pub const STORAGE_KEY: &str = "seance_active";

/// The persisted per-GUI split.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubPrefs {
    /// Circles rendered in the main sidebar list (client-side truth).
    #[serde(default)]
    pub active: Vec<String>,
    /// Circles this GUI has acknowledged. An unseen, non-active circle badges
    /// `needs` until it is first selected ("ctl spawns parked+needs").
    #[serde(default)]
    pub seen: Vec<String>,
    /// Circles pinned to their own section at the TOP of the sidebar. A pinned
    /// circle is always active (pinning a parked one activates it; parking a
    /// pinned one unpins it). `serde(default)` so pre-pin stored blobs parse.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// False until a stored list was loaded or the first `State` seeded one.
    /// Drives the three-valued `Attach.subscriptions` (None = "everything").
    #[serde(skip)]
    pub seeded: bool,
}

impl SubPrefs {
    /// Parse a stored blob. Anything unparseable is treated as "no list yet"
    /// (caller falls back to the migration path).
    pub fn parse(json: &str) -> Option<SubPrefs> {
        let mut p: SubPrefs = serde_json::from_str(json).ok()?;
        p.seeded = true;
        Some(p)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn is_active(&self, ws: &str) -> bool {
        self.active.iter().any(|w| w == ws)
    }

    pub fn has_seen(&self, ws: &str) -> bool {
        self.is_active(ws) || self.seen.iter().any(|w| w == ws)
    }

    /// `Attach.subscriptions`: `None` (subscribe to everything) until a list
    /// exists, then exactly the stored active list.
    pub fn attach_subscriptions(&self) -> Option<Vec<String>> {
        self.seeded.then(|| self.active.clone())
    }

    /// Add to active (and mark seen). Returns whether anything changed.
    pub fn activate(&mut self, ws: &str) -> bool {
        let mut changed = self.mark_seen(ws);
        if !self.is_active(ws) {
            self.active.push(ws.to_string());
            changed = true;
        }
        changed
    }

    pub fn is_pinned(&self, ws: &str) -> bool {
        self.pinned.iter().any(|w| w == ws)
    }

    /// Pin a circle to the top section. Pinning implies active (a parked circle
    /// is promoted). Returns whether anything changed.
    pub fn pin(&mut self, ws: &str) -> bool {
        let mut changed = self.activate(ws);
        if !self.is_pinned(ws) {
            self.pinned.push(ws.to_string());
            changed = true;
        }
        changed
    }

    /// Drop the pin, leaving the circle active. Returns whether anything changed.
    pub fn unpin(&mut self, ws: &str) -> bool {
        let before = self.pinned.len();
        self.pinned.retain(|w| w != ws);
        self.pinned.len() != before
    }

    /// Carry per-GUI state across a workspace rename (the daemon re-subscribes
    /// under the new name; the pin has no such channel).
    pub fn rename(&mut self, old: &str, new: &str) -> bool {
        let mut changed = false;
        if self.is_pinned(old) {
            self.unpin(old);
            changed = self.pin(new) || changed;
        }
        changed
    }

    /// Remove from active, keeping it seen. Returns whether anything changed.
    pub fn park(&mut self, ws: &str) -> bool {
        // Parking a pinned circle drops the pin.
        let mut changed = self.unpin(ws);
        if self.is_active(ws) {
            self.active.retain(|w| w != ws);
            changed = true;
        }
        // `has_seen` reads active too, so record it explicitly on the way out.
        if !self.seen.iter().any(|w| w == ws) {
            self.seen.push(ws.to_string());
            changed = true;
        }
        changed
    }

    pub fn mark_seen(&mut self, ws: &str) -> bool {
        if self.has_seen(ws) {
            return false;
        }
        self.seen.push(ws.to_string());
        true
    }

    /// First-run migration: no stored list → the active list *is* whatever the
    /// daemon subscribed us to (Attach sent `None` = everything), and every
    /// circle that already exists counts as seen so nothing badges `needs`
    /// retroactively.
    pub fn seed(&mut self, subscriptions: &[String], known: &[String]) {
        self.active = subscriptions.to_vec();
        self.seen = known.to_vec();
        self.seeded = true;
    }

    /// Fold a `State` push back in: daemon-side auto-subscribes (spawn / fork /
    /// create / select) join the active list, and circles the daemon no longer
    /// knows about drop out of both lists. Returns whether anything changed.
    pub fn reconcile(&mut self, subscriptions: &[String], known: &[String]) -> bool {
        let known: HashSet<&str> = known.iter().map(String::as_str).collect();
        let before = (self.active.clone(), self.seen.clone(), self.pinned.clone());
        for ws in subscriptions {
            if !self.is_active(ws) {
                self.active.push(ws.clone());
            }
        }
        self.active.retain(|w| known.contains(w.as_str()));
        self.seen.retain(|w| known.contains(w.as_str()));
        // Killed circles drop their pin; a pin can never outlive the active set.
        let active = &self.active;
        self.pinned
            .retain(|w| known.contains(w.as_str()) && active.iter().any(|a| a == w));
        (self.active.clone(), self.seen.clone(), self.pinned.clone()) != before
    }
}

/// Storage seam: one slot of string state (localStorage in the browser, a
/// `HashMap` in tests).
pub trait SubStore {
    fn get(&self) -> Option<String>;
    fn set(&self, val: &str);
}

/// Read prefs from the store; a missing/garbage blob yields unseeded defaults.
pub fn load(store: &dyn SubStore) -> SubPrefs {
    store
        .get()
        .as_deref()
        .and_then(SubPrefs::parse)
        .unwrap_or_default()
}

pub fn save(store: &dyn SubStore, prefs: &SubPrefs) {
    store.set(&prefs.to_json());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct MemStore(RefCell<Option<String>>);
    impl SubStore for MemStore {
        fn get(&self) -> Option<String> {
            self.0.borrow().clone()
        }
        fn set(&self, val: &str) {
            *self.0.borrow_mut() = Some(val.to_string());
        }
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_store_is_unseeded_and_attaches_to_everything() {
        let store = MemStore::default();
        let prefs = load(&store);
        assert!(!prefs.seeded);
        assert_eq!(prefs.attach_subscriptions(), None);
    }

    #[test]
    fn round_trips_through_the_store() {
        let store = MemStore::default();
        let mut prefs = SubPrefs::default();
        prefs.activate("lab");
        prefs.park("old");
        save(&store, &prefs);

        let back = load(&store);
        assert!(back.seeded);
        assert_eq!(back.active, v(&["lab"]));
        assert_eq!(back.seen, v(&["lab", "old"]));
        assert_eq!(back.attach_subscriptions(), Some(v(&["lab"])));
    }

    #[test]
    fn garbage_blob_falls_back_to_migration() {
        let store = MemStore::default();
        store.set("not json");
        assert!(!load(&store).seeded);
    }

    #[test]
    fn seed_takes_the_daemon_set_and_marks_everything_seen() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web"]), &v(&["lab", "web", "ghost"]));
        assert!(prefs.seeded);
        assert_eq!(prefs.active, v(&["lab", "web"]));
        // Parked-but-known circles must NOT badge `needs` on first run.
        assert!(prefs.has_seen("ghost"));
    }

    #[test]
    fn reconcile_adopts_daemon_auto_subscribes() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab"]), &v(&["lab"]));
        // Daemon subscribed us to a circle we just forked.
        assert!(prefs.reconcile(&v(&["lab", "fork-1"]), &v(&["lab", "fork-1"])));
        assert_eq!(prefs.active, v(&["lab", "fork-1"]));
        // Idempotent.
        assert!(!prefs.reconcile(&v(&["lab", "fork-1"]), &v(&["lab", "fork-1"])));
    }

    #[test]
    fn reconcile_prunes_dead_circles() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web"]), &v(&["lab", "web", "old"]));
        assert!(prefs.reconcile(&v(&["lab"]), &v(&["lab"])));
        assert_eq!(prefs.active, v(&["lab"]));
        assert!(prefs.seen.iter().all(|w| w == "lab"));
    }

    #[test]
    fn park_then_activate_is_a_round_trip() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web"]), &v(&["lab", "web"]));
        assert!(prefs.park("web"));
        assert!(!prefs.is_active("web"));
        assert!(prefs.has_seen("web"));
        assert!(prefs.activate("web"));
        assert!(prefs.is_active("web"));
        assert!(!prefs.activate("web"));
    }

    #[test]
    fn pin_implies_active_and_unpin_leaves_it_active() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab"]), &v(&["lab", "raid"]));
        // Pinning a PARKED circle activates it too.
        assert!(prefs.pin("raid"));
        assert!(prefs.is_pinned("raid"));
        assert!(prefs.is_active("raid"));
        assert!(prefs.has_seen("raid"));
        // Idempotent.
        assert!(!prefs.pin("raid"));
        // Unpin keeps it active.
        assert!(prefs.unpin("raid"));
        assert!(!prefs.is_pinned("raid"));
        assert!(prefs.is_active("raid"));
        assert!(!prefs.unpin("raid"));
    }

    #[test]
    fn park_unpins() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web"]), &v(&["lab", "web"]));
        prefs.pin("web");
        assert!(prefs.park("web"));
        assert!(!prefs.is_pinned("web"));
        assert!(!prefs.is_active("web"));
    }

    #[test]
    fn rename_carries_the_pin() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab"]), &v(&["lab"]));
        prefs.pin("lab");
        assert!(prefs.rename("lab", "lab-2"));
        assert!(!prefs.is_pinned("lab"));
        assert!(prefs.is_pinned("lab-2"));
        // Renaming an UNPINNED circle is a no-op for pins.
        prefs.activate("web");
        assert!(!prefs.rename("web", "web-2"));
        assert!(!prefs.is_pinned("web-2"));
    }

    #[test]
    fn reconcile_prunes_pins_of_dead_circles() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web"]), &v(&["lab", "web"]));
        prefs.pin("web");
        assert!(prefs.reconcile(&v(&["lab"]), &v(&["lab"])));
        assert!(prefs.pinned.is_empty());
    }

    #[test]
    fn pins_round_trip_and_old_blobs_still_parse() {
        let store = MemStore::default();
        let mut prefs = SubPrefs::default();
        prefs.pin("lab");
        save(&store, &prefs);
        let back = load(&store);
        assert_eq!(back.pinned, v(&["lab"]));
        assert_eq!(back.active, v(&["lab"]));

        // Pre-pin localStorage blob: no `pinned` key at all.
        let old = SubPrefs::parse(r#"{"active":["lab"],"seen":["lab","old"]}"#).unwrap();
        assert!(old.seeded);
        assert_eq!(old.active, v(&["lab"]));
        assert!(old.pinned.is_empty());
        assert!(!old.is_pinned("lab"));
    }

    #[test]
    fn unseen_parked_circle_is_the_ctl_spawn_case() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab"]), &v(&["lab"]));
        // ctl spawned `raid` with no GUI attribution: known, not subscribed.
        prefs.reconcile(&v(&["lab"]), &v(&["lab", "raid"]));
        assert!(!prefs.is_active("raid"));
        assert!(!prefs.has_seen("raid"));
        prefs.mark_seen("raid");
        assert!(prefs.has_seen("raid"));
    }
}
