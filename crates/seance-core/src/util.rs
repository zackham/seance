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

/// Claude Code / ink TUIs put a braille spinner in the OSC title while
/// streaming. Idle Claude uses `✳` (U+2733) — that is *not* busy.
///
/// Lives here because the **daemon** is the authority on busy: it sees every
/// title change, while a client only receives grid frames for the workspace
/// it has selected. Both GUIs consume the daemon's verdict rather than
/// re-deriving one from a title that may be hours stale.
pub fn title_looks_busy(title: &str) -> bool {
    matches!(
        title.trim_start().chars().next(),
        Some('\u{2800}'..='\u{28FF}')
    )
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
