//! URL detection over a rendered grid — what a ctrl+click (native) or a tap
//! (web) resolves to.
//!
//! Shared so the native GUI and the web client cannot drift, same rule as
//! `util::title_looks_busy`.

use crate::snapshot::GridSnapshot;

/// Rows joined either side of the hit row while each ends in a non-blank cell.
/// A snapshot carries no WRAPPED flag, so "the row is full to the last column"
/// is the only wrap signal there is; the cap bounds a full screen of
/// box-drawing from being glued into one line.
const STITCH_MAX: u16 = 6;

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// The link under one cell: an OSC-8 span covering it, else a bare http(s) URL
/// whose extent covers that column.
///
/// Cell-targeted on purpose. This used to open the first link anywhere on
/// screen and ignore the position, so ctrl+clicking a URL in a pane that also
/// had a PR reference higher up opened the PR.
pub fn url_at_cell(snap: &GridSnapshot, row: u16, col: u16) -> Option<String> {
    if let Some(h) = snap
        .hyperlinks
        .iter()
        .find(|h| h.row == row && col >= h.col_start && col < h.col_end)
    {
        return Some(h.uri.clone());
    }
    let (line, hit) = logical_line(snap, row, col)?;
    http_url_at(&line, hit)
}

/// The wrapped run of rows containing `row`, as one string, plus where `col`
/// landed in it. A phone terminal is ~45 columns wide, so any URL worth
/// tapping is wrapped — resolving one row alone hands back a fragment that
/// opens nothing.
fn logical_line(snap: &GridSnapshot, row: u16, col: u16) -> Option<(String, usize)> {
    let cols = snap.cols as usize;
    if cols == 0 || snap.cells.is_empty() {
        return None;
    }
    let rows = (snap.cells.len() / cols) as u16;
    if row >= rows {
        return None;
    }
    let full = |r: u16| -> bool {
        let last = (r as usize + 1) * cols - 1;
        snap.cells.get(last).map(|c| c.c != ' ').unwrap_or(false)
    };

    let mut start = row;
    while start > 0 && row - start < STITCH_MAX && full(start - 1) {
        start -= 1;
    }
    let mut end = row;
    while end + 1 < rows && end - row < STITCH_MAX && full(end) {
        end += 1;
    }

    let mut line = String::with_capacity(cols * (end - start + 1) as usize);
    for r in start..=end {
        let base = r as usize * cols;
        for c in 0..cols {
            line.push(snap.cells[base + c].c);
        }
    }
    Some((line, (row - start) as usize * cols + col as usize))
}

