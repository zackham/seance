//! Small pure helpers shared by daemon and clients.

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
