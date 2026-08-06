//! Markdown section folding for file panes — a pure model over the SOURCE.
//!
//! Seance does not own the markdown block model (`gpui_component`'s TextView
//! parses and virtualizes the document), so folding does not happen inside the
//! renderer. It happens *before* it: a collapsed section's lines are simply not
//! handed over, and every heading is swapped for a `seance-h` fence that
//! `fileview.rs` draws itself with a caret. The renderer sees an ordinary,
//! shorter document. Virtualization, selection, and theming all keep working.
//!
//! # Keys, not line numbers
//!
//! Fold state is keyed by a heading's **path** — its ancestor headings' text
//! plus its own — never by position. File panes watch a live file, and the
//! canonical case is an agent rewriting the document while a human reads it. A
//! line-indexed fold set would silently reopen everything on every write, or
//! worse, fold the wrong section. A path key survives insertions above it,
//! rewording elsewhere, and the section moving; it breaks only when the heading
//! itself is renamed, which is the one case where forgetting is correct.
//!
//! Duplicate paths (the same heading text twice under the same parent) get an
//! occurrence suffix, so they fold independently instead of in lockstep.

use std::collections::BTreeSet;

/// The fence language `fileview.rs` registers a parser + renderer for.
pub const HEAD_FENCE: &str = "seance-h";

/// One heading in a markdown source.
#[derive(Clone, Debug, PartialEq)]
pub struct Heading {
    /// 1–6.
    pub level: u8,
    /// Display text, whitespace-normalized, closing `#`s stripped.
    pub text: String,
    /// Stable path key — what fold state is stored under.
    pub key: String,
    /// 0-based index of the heading's own line.
    pub line: usize,
    /// 0-based index one past the section's last line.
    pub end: usize,
}

impl Heading {
    /// Lines hidden when this section is collapsed (its body, not its heading).
    pub fn body_lines(&self) -> usize {
        self.end.saturating_sub(self.line + 1)
    }
}

/// Whitespace-normalize a heading's text and drop any closing `#`s.
fn normalize(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches('#').trim();
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// ATX heading on this line — `#` to `######`, up to 3 leading spaces, and a
/// space after the hashes. `#hashtag` is not a heading.
fn atx_heading(line: &str) -> Option<(u8, String)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    // "### foo" or a bare "###"; "###foo" is not a heading.
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    Some((hashes as u8, normalize(after)))
}

/// Does this line open or close a fenced code block? Returns the fence char and
/// its length so a shorter inner fence can't close a longer outer one.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    for ch in ['`', '~'] {
        let n = rest.len() - rest.trim_start_matches(ch).len();
        if n >= 3 {
            return Some((ch, n));
        }
    }
    None
}

/// Every heading in `source`, in document order, with spans and path keys.
///
/// Fenced code is skipped: a `# comment` inside a shell block is not a section,
/// and treating it as one would fold the rest of the document into it.
pub fn headings(source: &str) -> Vec<Heading> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found: Vec<(usize, u8, String)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        match fence {
            Some((ch, n)) => {
                if let Some((c2, n2)) = fence_marker(line) {
                    if c2 == ch && n2 >= n {
                        fence = None;
                    }
                }
            }
            None => {
                if let Some(open) = fence_marker(line) {
                    fence = Some(open);
                } else if let Some((level, text)) = atx_heading(line) {
                    found.push((i, level, text));
                }
            }
        }
    }

    // Path keys: ancestors' text plus own, deduped by occurrence.
    let mut out: Vec<Heading> = Vec::with_capacity(found.len());
    let mut path: Vec<(u8, String)> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (line, level, text) in found {
        while path.last().is_some_and(|(l, _)| *l >= level) {
            path.pop();
        }
        let mut key = String::new();
        for (_, t) in &path {
            key.push_str(t);
            key.push('\u{1f}');
        }
        key.push_str(&text);
        let n = seen.entry(key.clone()).or_insert(0);
        if *n > 0 {
            key.push_str(&format!("\u{1f}#{n}"));
        }
        *n += 1;
        path.push((level, text.clone()));
        out.push(Heading {
            level,
            text,
            key,
            line,
            end: 0, // filled below
        });
    }

    // A section runs until the next heading of the same or shallower level.
    let total = lines.len();
    for i in 0..out.len() {
        let level = out[i].level;
        let end = out[(i + 1)..]
            .iter()
            .find(|h| h.level <= level)
            .map(|h| h.line)
            .unwrap_or(total);
        out[i].end = end;
    }
    out
}

