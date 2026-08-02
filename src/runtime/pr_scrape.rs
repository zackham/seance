//! PR-URL scraper for live PTY output.
//!
//! `gh pr create` prints the PR URL, `gh pr view` echoes it, agents paste it —
//! and the daemon sees every byte of pane output. This module turns that byte
//! stream into `https://github.com/OWNER/REPO/pull/N` URLs so the engine can
//! keep a per-workspace link list (see `engine/pr_links.rs`).
//!
//! Why chunks and not the rendered grid: a terminal hard-wraps a long URL at
//! the column edge, so the grid holds the URL split across two rows with no
//! marker. The raw stream has it contiguous.
//!
//! Cost discipline — this runs on the PTY I/O thread, once per read chunk:
//! 1. a raw `github` substring probe rejects ~every chunk in O(n) with no
//!    allocation (a spinner repaint never allocates here);
//! 2. only on a hit do we strip ANSI and scan;
//! 3. a bounded tail carry ([`TAIL_CARRY`] bytes) is kept so a URL split
//!    across two reads still matches, and the carry is truncated past the end
//!    of the last match so one URL is never emitted twice.

/// Bytes of sanitized tail kept between chunks. A GitHub PR URL is well under
/// this, so any split point is covered.
const TAIL_CARRY: usize = 256;

/// Per-pane scraper state (lives on the PTY I/O thread).
#[derive(Default)]
pub struct PrScraper {
    tail: String,
}

impl PrScraper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one PTY read chunk; returns any newly-seen PR URLs, in order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        // Fast path: with an empty carry, a chunk without the literal `github`
        // cannot complete a URL. This rejects essentially all TUI repaint
        // traffic without allocating.
        if self.tail.is_empty() && !contains_sub(chunk, b"github") {
            // The chunk may still END mid-URL (`…https://gi`). Probe only the
            // last TAIL_CARRY bytes, and only allocate if a head is there.
            let start = chunk.len().saturating_sub(TAIL_CARRY);
            let suffix = &chunk[start..];
            if contains_sub(suffix, b"http") || contains_sub(suffix, b"gith") {
                let text = strip_ansi(&String::from_utf8_lossy(suffix));
                if let Some(i) = carry_start(&text) {
                    self.tail = text[i..].to_string();
                }
            }
            return Vec::new();
        }

        let text = strip_ansi(&String::from_utf8_lossy(chunk));
        let mut hay = std::mem::take(&mut self.tail);
        hay.push_str(&text);

        let (urls, consumed) = scan(&hay);
        let rest = &hay[consumed..];
        let start = rest.len().saturating_sub(TAIL_CARRY);
        // Keep a char-boundary-safe suffix …
        let start = (start..=rest.len())
            .find(|i| rest.is_char_boundary(*i))
            .unwrap_or(rest.len());
        let carry = &rest[start..];
        // … and only the part that could still be the head of a URL, so an
        // unrelated chunk does not arm the slow path forever.
        self.tail = match carry_start(carry) {
            Some(i) => carry[i..].to_string(),
            None => String::new(),
        };
        urls
    }
}

