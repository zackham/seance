//! Terminal grid snapshot + compact binary wire format.
//!
//! # Live path: `SCG2` binary (not JSON cell soup)
//!
//! Busy TUIs used to push ~1MB/frame of verbose JSON and peg the GUI. The hot
//! path now encodes grids as a tight RLE binary blob (`encode_grid_bin` /
//! `decode_grid_bin`), base64'd inside a small JSON envelope (`grid_bin`
//! event). Blank default cells compress to 3 bytes per run (`0x00` + u16 count).
//!
//! JSON `GridSnapshot` remains for debugging / older tools; the daemon broadcasts
//! binary on the live path.

use serde::{Deserialize, Serialize};

fn default_color() -> u32 {
    0xFFFF_FFFF
}
fn is_default_color(v: &u32) -> bool {
    *v == 0xFFFF_FFFF
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn default_space() -> char {
    ' '
}
fn is_space(c: &char) -> bool {
    *c == ' '
}
fn default_true() -> bool {
    true
}
fn is_true(v: &bool) -> bool {
    *v
}

/// One visible cell.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CellSnap {
    /// Glyph. Omitted when space (the common case).
    #[serde(default = "default_space", skip_serializing_if = "is_space")]
    pub c: char,
    /// Packed RGB (0x00RRGGBB). 0xFFFFFFFF = default fg.
    #[serde(default = "default_color", skip_serializing_if = "is_default_color")]
    pub fg: u32,
    /// Packed RGB. 0xFFFFFFFF = default bg (transparent).
    #[serde(default = "default_color", skip_serializing_if = "is_default_color")]
    pub bg: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
}

impl CellSnap {
    pub fn blank() -> Self {
        Self {
            c: ' ',
            fg: 0xFFFF_FFFF,
            bg: 0xFFFF_FFFF,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }

    fn is_default_blank(&self) -> bool {
        self.c == ' '
            && self.fg == 0xFFFF_FFFF
            && self.bg == 0xFFFF_FFFF
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.inverse
    }

    fn style_byte(&self) -> u8 {
        let mut s = 0u8;
        if self.bold {
            s |= 1;
        }
        if self.dim {
            s |= 2;
        }
        if self.italic {
            s |= 4;
        }
        if self.underline {
            s |= 8;
        }
        if self.inverse {
            s |= 16;
        }
        s
    }

    fn from_parts(c: char, fg: u32, bg: u32, style: u8) -> Self {
        Self {
            c,
            fg,
            bg,
            bold: style & 1 != 0,
            dim: style & 2 != 0,
            italic: style & 4 != 0,
            underline: style & 8 != 0,
            inverse: style & 16 != 0,
        }
    }
}

/// Ghost proposal overlay.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhostSnap {
    pub id: String,
    pub text: String,
    pub from: String,
    pub reason: Option<String>,
}

/// Full visible-screen snapshot (in-memory + legacy JSON).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GridSnapshot {
    pub pane: String,
    pub rev: u64,
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub cursor_shape_block: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub running: bool,
    pub cells: Vec<CellSnap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ghost: Option<GhostSnap>,
    /// Plain-text screen — usually empty on the live path (paint uses cells).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub alt_screen: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub alternate_scroll: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub app_cursor: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mouse_mode: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sgr_mouse: bool,
    /// Who last wrote stdin to this PTY (`human` / `agent:x` / `cli` / `propose`).
    /// Carried on JSON grid path; binary path also embeds it (SCG3 v2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_origin: Option<String>,
    /// OSC-8 hyperlink spans on the visible screen (row/col are 0-based cell coords).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hyperlinks: Vec<HyperlinkSpan>,
}

/// One OSC-8 hyperlink region on the visible grid.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HyperlinkSpan {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub uri: String,
}

