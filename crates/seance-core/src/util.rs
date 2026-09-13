//! Small pure helpers shared by daemon and clients.

use std::collections::{BTreeSet, HashMap};

/// Lowercases, keeps ASCII alphanumerics, maps every other run of characters to
/// a single `-`, trims leading/trailing `-`, and falls back to `"session"` when
/// nothing usable remains.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            // Collapse any run of non-alnum (incl. existing dashes) into one dash.
            out.push('-');
            prev_dash = true;
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Claude Code / ink TUIs put an animated spinner at the head of the OSC title
/// while working. Idle Claude uses `✳` (U+2733) — that is *not* busy.
///
/// Two frame families, because the spinner is not one fixed set:
/// * braille `U+2800..=U+28FF` — the classic ink spinner,
/// * circle quadrants `U+25D0..=U+25D3` — what Claude Code emits today.
///
/// The quadrant family was missing until 2026-08-31, and its absence was
/// silent in the worst way: the daemon reported "not busy" for panes that were
/// visibly working, so every working circle fell into the idle band and its
/// row churned against the activity clock. Sampled live over ~6s across a
/// dozen panes, working ones alternated `◐`/`◑` and idle ones sat on `✳`; the
/// other two quadrants complete that rotation. **If a working agent stops
/// reading as busy, suspect this list before anything else** — a spinner
/// change upstream lands here as a lie, not as an error.
///
/// Lives here because the **daemon** is the authority on busy: it sees every
/// title change, while a client only receives grid frames for the workspace
/// it has selected. Both GUIs consume the daemon's verdict rather than
/// re-deriving one from a title that may be hours stale.
pub fn title_looks_busy(title: &str) -> bool {
    matches!(
        title.trim_start().chars().next(),
        Some('\u{2800}'..='\u{28FF}') | Some('\u{25D0}'..='\u{25D3}')
    )
}

/// Age bucketed for ORDERING circles in the rail — coarser than the label the
/// row displays, on purpose.
///
/// Everything inside the last TEN MINUTES ties. An agent TUI repaints on its
/// own timer and then pauses, so its age sweeps up and back down across any
/// edge you pick; each crossing reshuffles the list. Measured on the live
/// rail: per-second ranking reordered 4x in 24s, per-minute still reordered
/// because circles kept sweeping the 60s edge. The bucket has to be far wider
/// than the jitter, and "roughly how long ago" at ten-minute resolution is all
/// an ordering needs — `rel_label` keeps showing the precise age, which is a
/// fine thing to READ and a terrible thing to sort on.
///
/// Ties fall back to name, so a tied group renders in a fixed order. Monotonic:
/// fresher always ranks first.
///
/// Shared so the native and web rails cannot drift, same as `title_looks_busy`.
pub fn recency_rank(age_ms: u64) -> u64 {
    let s = age_ms / 1000;
    match s {
        0..=599 => 0,
        600..=3599 => 1 + s / 600,
        3600..=86_399 => 10 + s / 3600,
        _ => 100 + s / 86_400,
    }
}

/// Slugify `name`, then disambiguate against already-taken slugs.
///
/// On collision, appends `-2`, `-3`, ... until the result is unused. `taken` is
/// the set of slugs already in play (compared case-sensitively against the
/// lowercase slug output).
pub fn unique_slug(name: &str, taken: &[&str]) -> String {
    let base = slugify(name);
    if !taken.contains(&base.as_str()) {
        return base;
    }

    let mut n = 2u64;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

/// How long a name the daemon has stopped mentioning is shielded from `prune`
/// before it is dropped.
///
/// The asymmetry is the whole point: keeping a dead name costs one string in a
/// json file, dropping a live one destroys something he asked for. Two ways a
/// name goes briefly missing — a quicklaunch pin lands before its spawn round
/// trip completes, and any window's `State` can lag another window's fresh
/// circle. Both resolve in well under a minute.
pub const ABSENT_GRACE_MS: u64 = 60_000;

/// Track how long each name the arrangement references has been missing from
/// the daemon's world, and return the set `prune` may keep: what the daemon
/// knows, plus anything missing for less than [`ABSENT_GRACE_MS`].
///
/// Pruning straight against `known` is destructive on a view that merely lags:
/// a quicklaunch pin placed before its spawn lands, or a circle another window
/// created a moment ago, both look identical to "killed" for one `State`.
///
/// Shared by both clients — the web rail hit the identical bug, since the same
/// pin-then-spawn ordering produces the same one-`State` gap there.
pub fn settle_absent<'a>(
    absent: &mut HashMap<String, u64>,
    referenced: impl Iterator<Item = &'a str>,
    known: &BTreeSet<String>,
    now: u64,
) -> BTreeSet<String> {
    for name in referenced {
        if !known.contains(name) {
            absent.entry(name.to_string()).or_insert(now);
        }
    }
    // Back in the world → forget it ever went missing.
    absent.retain(|name, _| !known.contains(name));
    let mut protected = known.clone();
    protected.extend(
        absent
            .iter()
            .filter(|(_, since)| now.saturating_sub(**since) < ABSENT_GRACE_MS)
            .map(|(name, _)| name.clone()),
    );
    protected
}

