//! Session-replay **player** — the shared-recording surface.
//!
//! A recording is a manifest plus one `SRR1` stream per pane
//! ([`seance_core::replay`]). The player is a small deterministic machine over
//! those streams:
//!
//! - a **virtual clock** `t` in recording time (unix ms, the daemon clock the
//!   records carry). A rAF stepper advances it by `wall_dt × speed` — *in
//!   compressed time* (see below) — and applies every record with
//!   `t_ms <= t`, in order, per pane.
//! - **grid records** fold into that pane's [`GridSnapshot`] via
//!   [`decode_grid_bin_onto`] (FULL decodes cold, DAMAGE needs the previous
//!   snapshot) and paint through the live client's [`TermRenderer`], verbatim —
//!   replay and live share one renderer so they cannot drift.
//! - **event records** drive the human-attention affordances: the input flash on
//!   a tile and the flame flash on the pane that receives a chapter.
//!
//! # Original resolution, always
//!
//! A recording has a fixed geometry (`cols × rows` per pane, as recorded). The
//! player never refits that grid to the browser: each canvas is painted at its
//! **recorded** size (`cols × cell_w`, `rows × cell_h` at [`FONT_PX`], DPR-aware)
//! and the whole tile workspace — borders and headers included — is scaled by a
//! single CSS `transform: scale(S)` to fit the viewport, letterboxed and
//! centered. `S` never exceeds 1.0 (we letterbox, we never upscale). A mid-recording
//! resize changes the snapshot's `cols`/`rows`, which re-derives the natural size
//! on the next size probe.
//!
//! # Idle compression — the timeline is ACTIVITY, not wall time
//!
//! Recordings are lossless; the *player* is what compresses dead air. At load
//! time the merged, time-sorted record stream (all panes) is folded into a
//! monotonic piecewise-linear [`TimeMap`]: every wall gap longer than
//! [`GAP_MAX_MS`] contributes exactly [`GAP_BEAT_MS`] of compressed time,
//! everything else contributes its real duration.
//!
//! The virtual clock, the timeline coordinate system, chapter ticks, seeks,
//! click-to-scrub and the elapsed/total readout all live in **compressed** ms.
//! Record application still keys off wall `t_ms` — conversion happens at that
//! boundary and nowhere else. Playback, scrubbing and flyTo all advance the
//! compressed clock, so "1×" means *as it happened, minus the dead air*.
//!
//! Seeking never replays from `t=0`: each pane keeps the record indices of its
//! `KIND_FULL` keyframes, so a seek resets to the nearest keyframe at or before
//! the target and applies forward from there (a forward seek that is already
//! past that keyframe just continues from the current cursor — no reset at all).
//!
//! # Playback
//!
//! There are no modes. Playback runs at an honest multiplier (1× / 1.5× / 2× /
//! 5×) over compressed time and never auto-pauses. Prompts are *navigation*:
//! ticks on the scrubber and the targets of the prev/next buttons, which
//! **flyTo** — an eased ~500-700ms scrub through compressed time that paints as
//! fast as decode allows and lands paused exactly on the chapter's submit stamp.
//!
//! # Time arguments
//!
//! Chapters carry absolute unix ms, but a share UI naturally talks in offsets
//! from the start of the recording. Every public `_ms` argument therefore
//! accepts **either**: a value below `from_ms` is read as an offset from
//! `from_ms`, anything else as absolute. [`Player::current_ms`] returns the
//! absolute clock (what the editor wants to persist).
//!
//! Public time is always **absolute wall ms** — [`Player::seek_ms`],
//! [`Player::current_ms`] and [`Player::set_range`] speak the same recording
//! clock the manifest and the chapters do. Compression is strictly internal;
//! callers never see a compressed value.
//!
//! # Position in the URL (bundle mode only)
//!
//! A shared recording is a link, so the paused position is part of the link:
//! whenever the player comes to rest in [`Source::Bundle`] mode it writes
//! `#t=<absolute wall ms>` with `history.replaceState`, and on load it seeks
//! there (paused). Playback never touches the URL, and neither does
//! [`Source::Bridge`] — in the editor the hash belongs to the editor route.
//!
//! # DOM
//!
//! Everything the player creates lives inside the caller's `mount`, and its CSS
//! is one injected `<style id="replay-style">` (the `menus.rs` pattern) — the
//! page's own stylesheet is never touched, so the same player drops into the
//! static share page and into the editor.
//!
//! ## web-sys features
//!
//! `Window::fetch_with_str` needs only `Window` (already on), but reading the
//! response does not:
//!
//! ```text
//! // NEEDS web-sys feature: Response
//! ```
//!
//! `Request` / `RequestInit` are deliberately **not** needed: both sources hand
//! us fully-qualified GET URLs (the bridge bakes its token and `from`/`to` into
//! the query string), so the string form of `fetch` is enough.

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use seance_core::replay::{
    records, Chapter, Manifest, ReplayEvent, KIND_DAMAGE, KIND_EVENT, KIND_FULL,
};
use seance_core::snapshot::{decode_grid_bin_onto, GridSnapshot};

use crate::renderer::{RenderOpts, TermRenderer};

// The player is self-contained on purpose (it is embedded by two different
// pages), so it carries its own copies of the font/palette constants rather
// than reaching into the live app module.
const FONT_FAMILY: &str =
    "ui-monospace, 'Cascadia Mono', 'CaskaydiaMono Nerd Font Mono', 'JetBrains Mono', monospace";
/// The recording's own font size — the natural-resolution basis, so it is a
/// constant of the *format*, not a display preference.
const FONT_PX: f32 = 14.0;

/// Selectable playback multipliers (over compressed time).
const SPEEDS: [f64; 4] = [1.0, 1.5, 2.0, 5.0];

/// flyTo duration floor / ceiling. A short hop still reads as a move; a
/// half-hour jump still lands inside a beat.
const FLY_MIN_MS: f64 = 500.0;
const FLY_MAX_MS: f64 = 700.0;
/// Compressed distance at which flyTo saturates at [`FLY_MAX_MS`].
const FLY_FULL_DIST_MS: f64 = 60_000.0;

/// Tile chrome outside the canvas: 1px border ×2 (both axes) plus the header
/// strip. Baked in so the natural-size math is a pure function.
const TILE_CHROME_W: f64 = 2.0;
const TILE_CHROME_H: f64 = 26.0;
/// Gap between tiles, and the stage padding around the scaled workspace.
const TILE_GAP: f64 = 10.0;

/// Input-flash decay on a tile after a human keystroke record.
const FLASH_MS: f64 = 420.0;
/// Flame flash on the tile that receives a chapter.
const CHAPTER_FLASH_MS: f64 = 1200.0;
/// A tab that was backgrounded must not "fast-forward" the wall gap.
const MAX_FRAME_DT_MS: f64 = 250.0;

/// A wall gap longer than this is dead air and gets collapsed.
const GAP_MAX_MS: u64 = 3_000;
/// …to exactly this much compressed time — a beat, not a jump cut.
const GAP_BEAT_MS: u64 = 1_500;

// ---------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------

/// Where recording bytes come from.
pub enum Source {
    /// Bundle mode: manifest at this URL; pane files resolved relative to it.
    Bundle { manifest_url: String },
    /// Editor mode: bridge endpoints (already token-qualified query strings).
    Bridge {
        manifest_url: String,
        /// Contains `{slug}`; `from`/`to` are already baked in.
        pane_url_template: String,
    },
}

impl Source {
    fn manifest_url(&self) -> &str {
        match self {
            Source::Bundle { manifest_url } => manifest_url,
            Source::Bridge { manifest_url, .. } => manifest_url,
        }
    }

    fn pane_url(&self, slug: &str, file: &str) -> String {
        match self {
            Source::Bundle { manifest_url } => join_relative(manifest_url, file),
            Source::Bridge {
                pane_url_template, ..
            } => pane_url_template.replace("{slug}", slug),
        }
    }
}

impl Source {
    fn is_bundle(&self) -> bool {
        matches!(self, Source::Bundle { .. })
    }
}

// ---------------------------------------------------------------------------
// pure logic (native-testable — nothing here touches web_sys)
// ---------------------------------------------------------------------------

/// One record, copied out of the borrowed [`seance_core::replay::Record`].
#[derive(Clone, Debug)]
pub struct OwnedRecord {
    pub kind: u8,
    pub t_ms: u64,
    pub payload: Vec<u8>,
}

/// `mm:ss` for a duration in ms (hours fold into minutes: `72:05`).
fn fmt_mmss(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// A coarse idle duration for the collapsed-gap tooltip: `"8s"` / `"14m"` /
/// `"1h 07m"`. Deliberately rounded — this labels dead air, not a benchmark.
fn fmt_idle(ms: u64) -> String {
    let s = ms / 1_000;
    if s < 90 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m", (s + 30) / 60)
    } else {
        format!("{}h {:02}m", s / 3_600, (s % 3_600) / 60)
    }
}

/// Collapse a prompt to one truncated line for the hover bubble.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

// --- wall ↔ compressed time ------------------------------------------------

/// One run of the piecewise-linear map. `collapsed` runs are the dead air:
/// their wall span exceeds [`GAP_MAX_MS`] and their compressed span is exactly
/// [`GAP_BEAT_MS`]. Every other run is 1:1.
#[derive(Clone, Copy, Debug, PartialEq)]
struct TimeSeg {
    wall_start: u64,
    wall_end: u64,
    comp_start: u64,
    comp_end: u64,
    collapsed: bool,
}

impl TimeSeg {
    fn wall_span(&self) -> u64 {
        self.wall_end - self.wall_start
    }
    fn comp_span(&self) -> u64 {
        self.comp_end - self.comp_start
    }
}

/// Monotonic wall ↔ compressed mapping for one recording.
///
/// Compressed time is measured from `0` at the map's wall origin, so only
/// *differences* are meaningful across the two clocks — which is all the UI
/// ever asks for. An empty map is the identity (the pre-load state): both
/// conversions pass their argument through, so every difference still holds.
#[derive(Clone, Debug, Default)]
struct TimeMap {
    segs: Vec<TimeSeg>,
}

/// Fold a merged, ascending record-time stream into a [`TimeMap`] spanning
/// `[lo, hi]`. `lo`/`hi` are part of the walk, so idle at the *edges* of the
/// window compresses exactly like idle in the middle.
fn build_time_map(sorted_times: &[u64], lo: u64, hi: u64) -> TimeMap {
    let hi = hi.max(lo);
    let mut pts: Vec<u64> = Vec::with_capacity(sorted_times.len() + 2);
    pts.push(lo);
    // The stream may run wider than the window (the bridge is generous); only
    // the interior matters, and it already arrives ascending.
    pts.extend(sorted_times.iter().copied().filter(|&t| t > lo && t < hi));
    pts.push(hi);
    pts.dedup();

    let mut segs: Vec<TimeSeg> = Vec::new();
    let mut comp = 0u64;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let span = b - a;
        let collapsed = span > GAP_MAX_MS;
        let cspan = if collapsed { GAP_BEAT_MS } else { span };
        // Consecutive live runs are one segment — the map stays O(gaps), not
        // O(records), which is what makes the binary search cheap.
        if !collapsed {
            if let Some(last) = segs.last_mut() {
                if !last.collapsed {
                    last.wall_end = b;
                    last.comp_end += cspan;
                    comp = last.comp_end;
                    continue;
                }
            }
        }
        segs.push(TimeSeg {
            wall_start: a,
            wall_end: b,
            comp_start: comp,
            comp_end: comp + cspan,
            collapsed,
        });
        comp += cspan;
    }
    if segs.is_empty() {
        // Empty / single-record stream: a degenerate but well-formed map, so
        // callers never have to special-case it.
        segs.push(TimeSeg {
            wall_start: lo,
            wall_end: hi,
            comp_start: 0,
            comp_end: hi - lo,
            collapsed: false,
        });
    }
    TimeMap { segs }
}