/// Every fold key in the document. Used to prune state against a rewrite —
/// deliberately NOT what "collapse all" sets; see [`outline_keys`].
pub fn all_keys(source: &str) -> BTreeSet<String> {
    headings(source).into_iter().map(|h| h.key).collect()
}

/// The keys that fold a document down to its **outline**: every heading still
/// on screen, no prose under any of them.
///
/// These are the leaves — headings with no headings beneath them — not every
/// heading. Collapsing hides a whole subtree, so collapsing *everything* in a
/// document with a single `#` title collapses the document to one line:
/// technically obedient, completely useless. The leaves are the deepest thing
/// you can fold while still being able to see the shape of what you folded.
pub fn outline_keys(source: &str) -> BTreeSet<String> {
    let heads = headings(source);
    heads
        .iter()
        .enumerate()
        .filter(|(i, h)| !heads[(*i + 1)..].iter().any(|c| c.line < h.end))
        .map(|(_, h)| h.key.clone())
        .collect()
}

/// Encode one heading as the fence `fileview.rs` renders itself.
/// Fields are tab-separated; the key can't contain a tab (it is built from
/// whitespace-normalized text), and the text is flattened for safety.
fn head_fence(h: &Heading, collapsed: bool) -> String {
    format!(
        "```{HEAD_FENCE}\n{}\t{}\t{}\t{}\t{}\n```\n",
        h.level,
        u8::from(collapsed),
        h.body_lines(),
        h.key.replace(['\t', '\n', '\r'], " "),
        h.text.replace(['\t', '\n', '\r'], " "),
    )
}

/// One `seance-h` fence, decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct FoldHead {
    pub level: u8,
    pub collapsed: bool,
    pub hidden_lines: usize,
    pub key: String,
    pub text: String,
}

/// Decode a `seance-h` fence body. `None` if it isn't one of ours.
pub fn parse_head_fence(value: &str) -> Option<FoldHead> {
    let line = value.lines().next()?;
    let mut parts = line.splitn(5, '\t');
    let level = parts.next()?.parse::<u8>().ok()?;
    let collapsed = parts.next()? == "1";
    let hidden_lines = parts.next()?.parse::<usize>().unwrap_or(0);
    let key = parts.next()?.to_string();
    let text = parts.next().unwrap_or("").to_string();
    Some(FoldHead {
        level: level.clamp(1, 6),
        collapsed,
        hidden_lines,
        key,
        text,
    })
}

