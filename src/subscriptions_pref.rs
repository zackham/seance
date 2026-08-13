//! Active/parked/pinned rail arrangement — the shape of this module, not its
//! storage. **The daemon owns the arrangement** (`FsOp::SubsLoad/SubsSave`,
//! beside `layout.json` in its state dir), so the circles you keep in front of
//! you follow you to any window: desk, mac thin client, browser. Changing it
//! anywhere pushes [`GuiEvent::RailPrefs`] to every other window.
//!
//! `~/.config/seance/subscriptions.json` survives as a **local seed cache**,
//! not the source of truth. It exists for one reason: the `Attach` seed has to
//! be readable *before* a daemon connection exists, and attaching with the
//! arrangement we last saw beats attaching to everything and unsubscribing 40
//! circles a beat later. The daemon's copy is read straight after connecting
//! and wins any disagreement.
//!
//! Was per-GUI local state through 0.22; see the 0.23 CHANGELOG entry.
//!
//! Shape: `{ "active": [...], "seen": [...], "pinned": [...] }`.
//! - `active` — workspaces rendered in the normal sidebar band; everything
//!   else the daemon knows about lands in the collapsed `parked` group.
//! - `seen` — every workspace this GUI has ever had in `active`. A workspace
//!   that appears while parked and was never seen badges `needs` until it is
//!   first selected ("ctl spawns parked+needs").
//! - `pinned` — a subset of `active` rendered in its own section at the very
//!   top of the rail, above a divider. Pinning implies active/subscribed;
//!   parking unpins first. Same internal sort as every other band.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionsPref {
    #[serde(default)]
    pub active: BTreeSet<String>,
    #[serde(default)]
    pub seen: BTreeSet<String>,
    /// Subset of `active` pinned to the top section of the rail. `default` so
    /// pre-pin files still parse.
    #[serde(default)]
    pub pinned: BTreeSet<String>,
    /// Collapsed rail nodes: a section (`"active"`) or a prefix group inside
    /// one (`"active/mtg"`). Collapsing is per-section by design — the `mtg`
    /// circles you're working in and the ones you've slept are different
    /// piles, and folding one shouldn't fold the other.
    ///
    /// `None` means never touched, which is not the same as "everything is
    /// open": the quiet bands start folded. Once the human folds anything the
    /// set becomes authoritative, so unfolding everything stays unfolded
    /// instead of springing back to the defaults.
    #[serde(default)]
    pub collapsed: Option<BTreeSet<String>>,
}

/// Collapse key for a prefix group inside a section.
pub fn group_key(section: &str, prefix: &str) -> String {
    format!("{section}/{}", prefix.to_ascii_lowercase())
}

pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("seance/subscriptions.json");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.config/seance/subscriptions.json").as_ref())
}

/// `None` = no cached list yet → migrate (Attach with `subscriptions: None`
/// so the daemon seeds everything, then adopt what it sends).
pub fn load() -> Option<SubscriptionsPref> {
    let bytes = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&bytes).ok()
}

/// Decode a blob handed over by the daemon (`SubsLoad`, `RailPrefs`).
/// `None` when it isn't readable as an arrangement — the caller keeps what it
/// has rather than blanking a rail over one bad byte.
pub fn parse(json: &str) -> Option<SubscriptionsPref> {
    serde_json::from_str(json).ok()
}

/// Encode for the daemon. `None` only on a serializer failure, which cannot
/// happen for this shape — the caller simply skips the push.
pub fn encode(pref: &SubscriptionsPref) -> Option<String> {
    serde_json::to_string_pretty(pref).ok()
}

pub fn save(pref: &SubscriptionsPref) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(json) = serde_json::to_string_pretty(pref) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

impl SubscriptionsPref {
    /// Is this rail node folded?
    pub fn is_collapsed(&self, key: &str) -> bool {
        match &self.collapsed {
            // Never touched: sleeping and parked are piles you open on
            // purpose; the rest of the rail is what you're working in.
            None => matches!(key, "sleeping" | "parked"),
            Some(set) => set.contains(key),
        }
    }

    /// Unfold `key` if it is folded. Reports whether anything changed, so the
    /// caller only persists on a real edit.
    ///
    /// Used when the rail has to *reveal* a row — jumping to a circle inside a
    /// folded band can't just scroll to it, because the row isn't drawn at all.
    pub fn uncollapse(&mut self, key: &str) -> bool {
        if !self.is_collapsed(key) {
            return false;
        }
        self.toggle_collapsed(key);
        true
    }

