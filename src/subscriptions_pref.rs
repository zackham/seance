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
//! Shape: `{ "active": [...], "seen": [...] }`.
//! - `active` — workspaces rendered in the normal sidebar band; everything
//!   else the daemon knows about lands in the collapsed `parked` group.
//! - `seen` — every workspace this GUI has ever had in `active`. A workspace
//!   that appears while parked and was never seen badges `needs` until it is
//!   first selected ("ctl spawns parked+needs").

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionsPref {
    #[serde(default)]
    pub active: BTreeSet<String>,
    #[serde(default)]
    pub seen: BTreeSet<String>,
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

    /// Remove from active. Stays in `seen` — you've looked at it before, so it
    /// must not badge `needs` merely for being parked.
    pub fn park(&mut self, ws: &str) -> bool {
        self.active.remove(ws)
    }

    /// Drop names the daemon no longer knows about (killed / renamed circles),
    /// so the file doesn't accrete forever. Returns true when it changed.
    pub fn prune(&mut self, known: &BTreeSet<String>) -> bool {
        let before = (self.active.len(), self.seen.len());
        self.active.retain(|w| known.contains(w));
        self.seen.retain(|w| known.contains(w));
        before != (self.active.len(), self.seen.len())
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
