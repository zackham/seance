//! Pinned/seen/folds rail arrangement — the shape of this module, not its
//! storage. **The daemon owns the arrangement** (`FsOp::SubsLoad/SubsSave`,
//! beside `layout.json` in its state dir), so the circles you keep in front of
//! you follow you to any window: desk, mac thin client, browser. Changing it
//! anywhere pushes [`GuiEvent::RailPrefs`] to every other window.
//!
//! `~/.config/seance/subscriptions.json` survives as a **local seed cache**,
//! not the source of truth — the daemon's copy is read straight after
//! connecting and wins any disagreement.
//!
//! Was per-GUI local state through 0.22 (0.23 CHANGELOG). The `active` band
//! and its park verb were removed in 0.26: every window subscribes to every
//! circle, so an `active` key in an older blob parses and is discarded.
//!
//! Shape: `{ "seen": [...], "pinned": [...], "collapsed": [...],
//! "flipped": "slug" }`.
//! - `seen` — every workspace this GUI has selected at least once (plus
//!   everything that already existed on first run). One that appears without
//!   ever being selected badges `needs` — that's a ctl-spawned circle you
//!   haven't looked at.
//! - `pinned` — rendered in its own section at the very top of the rail, above
//!   a divider. Same internal sort as every other band.
//! - `collapsed` — folded rail nodes; this is the whole organization story now
//!   that park is gone. Fold a band or a prefix cluster to get it out of the
//!   way.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionsPref {
    #[serde(default)]
    pub seen: BTreeSet<String>,
    /// Circles pinned to the top section of the rail. `default` so pre-pin
    /// files still parse.
    #[serde(default)]
    pub pinned: BTreeSet<String>,
    /// Folded prefix clusters, keyed `"<band>/<prefix>"`. Scoped per band by
    /// design: folding the pinned `mtg` cluster leaves the unpinned one open.
    ///
    /// Bands themselves are no longer foldable — 0.26 dropped the band headers
    /// that were the only affordance, so a bare key here is dead weight and
    /// `prune_collapsed` sweeps it. Kept `Option` for the blob shape.
    #[serde(default)]
    pub collapsed: Option<BTreeSet<String>>,
    /// Pane showing its notes face instead of its terminal. Part of the
    /// arrangement for the same reason the rest of it is: the face you left up
    /// is what you expect to find when the app comes back, and on the other
    /// machine too. One at a time, matching the app model.
    #[serde(default)]
    pub flipped: Option<String>,
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
    /// Is this cluster folded?
    pub fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.as_ref().is_some_and(|s| s.contains(key))
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
        let set = self.collapsed.get_or_insert_with(BTreeSet::new);
        if !set.remove(key) {
            set.insert(key.to_string());
        }
        true
    }

    /// Drop folds for clusters that no longer exist, so a cluster you renamed
    /// away doesn't leave a fold behind that surprises you when the name comes
    /// back. Bare band keys (`active`, and `parked`/`sleeping` from older
    /// blobs) name nothing foldable now and go the same way.
    pub fn prune_collapsed(&mut self, live_groups: &BTreeSet<String>) -> bool {
        let Some(set) = self.collapsed.as_mut() else {
            return false;
        };
        let before = set.len();
        set.retain(|k| live_groups.contains(k));
        before != set.len()
    }
    /// First run: everything that already exists counts as looked-at, so an
    /// upgrade doesn't badge the whole rail `needs`.
    pub fn seed_seen(&mut self, known: &BTreeSet<String>) {
        self.seen.extend(known.iter().cloned());
    }

    /// Record that the human has looked at this circle. Returns true when
    /// something changed.
    pub fn mark_seen(&mut self, ws: &str) -> bool {
        self.seen.insert(ws.to_string())
    }

    /// Pin to the top section. Implies seen — you can't pin what you haven't
    /// looked at.
    pub fn pin(&mut self, ws: &str) -> bool {
        let mut changed = self.mark_seen(ws);
        changed |= self.pinned.insert(ws.to_string());
        changed
    }

    /// Unpin. The circle falls back below the divider into its lifecycle band.
    pub fn unpin(&mut self, ws: &str) -> bool {
        self.pinned.remove(ws)
    }

    pub fn is_pinned(&self, ws: &str) -> bool {
        self.pinned.contains(ws)
    }


    /// Drop names the daemon no longer knows about (killed / renamed circles),
    /// so the file doesn't accrete forever. Returns true when it changed.
    pub fn prune(&mut self, known: &BTreeSet<String>) -> bool {
        let before = (self.seen.len(), self.pinned.len());
        self.seen.retain(|w| known.contains(w));
        self.pinned.retain(|w| known.contains(w));
        before != (self.seen.len(), self.pinned.len())
    }

    /// Never selected in this GUI — badge `needs` until it is.
    pub fn never_seen(&self, ws: &str) -> bool {
        !self.seen.contains(ws)
    }
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
        pref.mark_seen("lab");
        pref.mark_seen("cadence");
        let json = serde_json::to_string(&pref).unwrap();
        assert!(json.contains(r#""seen":["cadence","lab"]"#), "{json}");
        let back: SubscriptionsPref = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pref);
    }

    /// The wire codec the daemon hands back over `SubsLoad` / `RailPrefs`.
    /// Every band has to survive the trip — a pin that arrives as an ordinary
    /// circle is the bug this guards.
    #[test]
    fn daemon_blob_roundtrips_every_band() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("growth");
        pref.mark_seen("lab");
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

    /// A pre-0.26 blob still carries `active`. Serde drops the unknown key, so
    /// upgrading reads the pins and folds and quietly forgets the band — the
    /// alternative (a parse failure) would blank the rail on first launch.
    #[test]
    fn a_pre_park_removal_blob_drops_the_active_key() {
        let blob = r#"{"active":["lab"],"seen":["lab","old"],"pinned":["lab"]}"#;
        let back = parse(blob).unwrap();
        assert_eq!(back.seen, set(&["lab", "old"]));
        assert!(back.is_pinned("lab"));
    }

    #[test]
    fn missing_fields_default_empty() {
        let back: SubscriptionsPref = serde_json::from_str("{}").unwrap();
        assert!(back.seen.is_empty());
        assert!(back.pinned.is_empty());
        assert!(back.flipped.is_none());
    }

    /// The notes face rides the same blob as the bands — a flip that doesn't
    /// survive the trip is a face that closes itself on restart.
    #[test]
    fn the_notes_face_survives_the_daemon_blob() {
        let mut pref = SubscriptionsPref::default();
        pref.mark_seen("lab");
        pref.flipped = Some("claude-7".into());
        let back = parse(&encode(&pref).unwrap()).unwrap();
        assert_eq!(back, pref);
        assert_eq!(back.flipped.as_deref(), Some("claude-7"));
    }

    /// A blob written before the field existed reads as "no face up", not as
    /// a parse failure that would blank the whole rail.
    #[test]
    fn a_pre_flip_blob_still_parses() {
        let back = parse(r#"{"seen":["lab"],"pinned":[]}"#).unwrap();
        assert_eq!(back.seen, set(&["lab"]));
        assert!(back.flipped.is_none());
    }

    /// Back-compat: a file written before pins existed still parses, with an
    /// empty pinned set (nothing jumps to the top on upgrade).
    #[test]
    fn pre_pin_file_parses_with_empty_pinned() {
        let back: SubscriptionsPref =
            serde_json::from_str(r#"{"seen":["lab","old"]}"#).unwrap();
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

    #[test]
    fn pin_implies_seen() {
        let mut pref = SubscriptionsPref::default();
        assert!(pref.pin("lab"));
        assert!(pref.is_pinned("lab"));
        assert!(!pref.never_seen("lab"));
        assert!(!pref.pin("lab"), "re-pinning is a no-op");
    }

    #[test]
    fn unpin_keeps_it_seen() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        assert!(pref.unpin("lab"));
        assert!(!pref.is_pinned("lab"));
        assert!(!pref.never_seen("lab"));
        assert!(!pref.unpin("lab"));
    }

    #[test]
    fn prune_drops_dead_pins() {
        let mut pref = SubscriptionsPref::default();
        pref.pin("lab");
        pref.pin("gone");
        assert!(pref.prune(&set(&["lab"])));
        assert_eq!(pref.pinned, set(&["lab"]));
        assert_eq!(pref.seen, set(&["lab"]));
        assert!(!pref.prune(&set(&["lab"])));
    }

    /// Nothing is folded until the human folds it — there are no band
    /// defaults left to spring back to.
    #[test]
    fn nothing_is_folded_by_default() {
        let mut pref = SubscriptionsPref::default();
        let k = group_key("active", "mtg");
        assert!(!pref.is_collapsed(&k));
        pref.toggle_collapsed(&k);
        assert!(pref.is_collapsed(&k));
        pref.toggle_collapsed(&k);
        assert!(!pref.is_collapsed(&k));
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

    #[test]
    fn prune_drops_dead_group_folds() {
        let mut pref = SubscriptionsPref::default();
        pref.toggle_collapsed(&group_key("active", "mtg"));
        pref.toggle_collapsed(&group_key("active", "gone"));
        assert!(pref.prune_collapsed(&set(&["active/mtg"])));
        assert!(pref.is_collapsed("active/mtg"));
        assert!(!pref.is_collapsed("active/gone"));
    }

    /// Older blobs carry bare band keys (`parked`, `sleeping`, `active`) that
    /// name nothing foldable now — sweep them instead of carrying them.
    #[test]
    fn prune_sweeps_bare_band_folds() {
        let mut pref = parse(r#"{"collapsed":["parked","sleeping","active/mtg"]}"#).unwrap();
        assert!(pref.prune_collapsed(&set(&["active/mtg"])));
        assert!(!pref.is_collapsed("parked"));
        assert!(!pref.is_collapsed("sleeping"));
        assert!(pref.is_collapsed("active/mtg"), "live clusters survive");
    }

    /// First run: everything that already exists counts as looked-at, so an
    /// upgrade doesn't badge the whole rail.
    #[test]
    fn seed_marks_everything_known_seen() {
        let known = set(&["lab", "cadence", "notes"]);
        let mut pref = SubscriptionsPref::default();
        pref.seed_seen(&known);
        assert!(!pref.never_seen("notes"));
    }

    /// `needs` is now purely "never selected here" — the ctl-spawned circle
    /// you haven't looked at yet.
    #[test]
    fn unselected_circle_is_the_ctl_spawn_case() {
        let mut pref = SubscriptionsPref::default();
        pref.mark_seen("lab");
        assert!(!pref.never_seen("lab"));
        assert!(pref.never_seen("ctl-spawned"));
    }

    #[test]
    fn prune_drops_dead_workspaces() {
        let mut pref = SubscriptionsPref::default();
        pref.mark_seen("lab");
        pref.mark_seen("gone");
        assert!(pref.prune(&set(&["lab"])));
        assert_eq!(pref.seen, set(&["lab"]));
        assert!(!pref.prune(&set(&["lab"])));
    }
}
