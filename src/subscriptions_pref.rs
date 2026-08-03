//! Per-GUI active/parked presentation state (subscriptions phase 2).
//!
//! The daemon owns the canonical workspace list and this connection's
//! subscription set; *which* of those the human wants in the sidebar's active
//! band is presentation state belonging to this GUI alone. It is persisted at
//! `~/.config/seance/subscriptions.json` (or `$XDG_CONFIG_HOME/seance/`) —
//! same local-config seam as `launch.json`, deliberately outside the
//! thin-client fs-bridge invariant (it affects nothing but this window's
//! chrome, and it must be readable *before* a daemon connection exists so the
//! `Attach` seed can carry it).
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
}

pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("seance/subscriptions.json");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.config/seance/subscriptions.json").as_ref())
}

/// `None` = no persisted list yet → migrate (Attach with `subscriptions: None`
/// so the daemon seeds everything, then adopt what it sends).
pub fn load() -> Option<SubscriptionsPref> {
    let bytes = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&bytes).ok()
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

    /// Carry every membership across a workspace rename (the daemon renames in
    /// place; to this file it would otherwise look like a kill plus a birth).
    pub fn rename(&mut self, old: &str, new: &str) -> bool {
        fn swap(set: &mut BTreeSet<String>, old: &str, new: &str) -> bool {
            if set.remove(old) {
                set.insert(new.to_string());
                true
            } else {
                false
            }
        }
        let mut changed = swap(&mut self.active, old, new);
        changed |= swap(&mut self.seen, old, new);
        changed |= swap(&mut self.pinned, old, new);
        changed
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
/// `active` says (defensive against a hand-edited config file).
pub fn partition3(
    ordered: &[String],
    active: &BTreeSet<String>,
    pinned: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut pin = Vec::new();
    let mut act = Vec::new();
    let mut parked = Vec::new();
    for ws in ordered {
        if pinned.contains(ws) {
            pin.push(ws.clone());
        } else if active.contains(ws) {
            act.push(ws.clone());
        } else {
            parked.push(ws.clone());
        }
    }
    (pin, act, parked)
}

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

    /// Rename carries the pin (kill drops it — that's `prune`).
    #[test]
    fn rename_carries_pin() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        assert!(pref.rename("lab", "workshop"));
        assert_eq!(pref.pinned, set(&["workshop"]));
        assert_eq!(pref.active, set(&["workshop"]));
        assert_eq!(pref.seen, set(&["workshop"]));
        assert!(!pref.rename("nobody", "nothing"));
    }

    #[test]
    fn partition3_preserves_sort_and_floats_pins() {
        let ordered: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let (pin, act, parked) = partition3(&ordered, &set(&["a", "b", "c"]), &set(&["c", "a"]));
        assert_eq!(pin, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(act, vec!["b".to_string()]);
        assert_eq!(parked, vec!["d".to_string()]);
    }

    /// A pinned name missing from `active` (hand-edited file) still renders
    /// pinned rather than falling into the parked accordion.
    #[test]
    fn partition3_claims_pinned_even_if_not_active() {
        let ordered: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let (pin, act, parked) = partition3(&ordered, &set(&["b"]), &set(&["a"]));
        assert_eq!(pin, vec!["a".to_string()]);
        assert_eq!(act, vec!["b".to_string()]);
        assert!(parked.is_empty());
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