impl GridSnapshot {
    pub fn empty(pane: &str) -> Self {
        Self {
            pane: pane.to_string(),
            rev: 0,
            cols: 80,
            rows: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_shape_block: true,
            title: None,
            running: true,
            cells: Vec::new(),
            ghost: None,
            text: String::new(),
            alt_screen: false,
            alternate_scroll: false,
            app_cursor: false,
            mouse_mode: false,
            sgr_mouse: false,
            last_input_origin: None,
            hyperlinks: Vec::new(),
        }
    }

    /// URI under cell (row, col), if any.
    pub fn hyperlink_at(&self, row: u16, col: u16) -> Option<&str> {
        self.hyperlinks
            .iter()
            .find(|h| h.row == row && col >= h.col_start && col < h.col_end)
            .map(|h| h.uri.as_str())
    }
}

// ---------------------------------------------------------------------------
// SCG3 binary wire format (full frame or row-damage)
// ---------------------------------------------------------------------------
//
// Magic `SCG3`. Frame kinds:
//   FULL (0)    — RLE of all cols*rows cells (same RLE ops as SCG2)
//   DAMAGE (1)  — u16 n, then n × (u16 row + RLE of exactly `cols` cells)
//
// GUI applies DAMAGE onto the previous snapshot for that pane. Daemon always
// sends FULL on resize / first frame / when >50% of rows change.

const MAGIC: &[u8; 4] = b"SCG3";
const VERSION: u8 = 1;
const FRAME_FULL: u8 = 0;
const FRAME_DAMAGE: u8 = 1;

// RLE ops
const OP_BLANKS: u8 = 0x00; // + u16 n default blanks
const OP_CELL: u8 = 0x01; // + char u32 + fg u32 + bg u32 + style u8
const OP_REPEAT: u8 = 0x02; // + u16 n + cell payload

/// Which rows differ between two equal-sized grids.
pub fn dirty_rows(prev: &[CellSnap], next: &[CellSnap], cols: usize, rows: usize) -> Vec<u16> {
    let mut out = Vec::new();
    if cols == 0 || rows == 0 {
        return out;
    }
    if prev.len() != cols * rows || next.len() != cols * rows {
        return (0..rows as u16).collect();
    }
    for r in 0..rows {
        let a = r * cols;
        let b = a + cols;
        if prev[a..b] != next[a..b] {
            out.push(r as u16);
        }
    }
    out
}

/// Encode a **full** grid frame.
pub fn encode_grid_bin(snap: &GridSnapshot) -> Result<Vec<u8>, String> {
    encode_grid_bin_ex(snap, None)
}

