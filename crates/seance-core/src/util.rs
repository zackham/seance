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