impl TimeMap {
    /// Total compressed duration — the "active" length of the whole recording.
    /// The UI asks for the *trim-scoped* version ([`Player::active_duration`]);
    /// this is the map's own invariant, exercised by the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    fn comp_total(&self) -> u64 {
        self.segs.last().map(|s| s.comp_end).unwrap_or(0)
    }

    /// Absolute wall ms → compressed ms. Saturates at both ends.
    fn wall_to_comp(&self, w: u64) -> u64 {
        let (Some(first), Some(last)) = (self.segs.first(), self.segs.last()) else {
            return w; // identity map (not loaded yet)
        };
        if w <= first.wall_start {
            return first.comp_start;
        }
        if w >= last.wall_end {
            return last.comp_end;
        }
        let i = self.segs.partition_point(|s| s.wall_start <= w) - 1;
        let s = &self.segs[i];
        if s.wall_span() == 0 {
            return s.comp_start;
        }
        s.comp_start + scale(w - s.wall_start, s.comp_span(), s.wall_span())
    }

    /// Compressed ms → absolute wall ms. Saturates at both ends.
    fn comp_to_wall(&self, c: u64) -> u64 {
        let (Some(first), Some(last)) = (self.segs.first(), self.segs.last()) else {
            return c; // identity map (not loaded yet)
        };
        if c <= first.comp_start {
            return first.wall_start;
        }
        if c >= last.comp_end {
            return last.wall_end;
        }
        let i = self.segs.partition_point(|s| s.comp_start <= c) - 1;
        let s = &self.segs[i];
        if s.comp_span() == 0 {
            return s.wall_start;
        }
        s.wall_start + scale(c - s.comp_start, s.wall_span(), s.comp_span())
    }

    /// Collapsed runs as `(compressed position, skipped wall ms)` — what the
    /// timeline draws its hash markers from.
    fn collapsed_gaps(&self) -> Vec<(u64, u64)> {
        self.segs
            .iter()
            .filter(|s| s.collapsed)
            .map(|s| (s.comp_start + s.comp_span() / 2, s.wall_span()))
            .collect()
    }
}

/// `v * num / den` in u128, so a multi-hour span cannot overflow.
fn scale(v: u64, num: u64, den: u64) -> u64 {
    if den == 0 {
        return 0;
    }
    ((v as u128 * num as u128) / den as u128) as u64
}

/// Resolve a bundle-relative file against the manifest URL.
fn join_relative(manifest_url: &str, file: &str) -> String {
    if file.starts_with("http://") || file.starts_with("https://") || file.starts_with('/') {
        return file.to_string();
    }
    // Query/fragment are not part of the path for resolution purposes.
    let base = manifest_url
        .split(['?', '#'])
        .next()
        .unwrap_or(manifest_url);
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..=i], file),
        None => file.to_string(),
    }
}