/// Encode full frame, or row-damage when `dirty` is `Some` non-empty subset.
/// Pass `None` for full. Pass `Some(&[])` only when nothing changed (caller
/// should skip the send entirely).
pub fn encode_grid_bin_ex(snap: &GridSnapshot, dirty: Option<&[u16]>) -> Result<Vec<u8>, String> {
    let expect = (snap.cols as usize).saturating_mul(snap.rows as usize);
    if !snap.cells.is_empty() && snap.cells.len() != expect {
        return Err(format!(
            "cell count {} != cols*rows {}",
            snap.cells.len(),
            expect
        ));
    }

    let mut out = Vec::with_capacity(64 + snap.cells.len().saturating_mul(2));
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&snap.rev.to_le_bytes());
    out.extend_from_slice(&snap.cols.to_le_bytes());
    out.extend_from_slice(&snap.rows.to_le_bytes());
    out.extend_from_slice(&snap.cursor_col.to_le_bytes());
    out.extend_from_slice(&snap.cursor_row.to_le_bytes());

    let mut flags = 0u8;
    if snap.alt_screen {
        flags |= 1;
    }
    if snap.alternate_scroll {
        flags |= 2;
    }
    if snap.app_cursor {
        flags |= 4;
    }
    if snap.mouse_mode {
        flags |= 8;
    }
    if snap.sgr_mouse {
        flags |= 16;
    }
    if snap.running {
        flags |= 32;
    }
    if snap.cursor_shape_block {
        flags |= 64;
    }
    if snap.title.is_some() {
        flags |= 128;
    }
    out.push(flags);

    if let Some(ref title) = snap.title {
        let t = title.as_bytes();
        let len = (t.len().min(u16::MAX as usize)) as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&t[..len as usize]);
    }

    let p = snap.pane.as_bytes();
    let plen = (p.len().min(u16::MAX as usize)) as u16;
    out.extend_from_slice(&plen.to_le_bytes());
    out.extend_from_slice(&p[..plen as usize]);

    if let Some(ref g) = snap.ghost {
        out.push(1);
        write_str(&mut out, &g.id)?;
        write_str(&mut out, &g.text)?;
        write_str(&mut out, &g.from)?;
        match &g.reason {
            Some(r) => {
                out.push(1);
                write_str(&mut out, r)?;
            }
            None => out.push(0),
        }
    } else {
        out.push(0);
    }

    let cols = snap.cols as usize;
    let rows = snap.rows as usize;

    // Decide full vs damage
    let use_damage = match dirty {
        Some(d)
            if !d.is_empty()
                && !snap.cells.is_empty()
                && d.len() * 2 < rows.max(1)
                && d.iter().all(|&r| (r as usize) < rows) =>
        {
            true
        }
        _ => false,
    };

    if use_damage {
        let d = dirty.unwrap();
        out.push(FRAME_DAMAGE);
        out.extend_from_slice(&(d.len() as u16).to_le_bytes());
        for &row in d {
            out.extend_from_slice(&row.to_le_bytes());
            let start = row as usize * cols;
            let end = start + cols;
            write_rle(&mut out, &snap.cells[start..end])?;
        }
    } else {
        out.push(FRAME_FULL);
        if snap.cells.is_empty() && expect > 0 {
            write_rle_blanks(&mut out, expect)?;
        } else {
            write_rle(&mut out, &snap.cells)?;
        }
    }

    // Trailing hyperlink table (optional; older GUIs ignore leftover bytes).
    // Marker 0x48 'H' + u16 count + spans.
    if !snap.hyperlinks.is_empty() {
        out.push(b'H');
        let n = (snap.hyperlinks.len().min(u16::MAX as usize)) as u16;
        out.extend_from_slice(&n.to_le_bytes());
        for h in snap.hyperlinks.iter().take(n as usize) {
            out.extend_from_slice(&h.row.to_le_bytes());
            out.extend_from_slice(&h.col_start.to_le_bytes());
            out.extend_from_slice(&h.col_end.to_le_bytes());
            write_str(&mut out, &h.uri)?;
        }
    }

    Ok(out)
}

/// Decode SCG3 (or legacy SCG2) into a full snapshot.
///
/// For DAMAGE frames, `base` must be the previous snapshot for that pane
/// (same cols/rows). If base is missing or size mismatches, returns an error
/// so the caller can request a full frame (engine will send full next push).
pub fn decode_grid_bin(data: &[u8]) -> Result<GridSnapshot, String> {
    decode_grid_bin_onto(data, None)
}