/// The document to hand the renderer: headings swapped for `seance-h` fences,
/// collapsed sections' bodies dropped.
///
/// A heading nested inside a collapsed parent disappears entirely rather than
/// becoming its own fence — collapsing a section means the whole subtree goes,
/// which is the only reading that makes nesting useful.
pub fn fold_source(source: &str, collapsed: &BTreeSet<String>) -> String {
    let heads = headings(source);
    if heads.is_empty() {
        return source.to_string();
    }
    let lines: Vec<&str> = source.lines().collect();
    let mut hidden = vec![false; lines.len()];
    for h in &heads {
        if collapsed.contains(&h.key) {
            for flag in hidden.iter_mut().take(h.end).skip(h.line + 1) {
                *flag = true;
            }
        }
    }
    let head_at: std::collections::HashMap<usize, &Heading> =
        heads.iter().map(|h| (h.line, h)).collect();

    let mut out = String::with_capacity(source.len());
    for (i, line) in lines.iter().enumerate() {
        if hidden[i] {
            continue;
        }
        match head_at.get(&i) {
            Some(h) => out.push_str(&head_fence(h, collapsed.contains(&h.key))),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Drop keys the document no longer has, so the set can't grow forever across a
/// long-lived pane. Headings that merely moved keep their key, and therefore
/// their fold.
pub fn prune(collapsed: &BTreeSet<String>, source: &str) -> BTreeSet<String> {
    let live = all_keys(source);
    collapsed.intersection(&live).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(source: &str) -> Vec<String> {
        headings(source).into_iter().map(|h| h.key).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const DOC: &str = "\
# Title
intro line
## Alpha
alpha body
### Alpha child
child body
## Beta
beta body
";

    #[test]
    fn headings_carry_level_span_and_path() {
        let h = headings(DOC);
        assert_eq!(h.len(), 4);
        assert_eq!((h[0].level, h[0].text.as_str()), (1, "Title"));
        // Title's section runs to EOF; Alpha's stops at Beta.
        assert_eq!(h[0].end, 8);
        assert_eq!(h[1].text, "Alpha");
        assert_eq!(h[1].end, 6);
        assert_eq!(h[1].body_lines(), 3); // body + nested heading + its body
                                          // Path keys nest.
        assert_eq!(h[2].key, "Title\u{1f}Alpha\u{1f}Alpha child");
        assert_eq!(h[3].key, "Title\u{1f}Beta");
    }

    #[test]
    fn a_hash_inside_a_code_fence_is_not_a_heading() {
        // The bug this prevents: a shell comment folding the rest of the doc.
        let doc = "# Real\n```bash\n# not a heading\n```\n## Also real\n";
        assert_eq!(keys(&doc.to_string()), ["Real", "Real\u{1f}Also real"]);
        // A tilde fence closes only on tildes, and a shorter fence can't close
        // a longer one.
        let doc = "````\n# no\n```\n# still no\n````\n# yes\n";
        assert_eq!(keys(doc), ["yes"]);
    }

    #[test]
    fn hashes_without_a_space_are_not_headings() {
        assert!(headings("#hashtag\n").is_empty());
        assert!(headings("####### seven\n").is_empty());
        // Bare hashes are a heading with empty text (rare, but not a crash).
        assert_eq!(headings("###\n").len(), 1);
        // Four spaces of indent is a code block, not a heading.
        assert!(headings("    # indented\n").is_empty());
        assert_eq!(headings("   # three spaces ok\n").len(), 1);
    }

    #[test]
    fn duplicate_paths_fold_independently() {
        let doc = "## Notes\na\n## Notes\nb\n";
        let k = keys(doc);
        assert_eq!(k[0], "Notes");
        assert_eq!(k[1], "Notes\u{1f}#1");
        assert_ne!(k[0], k[1]);
    }

    #[test]
    fn folding_drops_a_section_body_and_keeps_its_heading() {
        let out = fold_source(DOC, &set(&["Title\u{1f}Alpha"]));
        assert!(out.contains("alpha body") == false, "body gone");
        assert!(out.contains("Alpha"), "heading kept");
        assert!(out.contains("beta body"), "sibling untouched");
        // The nested heading goes with its parent — subtree, not one level.
        assert!(!out.contains("Alpha child"));
    }

    #[test]
    fn folding_encodes_every_heading_as_a_fence_with_its_state() {
        let out = fold_source(DOC, &set(&["Title\u{1f}Alpha"]));
        let fences: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with(&format!("```{HEAD_FENCE}")))
            .collect();
        assert_eq!(fences.len(), 3, "Title, Alpha, Beta (child is hidden)");
        // Alpha is marked collapsed and reports what it is hiding.
        let alpha = out
            .lines()
            .find(|l| l.contains("Alpha") && l.contains('\t'))
            .unwrap();
        let head = parse_head_fence(alpha).unwrap();
        assert!(head.collapsed);
        assert_eq!(head.hidden_lines, 3);
        assert_eq!(head.level, 2);
        assert_eq!(head.text, "Alpha");
    }

    #[test]
    fn nothing_collapsed_still_round_trips_every_line() {
        let out = fold_source(DOC, &BTreeSet::new());
        for body in ["intro line", "alpha body", "child body", "beta body"] {
            assert!(out.contains(body), "{body} survived");
        }
        assert_eq!(headings(DOC).len(), 4);
    }

    #[test]
    fn a_document_without_headings_is_passed_through_untouched() {
        let plain = "just\nsome\nlines\n";
        assert_eq!(fold_source(plain, &set(&["whatever"])), plain);
    }

    #[test]
    fn fence_round_trips_through_the_parser() {
        let h = Heading {
            level: 3,
            text: "1 · Review comms — close the DM (~8 min)".into(),
            key: "Title\u{1f}1 · Review comms".into(),
            line: 4,
            end: 20,
        };
        let fence = head_fence(&h, true);
        let body = fence
            .trim_start_matches(&format!("```{HEAD_FENCE}\n"))
            .trim_end_matches("```\n");
        let parsed = parse_head_fence(body).unwrap();
        assert_eq!(parsed.level, 3);
        assert!(parsed.collapsed);
        assert_eq!(parsed.hidden_lines, 15);
        assert_eq!(parsed.key, h.key);
        assert_eq!(parsed.text, h.text);
    }

    #[test]
    fn parse_head_fence_refuses_junk() {
        assert!(parse_head_fence("").is_none());
        assert!(parse_head_fence("not\tour\tfence").is_none());
    }

    #[test]
    fn a_fold_survives_the_document_being_rewritten_around_it() {
        // The whole reason keys are paths: an agent rewriting the file must not
        // silently reopen what the human folded.
        let before = "# T\n## Keep\nold body\n## Other\nx\n";
        let after = "# T\nnew intro\n## Added\ny\n## Keep\ncompletely new body\n## Other\nx\n";
        let folded = set(&["T\u{1f}Keep"]);
        assert!(fold_source(before, &folded).contains("Keep"));
        assert!(!fold_source(before, &folded).contains("old body"));
        // Same key still folds after the rewrite, at its new position.
        let out = fold_source(after, &folded);
        assert!(!out.contains("completely new body"), "still folded");
        assert!(out.contains('y'), "the new sibling is open");
        assert_eq!(prune(&folded, after), folded, "key still live");
    }

    #[test]
    fn the_outline_set_keeps_every_heading_visible() {
        // Leaves only: folding Title too would collapse the whole document to
        // one line, which is what "collapse everything" naively means and what
        // nobody wants.
        let keys = outline_keys(DOC);
        assert!(!keys.contains("Title"), "an ancestor is not folded");
        assert!(!keys.contains("Title\u{1f}Alpha"), "nor a parent");
        assert_eq!(
            keys,
            set(&["Title\u{1f}Alpha\u{1f}Alpha child", "Title\u{1f}Beta"])
        );
        // And folding that set really does leave every heading on screen.
        let out = fold_source(DOC, &keys);
        for text in ["Title", "Alpha", "Alpha child", "Beta"] {
            assert!(out.contains(text), "{text} still visible");
        }
        for body in ["child body", "beta body"] {
            assert!(!out.contains(body), "{body} folded away");
        }
        // A parent's own preamble survives — it is the orientation text.
        assert!(out.contains("intro line"));
    }

    #[test]
    fn a_flat_document_folds_to_its_spine() {
        // The shape of a real agenda: one title, sections all at one level.
        let doc = "# Agenda\npreamble\n### One\nfirst body\nmore first\n### Two\nsecond body\n";
        let out = fold_source(doc, &outline_keys(doc));
        assert!(out.contains("Agenda") && out.contains("One") && out.contains("Two"));
        assert!(
            out.contains("preamble"),
            "the preamble is orientation, kept"
        );
        for body in ["first body", "more first", "second body"] {
            assert!(!out.contains(body), "{body} folded");
        }
        // Each folded leaf reports what it hid.
        assert!(out.contains("\t2\t"), "One hid two lines");
    }

    #[test]
    fn a_document_of_one_heading_still_folds_that_heading() {
        // Degenerate but real: a single section IS a leaf.
        let doc = "# Only\nbody\n";
        assert_eq!(outline_keys(doc), set(&["Only"]));
    }

    #[test]
    fn renaming_a_heading_forgets_its_fold() {
        // The one case where forgetting is correct — there is no honest way to
        // say the fold still refers to this section.
        let folded = set(&["T\u{1f}Keep"]);
        let renamed = "# T\n## Renamed\nbody\n";
        assert!(prune(&folded, renamed).is_empty());
        assert!(fold_source(renamed, &folded).contains("body"));
    }
}