/// Index of the last entry `<= t`, over an ascending slice. `None` when every
/// entry is later than `t`.
fn last_at_or_before(times: &[u64], t: u64) -> Option<usize> {
    if times.is_empty() || times[0] > t {
        return None;
    }
    let (mut lo, mut hi) = (0usize, times.len() - 1);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        if times[mid] <= t {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

/// Index of the first entry `> t`, over an ascending slice.
fn first_after(times: &[u64], t: u64) -> Option<usize> {
    let start = match last_at_or_before(times, t) {
        Some(i) => i + 1,
        None => 0,
    };
    (start < times.len()).then_some(start)
}

/// The first chapter strictly inside `(t, target]` — what a playback frame
/// stepped over, and therefore what flashes its pane.
fn chapter_crossed(chapter_times: &[u64], t: u64, target: u64) -> Option<usize> {
    let i = first_after(chapter_times, t)?;
    (chapter_times[i] <= target).then_some(i)
}

/// The chapter *before* `t` (strictly), for ⏮.
fn chapter_before(chapter_times: &[u64], t: u64) -> Option<usize> {
    let i = chapter_times.partition_point(|&c| c < t);
    i.checked_sub(1)
}

// --- flyTo: an eased scrub, not a jump cut ---------------------------------

/// How long a flyTo over `dist` compressed ms should take. Short hops sit at
/// the floor, long ones ramp to — and stop at — the ceiling: distance buys
/// *speed*, never more of the viewer's time.
fn fly_duration_ms(dist_comp: u64) -> f64 {
    let f = (dist_comp as f64 / FLY_FULL_DIST_MS).clamp(0.0, 1.0);
    FLY_MIN_MS + (FLY_MAX_MS - FLY_MIN_MS) * f
}

/// Cubic ease-in-out over `p ∈ [0,1]`.
fn ease_in_out(p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    if p < 0.5 {
        4.0 * p * p * p
    } else {
        let q = -2.0 * p + 2.0;
        1.0 - q * q * q / 2.0
    }
}

/// Eased compressed position `p` of the way from `a` to `b` (either direction).
fn fly_position(a: u64, b: u64, p: f64) -> u64 {
    let e = ease_in_out(p);
    let (a, b) = (a as f64, b as f64);
    (a + (b - a) * e).round().max(0.0) as u64
}

// --- `#t=` fragment --------------------------------------------------------

/// Parse `#t=<absolute wall ms>`. Anything else — another route, a non-number,
/// no hash at all — is `None`, which the caller reads as "start of range".
fn parse_time_fragment(hash: &str) -> Option<u64> {
    let body = hash.strip_prefix('#').unwrap_or(hash);
    let v = body.strip_prefix("t=")?;
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<u64>().ok()
}

fn format_time_fragment(t_ms: u64) -> String {
    format!("#t={t_ms}")
}

// --- original-resolution layout --------------------------------------------

/// Natural (unscaled) size of the tile grid, given each pane's natural tile
/// size in CSS px and the column count. Columns take their widest tile and
/// rows their tallest, so the workspace framing survives ragged geometries.
fn natural_layout(tiles: &[(f64, f64)], cols: usize) -> (f64, f64) {
    if tiles.is_empty() || cols == 0 {
        return (0.0, 0.0);
    }
    let rows = tiles.len().div_ceil(cols);
    let mut col_w = vec![0f64; cols];
    let mut row_h = vec![0f64; rows];
    for (i, &(w, h)) in tiles.iter().enumerate() {
        let (c, r) = (i % cols, i / cols);
        col_w[c] = col_w[c].max(w);
        row_h[r] = row_h[r].max(h);
    }
    let used_cols = col_w.iter().filter(|w| **w > 0.0).count().max(1);
    (
        col_w.iter().sum::<f64>() + TILE_GAP * (used_cols.saturating_sub(1)) as f64,
        row_h.iter().sum::<f64>() + TILE_GAP * (rows.saturating_sub(1)) as f64,
    )
}

/// Letterbox scale: shrink to fit, never upscale past the recorded pixels.
fn fit_scale(natural: (f64, f64), avail: (f64, f64)) -> f64 {
    let (nw, nh) = natural;
    let (aw, ah) = avail;
    if nw <= 0.0 || nh <= 0.0 || aw <= 0.0 || ah <= 0.0 {
        return 1.0;
    }
    (aw / nw).min(ah / nh).min(1.0).max(0.02)
}

/// Accept an offset-from-start or an absolute stamp (see module docs).
fn absolutize(t: u64, from_ms: u64) -> u64 {
    if t < from_ms {
        from_ms.saturating_add(t)
    } else {
        t
    }
}

/// Near-square tile column count.
fn grid_cols(n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let mut c = 1;
    while c * c < n {
        c += 1;
    }
    c
}

/// gunzip when gzipped, pass through when the stream is already raw `SRR1`.
/// (The bundle ships `.srr.gz`; the bridge may serve either.)
fn decompress(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.starts_with(b"SRR1") {
        return Ok(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len() * 8);
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_end(&mut out)
        .map_err(|e| format!("gunzip failed: {e}"))?;
    Ok(out)
}

/// Copy an `SRR1` stream out into owned records (`records()` borrows).
fn parse_records(data: &[u8]) -> Result<Vec<OwnedRecord>, String> {
    let iter = records(data).ok_or_else(|| "not an SRR1 stream".to_string())?;
    Ok(iter
        .map(|r| OwnedRecord {
            kind: r.kind,
            t_ms: r.t_ms,
            payload: r.payload.to_vec(),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// per-pane state
// ---------------------------------------------------------------------------

struct PaneTrack {
    slug: String,
    name: String,
    recs: Vec<OwnedRecord>,
    /// `recs` timestamps, extracted for binary search.
    times: Vec<u64>,
    /// Record indices of `KIND_FULL` frames (keyframes) …
    keyframes: Vec<usize>,
    /// … and their timestamps, for the seek search.
    keyframe_times: Vec<u64>,
    /// Next record to apply.
    cursor: usize,
    snap: Option<GridSnapshot>,
    renderer: Option<TermRenderer>,
    canvas_id: String,
    dirty: bool,
    css_size: (f64, f64),
    /// Wall-clock ms after which the input flash is over (0 = not flashing).
    flash_until: f64,
    flashing: bool,
    /// Wall-clock ms after which the chapter-arrival flame flash is over.
    chapter_flash_until: f64,
    chapter_flashing: bool,
}

impl PaneTrack {
    fn new(slug: String, name: String, recs: Vec<OwnedRecord>) -> PaneTrack {
        let times = recs.iter().map(|r| r.t_ms).collect();
        let keyframes: Vec<usize> = recs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == KIND_FULL)
            .map(|(i, _)| i)
            .collect();
        let keyframe_times = keyframes.iter().map(|&i| recs[i].t_ms).collect();
        PaneTrack {
            canvas_id: format!("rp-canvas-{slug}"),
            slug,
            name,
            recs,
            times,
            keyframes,
            keyframe_times,
            cursor: 0,
            snap: None,
            renderer: None,
            dirty: true,
            css_size: (0.0, 0.0),
            flash_until: 0.0,
            flashing: false,
            chapter_flash_until: 0.0,
            chapter_flashing: false,
        }
    }

    /// Apply every record up to and including `target`. `announce` is called
    /// for human input records so the caller can flash the tile.
    fn advance_to(&mut self, target: u64, wall_now: f64) {
        while self.cursor < self.recs.len() && self.recs[self.cursor].t_ms <= target {
            let idx = self.cursor;
            self.cursor += 1;
            self.apply(idx, wall_now);
        }
    }

    fn apply(&mut self, idx: usize, wall_now: f64) {
        let rec = &self.recs[idx];
        match rec.kind {
            KIND_FULL => match decode_grid_bin_onto(&rec.payload, None) {
                Ok(s) => {
                    self.snap = Some(s);
                    self.dirty = true;
                }
                Err(_) => { /* corrupt keyframe: keep the last good screen */ }
            },
            KIND_DAMAGE => {
                // A DAMAGE frame before any keyframe (or after a decode miss)
                // has no base — dropping it is correct; the next FULL resyncs.
                if let Some(base) = self.snap.as_ref() {
                    if let Ok(s) = decode_grid_bin_onto(&rec.payload, Some(base)) {
                        self.snap = Some(s);
                        self.dirty = true;
                    }
                }
            }
            KIND_EVENT => {
                if wall_now > 0.0 {
                    if let Ok(ev) = serde_json::from_slice::<ReplayEvent>(&rec.payload) {
                        let human = match &ev {
                            ReplayEvent::Input { origin, .. } => origin == "human",
                            ReplayEvent::Send { submit, .. } => *submit,
                            _ => false,
                        };
                        if human {
                            self.flash_until = wall_now + FLASH_MS;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Reset to the nearest keyframe `<= target` and fold forward. Silent: no
    /// flashes, no caption — a seek is not playback.
    fn seek(&mut self, target: u64) {
        let current_t = self
            .cursor
            .checked_sub(1)
            .and_then(|i| self.recs.get(i))
            .map(|r| r.t_ms);
        let kf = last_at_or_before(&self.keyframe_times, target).map(|k| self.keyframes[k]);

        let can_continue = match (current_t, kf) {
            // Already past that keyframe and not moving backwards: the current
            // snapshot is a valid base, so fold only the delta.
            (Some(ct), Some(k)) => ct <= target && self.cursor > k && self.snap.is_some(),
            _ => false,
        };
        if !can_continue {
            match kf {
                Some(k) => {
                    self.cursor = k;
                    self.snap = None;
                }
                None => {
                    // Target precedes every keyframe: blank screen.
                    self.cursor = 0;
                    self.snap = None;
                    self.dirty = true;
                    return;
                }
            }
        }
        while self.cursor < self.recs.len() && self.recs[self.cursor].t_ms <= target {
            let idx = self.cursor;
            self.cursor += 1;
            self.apply(idx, 0.0);
        }
        self.flash_until = 0.0;
        self.chapter_flash_until = 0.0;
        self.dirty = true;
    }

    fn has_more(&self) -> bool {
        self.cursor < self.recs.len()
    }

    fn last_t(&self) -> u64 {
        self.times.last().copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------------

struct Dom {
    tiles: web_sys::Element,
    /// Wrapper that carries the `transform: scale(S)` — the tiles, their
    /// borders and their headers all live inside it, so the framing scales
    /// as one piece.
    scale_wrap: web_sys::Element,
    /// Centering box the wrapper is letterboxed inside.
    fit: web_sys::Element,
    timeline: web_sys::Element,
    played: web_sys::Element,
    playhead: web_sys::Element,
    ticks: web_sys::Element,
    tooltip: web_sys::Element,
    reset_btn: web_sys::Element,
    prev_btn: web_sys::Element,
    play_btn: web_sys::Element,
    next_btn: web_sys::Element,
    /// Speed group — hidden (not removed) while paused.
    speeds: web_sys::Element,
    speed_btns: Vec<web_sys::Element>,
    time_label: web_sys::Element,
    time_wall: web_sys::Element,
    status: web_sys::Element,
}

/// An in-flight flyTo. Positions are **compressed** ms; the landing is stored
/// in wall ms so the touchdown is exact rather than interpolated.
struct Fly {
    from_comp: u64,
    to_comp: u64,
    land_wall: u64,
    start: f64,
    dur: f64,
}

pub struct Player {
    dom: Dom,
    source: Source,
    manifest: Option<Manifest>,
    on_ready: Option<Box<dyn FnOnce(Result<Manifest, String>)>>,
    panes: Vec<PaneTrack>,
    chapters: Vec<Chapter>,
    chapter_times: Vec<u64>,
    /// Virtual clock, absolute **wall** recording ms. The clock *advances* in
    /// compressed time (`map`), but it is stored in wall ms because that is
    /// what record application, chapters and the public API all key off.
    t: u64,
    from_ms: u64,
    to_ms: u64,
    /// Wall ↔ compressed mapping over the loaded window. Built once, in
    /// [`Player::finish_load`]; trims re-use it (they only narrow `from`/`to`).
    map: TimeMap,
    /// Playback multiplier, one of [`SPEEDS`]. Survives pause/resume.
    speed: f64,
    playing: bool,
    /// Wall clock (performance.now) at the previous stepper frame.
    last_wall: f64,
    /// In-flight prompt-to-prompt scrub (⏮ / ⏭).
    fly: Option<Fly>,
    current_chapter: Option<usize>,
    pending_loads: usize,
    load_error: Option<String>,
    dragging: bool,
    /// Latest drag position, applied once per rAF.
    drag_x: Option<i32>,
    tiles_built: bool,
    last_css_probe: f64,
    /// Natural (unscaled) workspace size in CSS px, and the scale last applied.
    natural: (f64, f64),
    scale: f64,
}

impl Player {
    /// Build UI inside `mount` (an empty container element) and begin async
    /// load. `on_ready` fires with the loaded manifest (editor uses it).
    pub fn create(
        mount: web_sys::Element,
        source: Source,
        on_ready: Box<dyn FnOnce(Result<Manifest, String>)>,
    ) -> Rc<RefCell<Player>> {
        let doc = document();
        if let Some(d) = doc.as_ref() {
            ensure_style(d);
        }
        let dom = build_dom(&mount);

        let player = Rc::new(RefCell::new(Player {
            dom,
            source,
            manifest: None,
            on_ready: Some(on_ready),
            panes: Vec::new(),
            chapters: Vec::new(),
            chapter_times: Vec::new(),
            t: 0,
            from_ms: 0,
            to_ms: 0,
            map: TimeMap::default(),
            speed: SPEEDS[0],
            playing: false,
            last_wall: now_ms(),
            fly: None,
            current_chapter: None,
            pending_loads: 0,
            load_error: None,
            dragging: false,
            drag_x: None,
            tiles_built: false,
            last_css_probe: 0.0,
            natural: (0.0, 0.0),
            scale: 1.0,
        }));

        wire_controls(&player);
        start_load(&player);
        start_raf(&player);
        player
    }

    /// Resume at the **current** speed. A flyTo in flight is abandoned (the
    /// viewer changed their mind), and a play from the end rewinds first.
    pub fn play(&mut self) {
        self.fly = None;
        if self.t >= self.to_ms {
            self.seek_absolute(self.from_ms);
        }
        self.playing = true;
        self.last_wall = now_ms();
        self.sync_controls();
    }

    pub fn pause(&mut self) {
        self.fly = None;
        self.playing = false;
        self.sync_controls();
        self.publish_position();
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Seek to an **absolute wall** ms (or an offset from `from_ms`, see the
    /// module docs). Idle compression is internal — callers never convert.
    pub fn seek_ms(&mut self, t_ms: u64) {
        let t = absolutize(t_ms, self.from_ms);
        self.fly = None;
        self.seek_absolute(t);
    }

    /// Editor trim: playback + timeline clamp. Both bounds are **absolute
    /// wall** ms; the compressed timeline is re-derived from them.
    pub fn set_range(&mut self, from_ms: u64, to_ms: u64) {
        let (a, b) = (from_ms.min(to_ms), from_ms.max(to_ms));
        self.from_ms = a;
        self.to_ms = b.max(a + 1);
        let clamped = self.t.clamp(self.from_ms, self.to_ms);
        self.seek_absolute(clamped);
        self.rebuild_ticks();
        self.sync_controls();
    }

    /// Editor renames/drops.
    pub fn set_chapters(&mut self, chapters: Vec<Chapter>) {
        let mut chapters = chapters;
        chapters.sort_by_key(|c| c.t_ms);
        self.chapter_times = chapters.iter().map(|c| c.t_ms).collect();
        self.chapters = chapters;
        self.current_chapter = last_at_or_before(&self.chapter_times, self.t);
        self.rebuild_ticks();
    }

    /// The virtual clock as an **absolute wall** ms (what the editor persists).
    pub fn current_ms(&self) -> u64 {
        self.t
    }

    // -- internals ---------------------------------------------------------

    fn seek_absolute(&mut self, t: u64) {
        let t = t.clamp(self.from_ms, self.to_ms);
        self.t = t;
        for p in self.panes.iter_mut() {
            p.seek(t);
        }
        self.current_chapter = last_at_or_before(&self.chapter_times, t);
        self.last_wall = now_ms();
        self.sync_controls();
    }

    /// One rAF step: advance the clock (or the flyTo), fold records, paint.
    fn frame(&mut self) {
        let wall = now_ms();
        let dt = (wall - self.last_wall).clamp(0.0, MAX_FRAME_DT_MS);
        self.last_wall = wall;

        if let Some(x) = self.drag_x.take() {
            self.seek_from_client_x(x);
        }

        if self.fly.is_some() {
            self.step_fly(wall);
        } else if self.playing && !self.panes.is_empty() {
            // The clock advances in COMPRESSED ms; the wall target is whatever
            // that lands on. Dead air therefore always costs GAP_BEAT_MS of
            // watching, and everything live plays at the honest multiplier.
            let comp_now = self.map.wall_to_comp(self.t);
            let comp_target = comp_now.saturating_add((dt * self.speed).round().max(0.0) as u64);
            let mut target = self.map.comp_to_wall(comp_target).max(self.t);

            // Playback never stops at a prompt any more — it only flashes the
            // pane that received it.
            let crossed = chapter_crossed(&self.chapter_times, self.t, target);
            let mut ended = false;
            if target >= self.to_ms {
                target = self.to_ms;
                ended = true;
            }
            self.t = target;
            for p in self.panes.iter_mut() {
                p.advance_to(target, wall);
            }
            self.current_chapter = last_at_or_before(&self.chapter_times, target);
            if let Some(i) = crossed {
                if self.chapter_times[i] <= target {
                    self.flash_chapter(i);
                }
            }
            if ended {
                self.playing = false;
                self.sync_controls();
                self.publish_position();
            }
            self.update_progress();
        }

        for p in self.panes.iter_mut() {
            let on = p.flash_until > wall;
            if on != p.flashing {
                p.flashing = on;
                if let Some(tile) = tile_of(&p.canvas_id) {
                    let _ = tile.set_attribute("data-flash", if on { "1" } else { "0" });
                }
            }
            // The chapter flash fades via a CSS animation; clearing the flag at
            // the deadline is only what re-arms it for the next chapter.
            let ch_on = p.chapter_flash_until > wall;
            if ch_on != p.chapter_flashing {
                p.chapter_flashing = ch_on;
                if let Some(tile) = tile_of(&p.canvas_id) {
                    let _ = tile.set_attribute("data-chflash", if ch_on { "1" } else { "0" });
                }
            }
        }

        self.sync_sizes(wall);
        self.paint();
    }

    /// **Original resolution.** Each canvas is sized from its snapshot's
    /// recorded `cols × rows`, never from the browser box; the workspace is
    /// then letterboxed by one CSS transform. A mid-recording resize simply
    /// changes the snapshot geometry, so it re-derives here for free.
    ///
    /// Measuring forces layout, so it is throttled — natural sizes only change
    /// on a recorded resize, and the scale only on a window resize.
    fn sync_sizes(&mut self, wall: f64) {
        if wall - self.last_css_probe < 250.0 {
            return;
        }
        self.last_css_probe = wall;
        let Some(doc) = document() else { return };
        // One entry per pane, in pane order — a pane whose canvas is not in the
        // document yet contributes a zero cell rather than shifting the grid.
        let mut natural_tiles: Vec<(f64, f64)> = vec![(0.0, 0.0); self.panes.len()];
        for (i, p) in self.panes.iter_mut().enumerate() {
            let Some(el) = doc.get_element_by_id(&p.canvas_id) else {
                continue;
            };
            if p.renderer.is_none() {
                let canvas: web_sys::HtmlCanvasElement = match el.clone().dyn_into() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                match TermRenderer::new(canvas) {
                    Ok(mut r) => {
                        r.set_font(FONT_FAMILY, FONT_PX, device_pixel_ratio());
                        p.renderer = Some(r);
                    }
                    Err(e) => {
                        web_sys::console::error_2(&"replay renderer init failed".into(), &e);
                        continue;
                    }
                }
            }
            // Before the first keyframe lands there is no recorded geometry —
            // a conventional 80×24 keeps the frame from collapsing to nothing.
            let (cols, rows) = p
                .snap
                .as_ref()
                .map(|s| (s.cols.max(1) as f64, s.rows.max(1) as f64))
                .unwrap_or((80.0, 24.0));
            let (w, h) = match p.renderer.as_ref() {
                Some(r) => {
                    let (cw, ch) = r.cell_size_css();
                    (
                        (cols * cw as f64).round().max(8.0),
                        (rows * ch as f64).round().max(8.0),
                    )
                }
                None => continue,
            };
            if (w - p.css_size.0).abs() > 0.5 || (h - p.css_size.1).abs() > 0.5 {
                if let Some(r) = p.renderer.as_mut() {
                    r.resize_to(w, h);
                }
                p.css_size = (w, h);
                p.dirty = true;
                // The tile body is the canvas's box: pin it so the grid cell
                // takes the recorded size rather than stretching to the page.
                if let Some(body) = el.parent_element() {
                    set_style(&body, "width", &format!("{w}px"));
                    set_style(&body, "height", &format!("{h}px"));
                }
            }
            natural_tiles[i] = (w + TILE_CHROME_W, h + TILE_CHROME_H);
        }
        self.apply_fit(&natural_tiles);
    }

    /// Compute the natural workspace box, then letterbox it into the stage.
    fn apply_fit(&mut self, natural_tiles: &[(f64, f64)]) {
        if natural_tiles.is_empty() {
            return;
        }
        let cols = grid_cols(natural_tiles.len());
        let natural = natural_layout(natural_tiles, cols);
        if natural.0 <= 0.0 || natural.1 <= 0.0 {
            return;
        }
        let rect = self.dom.fit.get_bounding_client_rect();
        let s = fit_scale(natural, (rect.width(), rect.height()));
        let same_natural =
            (natural.0 - self.natural.0).abs() < 0.5 && (natural.1 - self.natural.1).abs() < 0.5;
        if same_natural && (s - self.scale).abs() < 0.0005 {
            return; // nothing moved — don't touch style and force a relayout
        }
        if !same_natural {
            set_style(&self.dom.tiles, "width", &format!("{}px", natural.0));
            set_style(&self.dom.tiles, "height", &format!("{}px", natural.1));
        }
        self.natural = natural;
        // The wrapper carries the *scaled* box so the flex parent can centre
        // it; the tiles inside keep their recorded pixels and just transform.
        self.scale = s;
        set_style(
            &self.dom.scale_wrap,
            "width",
            &format!("{:.2}px", natural.0 * s),
        );
        set_style(
            &self.dom.scale_wrap,
            "height",
            &format!("{:.2}px", natural.1 * s),
        );
        set_style(&self.dom.tiles, "transform", &format!("scale({s:.5})"));
    }

    fn paint(&mut self) {
        for p in self.panes.iter_mut() {
            if !p.dirty {
                continue;
            }
            let (Some(r), Some(snap)) = (p.renderer.as_mut(), p.snap.as_ref()) else {
                continue;
            };
            p.dirty = false;
            r.render(
                snap,
                &RenderOpts {
                    // A replay has no focus and no live selection; the cursor
                    // is part of the recording, so it is always drawn.
                    focused: false,
                    cursor_visible: true,
                    selection: None,
                },
            );
        }
    }

    /// Flame-flash the pane a chapter landed on. The player has no prompt
    /// chrome any more — this subtle cue is all that marks the crossing.
    fn flash_chapter(&mut self, idx: usize) {
        let Some(pane) = self.chapters.get(idx).map(|ch| ch.pane.clone()) else {
            return;
        };
        self.current_chapter = Some(idx);
        let wall = now_ms();
        for p in self.panes.iter_mut() {
            if p.slug == pane {
                p.chapter_flash_until = wall + CHAPTER_FLASH_MS;
            }
        }
    }

    // -- flyTo -------------------------------------------------------------

    /// Start an eased scrub from the current position to `target` (absolute
    /// wall ms), landing PAUSED. Distances are measured — and interpolated —
    /// in compressed time, so dead air costs a beat here exactly as it does
    /// during playback.
    fn fly_to(&mut self, target: u64) {
        let target = target.clamp(self.from_ms, self.to_ms);
        self.playing = false;
        let from_comp = self.map.wall_to_comp(self.t);
        let to_comp = self.map.wall_to_comp(target);
        if from_comp == to_comp {
            self.fly = None;
            self.seek_absolute(target);
            self.publish_position();
            return;
        }
        let dist = from_comp.abs_diff(to_comp);
        self.fly = Some(Fly {
            from_comp,
            to_comp,
            land_wall: target,
            start: now_ms(),
            dur: fly_duration_ms(dist),
        });
        self.sync_controls();
    }

    /// One flyTo frame. Painting is whatever the decode can do inside this rAF
    /// — the position comes from the *clock*, not from a frame counter, so a
    /// slow decode drops intermediate states instead of stretching the flight.
    fn step_fly(&mut self, wall: f64) {
        // Copied out: the rest of this method needs `&mut self`.
        let Some((from_comp, to_comp, land, start, dur)) = self
            .fly
            .as_ref()
            .map(|f| (f.from_comp, f.to_comp, f.land_wall, f.start, f.dur))
        else {
            return;
        };
        let p = if dur > 0.0 {
            ((wall - start) / dur).clamp(0.0, 1.0)
        } else {
            1.0
        };
        if p >= 1.0 {
            self.fly = None;
            self.seek_absolute(land);
            self.publish_position();
            return;
        }
        let comp = fly_position(from_comp, to_comp, p);
        let t = self.map.comp_to_wall(comp);
        self.seek_absolute(t);
    }

    /// ⏭ — the next chapter after the playhead, or the end of the range.
    fn go_next_chapter(&mut self) {
        let target = match first_after(&self.chapter_times, self.t) {
            Some(i) => self.chapter_times[i],
            None => self.to_ms,
        };
        self.fly_to(target);
    }

    /// ⏮ — the previous chapter, or the start of the recording when the
    /// playhead sits before the first one.
    fn go_prev_chapter(&mut self) {
        let target = match chapter_before(&self.chapter_times, self.t) {
            Some(i) => self.chapter_times[i],
            None => self.from_ms,
        };
        self.fly_to(target);
    }

    fn set_speed(&mut self, speed: f64) {
        self.speed = speed;
        self.sync_controls();
    }

    // -- URL state (bundle only) -------------------------------------------

    /// Write `#t=<wall ms>` for a position the player has come to REST at.
    /// Never during playback, never in bridge mode — the editor owns the hash
    /// there, and a hash that churned at 60fps would poison history and the
    /// back button alike.
    fn publish_position(&self) {
        if !self.source.is_bundle() || self.playing || self.fly.is_some() {
            return;
        }
        replace_fragment(Some(&format_time_fragment(self.t)));
    }

    /// ↺ — back to the start, paused, with the position dropped from the URL.
    fn reset(&mut self) {
        self.fly = None;
        self.playing = false;
        self.seek_absolute(self.from_ms);
        if self.source.is_bundle() {
            replace_fragment(None);
        }
        self.sync_controls();
    }

    /// Wall span of the trim window (what actually elapsed, idle included).
    fn duration(&self) -> u64 {
        self.to_ms.saturating_sub(self.from_ms).max(1)
    }

    /// Compressed span of the trim window — the length of the timeline.
    fn active_duration(&self) -> u64 {
        self.map
            .wall_to_comp(self.to_ms)
            .saturating_sub(self.map.wall_to_comp(self.from_ms))
            .max(1)
    }

    /// Compressed offset of `t` inside the trim window.
    fn active_elapsed(&self, t: u64) -> u64 {
        self.map
            .wall_to_comp(t)
            .saturating_sub(self.map.wall_to_comp(self.from_ms))
    }

    fn update_progress(&self) {
        let elapsed = self.active_elapsed(self.t);
        let total = self.active_duration();
        let frac = (elapsed as f64 / total as f64).clamp(0.0, 1.0);
        set_style(&self.dom.played, "width", &format!("{:.4}%", frac * 100.0));
        set_style(&self.dom.playhead, "left", &format!("{:.4}%", frac * 100.0));
        self.dom
            .time_label
            .set_text_content(Some(&format!("{} / {}", fmt_mmss(elapsed), fmt_mmss(total))));
        // The wall suffix only earns its pixels when the recording is mostly
        // dead air; at parity it would be noise.
        let wall_total = self.duration();
        self.dom
            .time_wall
            .set_text_content(Some(&if wall_total > total.saturating_mul(2) {
                format!(" · {} wall", fmt_mmss(wall_total))
            } else {
                String::new()
            }));
    }

    fn sync_controls(&self) {
        self.dom
            .play_btn
            .set_text_content(Some(if self.playing { "⏸" } else { "▶" }));
        // Progressive disclosure: the speed group is meaningless while paused,
        // so it hides — with `visibility`, which keeps its box and therefore
        // keeps the bar from twitching every time playback stops.
        let _ = self
            .dom
            .speeds
            .set_attribute("data-show", if self.playing { "1" } else { "0" });
        for (i, b) in self.dom.speed_btns.iter().enumerate() {
            let on = SPEEDS.get(i).map(|s| *s == self.speed).unwrap_or(false);
            let _ = b.set_attribute("data-on", if on { "1" } else { "0" });
        }
        self.update_progress();
    }

    /// Fraction along the (compressed) timeline for an absolute wall stamp.
    fn frac_of(&self, t_ms: u64) -> f64 {
        (self.active_elapsed(t_ms) as f64 / self.active_duration() as f64).clamp(0.0, 1.0)
    }

    /// Chapter ticks *and* collapsed-idle hash markers — both positioned in
    /// compressed time, which is the only coordinate the bar knows.
    fn rebuild_ticks(&self) {
        let Some(doc) = document() else { return };
        self.dom.ticks.set_text_content(Some(""));
        for (comp_mid, skipped) in self.map.collapsed_gaps() {
            let wall_mid = self.map.comp_to_wall(comp_mid);
            if wall_mid < self.from_ms || wall_mid > self.to_ms {
                continue;
            }
            let Ok(gap) = doc.create_element("div") else {
                continue;
            };
            gap.set_class_name("rp-gap");
            set_style(&gap, "left", &format!("{:.4}%", self.frac_of(wall_mid) * 100.0));
            let _ = gap.set_attribute("data-skipped", &skipped.to_string());
            append(&self.dom.ticks, &gap);
        }
        for (i, ch) in self.chapters.iter().enumerate() {
            if ch.t_ms < self.from_ms || ch.t_ms > self.to_ms {
                continue;
            }
            let Ok(tick) = doc.create_element("div") else {
                continue;
            };
            tick.set_class_name("rp-tick");
            set_style(&tick, "left", &format!("{:.4}%", self.frac_of(ch.t_ms) * 100.0));
            let _ = tick.set_attribute("data-idx", &i.to_string());
            append(&self.dom.ticks, &tick);
        }
    }

    /// Timeline geometry → compressed time → recording time.
    fn seek_from_client_x(&mut self, client_x: i32) {
        let rect = self.dom.timeline.get_bounding_client_rect();
        if rect.width() <= 0.0 {
            return;
        }
        self.fly = None;
        let frac = ((client_x as f64 - rect.x()) / rect.width()).clamp(0.0, 1.0);
        let comp = self.map.wall_to_comp(self.from_ms)
            + (frac * self.active_duration() as f64).round() as u64;
        let t = self.map.comp_to_wall(comp);
        self.seek_absolute(t);
    }

    /// Hover bubble: the compressed elapsed under the cursor, plus the nearest
    /// chapter (or collapsed gap) when one is within 8px. One line — the bar
    /// is a scrubber, not a reading surface.
    fn hover_timeline(&self, client_x: i32) {
        let rect = self.dom.timeline.get_bounding_client_rect();
        if rect.width() <= 0.0 {
            return;
        }
        let frac = ((client_x as f64 - rect.x()) / rect.width()).clamp(0.0, 1.0);
        let at_comp = (frac * self.active_duration() as f64).round() as u64;
        let x_of = |t: u64| rect.x() + rect.width() * self.frac_of(t);

        // (distance, label) — chapters win ties by being checked first with a
        // strict `<`.
        let mut best: Option<(f64, String)> = None;
        for ch in self.chapters.iter() {
            if ch.t_ms < self.from_ms || ch.t_ms > self.to_ms {
                continue;
            }
            let d = (x_of(ch.t_ms) - client_x as f64).abs();
            if d <= 8.0 && best.as_ref().map(|(bd, _)| d < *bd).unwrap_or(true) {
                best = Some((d, one_line(&ch.text, 60)));
            }
        }
        for (comp_mid, skipped) in self.map.collapsed_gaps() {
            let wall_mid = self.map.comp_to_wall(comp_mid);
            if wall_mid < self.from_ms || wall_mid > self.to_ms {
                continue;
            }
            let d = (x_of(wall_mid) - client_x as f64).abs();
            if d <= 8.0 && best.as_ref().map(|(bd, _)| d < *bd).unwrap_or(true) {
                best = Some((d, format!("skipped {} idle", fmt_idle(skipped))));
            }
        }
        let text = match best {
            Some((_, label)) => format!("{}  ·  {}", fmt_mmss(at_comp), label),
            None => fmt_mmss(at_comp),
        };
        self.dom.tooltip.set_text_content(Some(&text));
        set_style(
            &self.dom.tooltip,
            "left",
            &format!("{:.4}%", frac * 100.0),
        );
        let _ = self.dom.tooltip.set_attribute("data-show", "1");
    }

    fn set_status(&self, text: &str) {
        self.dom.status.set_text_content(Some(text));
        let _ = self
            .dom
            .status
            .set_attribute("data-show", if text.is_empty() { "0" } else { "1" });
    }

    /// Called once every pane stream has landed (or failed).
    fn finish_load(&mut self) {
        let Some(m) = self.manifest.clone() else {
            return;
        };
        self.from_ms = m.from_ms;
        // A manifest with a bogus range still plays: fall back to the last
        // record we actually hold.
        let last = self.panes.iter().map(|p| p.last_t()).max().unwrap_or(0);
        self.to_ms = m.to_ms.max(self.from_ms + 1).max(last);
        // Idle compression is a property of the *recording*, so the map spans
        // the whole loaded window and survives every later trim.
        let mut merged: Vec<u64> = self.panes.iter().flat_map(|p| p.times.iter().copied()).collect();
        merged.sort_unstable();
        self.map = build_time_map(&merged, self.from_ms, self.to_ms);
        self.build_tiles();
        self.set_chapters(m.chapters.clone());
        self.seek_absolute(self.from_ms);
        self.set_status("");
        self.sync_controls();
        if let Some(cb) = self.on_ready.take() {
            match self.load_error.clone() {
                Some(e) if self.panes.is_empty() => cb(Err(e)),
                _ => cb(Ok(m)),
            }
        }
        // Bundle mode restores the shared position; everything else opens
        // paused on the first frame of the range.
        if self.source.is_bundle() {
            if let Some(t) = location_hash().as_deref().and_then(parse_time_fragment) {
                self.seek_absolute(t);
            }
        }
    }

    fn build_tiles(&mut self) {
        if self.tiles_built {
            return;
        }
        self.tiles_built = true;
        let Some(doc) = document() else { return };
        self.dom.tiles.set_text_content(Some(""));
        let cols = grid_cols(self.panes.len());
        // `max-content`, not `1fr`: tiles are sized by the recording, and the
        // whole grid is what gets scaled.
        set_style(
            &self.dom.tiles,
            "grid-template-columns",
            &format!("repeat({cols}, max-content)"),
        );
        for p in self.panes.iter() {
            let Ok(tile) = doc.create_element("div") else {
                continue;
            };
            tile.set_class_name("rtile");
            let _ = tile.set_attribute("data-slug", &p.slug);
            let _ = tile.set_attribute("data-flash", "0");
            if let Ok(head) = doc.create_element("div") {
                head.set_class_name("rtile-head");
                if let Ok(dot) = doc.create_element("span") {
                    dot.set_class_name("rtile-dot");
                    append(&head, &dot);
                }
                if let Ok(name) = doc.create_element("span") {
                    name.set_class_name("rtile-name");
                    name.set_text_content(Some(&p.name));
                    append(&head, &name);
                }
                if let Ok(slug) = doc.create_element("span") {
                    slug.set_class_name("rtile-slug");
                    slug.set_text_content(Some(&p.slug));
                    append(&head, &slug);
                }
                append(&tile, &head);
            }
            if let Ok(wrap) = doc.create_element("div") {
                wrap.set_class_name("rtile-body");
                if let Ok(canvas) = doc.create_element("canvas") {
                    canvas.set_id(&p.canvas_id);
                    canvas.set_class_name("rtile-canvas");
                    append(&wrap, &canvas);
                }
                append(&tile, &wrap);
            }
            append(&self.dom.tiles, &tile);
        }
    }
}

// ---------------------------------------------------------------------------
// loading (raw fetch + Promise callbacks — no wasm-bindgen-futures)
// ---------------------------------------------------------------------------

/// `promise.then(ok, err)` with owned closures. Load promises are one-shot and
/// bounded (1 manifest + 1 per pane), so leaking their closures is cheaper than
/// keeping a registry alive for the page's lifetime.
fn then2(promise: &js_sys::Promise, ok: Box<dyn FnMut(JsValue)>, err: Box<dyn FnMut(JsValue)>) {
    let okc = Closure::<dyn FnMut(JsValue)>::wrap_assert_unwind_safe(ok);
    let errc = Closure::<dyn FnMut(JsValue)>::wrap_assert_unwind_safe(err);
    let _ = promise.then2(&okc, &errc);
    okc.forget();
    errc.forget();
}

/// GET `url` and hand the body back as bytes.
///
/// NEEDS web-sys feature: `Response` (`Response::ok`, `Response::array_buffer`).
fn fetch_bytes(url: &str, done: Rc<RefCell<Option<Box<dyn FnOnce(Result<Vec<u8>, String>)>>>>) {
    let Some(win) = web_sys::window() else {
        if let Some(cb) = done.borrow_mut().take() {
            cb(Err("no window".into()));
        }
        return;
    };
    let url_owned = url.to_string();
    let promise = win.fetch_with_str(url);

    let d_ok = Rc::clone(&done);
    let d_err = Rc::clone(&done);
    let url_err = url_owned.clone();
    then2(
        &promise,
        Box::new(move |v: JsValue| {
            let resp = match v.dyn_into::<web_sys::Response>() {
                Ok(r) => r,
                Err(_) => {
                    if let Some(cb) = d_ok.borrow_mut().take() {
                        cb(Err(format!("{url_owned}: not a Response")));
                    }
                    return;
                }
            };
            if !resp.ok() {
                if let Some(cb) = d_ok.borrow_mut().take() {
                    cb(Err(format!("{url_owned}: HTTP {}", resp.status())));
                }
                return;
            }
            let buf_promise = match resp.array_buffer() {
                Ok(p) => p,
                Err(_) => {
                    if let Some(cb) = d_ok.borrow_mut().take() {
                        cb(Err(format!("{url_owned}: body unreadable")));
                    }
                    return;
                }
            };
            let d2 = Rc::clone(&d_ok);
            let d2e = Rc::clone(&d_ok);
            let u2 = url_owned.clone();
            then2(
                &buf_promise,
                Box::new(move |b: JsValue| {
                    let bytes = js_sys::Uint8Array::new(&b).to_vec();
                    if let Some(cb) = d2.borrow_mut().take() {
                        cb(Ok(bytes));
                    }
                }),
                Box::new(move |_e: JsValue| {
                    if let Some(cb) = d2e.borrow_mut().take() {
                        cb(Err(format!("{u2}: arrayBuffer rejected")));
                    }
                }),
            );
        }),
        Box::new(move |_e: JsValue| {
            if let Some(cb) = d_err.borrow_mut().take() {
                cb(Err(format!("{url_err}: fetch failed")));
            }
        }),
    );
}

fn once(f: impl FnOnce(Result<Vec<u8>, String>) + 'static) -> Rc<RefCell<Option<Box<dyn FnOnce(Result<Vec<u8>, String>)>>>> {
    Rc::new(RefCell::new(Some(
        Box::new(f) as Box<dyn FnOnce(Result<Vec<u8>, String>)>
    )))
}

fn start_load(player: &Rc<RefCell<Player>>) {
    let url = {
        let p = player.borrow();
        p.set_status("loading recording…");
        p.source.manifest_url().to_string()
    };
    let pl = Rc::clone(player);
    fetch_bytes(
        &url,
        once(move |res| {
            let manifest = res.and_then(|bytes| {
                serde_json::from_slice::<Manifest>(&bytes)
                    .map_err(|e| format!("bad manifest json: {e}"))
            });
            match manifest {
                Ok(m) => on_manifest(&pl, m),
                Err(e) => {
                    let cb = {
                        let mut p = pl.borrow_mut();
                        p.set_status(&format!("could not load recording — {e}"));
                        p.load_error = Some(e.clone());
                        p.on_ready.take()
                    };
                    if let Some(cb) = cb {
                        cb(Err(e));
                    }
                }
            }
        }),
    );
}

fn on_manifest(player: &Rc<RefCell<Player>>, m: Manifest) {
    let pane_urls: Vec<(String, String, String)> = {
        let mut p = player.borrow_mut();
        p.set_status(&format!("loading {} pane(s)…", m.panes.len()));
        p.pending_loads = m.panes.len();
        let urls = m
            .panes
            .iter()
            .map(|pm| {
                (
                    pm.slug.clone(),
                    pm.name.clone(),
                    p.source.pane_url(&pm.slug, &pm.file),
                )
            })
            .collect();
        p.manifest = Some(m);
        urls
    };

    if pane_urls.is_empty() {
        player.borrow_mut().finish_load();
        return;
    }
    for (slug, name, url) in pane_urls {
        let pl = Rc::clone(player);
        fetch_bytes(
            &url,
            once(move |res| {
                let parsed = res
                    .and_then(decompress)
                    .and_then(|raw| parse_records(&raw));
                let mut p = pl.borrow_mut();
                match parsed {
                    Ok(recs) => p.panes.push(PaneTrack::new(slug.clone(), name, recs)),
                    Err(e) => {
                        web_sys::console::warn_1(&format!("replay: pane {slug}: {e}").into());
                        p.load_error = Some(e);
                    }
                }
                p.pending_loads = p.pending_loads.saturating_sub(1);
                if p.pending_loads == 0 {
                    p.finish_load();
                }
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// DOM construction + wiring
// ---------------------------------------------------------------------------

fn document() -> Option<web_sys::Document> {
    web_sys::window().and_then(|w| w.document())
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

fn location_hash() -> Option<String> {
    web_sys::window()?.location().hash().ok()
}

/// Rewrite the fragment in place — no history entry, no scroll, no reload.
/// `None` drops the fragment entirely (path + query only).
///
/// NEEDS web-sys feature: `History` (`History::replace_state_with_url`),
/// `Location` (`pathname`, `search`). Both are already enabled.
fn replace_fragment(frag: Option<&str>) {
    let Some(win) = web_sys::window() else { return };
    let loc = win.location();
    let path = loc.pathname().unwrap_or_default();
    let search = loc.search().unwrap_or_default();
    let url = format!("{path}{search}{}", frag.unwrap_or(""));
    if let Ok(h) = win.history() {
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&url));
    }
}

fn device_pixel_ratio() -> f64 {
    web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0)
}

/// `Element` → `Node` without leaning on deref coercion through `?`.
fn append(parent: &web_sys::Element, child: &web_sys::Element) {
    let p: &web_sys::Node = parent.unchecked_ref();
    let c: &web_sys::Node = child.unchecked_ref();
    let _ = p.append_child(c);
}

fn set_style(el: &web_sys::Element, prop: &str, value: &str) {
    if let Some(h) = el.dyn_ref::<web_sys::HtmlElement>() {
        let _ = h.style().set_property(prop, value);
    }
}

fn tile_of(canvas_id: &str) -> Option<web_sys::Element> {
    document()
        .and_then(|d| d.get_element_by_id(canvas_id))
        .and_then(|c| c.closest(".rtile").ok().flatten())
}

fn el(doc: &web_sys::Document, tag: &str, class: &str) -> web_sys::Element {
    let e = doc
        .create_element(tag)
        .unwrap_or_else(|_| doc.create_element("div").expect("create_element div"));
    e.set_class_name(class);
    e
}

fn build_dom(mount: &web_sys::Element) -> Dom {
    let doc = document().expect("document");
    mount.set_text_content(Some(""));

    let root = el(&doc, "div", "rp-root");
    let body = el(&doc, "div", "rp-body");

    // Stage: a centering box (`rp-fit`) holding the scaled workspace.
    let stage = el(&doc, "div", "rp-stage");
    let fit = el(&doc, "div", "rp-fit");
    let scale_wrap = el(&doc, "div", "rp-scale");
    let tiles = el(&doc, "div", "rp-tiles");
    let status = el(&doc, "div", "rp-status");
    let _ = status.set_attribute("data-show", "1");
    status.set_text_content(Some("loading recording…"));
    append(&scale_wrap, &tiles);
    append(&fit, &scale_wrap);
    append(&stage, &fit);
    append(&stage, &status);
    append(&body, &stage);

    // Control bar
    let bar = el(&doc, "div", "rp-bar");
    // The hit area is the whole 24px strip; the visible track lives inside it.
    let timeline = el(&doc, "div", "rp-timeline");
    let track = el(&doc, "div", "rp-track");
    let played = el(&doc, "div", "rp-played");
    let ticks = el(&doc, "div", "rp-ticks");
    let playhead = el(&doc, "div", "rp-playhead");
    let tooltip = el(&doc, "div", "rp-tip");
    let _ = tooltip.set_attribute("data-show", "0");
    append(&timeline, &track);
    append(&timeline, &played);
    append(&timeline, &ticks);
    append(&timeline, &playhead);
    append(&timeline, &tooltip);

    let controls = el(&doc, "div", "rp-controls");
    let reset_btn = el(&doc, "button", "rp-btn rp-reset");
    reset_btn.set_text_content(Some("↺"));
    let _ = reset_btn.set_attribute("title", "back to the start");
    let prev_btn = el(&doc, "button", "rp-btn rp-nav rp-prev");
    prev_btn.set_text_content(Some("⏮"));
    let _ = prev_btn.set_attribute("title", "previous prompt");
    let play_btn = el(&doc, "button", "rp-btn rp-play");
    play_btn.set_text_content(Some("▶"));
    let next_btn = el(&doc, "button", "rp-btn rp-nav rp-next");
    next_btn.set_text_content(Some("⏭"));
    let _ = next_btn.set_attribute("title", "next prompt");

    let speeds = el(&doc, "div", "rp-speeds");
    let _ = speeds.set_attribute("data-show", "0");
    let mut speed_btns = Vec::with_capacity(SPEEDS.len());
    for (i, s) in SPEEDS.iter().enumerate() {
        let b = el(&doc, "button", "rp-btn rp-speed");
        b.set_text_content(Some(&fmt_speed(*s)));
        let _ = b.set_attribute("data-speed", &i.to_string());
        let _ = b.set_attribute("data-on", if i == 0 { "1" } else { "0" });
        append(&speeds, &b);
        speed_btns.push(b);
    }

    let time_label = el(&doc, "div", "rp-time");
    time_label.set_text_content(Some("0:00 / 0:00"));
    let time_wall = el(&doc, "div", "rp-time-wall");
    let spacer = el(&doc, "div", "rp-spacer");
    let chip = el(&doc, "div", "rp-chip");
    chip.set_text_content(Some("recorded with seance ✦"));
    append(&controls, &reset_btn);
    append(&controls, &prev_btn);
    append(&controls, &play_btn);
    append(&controls, &next_btn);
    append(&controls, &speeds);
    append(&controls, &time_label);
    append(&controls, &time_wall);
    append(&controls, &spacer);
    append(&controls, &chip);

    append(&bar, &timeline);
    append(&bar, &controls);

    append(&root, &body);
    append(&root, &bar);
    append(mount, &root);

    Dom {
        tiles,
        scale_wrap,
        fit,
        timeline,
        played,
        playhead,
        ticks,
        tooltip,
        reset_btn,
        prev_btn,
        play_btn,
        next_btn,
        speeds,
        speed_btns,
        time_label,
        time_wall,
        status,
    }
}

/// `1×` / `1.5×` — no trailing `.0`.
fn fmt_speed(s: f64) -> String {
    if (s - s.round()).abs() < 1e-9 {
        format!("{}×", s.round() as u32)
    } else {
        format!("{s}×")
    }
}

fn wire_controls(player: &Rc<RefCell<Player>>) {
    let (reset_btn, prev_btn, play_btn, next_btn, speeds, timeline) = {
        let p = player.borrow();
        (
            p.dom.reset_btn.clone(),
            p.dom.prev_btn.clone(),
            p.dom.play_btn.clone(),
            p.dom.next_btn.clone(),
            p.dom.speeds.clone(),
            p.dom.timeline.clone(),
        )
    };

    on_click(&reset_btn, player, |p, _ev| p.reset());
    on_click(&prev_btn, player, |p, _ev| p.go_prev_chapter());
    on_click(&next_btn, player, |p, _ev| p.go_next_chapter());

    // play / pause
    on_click(&play_btn, player, |p, _ev| {
        if p.is_playing() {
            p.pause();
        } else {
            p.play();
        }
    });

    // speed group (delegated — one listener, four buttons)
    on_click(&speeds, player, |p, ev| {
        let Some(i) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|e| e.closest(".rp-speed").ok().flatten())
            .and_then(|b| b.get_attribute("data-speed"))
            .and_then(|s| s.parse::<usize>().ok())
        else {
            return;
        };
        if let Some(s) = SPEEDS.get(i).copied() {
            p.set_speed(s);
        }
    });

    // timeline: click / drag to seek, hover for the prompt tooltip
    {
        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            if let Ok(mut p) = pl.try_borrow_mut() {
                p.dragging = true;
                p.seek_from_client_x(ev.client_x());
            }
        });
        let _ = timeline.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    {
        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            if let Ok(p) = pl.try_borrow() {
                p.hover_timeline(ev.client_x());
            }
        });
        let _ = timeline.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    {
        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |_ev: web_sys::MouseEvent| {
            if let Ok(p) = pl.try_borrow() {
                let _ = p.dom.tooltip.set_attribute("data-show", "0");
            }
        });
        let _ = timeline.add_event_listener_with_callback("mouseleave", cb.as_ref().unchecked_ref());
        cb.forget();
    }
    if let Some(doc) = document() {
        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            if let Ok(mut p) = pl.try_borrow_mut() {
                if p.dragging {
                    // Painted on the next rAF, not here: mousemove fires far
                    // faster than a decode+render is worth.
                    p.drag_x = Some(ev.client_x());
                }
            }
        });
        let _ = doc.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref());
        cb.forget();

        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            if let Ok(mut p) = pl.try_borrow_mut() {
                if p.dragging {
                    p.dragging = false;
                    p.drag_x = None;
                    p.seek_from_client_x(ev.client_x());
                    // Release while paused is a resting position, so it is
                    // shareable.
                    p.publish_position();
                }
            }
        });
        let _ = doc.add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // window resize: force a re-measure on the next frame
    if let Some(win) = web_sys::window() {
        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_ev: web_sys::Event| {
            if let Ok(mut p) = pl.try_borrow_mut() {
                p.last_css_probe = 0.0;
            }
        });
        let _ = win.add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

fn on_click(
    el: &web_sys::Element,
    player: &Rc<RefCell<Player>>,
    f: impl Fn(&mut Player, &web_sys::MouseEvent) + 'static,
) {
    let pl = Rc::clone(player);
    let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        if let Ok(mut p) = pl.try_borrow_mut() {
            f(&mut p, &ev);
        }
    });
    let _ = el.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Self-perpetuating rAF stepper (the `lib.rs` pattern). The closure cycle
/// keeps itself — and the player — alive for the page lifetime.
fn start_raf(player: &Rc<RefCell<Player>>) {
    let Some(win) = web_sys::window() else { return };
    let raf: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let raf2 = Rc::clone(&raf);
    let pl = Rc::clone(player);
    *raf.borrow_mut() = Some(Closure::new(move || {
        if let Ok(mut p) = pl.try_borrow_mut() {
            p.frame();
        }
        if let (Some(w), Some(cb)) = (web_sys::window(), raf2.borrow().as_ref()) {
            let _ = w.request_animation_frame(cb.as_ref().unchecked_ref());
        }
    }));
    if let Some(cb) = raf.borrow().as_ref() {
        let _ = win.request_animation_frame(cb.as_ref().unchecked_ref());
    }
    std::mem::forget(raf);
}

// ---------------------------------------------------------------------------
// styles (self-contained injection — www/style.css is not ours)
// ---------------------------------------------------------------------------

fn ensure_style(doc: &web_sys::Document) {
    if doc.get_element_by_id("replay-style").is_some() {
        return;
    }
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    style.set_id("replay-style");
    style.set_text_content(Some(REPLAY_CSS));
    if let Some(head) = doc.head() {
        let h: &web_sys::Node = head.unchecked_ref();
        let s: &web_sys::Node = style.unchecked_ref();
        let _ = h.append_child(s);
    }
}

const REPLAY_CSS: &str = r#"
.rp-root{--rp-bg:#131111;--rp-elev:#1C1718;--rp-surf:#211C1D;--rp-border:#352C2E;
--rp-text:#EBE3DB;--rp-dim:#A69A91;--rp-faint:#69605D;--rp-flame:#E9A03A;
--rp-flame-dim:#A97328;
position:absolute;inset:0;display:flex;flex-direction:column;background:var(--rp-bg);
color:var(--rp-text);font:13px/1.45 ui-sans-serif,system-ui,-apple-system,'Segoe UI',sans-serif;
overflow:hidden;}
.rp-body{flex:1;display:flex;min-height:0;}

.rp-stage{position:relative;flex:1;min-width:0;min-height:0;}
/* Letterbox: the workspace keeps its recorded pixels and is centred inside. */
.rp-fit{position:absolute;inset:10px;display:flex;align-items:center;
justify-content:center;overflow:hidden;}
.rp-scale{position:relative;flex:0 0 auto;}
.rp-tiles{position:absolute;left:0;top:0;display:grid;gap:10px;
transform-origin:0 0;will-change:transform;}
.rtile{display:flex;flex-direction:column;background:var(--rp-bg);
border:1px solid var(--rp-border);border-radius:7px;overflow:hidden;
box-shadow:0 6px 22px rgba(0,0,0,.35);transition:border-color .18s ease,box-shadow .18s ease;}
.rtile[data-flash="1"]{border-color:var(--rp-flame);
box-shadow:0 0 0 1px rgba(233,160,58,.35),0 6px 26px rgba(233,160,58,.14);}
/* Fixed 24px + the tile's 2 borders == TILE_CHROME_H, which is what the
   natural-size math assumes. Change one, change the other. */
.rtile-head{display:flex;align-items:center;gap:7px;padding:0 9px;height:24px;
box-sizing:border-box;flex:0 0 auto;background:var(--rp-elev);
border-bottom:1px solid var(--rp-border);font-size:11px;}
.rtile-dot{width:6px;height:6px;border-radius:50%;background:var(--rp-faint);flex:0 0 auto;}
.rtile[data-flash="1"] .rtile-dot{background:var(--rp-flame);}
.rtile[data-chflash="1"]{animation:rp-chflash 1.2s ease-out 1;}
@keyframes rp-chflash{
from{border-color:var(--rp-flame);box-shadow:0 0 0 1px rgba(233,160,58,.45),
0 6px 28px rgba(233,160,58,.22);}
to{border-color:var(--rp-border);box-shadow:0 6px 22px rgba(0,0,0,.35);}}
.rtile-name{color:var(--rp-text);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.rtile-slug{color:var(--rp-faint);margin-left:auto;font-variant-numeric:tabular-nums;}
/* Sized in JS from the recording's cols×rows — never from the viewport. */
.rtile-body{position:relative;flex:0 0 auto;}
.rtile-canvas{display:block;}

.rp-status{position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);
color:var(--rp-dim);font-size:12px;letter-spacing:.03em;pointer-events:none;}
.rp-status[data-show="0"]{display:none;}

.rp-bar{flex:0 0 auto;background:var(--rp-elev);border-top:1px solid var(--rp-border);}

/* Scrubber: 24px of hit area, 6px of ink. --rp-mid is the track centre. */
.rp-timeline{position:relative;height:24px;--rp-mid:12px;cursor:pointer;
padding:0 0;}
.rp-track{position:absolute;left:0;right:0;top:calc(var(--rp-mid) - 3px);height:6px;
border-radius:3px;background:var(--rp-surf);
box-shadow:inset 0 1px 2px rgba(0,0,0,.55);}
.rp-played{position:absolute;left:0;top:calc(var(--rp-mid) - 3px);height:6px;width:0;
border-radius:3px;z-index:1;
background:linear-gradient(90deg,var(--rp-flame-dim),var(--rp-flame));}
.rp-ticks{position:absolute;inset:0;z-index:2;pointer-events:none;}
/* Chapter ticks rise ABOVE the track; idle hashes sit faintly below it. */
.rp-tick{position:absolute;top:calc(var(--rp-mid) - 9px);width:3px;height:7px;
margin-left:-1.5px;border-radius:1.5px;background:var(--rp-flame);
box-shadow:0 0 6px rgba(233,160,58,.5);}
.rp-gap{position:absolute;top:calc(var(--rp-mid) + 4px);width:2px;height:5px;
margin-left:-1px;background:var(--rp-faint);opacity:.5;}
.rp-playhead{position:absolute;top:calc(var(--rp-mid) - 7px);width:14px;height:14px;
margin-left:-7px;z-index:3;border-radius:50%;background:var(--rp-flame);
box-shadow:0 0 0 2px var(--rp-elev),0 2px 6px rgba(0,0,0,.5);
transition:transform .12s ease;}
.rp-timeline:hover .rp-playhead,.rp-timeline:active .rp-playhead{transform:scale(1.15);}
.rp-tip{position:absolute;bottom:26px;transform:translateX(-50%);max-width:360px;
background:var(--rp-surf);border:1px solid var(--rp-border);border-radius:6px;
padding:5px 9px;font-size:11px;color:var(--rp-text);white-space:nowrap;overflow:hidden;
text-overflow:ellipsis;z-index:4;pointer-events:none;box-shadow:0 8px 24px rgba(0,0,0,.5);}
.rp-tip[data-show="0"]{display:none;}

.rp-controls{display:flex;align-items:center;gap:8px;padding:6px 10px 9px;}
.rp-btn{background:var(--rp-surf);border:1px solid var(--rp-border);color:var(--rp-dim);
border-radius:5px;padding:4px 10px;font-size:12px;cursor:pointer;line-height:1.3;}
.rp-btn:hover{color:var(--rp-text);border-color:var(--rp-flame-dim);}
/* Utility, not a primary: dim and small. */
.rp-reset{color:var(--rp-faint);padding:4px 8px;}
.rp-reset:hover{color:var(--rp-flame);}
.rp-play{color:var(--rp-flame);min-width:40px;font-size:13px;}
/* prev/next are THE primary controls — bigger targets, flame accent. */
.rp-nav{color:var(--rp-flame);font-size:15px;line-height:1;padding:6px 14px;min-width:46px;
border-color:var(--rp-flame-dim);}
.rp-nav:hover{background:rgba(233,160,58,.12);border-color:var(--rp-flame);
color:var(--rp-flame);}
/* Progressive disclosure: visible only while playing, and hidden with
   `visibility` so the bar never reflows on pause. */
.rp-speeds{display:flex;gap:4px;margin-left:4px;}
.rp-speeds[data-show="0"]{visibility:hidden;pointer-events:none;}
.rp-speed{padding:3px 7px;font-size:11px;min-width:32px;}
.rp-speed[data-on="1"]{color:var(--rp-flame);border-color:var(--rp-flame-dim);
background:rgba(233,160,58,.12);}
.rp-time{font-variant-numeric:tabular-nums;color:var(--rp-dim);font-size:12px;margin-left:4px;}
.rp-time-wall{font-variant-numeric:tabular-nums;color:var(--rp-faint);font-size:12px;}
.rp-spacer{flex:1;}
.rp-chip{font-size:10px;letter-spacing:.1em;text-transform:uppercase;color:var(--rp-faint);
border:1px solid var(--rp-border);border-radius:999px;padding:3px 9px;user-select:none;}
"#;

// ---------------------------------------------------------------------------
// tests (pure logic only — no web_sys, so these run natively)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seance_core::replay::{encode_record, MAGIC};

    #[test]
    fn mmss_formats_minutes_and_pads_seconds() {
        assert_eq!(fmt_mmss(0), "0:00");
        assert_eq!(fmt_mmss(9_000), "0:09");
        assert_eq!(fmt_mmss(69_000), "1:09");
        assert_eq!(fmt_mmss(3_725_000), "62:05");
    }

    #[test]
    fn idle_labels_are_coarse_and_readable() {
        assert_eq!(fmt_idle(8_400), "8s");
        assert_eq!(fmt_idle(89_000), "89s");
        assert_eq!(fmt_idle(840_000), "14m");
        assert_eq!(fmt_idle(3_599_000), "60m");
        assert_eq!(fmt_idle(4_020_000), "1h 07m");
    }

    // --- wall ↔ compressed -------------------------------------------------

    #[test]
    fn small_gaps_pass_through_untouched() {
        // Every step is under GAP_MAX, so the map is one 1:1 segment.
        let map = build_time_map(&[1_000, 2_000, 4_000], 1_000, 4_000);
        assert_eq!(map.segs.len(), 1);
        assert!(!map.segs[0].collapsed);
        assert_eq!(map.comp_total(), 3_000);
        assert_eq!(map.wall_to_comp(2_500), 1_500);
        assert_eq!(map.comp_to_wall(1_500), 2_500);
        assert!(map.collapsed_gaps().is_empty());
    }

    #[test]
    fn a_long_gap_costs_exactly_one_beat() {
        // 0..1s live, then 10 minutes of nothing, then 1s live.
        let map = build_time_map(&[0, 1_000, 601_000, 602_000], 0, 602_000);
        assert_eq!(map.comp_total(), 1_000 + GAP_BEAT_MS + 1_000);
        assert_eq!(map.segs.len(), 3);
        assert!(map.segs[1].collapsed);
        // The far side of the gap is one beat past the near side.
        assert_eq!(map.wall_to_comp(1_000), 1_000);
        assert_eq!(map.wall_to_comp(601_000), 1_000 + GAP_BEAT_MS);
        // Mid-gap interpolates rather than jumping — the playhead keeps moving.
        let mid = map.wall_to_comp(301_000);
        assert!(mid > 1_000 && mid < 1_000 + GAP_BEAT_MS);
        assert_eq!(
            map.collapsed_gaps(),
            vec![(1_000 + GAP_BEAT_MS / 2, 600_000)]
        );
    }

    #[test]
    fn wall_compressed_roundtrips_on_record_boundaries() {
        let times = [0u64, 500, 900, 60_900, 61_400, 61_500, 400_000, 400_100];
        let map = build_time_map(&times, 0, 400_100);
        for &t in times.iter() {
            assert_eq!(map.comp_to_wall(map.wall_to_comp(t)), t, "roundtrip at {t}");
        }
        // …and the compressed clock is monotonic across the whole window.
        let mut prev = 0;
        for w in (0..400_100).step_by(997) {
            let c = map.wall_to_comp(w);
            assert!(c >= prev, "non-monotonic at {w}");
            prev = c;
        }
    }

    #[test]
    fn gaps_at_the_stream_edges_compress_too() {
        // One record in the middle of a wide window: idle on both sides.
        let map = build_time_map(&[500_000], 0, 1_000_000);
        assert_eq!(map.segs.len(), 2);
        assert!(map.segs.iter().all(|s| s.collapsed));
        assert_eq!(map.comp_total(), 2 * GAP_BEAT_MS);
        assert_eq!(map.wall_to_comp(500_000), GAP_BEAT_MS);
        assert_eq!(map.wall_to_comp(0), 0);
        assert_eq!(map.wall_to_comp(1_000_000), 2 * GAP_BEAT_MS);
    }

    #[test]
    fn empty_and_single_record_streams_still_map() {
        // Empty stream over a live-length window: 1:1.
        let m = build_time_map(&[], 1_000, 3_000);
        assert_eq!(m.comp_total(), 2_000);
        assert_eq!(m.wall_to_comp(2_000), 1_000);

        // Empty stream over a wide window: one collapsed beat.
        let m = build_time_map(&[], 0, 600_000);
        assert_eq!(m.comp_total(), GAP_BEAT_MS);
        assert_eq!(m.comp_to_wall(GAP_BEAT_MS), 600_000);

        // Degenerate zero-width window: conversions still answer, never panic.
        let m = build_time_map(&[7], 7, 7);
        assert_eq!(m.comp_total(), 0);
        assert_eq!(m.wall_to_comp(7), 0);
        assert_eq!(m.comp_to_wall(0), 7);

        // A single record equal to the bounds behaves the same way.
        let m = build_time_map(&[5_000], 5_000, 5_000);
        assert_eq!(m.wall_to_comp(9_999), 0);

        // Not-yet-loaded map is the identity, so differences still hold.
        let m = TimeMap::default();
        assert_eq!(m.wall_to_comp(1_234), 1_234);
        assert_eq!(m.comp_to_wall(1_234), 1_234);
    }

    #[test]
    fn out_of_range_stamps_saturate_at_the_edges() {
        let map = build_time_map(&[1_000, 1_500], 1_000, 1_500);
        assert_eq!(map.wall_to_comp(0), 0);
        assert_eq!(map.wall_to_comp(9_000_000), 500);
        assert_eq!(map.comp_to_wall(0), 1_000);
        assert_eq!(map.comp_to_wall(9_000_000), 1_500);
    }

    #[test]
    fn records_outside_the_window_do_not_split_the_map() {
        // The bridge may ship wider than the window; only the interior counts.
        let map = build_time_map(&[0, 100, 2_000, 2_500, 90_000], 1_000, 3_000);
        assert_eq!(map.segs.len(), 1);
        assert_eq!(map.comp_total(), 2_000);
    }

    #[test]
    fn relative_urls_resolve_against_the_manifest() {
        assert_eq!(
            join_relative("https://x.dev/r/abc/manifest.json", "w-1.srr.gz"),
            "https://x.dev/r/abc/w-1.srr.gz"
        );
        assert_eq!(
            join_relative("/share/m.json?t=1", "w-1.srr.gz"),
            "/share/w-1.srr.gz"
        );
        assert_eq!(join_relative("/share/m.json", "/abs/w.gz"), "/abs/w.gz");
        assert_eq!(
            join_relative("/share/m.json", "https://cdn/w.gz"),
            "https://cdn/w.gz"
        );
    }

    #[test]
    fn bridge_template_substitutes_the_slug() {
        let s = Source::Bridge {
            manifest_url: "/b/manifest?tok=z&from=1&to=2".into(),
            pane_url_template: "/b/pane/{slug}?tok=z&from=1&to=2".into(),
        };
        assert_eq!(s.pane_url("w-7", "ignored"), "/b/pane/w-7?tok=z&from=1&to=2");
    }

    #[test]
    fn keyframe_search_finds_the_last_at_or_before() {
        let t = [10u64, 20, 30, 40];
        assert_eq!(last_at_or_before(&t, 9), None);
        assert_eq!(last_at_or_before(&t, 10), Some(0));
        assert_eq!(last_at_or_before(&t, 29), Some(1));
        assert_eq!(last_at_or_before(&t, 30), Some(2));
        assert_eq!(last_at_or_before(&t, 999), Some(3));
        assert_eq!(last_at_or_before(&[], 5), None);
    }

    #[test]
    fn first_after_is_the_next_strictly_later_entry() {
        let t = [10u64, 20, 20, 30];
        assert_eq!(first_after(&t, 0), Some(0));
        assert_eq!(first_after(&t, 10), Some(1));
        assert_eq!(first_after(&t, 20), Some(3));
        assert_eq!(first_after(&t, 30), None);
    }

    #[test]
    fn chapter_crossing_fires_once_per_chapter() {
        let ch = [100u64, 200, 300];
        // A frame that steps over 100 stops there …
        assert_eq!(chapter_crossed(&ch, 50, 150), Some(0));
        // … and after stopping exactly on it, the same chapter is not re-fired.
        assert_eq!(chapter_crossed(&ch, 100, 150), None);
        assert_eq!(chapter_crossed(&ch, 100, 250), Some(1));
        assert_eq!(chapter_crossed(&ch, 300, 9_999), None);
        assert_eq!(chapter_crossed(&[], 0, 100), None);
    }

    #[test]
    // Modes are gone (2nd design pass): playback is one honest multiplier and
    // never auto-pauses, so what is asserted now is the *offered* speeds.
    fn speeds_are_the_four_offered_multipliers() {
        assert_eq!(SPEEDS, [1.0, 1.5, 2.0, 5.0]);
        assert_eq!(fmt_speed(1.0), "1×");
        assert_eq!(fmt_speed(1.5), "1.5×");
        assert_eq!(fmt_speed(5.0), "5×");
    }

    // --- flyTo -------------------------------------------------------------

    #[test]
    fn fly_duration_scales_with_distance_and_caps() {
        assert_eq!(fly_duration_ms(0), FLY_MIN_MS);
        let short = fly_duration_ms(5_000);
        assert!(short > FLY_MIN_MS && short < FLY_MAX_MS, "{short}");
        // Long jumps do not cost proportionally more time — they go faster.
        assert_eq!(fly_duration_ms(FLY_FULL_DIST_MS as u64), FLY_MAX_MS);
        assert_eq!(fly_duration_ms(60 * 60 * 1_000), FLY_MAX_MS);
        // Monotonic in distance.
        let mut prev = 0.0;
        for d in (0..120_000).step_by(3_331) {
            let v = fly_duration_ms(d);
            assert!(v >= prev, "non-monotonic at {d}");
            prev = v;
        }
    }

    #[test]
    fn easing_is_symmetric_and_pinned_at_the_ends() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-9);
        // s-curve: slow at the edges, fast in the middle.
        assert!(ease_in_out(0.25) < 0.25);
        assert!(ease_in_out(0.75) > 0.75);
        // …and symmetric about the midpoint.
        for p in [0.1, 0.3, 0.42] {
            assert!((ease_in_out(p) + ease_in_out(1.0 - p) - 1.0).abs() < 1e-9);
        }
        // Out-of-range progress clamps rather than overshooting.
        assert_eq!(ease_in_out(-1.0), 0.0);
        assert_eq!(ease_in_out(9.0), 1.0);
    }

    #[test]
    fn fly_position_interpolates_both_directions() {
        assert_eq!(fly_position(1_000, 5_000, 0.0), 1_000);
        assert_eq!(fly_position(1_000, 5_000, 1.0), 5_000);
        assert_eq!(fly_position(1_000, 5_000, 0.5), 3_000);
        // Rewind: the same curve, run backwards, never below zero.
        assert_eq!(fly_position(5_000, 1_000, 0.5), 3_000);
        assert_eq!(fly_position(5_000, 0, 1.0), 0);
        let a = fly_position(0, 10_000, 0.25);
        assert!(a < 2_500, "ease-in should lag a linear scrub: {a}");
    }

    // --- `#t=` fragment ----------------------------------------------------

    #[test]
    fn time_fragment_roundtrips_and_rejects_junk() {
        assert_eq!(format_time_fragment(1_700_000_000_123), "#t=1700000000123");
        assert_eq!(
            parse_time_fragment(&format_time_fragment(1_700_000_000_123)),
            Some(1_700_000_000_123)
        );
        assert_eq!(parse_time_fragment("t=42"), Some(42));
        assert_eq!(parse_time_fragment("#t=0"), Some(0));
        // Anything that is not our fragment is "no position", not an error.
        assert_eq!(parse_time_fragment(""), None);
        assert_eq!(parse_time_fragment("#"), None);
        assert_eq!(parse_time_fragment("#t="), None);
        assert_eq!(parse_time_fragment("#t=abc"), None);
        assert_eq!(parse_time_fragment("#t=-5"), None);
        assert_eq!(parse_time_fragment("#t=1.5"), None);
        assert_eq!(parse_time_fragment("#replay-edit?workspace=w"), None);
    }

    // --- original-resolution layout ---------------------------------------

    #[test]
    fn natural_layout_sums_column_and_row_extents() {
        // 2×2 of identical tiles: two gaps' worth of chrome, no more.
        let t = [(100.0, 50.0); 4];
        assert_eq!(
            natural_layout(&t, 2),
            (200.0 + TILE_GAP, 100.0 + TILE_GAP)
        );
        // Ragged geometry: each column takes its widest, each row its tallest.
        let t = [(100.0, 50.0), (300.0, 20.0), (80.0, 90.0)];
        assert_eq!(
            natural_layout(&t, 2),
            (400.0 + TILE_GAP, 140.0 + TILE_GAP)
        );
        // Single tile: exactly the recording, no gap.
        assert_eq!(natural_layout(&[(640.0, 384.0)], 1), (640.0, 384.0));
        assert_eq!(natural_layout(&[], 2), (0.0, 0.0));
    }

    #[test]
    fn fit_scale_shrinks_to_fit_but_never_upscales() {
        // Roomy viewport: original resolution, full stop.
        assert_eq!(fit_scale((800.0, 600.0), (1600.0, 1200.0)), 1.0);
        assert_eq!(fit_scale((800.0, 600.0), (800.0, 600.0)), 1.0);
        // Width-bound and height-bound both pick the tighter axis.
        assert_eq!(fit_scale((800.0, 600.0), (400.0, 1200.0)), 0.5);
        assert_eq!(fit_scale((800.0, 600.0), (1600.0, 300.0)), 0.5);
        // Degenerate inputs answer instead of dividing by zero.
        assert_eq!(fit_scale((0.0, 600.0), (100.0, 100.0)), 1.0);
        assert_eq!(fit_scale((800.0, 600.0), (0.0, 0.0)), 1.0);
        // Absurdly small viewports still leave something on screen.
        assert!(fit_scale((8_000.0, 6_000.0), (1.0, 1.0)) >= 0.02);
    }

    #[test]
    fn chapter_before_is_the_previous_prompt() {
        let ch = [100u64, 200, 300];
        assert_eq!(chapter_before(&ch, 0), None);
        assert_eq!(chapter_before(&ch, 100), None); // sitting ON one → go back past it
        assert_eq!(chapter_before(&ch, 101), Some(0));
        assert_eq!(chapter_before(&ch, 300), Some(1));
        assert_eq!(chapter_before(&ch, 9_999), Some(2));
        assert_eq!(chapter_before(&[], 5), None);
    }

    #[test]
    fn hover_labels_are_one_truncated_line() {
        assert_eq!(one_line("hello world", 40), "hello world");
        assert_eq!(one_line("  a\n\tb   c  ", 40), "a b c");
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
        assert_eq!(one_line("abcd efghij", 6), "abcd…");
    }

    #[test]
    fn time_args_accept_offsets_and_absolutes() {
        let from = 1_700_000_000_000u64;
        assert_eq!(absolutize(0, from), from);
        assert_eq!(absolutize(5_000, from), from + 5_000);
        assert_eq!(absolutize(from + 42, from), from + 42);
    }

    #[test]
    fn tile_grid_is_near_square() {
        assert_eq!(grid_cols(0), 1);
        assert_eq!(grid_cols(1), 1);
        assert_eq!(grid_cols(2), 2);
        assert_eq!(grid_cols(4), 2);
        assert_eq!(grid_cols(5), 3);
        assert_eq!(grid_cols(9), 3);
        assert_eq!(grid_cols(10), 4);
    }

    fn stream(recs: &[(u8, u64, &[u8])]) -> Vec<u8> {
        let mut s = MAGIC.to_vec();
        for (k, t, p) in recs {
            s.extend_from_slice(&encode_record(*k, *t, p));
        }
        s
    }

    #[test]
    fn records_are_copied_out_in_order() {
        let s = stream(&[
            (KIND_FULL, 10, b"a"),
            (KIND_DAMAGE, 20, b"b"),
            (KIND_EVENT, 30, b"{}"),
        ]);
        let recs = parse_records(&s).unwrap();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].kind, KIND_FULL);
        assert_eq!(recs[1].t_ms, 20);
        assert_eq!(recs[2].payload, b"{}".to_vec());
        assert!(parse_records(b"NOPE").is_err());
    }

    #[test]
    fn keyframe_index_marks_every_full_frame() {
        let s = stream(&[
            (KIND_FULL, 10, b"a"),
            (KIND_DAMAGE, 20, b"b"),
            (KIND_FULL, 30, b"c"),
            (KIND_EVENT, 40, b"{}"),
        ]);
        let track = PaneTrack::new("w-1".into(), "worker".into(), parse_records(&s).unwrap());
        assert_eq!(track.keyframes, vec![0, 2]);
        assert_eq!(track.keyframe_times, vec![10, 30]);
        assert_eq!(track.times, vec![10, 20, 30, 40]);
        assert_eq!(track.last_t(), 40);
        assert!(track.has_more());
    }

    #[test]
    fn seek_rewinds_to_the_nearest_keyframe_not_to_zero() {
        // Payloads are not valid SCG3, so nothing decodes — what is asserted
        // here is the *cursor* discipline, which is the seek cost.
        let s = stream(&[
            (KIND_FULL, 10, b"a"),
            (KIND_DAMAGE, 20, b"b"),
            (KIND_FULL, 30, b"c"),
            (KIND_DAMAGE, 40, b"d"),
            (KIND_DAMAGE, 50, b"e"),
        ]);
        let mut track = PaneTrack::new("w".into(), "w".into(), parse_records(&s).unwrap());
        track.seek(50);
        assert_eq!(track.cursor, 5);
        // Backwards seek lands on the keyframe at 30, then folds 40.
        track.seek(45);
        assert_eq!(track.cursor, 4);
        // Before the first keyframe: blank.
        track.seek(5);
        assert_eq!(track.cursor, 0);
        assert!(track.snap.is_none());
    }

    #[test]
    fn gzip_and_raw_streams_both_decompress() {
        let raw = stream(&[(KIND_FULL, 1, b"x")]);
        assert_eq!(decompress(raw.clone()).unwrap(), raw);

        let mut gz = Vec::new();
        {
            use std::io::Write;
            let mut enc =
                flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
            enc.write_all(&raw).unwrap();
            enc.finish().unwrap();
        }
        assert_eq!(decompress(gz).unwrap(), raw);
        assert!(decompress(vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0]).is_err());
    }
}
