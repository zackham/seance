//! Session-replay format: the recording IS the wire protocol, persisted.
//!
//! The daemon's grid pushes (SCG3 full/damage frames) plus an attributed
//! event track are everything a player needs to reconstruct a session. This
//! module owns the **on-disk/record framing**, the **bundle manifest**, and
//! **chapter extraction** (human prompts as jump targets) — all sans-io, so
//! the daemon recorder (native), the exporter (native), and the web player
//! (wasm) share one implementation and cannot drift.
//!
//! # Record framing (`SRR1`)
//!
//! A segment/stream is `b"SRR1"` followed by records:
//!
//! ```text
//! [u8 kind] [u64 t_ms LE] [u32 len LE] [payload…]
//! ```
//!
//! - `kind 1` FULL    — a complete SCG3 FULL grid frame ([`crate::snapshot`]).
//!   Every FULL is a **keyframe**: a player may seek to it cold.
//! - `kind 2` DAMAGE  — an SCG3 DAMAGE frame; applies onto the previous state.
//! - `kind 3` EVENT   — one JSON [`ReplayEvent`].
//!
//! `t_ms` is unix milliseconds on the daemon clock. Readers are
//! truncation-tolerant: a segment cut mid-record (crash, live tail) yields
//! every complete record and stops — never an error for the tail case.

use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8; 4] = b"SRR1";

pub const KIND_FULL: u8 = 1;
pub const KIND_DAMAGE: u8 = 2;
pub const KIND_EVENT: u8 = 3;

/// One attributed moment on a pane's event track.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum ReplayEvent {
    /// Raw stdin write. `origin`: `human` | `agent:<slug>` | `cli` | `propose`.
    Input {
        origin: String,
        bytes_b64: String,
    },
    /// Structured text injection (ctl send / GUI inject) — full prompt text.
    Send {
        from: String,
        text: String,
        submit: bool,
    },
    Status {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Title {
        title: String,
    },
    Spawned {
        name: String,
        workspace: String,
        command: String,
    },
    Exited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    Resized {
        cols: u16,
        rows: u16,
    },
}

/// Encode one record (header + payload).
pub fn encode_record(kind: u8, t_ms: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + payload.len());
    out.push(kind);
    out.extend_from_slice(&t_ms.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A decoded record borrowing the source buffer.
#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    pub kind: u8,
    pub t_ms: u64,
    pub payload: &'a [u8],
}

/// Iterate records in a `SRR1` stream. Tolerates a truncated tail (stops).
/// Returns `None` immediately if the magic is absent.
pub fn records(data: &[u8]) -> Option<RecordIter<'_>> {
    let body = data.strip_prefix(MAGIC.as_slice())?;
    Some(RecordIter { data: body })
}

pub struct RecordIter<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Record<'a>;
    fn next(&mut self) -> Option<Record<'a>> {
        if self.data.len() < 13 {
            return None;
        }
        let kind = self.data[0];
        let t_ms = u64::from_le_bytes(self.data[1..9].try_into().ok()?);
        let len = u32::from_le_bytes(self.data[9..13].try_into().ok()?) as usize;
        if self.data.len() < 13 + len {
            return None; // truncated tail — everything complete was yielded
        }
        let payload = &self.data[13..13 + len];
        self.data = &self.data[13 + len..];
        Some(Record { kind, t_ms, payload })
    }
}

// ---------------------------------------------------------------------------
// bundle manifest
// ---------------------------------------------------------------------------