/// Offset of the last plausible URL head in the carry, if any.
fn carry_start(tail: &str) -> Option<usize> {
    match (tail.rfind("http"), tail.rfind("gith")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn contains_sub(hay: &[u8], needle: &[u8]) -> bool {
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Drop ANSI/OSC escape sequences so a colored URL still matches.
///
/// Handles CSI (`ESC [ … final`), OSC (`ESC ] … BEL|ST`), and the short
/// two-byte escapes; a trailing partial escape is dropped (the tail carry then
/// re-sees whatever follows it in the next chunk).
fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            0x1b => {
                i += 1;
                match b.get(i) {
                    Some(b'[') => {
                        i += 1;
                        while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    Some(b']') => {
                        i += 1;
                        while i < b.len() {
                            if b[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    Some(_) => i += 1,
                    None => {}
                }
            }
            // Bare control bytes are separators, not URL characters.
            c if c < 0x20 => {
                out.push(' ');
                i += 1;
            }
            _ => {
                let ch_len = utf8_len(b[i]);
                let end = (i + ch_len).min(b.len());
                out.push_str(&s[i..end]);
                i = end;
            }
        }
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Find every `github.com/OWNER/REPO/pull/N` in `hay`.
///
/// Returns the URLs plus the byte offset just past the last complete match —
/// the caller carries only what follows, so a URL is emitted once.
fn scan(hay: &str) -> (Vec<String>, usize) {
    let mut urls = Vec::new();
    let mut consumed = 0usize;
    let mut from = 0usize;
    while let Some(rel) = hay[from..].find("github.com/") {
        let at = from + rel;
        let rest = &hay[at + "github.com/".len()..];
        match parse_pr_path(rest) {
            Some((owner, repo, num, used)) => {
                urls.push(format!("https://github.com/{owner}/{repo}/pull/{num}"));
                consumed = at + "github.com/".len() + used;
                from = consumed;
            }
            None => from = at + 1,
        }
    }
    (urls, consumed)
}

/// Parse `OWNER/REPO/pull/N` off the front of `s`; returns the parts and the
/// byte length consumed. Trailing path/query junk (`/files`, `#issuecomment`)
/// is deliberately dropped — one canonical URL per PR.
fn parse_pr_path(s: &str) -> Option<(&str, &str, u64, usize)> {
    let mut it = s.split('/');
    let owner = it.next().filter(|p| is_slug(p))?;
    let repo = it.next().filter(|p| is_slug(p))?;
    let kind = it.next()?;
    if kind != "pull" {
        return None;
    }
    let tail = it.next()?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let num: u64 = digits.parse().ok()?;
    let used = owner.len() + 1 + repo.len() + 1 + kind.len() + 1 + digits.len();
    Some((owner, repo, num, used))
}

fn is_slug(p: &str) -> bool {
    !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_url_in_one_chunk() {
        let mut s = PrScraper::new();
        let out = s.feed(b"remote: https://github.com/zackham/vita/pull/42\r\n");
        assert_eq!(out, vec!["https://github.com/zackham/vita/pull/42"]);
    }

    #[test]
    fn chunk_split_url_still_matches() {
        let mut s = PrScraper::new();
        assert!(s.feed(b"created https://github.com/ride/rw").is_empty());
        let out = s.feed(b"gps/pull/1234\n");
        assert_eq!(out, vec!["https://github.com/ride/rwgps/pull/1234"]);
    }

    #[test]
    fn split_inside_the_word_github_still_matches() {
        let mut s = PrScraper::new();
        assert!(s.feed(b"see https://gi").is_empty());
        let out = s.feed(b"thub.com/o/r/pull/8\n");
        assert_eq!(out, vec!["https://github.com/o/r/pull/8"]);
    }

    #[test]
    fn ansi_interleaved_url_matches() {
        let mut s = PrScraper::new();
        let out = s.feed(b"\x1b[1;34mhttps://github.com/o\x1b[0m/r/pull/7\x1b[0m\n");
        assert_eq!(out, vec!["https://github.com/o/r/pull/7"]);
    }

    #[test]
    fn osc_title_sequence_does_not_break_scan() {
        let mut s = PrScraper::new();
        let out = s.feed(b"\x1b]0;claude\x07github.com/a/b/pull/9 ");
        assert_eq!(out, vec!["https://github.com/a/b/pull/9"]);
    }

    #[test]
    fn same_url_not_re_emitted_from_carry() {
        let mut s = PrScraper::new();
        assert_eq!(s.feed(b"github.com/a/b/pull/1 ").len(), 1);
        assert!(s.feed(b"nothing here\n").is_empty());
    }

    #[test]
    fn trailing_path_is_trimmed_and_multiple_urls_land() {
        let mut s = PrScraper::new();
        let out = s.feed(b"github.com/a/b/pull/1/files and github.com/c/d/pull/22#top\n");
        assert_eq!(
            out,
            vec![
                "https://github.com/a/b/pull/1",
                "https://github.com/c/d/pull/22"
            ]
        );
    }

    #[test]
    fn non_pr_github_urls_ignored() {
        let mut s = PrScraper::new();
        assert!(s.feed(b"https://github.com/a/b/issues/3\n").is_empty());
        assert!(s.feed(b"https://github.com/a/b/pull/abc\n").is_empty());
    }

    #[test]
    fn fast_path_skips_unrelated_chunks_without_growing_carry() {
        let mut s = PrScraper::new();
        for _ in 0..50 {
            assert!(s.feed(b"\x1b[2K\rworking... 12%").is_empty());
        }
        assert!(s.tail.is_empty());
    }

    #[test]
    fn carry_is_bounded() {
        let mut s = PrScraper::new();
        let big = format!("http {} github x", "y".repeat(4000));
        let _ = s.feed(big.as_bytes());
        assert!(s.tail.len() <= TAIL_CARRY);
    }
}