    /// Fold / unfold. Always reports `true` — the caller persists.
    pub fn toggle_collapsed(&mut self, key: &str) -> bool {
        let set = self.collapsed.get_or_insert_with(|| {
            // Materialise the defaults on first touch so they are a starting
            // point rather than a rule that keeps reasserting itself.
            ["sleeping", "parked"]
                .iter()
                .map(|k| k.to_string())
                .collect()
        });
        if !set.remove(key) {
            set.insert(key.to_string());
        }
        true
    }

    /// Drop group keys whose prefix no longer exists in that section, so a
    /// cluster you renamed away doesn't leave a fold behind that surprises you
    /// when the name comes back. Section keys are never pruned.
    pub fn prune_collapsed(&mut self, live_groups: &BTreeSet<String>) -> bool {
        let Some(set) = self.collapsed.as_mut() else {
            return false;
        };
        let before = set.len();
        set.retain(|k| !k.contains('/') || live_groups.contains(k));
        before != set.len()
    }
    /// First `State` after a migration Attach: adopt the daemon's seed as the
    /// active list, and treat everything currently known as already seen (a
    /// pre-existing circle isn't "new since you last looked").
    pub fn seed_from_daemon(&mut self, subscriptions: &[String], known: &BTreeSet<String>) {
        self.active = subscriptions.iter().cloned().collect();
        self.seen.extend(known.iter().cloned());
    }

    /// Fold the daemon's subscription set into the active list. The daemon
    /// auto-subscribes on select / spawn / create / fork from *this* window,
    /// so "the daemon subscribed it" is exactly the auto-add rule — except for
    /// workspaces we just parked, whose `Unsubscribe` may not have been
    /// processed when this `State` was composed.
    ///
    /// Returns true when the persisted set changed.
    pub fn adopt_daemon_subscriptions(
        &mut self,
        subscriptions: &[String],
        park_pending: &BTreeSet<String>,
    ) -> bool {
        let mut changed = false;
        for ws in subscriptions {
            if park_pending.contains(ws) {
                continue;
            }
            changed |= self.activate(ws);
        }
        changed
    }

    /// Add to active (and mark seen). Returns true when something changed.
    pub fn activate(&mut self, ws: &str) -> bool {
        let mut changed = self.active.insert(ws.to_string());
        changed |= self.seen.insert(ws.to_string());
        changed
    }

    /// Remove from active, unpinning first — parked is below the divider by
    /// definition. Stays in `seen`: you've looked at it before, so it must not
    /// badge `needs` merely for being parked.
    pub fn park(&mut self, ws: &str) -> bool {
        let mut changed = self.pinned.remove(ws);
        changed |= self.active.remove(ws);
        changed
    }

    /// Pin to the top section. Pinning implies active + seen (a pinned circle
    /// the human can't see would be a lie), so a parked circle activates here.
    pub fn pin(&mut self, ws: &str) -> bool {
        let mut changed = self.activate(ws);
        changed |= self.pinned.insert(ws.to_string());
        changed
    }

    /// Unpin. The circle stays active — it just falls back below the divider.
    pub fn unpin(&mut self, ws: &str) -> bool {
        self.pinned.remove(ws)
    }

    pub fn is_pinned(&self, ws: &str) -> bool {
        self.pinned.contains(ws)
    }

    /// Drop names the daemon no longer knows about (killed / renamed circles),
    /// so the file doesn't accrete forever. Returns true when it changed.
    pub fn prune(&mut self, known: &BTreeSet<String>) -> bool {
        let before = (self.active.len(), self.seen.len(), self.pinned.len());
        self.active.retain(|w| known.contains(w));
        self.seen.retain(|w| known.contains(w));
        self.pinned.retain(|w| known.contains(w));
        before != (self.active.len(), self.seen.len(), self.pinned.len())
    }

    /// A parked workspace this GUI has never had in its active list — badge
    /// `needs` until first selected.
    pub fn never_seen(&self, ws: &str) -> bool {
        !self.seen.contains(ws)
    }
}

/// Split a display-ordered workspace list into (active, parked), preserving
/// the caller's sort in both bands.
pub fn partition(ordered: &[String], active: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let mut act = Vec::new();
    let mut parked = Vec::new();
    for ws in ordered {
        if active.contains(ws) {
            act.push(ws.clone());
        } else {
            parked.push(ws.clone());
        }
    }
    (act, parked)
}