/// `manifest.json` at the root of an exported bundle. The player consumes
/// this plus the per-pane `*.srr.gz` files listed here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version — bump only with a player that reads both.
    pub version: u32,
    pub workspace: String,
    /// Optional human-given title for the share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub created_ms: u64,
    pub from_ms: u64,
    pub to_ms: u64,
    pub panes: Vec<PaneMeta>,
    /// Human prompts + injected tasks, ordered by time — the chapter list.
    pub chapters: Vec<Chapter>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneMeta {
    pub slug: String,
    pub name: String,
    /// Bundle-relative recording file (gzip'd SRR1 stream).
    pub file: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Chapter {
    pub t_ms: u64,
    pub pane: String,
    /// Reconstructed prompt text (editable in the editor before publish).
    pub text: String,
}

// ---------------------------------------------------------------------------
// chapter extraction
// ---------------------------------------------------------------------------

/// Reconstruct human prompts from a pane's event track.
///
/// Keystroke reconstruction is a deliberately *lite* line editor: printable
/// UTF-8 accumulates, backspace (0x7f/0x08) pops, `\r` submits a chapter,
/// escape sequences are skipped wholesale. TUI cursor movement is ignored, so
/// heavy mid-line editing degrades the text — the editor lets the human fix
/// titles before publish, and a degraded title still marks the right moment.
/// `Send { submit: true }` events contribute their full text verbatim.
pub fn extract_chapters(pane: &str, events: &[(u64, ReplayEvent)]) -> Vec<Chapter> {
    let mut out = Vec::new();
    let mut line = LineRecon::default();
    for (t_ms, ev) in events {
        match ev {
            ReplayEvent::Input { origin, bytes_b64 } if origin == "human" => {
                let Ok(bytes) = b64_decode(bytes_b64) else {
                    continue;
                };
                for submitted in line.feed(&bytes) {
                    if !submitted.trim().is_empty() {
                        out.push(Chapter {
                            t_ms: *t_ms,
                            pane: pane.to_string(),
                            text: submitted.trim().to_string(),
                        });
                    }
                }
            }
            ReplayEvent::Send { text, submit, .. } if *submit => {
                let text = text.trim();
                if !text.is_empty() {
                    out.push(Chapter {
                        t_ms: *t_ms,
                        pane: pane.to_string(),
                        text: text.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Minimal base64 (standard alphabet) decode — avoids a base64 crate dep in
/// core; recordings are trusted daemon output, strictness buys nothing.
fn b64_decode(s: &str) -> Result<Vec<u8>, ()> {
    fn val(c: u8) -> Result<u8, ()> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(()),
        }
    }
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut acc: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= (val(c)? as u32) << (18 - 6 * i);
        }
        let n = chunk.len();
        if n >= 2 {
            out.push((acc >> 16) as u8);
        }
        if n >= 3 {
            out.push((acc >> 8) as u8);
        }
        if n == 4 {
            out.push(acc as u8);
        }
    }
    Ok(out)
}

/// The lite line editor behind [`extract_chapters`].
#[derive(Default)]
struct LineRecon {
    buf: String,
    /// Pending UTF-8 continuation bytes.
    utf8: Vec<u8>,
    /// Inside an ESC sequence (skipped until a terminator).
    esc: bool,
    /// Inside a bracketed paste (`ESC[200~` … `ESC[201~`): text passes through.
    paste: bool,
}

impl LineRecon {
    /// Feed raw stdin bytes; returns any lines submitted by `\r`.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut submitted = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            // Bracketed-paste markers arrive as ESC[200~ / ESC[201~.
            if b == 0x1b {
                if bytes[i..].starts_with(b"\x1b[200~") {
                    self.paste = true;
                    i += 6;
                    continue;
                }
                if bytes[i..].starts_with(b"\x1b[201~") {
                    self.paste = false;
                    i += 6;
                    continue;
                }
                self.esc = true;
                i += 1;
                continue;
            }
            if self.esc {
                // CSI: consume until a final byte (0x40..=0x7e); other ESC
                // forms: single following byte ends it.
                if b == b'[' {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1; // final byte
                } else {
                    i += 1;
                }
                self.esc = false;
                continue;
            }
            match b {
                b'\r' => {
                    submitted.push(std::mem::take(&mut self.buf));
                    self.utf8.clear();
                }
                // Pasted newlines inside a bracketed paste stay in the line.
                b'\n' if self.paste => self.buf.push('\n'),
                b'\n' => {}
                0x7f | 0x08 => {
                    self.buf.pop();
                }
                0x00..=0x1f => {} // other control chars: ignore
                _ => {
                    self.utf8.push(b);
                    if let Ok(s) = std::str::from_utf8(&self.utf8) {
                        self.buf.push_str(s);
                        self.utf8.clear();
                    } else if self.utf8.len() >= 4 {
                        self.utf8.clear(); // invalid — drop
                    }
                }
            }
            i += 1;
        }
        submitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        // Tiny encoder for tests only.
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut acc = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                acc |= (c as u32) << (16 - 8 * i);
            }
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(A[((acc >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn records_roundtrip_and_tolerate_truncation() {
        let mut stream = MAGIC.to_vec();
        stream.extend_from_slice(&encode_record(KIND_FULL, 1000, b"frameA"));
        stream.extend_from_slice(&encode_record(KIND_EVENT, 2000, b"{}"));
        // Truncated third record: header promises more than present.
        stream.extend_from_slice(&encode_record(KIND_DAMAGE, 3000, b"frameB")[..10]);

        let got: Vec<_> = records(&stream).unwrap().collect();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].kind, got[0].t_ms, got[0].payload), (KIND_FULL, 1000, b"frameA".as_slice()));
        assert_eq!(got[1].kind, KIND_EVENT);
    }

    #[test]
    fn records_reject_bad_magic() {
        assert!(records(b"NOPE").is_none());
    }

    #[test]
    fn chapters_from_typed_keystrokes_with_backspace() {
        let ev = |b: &[u8]| ReplayEvent::Input {
            origin: "human".into(),
            bytes_b64: b64(b),
        };
        let events = vec![
            (10, ev(b"fix the bugg")),
            (20, ev(b"\x7f")),        // backspace
            (30, ev(b"\r")),          // submit
            (40, ev(b"\x1b[A")),      // arrow — ignored
            (50, ev(b"ls\r")),
        ];
        let ch = extract_chapters("p1", &events);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch[0].text, "fix the bug");
        assert_eq!(ch[0].t_ms, 30);
        assert_eq!(ch[1].text, "ls");
    }

    #[test]
    fn chapters_from_send_and_ignore_agent_input() {
        let events = vec![
            (
                5,
                ReplayEvent::Input {
                    origin: "agent:w-1".into(),
                    bytes_b64: b64(b"sneaky\r"),
                },
            ),
            (
                9,
                ReplayEvent::Send {
                    from: "orchestrator".into(),
                    text: "run the tests".into(),
                    submit: true,
                },
            ),
            (
                12,
                ReplayEvent::Send {
                    from: "x".into(),
                    text: "unsubmitted draft".into(),
                    submit: false,
                },
            ),
        ];
        let ch = extract_chapters("p1", &events);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].text, "run the tests");
    }

    #[test]
    fn bracketed_paste_keeps_newlines_and_skips_markers() {
        let mut paste = Vec::new();
        paste.extend_from_slice(b"\x1b[200~line1\nline2\x1b[201~");
        paste.extend_from_slice(b"\r");
        let events = vec![(
            7,
            ReplayEvent::Input {
                origin: "human".into(),
                bytes_b64: b64(&paste),
            },
        )];
        let ch = extract_chapters("p1", &events);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].text, "line1\nline2");
    }

    #[test]
    fn manifest_roundtrips() {
        let m = Manifest {
            version: 1,
            workspace: "lab".into(),
            title: Some("fixing the parser".into()),
            created_ms: 1,
            from_ms: 0,
            to_ms: 100,
            panes: vec![PaneMeta {
                slug: "w-1".into(),
                name: "worker".into(),
                file: "w-1.srr.gz".into(),
            }],
            chapters: vec![Chapter {
                t_ms: 5,
                pane: "w-1".into(),
                text: "hello".into(),
            }],
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.workspace, "lab");
        assert_eq!(back.chapters, m.chapters);
    }

    #[test]
    fn b64_roundtrip() {
        for case in [&b""[..], b"a", b"ab", b"abc", b"hello world\r\x7f"] {
            assert_eq!(b64_decode(&b64(case)).unwrap(), case);
        }
    }
}