/// The http(s) URL in `line` whose character range covers `col`.
///
/// Indexes by CHAR, not byte: a row of terminal cells is one char per column,
/// so a non-ASCII glyph anywhere left of the URL would slide every byte offset
/// off by one.
pub fn http_url_at(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let starts_at = |i: usize| {
        chars[i..].starts_with(&['h', 't', 't', 'p', 's', ':', '/', '/'])
            || chars[i..].starts_with(&['h', 't', 't', 'p', ':', '/', '/'])
    };
    let mut i = 0;
    while i < chars.len() {
        if !starts_at(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_url_char(chars[i]) {
            i += 1;
        }
        let url: String = chars[start..i].iter().collect();
        let url = url.trim_end_matches(['.', ',', ')', ']', ';', ':']);
        if url.len() > 10 && col >= start && col < start + url.chars().count() {
            return Some(url.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{CellSnap, HyperlinkSpan};

    /// A grid holding one line of text per row, padded to `cols`.
    fn grid_of(cols: usize, lines: &[&str]) -> GridSnapshot {
        let mut snap = GridSnapshot::empty("t-1");
        snap.cols = cols as u16;
        snap.rows = lines.len() as u16;
        snap.cells = Vec::with_capacity(cols * lines.len());
        for line in lines {
            let mut chars: Vec<char> = line.chars().collect();
            chars.resize(cols, ' ');
            for c in chars {
                let mut cell = CellSnap::blank();
                cell.c = c;
                snap.cells.push(cell);
            }
        }
        snap
    }

    /// The regression: ctrl+click used to ignore where you clicked and open
    /// the first link on screen, so clicking a plat URL under a PR reference
    /// opened the PR.
    #[test]
    fn a_click_resolves_the_url_under_the_cursor_not_the_first_on_screen() {
        let snap = grid_of(
            80,
            &[
                "see https://github.com/o/r/pull/6807 for the CI run",
                "plat https://cadence.ham.xyz/plats/86ff368c-92db for the numbers",
            ],
        );
        assert_eq!(
            url_at_cell(&snap, 1, 10).as_deref(),
            Some("https://cadence.ham.xyz/plats/86ff368c-92db")
        );
        assert_eq!(
            url_at_cell(&snap, 0, 10).as_deref(),
            Some("https://github.com/o/r/pull/6807")
        );
    }

    /// Clicking off any link opens nothing — better than opening something
    /// arbitrary from elsewhere on screen.
    #[test]
    fn a_click_on_plain_text_opens_nothing() {
        let snap = grid_of(80, &["plat https://cadence.ham.xyz/x for the numbers"]);
        assert_eq!(url_at_cell(&snap, 0, 0), None);
        assert_eq!(url_at_cell(&snap, 0, 40), None);
    }

    /// An OSC-8 span wins over bare text, but only on the cells it covers.
    #[test]
    fn an_osc8_span_only_claims_its_own_cells() {
        let mut snap = grid_of(80, &["#6807 and https://cadence.ham.xyz/plats/abc"]);
        snap.hyperlinks.push(HyperlinkSpan {
            row: 0,
            col_start: 0,
            col_end: 5,
            uri: "https://github.com/o/r/pull/6807".into(),
        });
        assert_eq!(
            url_at_cell(&snap, 0, 2).as_deref(),
            Some("https://github.com/o/r/pull/6807")
        );
        assert_eq!(
            url_at_cell(&snap, 0, 20).as_deref(),
            Some("https://cadence.ham.xyz/plats/abc")
        );
    }

    /// Column is a CHAR offset, not a byte one — a wide glyph left of the URL
    /// would otherwise slide the hit box.
    #[test]
    fn a_non_ascii_glyph_does_not_shift_the_hit_box() {
        let snap = grid_of(80, &["✦ ✦ https://cadence.ham.xyz/plats/abc"]);
        assert_eq!(
            url_at_cell(&snap, 0, 4).as_deref(),
            Some("https://cadence.ham.xyz/plats/abc")
        );
        assert_eq!(url_at_cell(&snap, 0, 3), None);
    }

    /// The phone case: 44 columns wide, so the URL is in three pieces. A tap
    /// on ANY of them has to produce the whole thing.
    #[test]
    fn a_wrapped_url_is_stitched_back_together_from_any_row() {
        // 40 cols. Row 0 is full to the last column — the wrap signal — and
        // row 1 is not, which ends the logical line.
        let snap = grid_of(
            40,
            &[
                "opened https://github.com/ridewithgps/rw",
                "gps/pull/12345 and the CI is green",
            ],
        );
        let want = Some("https://github.com/ridewithgps/rwgps/pull/12345");
        assert_eq!(url_at_cell(&snap, 0, 10).as_deref(), want);
        assert_eq!(url_at_cell(&snap, 1, 2).as_deref(), want);
    }

    /// Stitching must not walk off into unrelated rows: a row that ends in a
    /// blank is a line break, so the URL stops there.
    #[test]
    fn a_line_ending_in_blank_does_not_swallow_the_next_row() {
        let snap = grid_of(20, &["see https://a.io/x", "tra text here"]);
        assert_eq!(url_at_cell(&snap, 0, 6).as_deref(), Some("https://a.io/x"));
    }
}