/// Is a broadcast rail arrangement someone else's, or our own echo?
///
/// `pending > 0` means a write of ours is queued or in flight, so nothing the
/// daemon is broadcasting right now can reflect our newest state. Otherwise it
/// is ours exactly when it matches what we last sent.
pub fn rail_prefs_is_foreign(pending: usize, last_sent: Option<&str>, json: &str) -> bool {
    pending == 0 && last_sent != Some(json)
}

#[cfg(test)]
mod busy_title_tests {
    use super::title_looks_busy;

    /// The regression: Claude Code's spinner is circle quadrants now, and
    /// matching only braille reported every working agent as idle.
    #[test]
    fn the_circle_quadrant_spinner_reads_as_busy() {
        assert!(title_looks_busy("◐ Value per account churn adjustment"));
        assert!(title_looks_busy("◑ Ad monitoring integration into Cadence"));
        assert!(title_looks_busy("◒ working"));
        assert!(title_looks_busy("◓ working"));
    }

    #[test]
    fn the_braille_spinner_still_reads_as_busy() {
        assert!(title_looks_busy("⠂ Understanding parked variables"));
        assert!(title_looks_busy("⠐ doing a thing"));
        // Leading whitespace is trimmed before the check.
        assert!(title_looks_busy("  ⠋ indented"));
    }

    /// `✳` is idle Claude waiting on you. Calling it busy would park every
    /// finished circle in the working band, which is the inverse failure.
    #[test]
    fn idle_claude_and_plain_titles_are_not_busy() {
        assert!(!title_looks_busy("✳ Review support drafts application"));
        assert!(!title_looks_busy("zsh"));
        assert!(!title_looks_busy(""));
        assert!(!title_looks_busy("~/work/seance"));
    }
}

#[cfg(test)]
mod rail_tests {
    use super::*;

    fn known(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The quicklaunch bug: the pin lands before the spawn round trip does, so
    /// the next `State` carries no such circle.
    #[test]
    fn a_name_the_daemon_has_not_mentioned_yet_survives() {
        let mut absent = HashMap::new();
        let p = settle_absent(
            &mut absent,
            ["staff-report"].into_iter(),
            &known(&["lab"]),
            1_000,
        );
        assert!(p.contains("staff-report"), "a fresh pin must not be pruned");
        assert_eq!(absent.get("staff-report"), Some(&1_000));
    }

    /// Still missing a full grace period later — now it really is gone.
    #[test]
    fn a_name_missing_past_the_grace_is_dropped() {
        let mut absent = HashMap::new();
        settle_absent(&mut absent, ["gone"].into_iter(), &known(&["lab"]), 1_000);
        let p = settle_absent(
            &mut absent,
            ["gone"].into_iter(),
            &known(&["lab"]),
            1_000 + ABSENT_GRACE_MS,
        );
        assert!(!p.contains("gone"));
    }

    /// The clock starts at FIRST absence and doesn't restart on every `State`,
    /// or a killed circle would be shielded forever.
    #[test]
    fn the_absence_clock_does_not_restart_each_state() {
        let mut absent = HashMap::new();
        for t in [1_000, 2_000, 3_000] {
            settle_absent(&mut absent, ["gone"].into_iter(), &known(&[]), t);
        }
        assert_eq!(absent.get("gone"), Some(&1_000), "first sighting wins");
    }

    /// A circle that shows up resets: a later disappearance gets its own full
    /// grace rather than inheriting a stale clock.
    #[test]
    fn reappearing_clears_the_absence() {
        let mut absent = HashMap::new();
        settle_absent(&mut absent, ["ws"].into_iter(), &known(&[]), 1_000);
        settle_absent(&mut absent, ["ws"].into_iter(), &known(&["ws"]), 2_000);
        assert!(absent.is_empty(), "back in the world");
        let p = settle_absent(&mut absent, ["ws"].into_iter(), &known(&[]), 3_000);
        assert!(p.contains("ws"));
        assert_eq!(absent.get("ws"), Some(&3_000));
    }

    /// The daemon echoes every write back to its sender. Adopting that echo is
    /// how a pin undid itself: the window replaced fresh local state with an
    /// older copy of it.
    #[test]
    fn our_own_echo_is_never_adopted() {
        let mine = r#"{"pinned":["lab"]}"#;
        assert!(!rail_prefs_is_foreign(0, Some(mine), mine));
    }

    /// A real change from another window still lands.
    #[test]
    fn another_windows_arrangement_is_adopted() {
        assert!(rail_prefs_is_foreign(
            0,
            Some(r#"{"pinned":["lab"]}"#),
            r#"{"pinned":["lab","raid"]}"#
        ));
        // Nothing sent yet — anything inbound is foreign by definition.
        assert!(rail_prefs_is_foreign(0, None, r#"{"pinned":[]}"#));
    }

    /// While a write of ours is queued or in flight, ANY broadcast is stale by
    /// construction — including one that happens to differ from what we sent.
    #[test]
    fn nothing_is_adopted_while_our_write_is_in_flight() {
        assert!(!rail_prefs_is_foreign(1, Some("a"), "b"));
        assert!(!rail_prefs_is_foreign(3, None, "b"));
    }
}