/// Split a display-ordered workspace list into (pinned, active-unpinned,
/// parked), preserving the caller's sort inside every band. Pinning implies
/// active, so a pinned name is claimed by the first band regardless of what
#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pref_roundtrips_as_arrays() {
        let mut pref = SubscriptionsPref::default();
        pref.activate("lab");
        pref.activate("cadence");
        let json = serde_json::to_string(&pref).unwrap();
        assert!(json.contains(r#""active":["cadence","lab"]"#), "{json}");
        let back: SubscriptionsPref = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pref);
    }

    /// The wire codec the daemon hands back over `SubsLoad` / `RailPrefs`.
    /// Every band has to survive the trip — a pin that arrives as an ordinary
    /// active circle is the bug this guards.
    #[test]
    fn daemon_blob_roundtrips_every_band() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("growth");
        pref.activate("lab");
        pref.park("old");
        pref.toggle_collapsed("active/mtg");
        let back = parse(&encode(&pref).unwrap()).unwrap();
        assert_eq!(back, pref);
        assert!(back.is_pinned("growth"));
        assert!(back.is_collapsed("active/mtg"));
    }

    /// A blob we can't read means "keep the rail you have". Returning a
    /// default here would blank every circle out of the sidebar on one bad
    /// byte, which is worse than ignoring the push.
    #[test]
    fn unreadable_daemon_blob_is_none_not_default() {
        assert!(parse("").is_none());
        assert!(parse("not json").is_none());
        assert!(parse("[1,2,3]").is_none());
    }

    /// A daemon that predates the move sends nothing; the arrangement a
    /// window already cached locally has to remain usable as the seed.
    #[test]
    fn pre_move_local_cache_still_parses_as_a_daemon_blob() {
        let blob = r#"{"active":["lab"],"seen":["lab"],"pinned":["lab"]}"#;
        let back = parse(blob).unwrap();
        assert_eq!(back.active, set(&["lab"]));
        assert!(back.is_pinned("lab"));
    }

    #[test]
    fn missing_fields_default_empty() {
        let back: SubscriptionsPref = serde_json::from_str("{}").unwrap();
        assert!(back.active.is_empty());
        assert!(back.seen.is_empty());
        assert!(back.pinned.is_empty());
    }

    /// Back-compat: a file written before pins existed still parses, with an
    /// empty pinned set (nothing jumps to the top on upgrade).
    #[test]
    fn pre_pin_file_parses_with_empty_pinned() {
        let back: SubscriptionsPref =
            serde_json::from_str(r#"{"active":["lab"],"seen":["lab","old"]}"#).unwrap();
        assert_eq!(back.active, set(&["lab"]));
        assert_eq!(back.seen, set(&["lab", "old"]));
        assert!(back.pinned.is_empty());
        assert!(!back.is_pinned("lab"));
    }

    #[test]
    fn pinned_roundtrips() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        let json = serde_json::to_string(&pref).unwrap();
        assert!(json.contains(r#""pinned":["lab"]"#), "{json}");
        let back: SubscriptionsPref = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pref);
    }

    /// Pinning a parked circle also activates it (and marks it seen) — a
    /// pinned row is always rendered, so it must be subscribed.
    #[test]
    fn pin_implies_active_and_seen() {
        let mut pref = SubscriptionsPref::default();
        assert!(pref.pin("lab"));
        assert!(pref.is_pinned("lab"));
        assert!(pref.active.contains("lab"));
        assert!(!pref.never_seen("lab"));
        assert!(!pref.pin("lab"), "re-pinning is a no-op");
    }

    #[test]
    fn unpin_keeps_it_active() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        assert!(pref.unpin("lab"));
        assert!(!pref.is_pinned("lab"));
        assert!(pref.active.contains("lab"));
        assert!(!pref.unpin("lab"));
    }

    /// Parking a pinned circle unpins it first — nothing below the divider is
    /// allowed to stay in the pinned section.
    #[test]
    fn park_unpins() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        assert!(pref.park("lab"));
        assert!(!pref.is_pinned("lab"));
        assert!(!pref.active.contains("lab"));
        assert!(!pref.never_seen("lab"));
    }

    #[test]
    fn prune_drops_dead_pins() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        pref.pin("gone");
        assert!(pref.prune(&set(&["lab"])));
        assert_eq!(pref.pinned, set(&["lab"]));
        assert_eq!(pref.active, set(&["lab"]));
        assert!(!pref.prune(&set(&["lab"])));
    }

    /// Collapse defaults: untouched means the quiet bands are folded, and the
    /// first fold makes the set authoritative so unfolding everything stays
    /// unfolded instead of springing back.
    #[test]
    fn collapse_defaults_then_become_authoritative() {
        let mut pref = SubscriptionsPref::default();
        assert!(!pref.is_collapsed("active"));
        assert!(pref.is_collapsed("sleeping"));
        assert!(pref.is_collapsed("parked"));

        pref.toggle_collapsed("parked");
        assert!(!pref.is_collapsed("parked"), "unfolded on purpose");
        assert!(pref.is_collapsed("sleeping"), "the other default survived");

        pref.toggle_collapsed("sleeping");
        assert!(!pref.is_collapsed("sleeping"));
        assert!(!pref.is_collapsed("parked"), "does not spring back");
    }

    /// A cluster fold is per section: folding `mtg` under active leaves the
    /// slept `mtg` circles alone.
    #[test]
    fn group_folds_are_scoped_to_their_section() {
        let mut pref = SubscriptionsPref::default();
        let a = group_key("active", "mtg");
        let s = group_key("sleeping", "MTG");
        pref.toggle_collapsed(&a);
        assert!(pref.is_collapsed(&a));
        assert!(!pref.is_collapsed(&s));
        assert_eq!(s, "sleeping/mtg", "prefix key is case-insensitive");
    }

    /// A cluster that no longer exists takes its fold with it; band folds stay.
    #[test]
    fn prune_drops_dead_group_folds_only() {
        let mut pref = SubscriptionsPref::default();
        pref.toggle_collapsed("active");
        pref.toggle_collapsed(&group_key("active", "mtg"));
        pref.toggle_collapsed(&group_key("active", "gone"));
        assert!(pref.prune_collapsed(&set(&["active/mtg"])));
        assert!(pref.is_collapsed("active/mtg"));
        assert!(!pref.is_collapsed("active/gone"));
        assert!(pref.is_collapsed("active"), "band folds are never pruned");
    }

    /// First run: no file → daemon subscribes everything → adopt it, and
    /// nothing badges `needs` because everything is marked seen.
    #[test]
    fn seed_adopts_daemon_set_and_marks_everything_seen() {
        let known = set(&["lab", "cadence", "notes"]);
        let mut pref = SubscriptionsPref::default();
        pref.seed_from_daemon(&["lab".into(), "cadence".into(), "notes".into()], &known);
        assert_eq!(pref.active, known);
        assert!(!pref.never_seen("notes"));
    }

    #[test]
    fn partition_preserves_sort_in_both_bands() {
        let ordered: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let (act, parked) = partition(&ordered, &set(&["c", "a"]));
        assert_eq!(act, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(parked, vec!["b".to_string(), "d".to_string()]);
    }

    #[test]
    fn daemon_subscriptions_auto_add_except_just_parked() {
        let mut pref = SubscriptionsPref::default();
        pref.activate("lab");
        pref.park("lab");
        // Daemon still lists `lab` (Unsubscribe in flight) plus a fresh spawn.
        let changed =
            pref.adopt_daemon_subscriptions(&["lab".into(), "spawned".into()], &set(&["lab"]));
        assert!(changed);
        assert_eq!(pref.active, set(&["spawned"]));
    }

    /// Parking is not forgetting: a parked circle must not re-badge `needs`.
    #[test]
    fn park_keeps_seen() {
        let mut pref = SubscriptionsPref::default();
        pref.activate("lab");
        pref.park("lab");
        assert!(!pref.active.contains("lab"));
        assert!(!pref.never_seen("lab"));
        assert!(pref.never_seen("ctl-spawned"));
    }

    #[test]
    fn prune_drops_dead_workspaces() {
        let mut pref = SubscriptionsPref::default();
        pref.activate("lab");
        pref.activate("gone");
        assert!(pref.prune(&set(&["lab"])));
        assert_eq!(pref.active, set(&["lab"]));
        assert_eq!(pref.seen, set(&["lab"]));
        assert!(!pref.prune(&set(&["lab"])));
    }
}
