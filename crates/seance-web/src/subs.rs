//! Per-GUI rail preferences: pins, folds, and which circles this GUI has
//! looked at. The client subscribes to every circle the daemon knows — the
//! active/parked split and its park verb were removed in 0.26.
//!
//! Circles may be **pinned** — rendered in their own section at the top of the
//! sidebar, above the normal band.
//!
//! Persistence is `localStorage["seance_active"]` =
//! `{"seen":[…],"pinned":[…],"collapsed":[…]}` behind the [`SubStore`] seam so
//! the logic stays testable off-wasm. Every field is `serde(default)`, and an
//! `active` key from a pre-0.26 blob is simply ignored.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// localStorage key holding the serialized [`SubPrefs`].
pub const STORAGE_KEY: &str = "seance_active";

/// Fold key for a prefix cluster inside a band.
pub fn group_key(section: &str, prefix: &str) -> String {
    format!("{section}/{}", prefix.to_ascii_lowercase())
}

/// The persisted per-GUI split.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubPrefs {
    /// Circles this GUI has acknowledged. One it has never selected badges
    /// `needs` — that's how a ctl-spawned circle announces itself.
    #[serde(default)]
    pub seen: Vec<String>,
    /// Circles pinned to their own section at the TOP of the sidebar.
    /// `serde(default)` so pre-pin stored blobs parse.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Folded prefix clusters, keyed `"<band>/<prefix>"`. Bands themselves
    /// stopped being foldable in 0.26 (their headers are gone), so a bare key
    /// here is dead weight and `prune_collapsed` sweeps it.
    #[serde(default)]
    pub collapsed: Option<Vec<String>>,
    /// False until a stored blob was loaded or the first `State` seeded one.
    /// Gates the first-run "mark everything known as seen" pass.
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

    pub fn has_seen(&self, ws: &str) -> bool {
        self.seen.iter().any(|w| w == ws)
    }

    pub fn is_pinned(&self, ws: &str) -> bool {
        self.pinned.iter().any(|w| w == ws)
    }

    /// Pin a circle to the top section. Implies seen. Returns whether anything
    /// changed.
    pub fn pin(&mut self, ws: &str) -> bool {
        let mut changed = self.mark_seen(ws);
        if !self.is_pinned(ws) {
            self.pinned.push(ws.to_string());
            changed = true;
        }
        changed
    }

    /// Drop the pin. Returns whether anything changed.
    pub fn unpin(&mut self, ws: &str) -> bool {
        let before = self.pinned.len();
        self.pinned.retain(|w| w != ws);
        self.pinned.len() != before
    }

    /// Is this cluster folded?
    pub fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed
            .as_ref()
            .is_some_and(|l| l.iter().any(|k| k == key))
    }

    /// Fold / unfold. Always reports `true` — the caller persists.
    pub fn toggle_collapsed(&mut self, key: &str) -> bool {
        let list = self.collapsed.get_or_insert_with(Vec::new);
        if let Some(i) = list.iter().position(|k| k == key) {
            list.remove(i);
        } else {
            list.push(key.to_string());
        }
        true
    }

    /// Drop folds for clusters that no longer exist. Bare band keys
    /// (`active`, and `parked`/`sleeping` from older blobs) name nothing
    /// foldable now and go the same way.
    pub fn prune_collapsed(&mut self, live_groups: &[String]) -> bool {
        let Some(list) = self.collapsed.as_mut() else {
            return false;
        };
        let before = list.len();
        list.retain(|k| live_groups.iter().any(|g| g == k));
        before != list.len()
    }

    pub fn mark_seen(&mut self, ws: &str) -> bool {
        if self.has_seen(ws) {
            return false;
        }
        self.seen.push(ws.to_string());
        true
    }

    /// First run: every circle that already exists counts as seen, so an
    /// upgrade doesn't badge the whole rail `needs`.
    pub fn seed(&mut self, known: &[String]) {
        self.seen = known.to_vec();
        self.seeded = true;
    }

    /// Fold a `State` push back in: circles the daemon no longer knows about
    /// drop out. Returns whether anything changed.
    pub fn reconcile(&mut self, known: &[String]) -> bool {
        let known: HashSet<&str> = known.iter().map(String::as_str).collect();
        let before = (self.seen.clone(), self.pinned.clone());
        self.seen.retain(|w| known.contains(w.as_str()));
        self.pinned.retain(|w| known.contains(w.as_str()));
        (self.seen.clone(), self.pinned.clone()) != before
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
    fn empty_store_is_unseeded() {
        let store = MemStore::default();
        assert!(!load(&store).seeded);
    }

    #[test]
    fn round_trips_through_the_store() {
        let store = MemStore::default();
        let mut prefs = SubPrefs::default();
        prefs.mark_seen("lab");
        prefs.mark_seen("old");
        save(&store, &prefs);

        let back = load(&store);
        assert!(back.seeded);
        assert_eq!(back.seen, v(&["lab", "old"]));
    }

    #[test]
    fn garbage_blob_falls_back_to_migration() {
        let store = MemStore::default();
        store.set("not json");
        assert!(!load(&store).seeded);
    }

    #[test]
    fn seed_marks_everything_known_seen() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web", "ghost"]));
        assert!(prefs.seeded);
        // Nothing badges `needs` on first run.
        assert!(prefs.has_seen("ghost"));
    }

    #[test]
    fn reconcile_prunes_dead_circles() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web", "old"]));
        assert!(prefs.reconcile(&v(&["lab"])));
        assert!(prefs.seen.iter().all(|w| w == "lab"));
        assert!(!prefs.reconcile(&v(&["lab"])));
    }

    #[test]
    fn pin_implies_seen_and_unpin_keeps_it() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab"]));
        assert!(prefs.pin("raid"));
        assert!(prefs.is_pinned("raid"));
        assert!(prefs.has_seen("raid"));
        // Idempotent.
        assert!(!prefs.pin("raid"));
        assert!(prefs.unpin("raid"));
        assert!(!prefs.is_pinned("raid"));
        assert!(prefs.has_seen("raid"));
        assert!(!prefs.unpin("raid"));
    }

    #[test]
    fn reconcile_prunes_pins_of_dead_circles() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab", "web"]));
        prefs.pin("web");
        assert!(prefs.reconcile(&v(&["lab"])));
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

        // Pre-0.26 blob: an `active` key that no longer means anything, and no
        // `pinned` key at all. Both are ignored rather than fatal.
        let old = SubPrefs::parse(r#"{"active":["lab"],"seen":["lab","old"]}"#).unwrap();
        assert!(old.seeded);
        assert_eq!(old.seen, v(&["lab", "old"]));
        assert!(old.pinned.is_empty());
    }

    #[test]
    fn an_unselected_circle_is_the_ctl_spawn_case() {
        let mut prefs = SubPrefs::default();
        prefs.seed(&v(&["lab"]));
        // ctl spawned `raid` with no GUI attribution: known, never looked at.
        prefs.reconcile(&v(&["lab", "raid"]));
        assert!(!prefs.has_seen("raid"));
        prefs.mark_seen("raid");
        assert!(prefs.has_seen("raid"));
    }
    /// Nothing is folded until the human folds it — no band defaults left.
    #[test]
    fn nothing_is_folded_by_default() {
        let mut p = SubPrefs::default();
        let k = group_key("active", "mtg");
        assert!(!p.is_collapsed(&k));
        p.toggle_collapsed(&k);
        assert!(p.is_collapsed(&k));
        p.toggle_collapsed(&k);
        assert!(!p.is_collapsed(&k));
    }

    /// Cluster folds are per band: folding `mtg` under active leaves the
    /// slept `mtg` circles alone.
    #[test]
    fn group_folds_are_scoped_to_their_section() {
        let mut p = SubPrefs::default();
        let a = group_key("active", "MTG");
        let s = group_key("sleeping", "mtg");
        assert_eq!(a, "active/mtg", "prefix key is case-insensitive");
        p.toggle_collapsed(&a);
        assert!(p.is_collapsed(&a));
        assert!(!p.is_collapsed(&s));
    }

    #[test]
    fn prune_drops_dead_group_folds() {
        let mut p = SubPrefs::default();
        p.toggle_collapsed(&group_key("active", "mtg"));
        p.toggle_collapsed(&group_key("active", "gone"));
        assert!(p.prune_collapsed(&["active/mtg".to_string()]));
        assert!(p.is_collapsed("active/mtg"));
        assert!(!p.is_collapsed("active/gone"));
    }

    /// Older blobs carry bare band keys that name nothing foldable now.
    #[test]
    fn prune_sweeps_bare_band_folds() {
        let mut p = SubPrefs::parse(r#"{"collapsed":["parked","sleeping","active/mtg"]}"#).unwrap();
        assert!(p.prune_collapsed(&["active/mtg".to_string()]));
        assert!(!p.is_collapsed("parked"));
        assert!(!p.is_collapsed("sleeping"));
        assert!(p.is_collapsed("active/mtg"), "live clusters survive");
    }
}