pub fn decode_grid_bin_onto(
    data: &[u8],
    base: Option<&GridSnapshot>,
) -> Result<GridSnapshot, String> {
    let mut r = Reader::new(data);
    let magic = r.read_bytes(4)?;
    let legacy = magic == b"SCG2";
    if magic != MAGIC && !legacy {
        return Err("bad magic".into());
    }
    let ver = r.read_u8()?;
    if ver != VERSION {
        return Err(format!("unsupported SCG version {ver}"));
    }
    let rev = r.read_u64()?;
    let cols = r.read_u16()?;
    let rows = r.read_u16()?;
    let cursor_col = r.read_u16()?;
    let cursor_row = r.read_u16()?;
    let flags = r.read_u8()?;

    let title = if flags & 128 != 0 {
        Some(r.read_string()?)
    } else {
        None
    };
    let pane = r.read_string()?;

    let ghost = if r.read_u8()? == 1 {
        let id = r.read_string()?;
        let text = r.read_string()?;
        let from = r.read_string()?;
        let reason = if r.read_u8()? == 1 {
            Some(r.read_string()?)
        } else {
            None
        };
        Some(GhostSnap {
            id,
            text,
            from,
            reason,
        })
    } else {
        None
    };

    let expect = cols as usize * rows as usize;
    // A non-legacy FULL frame carries the complete hyperlink table (the
    // encoder omits it only when there are *no* links). So a full frame with
    // no trailing 'H' means the pane has no hyperlinks — do NOT inherit stale
    // ones from base. DAMAGE (and legacy) frames still fall back to base.
    let mut full_authoritative_links = false;
    let cells = if legacy {
        // SCG2: implicit full RLE of all cells
        read_rle_n(&mut r, expect)?
    } else {
        let kind = r.read_u8()?;
        match kind {
            FRAME_FULL => {
                full_authoritative_links = true;
                read_rle_n(&mut r, expect)?
            }
            FRAME_DAMAGE => {
                let base = base.ok_or_else(|| "damage frame without base".to_string())?;
                if base.cols != cols || base.rows != rows || base.cells.len() != expect {
                    return Err("damage size mismatch".into());
                }
                let mut cells = base.cells.clone();
                let n = r.read_u16()? as usize;
                for _ in 0..n {
                    let row = r.read_u16()? as usize;
                    if row >= rows as usize {
                        return Err(format!("damage row {row} oob"));
                    }
                    let row_cells = read_rle_n(&mut r, cols as usize)?;
                    let start = row * cols as usize;
                    cells[start..start + cols as usize].clone_from_slice(&row_cells);
                }
                cells
            }
            other => return Err(format!("bad frame kind {other:#x}")),
        }
    };

    Ok(GridSnapshot {
        pane,
        rev,
        cols,
        rows,
        cursor_col,
        cursor_row,
        cursor_shape_block: flags & 64 != 0,
        title,
        running: flags & 32 != 0,
        cells,
        ghost,
        text: String::new(),
        alt_screen: flags & 1 != 0,
        alternate_scroll: flags & 2 != 0,
        app_cursor: flags & 4 != 0,
        mouse_mode: flags & 8 != 0,
        sgr_mouse: flags & 16 != 0,
        last_input_origin: None,
        hyperlinks: {
            // Optional trailing 'H' + spans (new); fall back to base on damage
            // frames that omit the table.
            if !r.is_empty() && r.peek_u8() == Some(b'H') {
                let _ = r.read_u8();
                let n = r.read_u16().unwrap_or(0) as usize;
                let mut links = Vec::with_capacity(n);
                for _ in 0..n {
                    let row = match r.read_u16() {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let col_start = match r.read_u16() {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let col_end = match r.read_u16() {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let uri = match r.read_string() {
                        Ok(s) => s,
                        Err(_) => break,
                    };
                    links.push(HyperlinkSpan {
                        row,
                        col_start,
                        col_end,
                        uri,
                    });
                }
                links
            } else if full_authoritative_links {
                // Full frame, no table => authoritatively empty.
                Vec::new()
            } else {
                base.map(|b| b.hyperlinks.clone()).unwrap_or_default()
            }
        },
    })
}

fn write_rle_blanks(out: &mut Vec<u8>, mut n: usize) -> Result<(), String> {
    while n > 0 {
        let chunk = n.min(u16::MAX as usize) as u16;
        out.push(OP_BLANKS);
        out.extend_from_slice(&chunk.to_le_bytes());
        n -= chunk as usize;
    }
    Ok(())
}

fn write_rle(out: &mut Vec<u8>, cells: &[CellSnap]) -> Result<(), String> {
    let mut i = 0usize;
    while i < cells.len() {
        if cells[i].is_default_blank() {
            let start = i;
            i += 1;
            while i < cells.len() && cells[i].is_default_blank() && (i - start) < u16::MAX as usize
            {
                i += 1;
            }
            let n = (i - start) as u16;
            out.push(OP_BLANKS);
            out.extend_from_slice(&n.to_le_bytes());
            continue;
        }
        let cell = &cells[i];
        let mut j = i + 1;
        while j < cells.len() && &cells[j] == cell && (j - i) < u16::MAX as usize {
            j += 1;
        }
        let n = j - i;
        if n >= 3 {
            out.push(OP_REPEAT);
            out.extend_from_slice(&(n as u16).to_le_bytes());
            write_cell(out, cell);
        } else {
            for c in &cells[i..j] {
                out.push(OP_CELL);
                write_cell(out, c);
            }
        }
        i = j;
    }
    Ok(())
}

fn read_rle_n(r: &mut Reader<'_>, n: usize) -> Result<Vec<CellSnap>, String> {
    let mut cells = Vec::with_capacity(n);
    while cells.len() < n {
        if r.is_empty() {
            break;
        }
        let op = r.read_u8()?;
        match op {
            OP_BLANKS => {
                let k = r.read_u16()? as usize;
                cells.resize(cells.len() + k, CellSnap::blank());
            }
            OP_CELL => cells.push(read_cell(r)?),
            OP_REPEAT => {
                let k = r.read_u16()? as usize;
                let cell = read_cell(r)?;
                for _ in 0..k {
                    cells.push(cell.clone());
                }
            }
            other => return Err(format!("bad RLE op {other:#x}")),
        }
    }
    cells.resize(n, CellSnap::blank());
    Ok(cells)
}

fn write_str(out: &mut Vec<u8>, s: &str) -> Result<(), String> {
    let b = s.as_bytes();
    if b.len() > u16::MAX as usize {
        return Err("string too long".into());
    }
    out.extend_from_slice(&(b.len() as u16).to_le_bytes());
    out.extend_from_slice(b);
    Ok(())
}

fn write_cell(out: &mut Vec<u8>, c: &CellSnap) {
    let ch = c.c as u32;
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&c.fg.to_le_bytes());
    out.extend_from_slice(&c.bg.to_le_bytes());
    out.push(c.style_byte());
}

fn read_cell(r: &mut Reader<'_>) -> Result<CellSnap, String> {
    let ch = char::from_u32(r.read_u32()?).unwrap_or(' ');
    let fg = r.read_u32()?;
    let bg = r.read_u32()?;
    let style = r.read_u8()?;
    Ok(CellSnap::from_parts(ch, fg, bg, style))
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }
    fn peek_u8(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.data.len() {
            return Err("truncated".into());
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.read_bytes(1)?[0])
    }
    fn read_u16(&mut self) -> Result<u16, String> {
        let b = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn read_u32(&mut self) -> Result<u32, String> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn read_u64(&mut self) -> Result<u64, String> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn read_string(&mut self) -> Result<String, String> {
        let len = self.read_u16()? as usize;
        let b = self.read_bytes(len)?;
        String::from_utf8(b.to_vec()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod bin_tests {
    use super::*;

    fn sample() -> GridSnapshot {
        let mut cells = vec![CellSnap::blank(); 80 * 3];
        cells[0] = CellSnap {
            c: 'H',
            fg: 0x00FF_0000,
            bg: 0xFFFF_FFFF,
            bold: true,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        };
        cells[1] = cells[0].clone();
        cells[2] = cells[0].clone();
        cells[80] = CellSnap {
            c: 'i',
            fg: 0xFFFF_FFFF,
            bg: 0x0000_00FF,
            bold: false,
            dim: true,
            italic: false,
            underline: false,
            inverse: false,
        };
        GridSnapshot {
            pane: "term-1".into(),
            rev: 42,
            cols: 80,
            rows: 3,
            cursor_col: 5,
            cursor_row: 1,
            cursor_shape_block: true,
            title: Some("hi".into()),
            running: true,
            cells,
            ghost: None,
            text: String::new(),
            alt_screen: true,
            alternate_scroll: false,
            app_cursor: true,
            mouse_mode: false,
            sgr_mouse: false,
            last_input_origin: None,
            hyperlinks: vec![],
        }
    }

    #[test]
    fn roundtrip_full() {
        let s = sample();
        let bin = encode_grid_bin(&s).unwrap();
        let d = decode_grid_bin(&bin).unwrap();
        assert_eq!(d.pane, s.pane);
        assert_eq!(d.rev, s.rev);
        assert_eq!(d.cells, s.cells);
        assert!(d.alt_screen && d.app_cursor);
    }

    #[test]
    fn roundtrip_damage() {
        let mut a = sample();
        let mut b = a.clone();
        b.rev = 43;
        b.cells[80].c = 'X';
        b.cursor_col = 9;
        let dirty = dirty_rows(&a.cells, &b.cells, 80, 3);
        assert_eq!(dirty, vec![1]);
        let bin = encode_grid_bin_ex(&b, Some(&dirty)).unwrap();
        assert!(bin.len() < encode_grid_bin(&b).unwrap().len());
        let d = decode_grid_bin_onto(&bin, Some(&a)).unwrap();
        assert_eq!(d.cells, b.cells);
        assert_eq!(d.cursor_col, 9);
        assert_eq!(d.rev, 43);
    }

    #[test]
    fn full_frame_clears_stale_hyperlinks() {
        // Base has a link; the new full frame has none. A FULL frame must be
        // authoritative and NOT resurrect the base's stale link.
        let mut base = sample();
        base.hyperlinks = vec![HyperlinkSpan {
            row: 0,
            col_start: 0,
            col_end: 3,
            uri: "https://example.com".into(),
        }];
        let mut next = sample();
        next.rev = 43;
        next.hyperlinks = vec![];
        let bin = encode_grid_bin(&next).unwrap();
        let d = decode_grid_bin_onto(&bin, Some(&base)).unwrap();
        assert!(
            d.hyperlinks.is_empty(),
            "full frame should clear stale links, got {:?}",
            d.hyperlinks
        );
    }

    #[test]
    fn damage_frame_inherits_hyperlinks() {
        // A damage frame omits the link table => inherit base's links.
        let mut base = sample();
        base.hyperlinks = vec![HyperlinkSpan {
            row: 0,
            col_start: 0,
            col_end: 3,
            uri: "https://example.com".into(),
        }];
        let mut next = base.clone();
        next.rev = 43;
        next.cells[80].c = 'X';
        let dirty = dirty_rows(&base.cells, &next.cells, 80, 3);
        let bin = encode_grid_bin_ex(&next, Some(&dirty)).unwrap();
        let d = decode_grid_bin_onto(&bin, Some(&base)).unwrap();
        assert_eq!(d.hyperlinks, base.hyperlinks, "damage should inherit links");
    }

    #[test]
    fn full_frame_roundtrips_hyperlinks() {
        let mut s = sample();
        s.hyperlinks = vec![HyperlinkSpan {
            row: 1,
            col_start: 2,
            col_end: 9,
            uri: "https://seance.example/x".into(),
        }];
        let bin = encode_grid_bin(&s).unwrap();
        let d = decode_grid_bin(&bin).unwrap();
        assert_eq!(d.hyperlinks, s.hyperlinks);
    }

    #[test]
    fn blanks_compress() {
        let s = sample();
        let bin = encode_grid_bin(&s).unwrap();
        assert!(bin.len() < 500, "got {}", bin.len());
    }

    #[test]
    fn smaller_than_json() {
        let s = sample();
        let bin = encode_grid_bin(&s).unwrap();
        let json = serde_json::to_vec(&s).unwrap();
        assert!(bin.len() < json.len() / 2);
    }
}
