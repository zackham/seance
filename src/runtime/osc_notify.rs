//! OSC 9 / OSC 777 desktop-notification scraper for live PTY output.
//!
//! Programs ask the terminal for a desktop notification with
//! `ESC ] 9 ; body ST` (iTerm2) or `ESC ] 777 ; notify ; title ; body ST`
//! (rxvt, used by Ghostty and others). alacritty_terminal drops both, so the
//! I/O thread reads them from the raw stream, the same way `pr_scrape` finds
//! PR links. ConEmu reuses OSC 9 with a numeric subcommand (`9;4;…` is a
//! progress bar); those are not notifications, so any OSC 9 whose first field
//! is all digits is skipped.
//!
//! Cost: a chunk with no `ESC ]` and no carried partial sequence returns
//! without allocating.

/// Longest sequence kept across reads. A notification body past this is
/// dropped rather than buffered without bound.
const MAX_CARRY: usize = 4096;

#[derive(Default)]
pub struct OscNotifyScraper {
    /// An unterminated sequence from the previous chunk, starting at `ESC ]`.
    carry: Vec<u8>,
}

impl OscNotifyScraper {
    /// Feed one PTY read chunk; returns `(title, body)` per notification.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        if self.carry.is_empty() && !contains(chunk, b"\x1b]") {
            // A chunk can still end on a bare ESC that the next read completes.
            if chunk.last() == Some(&0x1b) {
                self.carry.push(0x1b);
            }
            return Vec::new();
        }
        let mut hay = std::mem::take(&mut self.carry);
        hay.extend_from_slice(chunk);

        let mut out = Vec::new();
        let mut i = 0;
        while let Some(start) = find(&hay[i..], b"\x1b]").map(|p| p + i) {
            let payload_start = start + 2;
            match terminator(&hay[payload_start..]) {
                End::Done { len, next } => {
                    if let Some(n) = parse(&hay[payload_start..payload_start + len]) {
                        out.push(n);
                    }
                    i = payload_start + next;
                }
                End::Aborted { resume } => i = payload_start + resume,
                End::Pending => {
                    if hay.len() - start <= MAX_CARRY {
                        self.carry = hay[start..].to_vec();
                    }
                    return out;
                }
            }
        }
        if hay.last() == Some(&0x1b) {
            self.carry.push(0x1b);
        }
        out
    }
}

enum End {
    /// Ended by BEL or ST (`ESC \`): payload length, bytes to skip past it.
    Done { len: usize, next: usize },
    /// Cut off the way vte cuts it off (any other ESC, CAN, SUB): nothing to
    /// post; scanning resumes at `resume` (an ESC may start the next sequence).
    Aborted { resume: usize },
    /// Not ended yet; wait for the next read.
    Pending,
}

fn terminator(rest: &[u8]) -> End {
    for (j, &b) in rest.iter().enumerate() {
        match b {
            0x07 => {
                return End::Done {
                    len: j,
                    next: j + 1,
                }
            }
            0x1b => {
                return match rest.get(j + 1) {
                    Some(b'\\') => End::Done {
                        len: j,
                        next: j + 2,
                    },
                    Some(_) => End::Aborted { resume: j },
                    None => End::Pending,
                }
            }
            0x18 | 0x1a => return End::Aborted { resume: j + 1 },
            _ => {}
        }
    }
    End::Pending
}

fn parse(payload: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(payload);
    if let Some(body) = text.strip_prefix("9;") {
        let numeric_sub = body
            .split(';')
            .next()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        if numeric_sub || body.is_empty() {
            return None;
        }
        return Some((String::new(), body.to_string()));
    }
    let rest = text.strip_prefix("777;notify;")?;
    let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
    Some((title.to_string(), body.to_string()))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str, body: &str) -> (String, String) {
        (title.to_string(), body.to_string())
    }

    #[test]
    fn osc9_and_osc777() {
        let mut s = OscNotifyScraper::default();
        assert_eq!(
            s.feed(b"hi\x1b]9;Build done\x07 \x1b]777;notify;Claude;Waiting\x1b\\"),
            vec![note("", "Build done"), note("Claude", "Waiting")]
        );
    }

    #[test]
    fn skips_progress_titles_and_plain_output() {
        let mut s = OscNotifyScraper::default();
        assert!(s.feed(b"\x1b]9;4;1;50\x07\x1b]0;title\x07plain").is_empty());
    }

    #[test]
    fn sequence_split_across_reads() {
        let mut s = OscNotifyScraper::default();
        assert!(s.feed(b"out\x1b").is_empty());
        assert!(s.feed(b"]777;notify;T;par").is_empty());
        assert_eq!(s.feed(b"tial\x07"), vec![note("T", "partial")]);
        assert!(s.feed(b"after").is_empty());
    }

    #[test]
    fn stray_escape_ends_the_sequence() {
        let mut s = OscNotifyScraper::default();
        assert_eq!(
            s.feed(b"\x1b]9;half\x1b[31mred\x07\x1b]777;notify;T;B\x07"),
            vec![note("T", "B")]
        );
    }

    #[test]
    fn unterminated_runaway_is_dropped() {
        let mut s = OscNotifyScraper::default();
        let mut big = b"\x1b]9;".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_CARRY + 10));
        assert!(s.feed(&big).is_empty());
        assert!(s.feed(b"\x07").is_empty());
    }
}
