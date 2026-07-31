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
//!   a tile, the prompt caption bar at chapters, and the flame flash on the
//!   receiving pane's tile.
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
//! boundary and nowhere else. Every mode advances the compressed clock, so
//! "real time" means *as it happened, minus the dead air*.
//!
//! Seeking never replays from `t=0`: each pane keeps the record indices of its
//! `KIND_FULL` keyframes, so a seek resets to the nearest keyframe at or before
//! the target and applies forward from there (a forward seek that is already
//! past that keyframe just continues from the current cursor — no reset at all).
//!
//! # Modes
//!
//! The default is [`Mode::FastForward`]: agent output runs ~20×, and the clock
//! *stops* on every chapter with the prompt up in the caption bar. That is the
//! whole thesis of the feature — a watcher should see the human's inputs at
//! human size and the machine's output compressed. [`Mode::RealTime`] plays at
//! 1× (cycle 2×/4×) without chapter stops; [`Mode::Chapters`] stays paused and
//! steps prompt to prompt.
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
const FONT_PX: f32 = 13.0;

/// Agent-output compression factor in [`Mode::FastForward`].
const FF_SPEED: f64 = 20.0;
/// How long the prompt caption lingers once playback resumes.
const CAPTION_MS: f64 = 4000.0;
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    RealTime,
    /// Agent output ~20×, auto-PAUSE at each chapter.
    FastForward,
    /// Paused; step via chapter list.
    Chapters,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::RealTime => "real time",
            Mode::FastForward => "fast forward",
            Mode::Chapters => "chapters",
        }
    }

    fn next(self) -> Mode {
        match self {
            Mode::RealTime => Mode::FastForward,
            Mode::FastForward => Mode::Chapters,
            Mode::Chapters => Mode::RealTime,
        }
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

/// The first chapter strictly inside `(t, target]` — the FastForward stop.
fn chapter_crossed(chapter_times: &[u64], t: u64, target: u64) -> Option<usize> {
    let i = first_after(chapter_times, t)?;
    (chapter_times[i] <= target).then_some(i)
}

/// Playback speed for the current mode/multiplier.
fn speed_for(mode: Mode, realtime_mult: f64) -> f64 {
    match mode {
        Mode::RealTime => realtime_mult,
        Mode::FastForward => FF_SPEED,
        Mode::Chapters => 0.0,
    }
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
    root: web_sys::Element,
    tiles: web_sys::Element,
    sidebar: web_sys::Element,
    chapter_list: web_sys::Element,
    timeline: web_sys::Element,
    played: web_sys::Element,
    playhead: web_sys::Element,
    ticks: web_sys::Element,
    tooltip: web_sys::Element,
    caption: web_sys::Element,
    caption_text: web_sys::Element,
    caption_resume: web_sys::Element,
    play_btn: web_sys::Element,
    mode_btn: web_sys::Element,
    speed_btn: web_sys::Element,
    time_label: web_sys::Element,
    time_wall: web_sys::Element,
    status: web_sys::Element,
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
    mode: Mode,
    realtime_mult: f64,
    playing: bool,
    /// Wall clock (performance.now) at the previous stepper frame.
    last_wall: f64,
    /// Wall time at which the prompt caption fades (0 = hidden/pinned).
    caption_until: f64,
    /// Caption pinned open until the viewer presses play (FastForward stop).
    caption_pinned: bool,
    current_chapter: Option<usize>,
    pending_loads: usize,
    load_error: Option<String>,
    dragging: bool,
    tiles_built: bool,
    last_css_probe: f64,
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
            mode: Mode::FastForward,
            realtime_mult: 1.0,
            playing: false,
            last_wall: now_ms(),
            caption_until: 0.0,
            caption_pinned: false,
            current_chapter: None,
            pending_loads: 0,
            load_error: None,
            dragging: false,
            tiles_built: false,
            last_css_probe: 0.0,
        }));

        wire_controls(&player);
        start_load(&player);
        start_raf(&player);
        player
    }

    pub fn play(&mut self) {
        if self.mode == Mode::Chapters {
            // "Play" in chapter mode means: step to the next prompt.
            if let Some(i) = first_after(&self.chapter_times, self.t) {
                let t = self.chapter_times[i].min(self.to_ms);
                self.seek_absolute(t);
                self.show_caption(i, true);
            }
            return;
        }
        if self.t >= self.to_ms {
            self.seek_absolute(self.from_ms);
        }
        self.playing = true;
        // Resuming releases a pinned caption but does not yank it: it stays
        // readable for a beat and fades on its own.
        self.caption_pinned = false;
        if self.caption_until > 0.0 {
            self.caption_until = now_ms() + CAPTION_MS;
        }
        self.sync_caption();
        self.last_wall = now_ms();
        self.sync_controls();
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.sync_controls();
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Seek to an **absolute wall** ms (or an offset from `from_ms`, see the
    /// module docs). Idle compression is internal — callers never convert.
    pub fn seek_ms(&mut self, t_ms: u64) {
        let t = absolutize(t_ms, self.from_ms);
        self.seek_absolute(t);
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if mode == Mode::Chapters {
            self.playing = false;
        }
        self.sync_controls();
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
        self.rebuild_chapter_list();
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

    /// One rAF step: advance the clock, fold records, paint dirty panes.
    fn frame(&mut self) {
        let wall = now_ms();
        let dt = (wall - self.last_wall).clamp(0.0, MAX_FRAME_DT_MS);
        self.last_wall = wall;

        if self.playing && !self.panes.is_empty() {
            let speed = speed_for(self.mode, self.realtime_mult);
            // The clock advances in COMPRESSED ms; the wall target is whatever
            // that lands on. Dead air therefore costs GAP_BEAT_MS of watching,
            // in every mode.
            let comp_now = self.map.wall_to_comp(self.t);
            let comp_target = comp_now.saturating_add((dt * speed).round().max(0.0) as u64);
            let mut target = self.map.comp_to_wall(comp_target).max(self.t);
            let mut stop_at_chapter = None;

            // Any mode captions a crossed chapter; only FastForward stops on it.
            let crossed = chapter_crossed(&self.chapter_times, self.t, target);
            if let Some(i) = crossed {
                if self.mode == Mode::FastForward {
                    target = self.chapter_times[i];
                    stop_at_chapter = Some(i);
                }
            }
            if target >= self.to_ms {
                target = self.to_ms;
                stop_at_chapter = None;
                self.playing = false;
                self.sync_controls();
            }
            self.t = target;
            for p in self.panes.iter_mut() {
                p.advance_to(target, wall);
            }
            self.current_chapter = last_at_or_before(&self.chapter_times, target);

            match stop_at_chapter {
                Some(i) => {
                    self.playing = false;
                    self.show_caption(i, true);
                    self.sync_controls();
                }
                // Rolled past it without stopping: caption only, self-fading.
                None => {
                    if let Some(i) = crossed {
                        if self.chapter_times[i] <= target {
                            self.show_caption(i, false);
                        }
                    }
                }
            }
            self.update_progress();
        }

        // Caption fade + tile flash decay are wall-clock, not recording-clock.
        if self.caption_until > 0.0 && !self.caption_pinned && wall > self.caption_until {
            self.caption_until = 0.0;
            self.sync_caption();
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

    /// Canvas backing stores follow their tiles. Measuring forces layout, so
    /// it is throttled — tiles only change size on window resize.
    fn sync_sizes(&mut self, wall: f64) {
        if wall - self.last_css_probe < 250.0 {
            return;
        }
        self.last_css_probe = wall;
        let Some(doc) = document() else { return };
        for p in self.panes.iter_mut() {
            let Some(el) = doc.get_element_by_id(&p.canvas_id) else {
                continue;
            };
            let Some(parent) = el.parent_element() else {
                continue;
            };
            let rect = parent.get_bounding_client_rect();
            let (w, h) = (rect.width(), rect.height().max(0.0));
            if w < 8.0 || h < 8.0 {
                continue;
            }
            if p.renderer.is_none() {
                let canvas: web_sys::HtmlCanvasElement = match el.dyn_into() {
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
            if (w - p.css_size.0).abs() > 0.5 || (h - p.css_size.1).abs() > 0.5 {
                if let Some(r) = p.renderer.as_mut() {
                    r.resize_to(w, h);
                }
                p.css_size = (w, h);
                p.dirty = true;
            }
        }
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

    /// Raise the prompt caption for `idx`, and flash the pane it landed on.
    /// `pinned` holds it open until the viewer presses play (the FastForward
    /// stop); otherwise it fades [`CAPTION_MS`] after now.
    fn show_caption(&mut self, idx: usize, pinned: bool) {
        // Copied out first: the rest of this method needs `&mut self`.
        let Some((text, pane)) = self
            .chapters
            .get(idx)
            .map(|ch| (ch.text.clone(), ch.pane.clone()))
        else {
            return;
        };
        self.dom.caption_text.set_text_content(Some(&text));
        let _ = self.dom.caption.set_attribute("data-expand", "0");
        // Pinning is about being *stopped* at the prompt, not about the mode:
        // a FastForward auto-pause and a chapter jump both land paused.
        self.caption_pinned = pinned && !self.playing;
        self.caption_until = now_ms() + CAPTION_MS;
        self.sync_caption();
        self.current_chapter = Some(idx);
        self.highlight_chapter();

        let wall = now_ms();
        for p in self.panes.iter_mut() {
            if p.slug == pane {
                p.chapter_flash_until = wall + CHAPTER_FLASH_MS;
            }
        }
    }

    fn hide_caption(&mut self) {
        self.caption_pinned = false;
        self.caption_until = 0.0;
        self.sync_caption();
    }

    /// Visibility + the pinned "▶ resume" affordance, from caption state.
    fn sync_caption(&self) {
        let show = self.caption_until > 0.0 || self.caption_pinned;
        let _ = self
            .dom
            .caption
            .set_attribute("data-show", if show { "1" } else { "0" });
        let _ = self.dom.caption.set_attribute(
            "data-pinned",
            if self.caption_pinned { "1" } else { "0" },
        );
        // The resume affordance only exists while the caption is holding
        // playback — otherwise it is a button that does nothing.
        let _ = self
            .dom
            .caption_resume
            .set_attribute("data-show", if self.caption_pinned { "1" } else { "0" });
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
        self.highlight_chapter();
    }

    fn highlight_chapter(&self) {
        let Some(doc) = document() else { return };
        for (i, _) in self.chapters.iter().enumerate() {
            if let Some(row) = doc.get_element_by_id(&format!("rp-ch-{i}")) {
                let on = self.current_chapter == Some(i);
                let _ = row.set_attribute("data-on", if on { "1" } else { "0" });
            }
        }
    }

    fn sync_controls(&self) {
        self.dom
            .play_btn
            .set_text_content(Some(if self.playing { "❚❚" } else { "▶" }));
        self.dom.mode_btn.set_text_content(Some(self.mode.label()));
        let speed = match self.mode {
            Mode::RealTime => format!("{}×", self.realtime_mult as u32),
            Mode::FastForward => format!("{}×", FF_SPEED as u32),
            Mode::Chapters => "step".to_string(),
        };
        self.dom.speed_btn.set_text_content(Some(&speed));
        let _ = self.dom.speed_btn.set_attribute(
            "data-live",
            if self.mode == Mode::RealTime { "1" } else { "0" },
        );
        self.update_progress();
    }

    fn rebuild_chapter_list(&self) {
        let Some(doc) = document() else { return };
        self.dom.chapter_list.set_text_content(Some(""));
        if self.chapters.is_empty() {
            if let Ok(empty) = doc.create_element("div") {
                empty.set_class_name("rp-ch-empty");
                empty.set_text_content(Some("no prompts in this range"));
                append(&self.dom.chapter_list, &empty);
            }
            return;
        }
        for (i, ch) in self.chapters.iter().enumerate() {
            let Ok(row) = doc.create_element("div") else {
                continue;
            };
            row.set_class_name("rp-ch");
            row.set_id(&format!("rp-ch-{i}"));
            let _ = row.set_attribute("data-idx", &i.to_string());
            let _ = row.set_attribute("title", &ch.text);
            if let Ok(t) = doc.create_element("div") {
                t.set_class_name("rp-ch-t");
                t.set_text_content(Some(&fmt_mmss(ch.t_ms.saturating_sub(self.from_ms))));
                append(&row, &t);
            }
            if let Ok(x) = doc.create_element("div") {
                x.set_class_name("rp-ch-x");
                x.set_text_content(Some(&ch.text));
                append(&row, &x);
            }
            append(&self.dom.chapter_list, &row);
        }
        self.highlight_chapter();
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
        let frac = ((client_x as f64 - rect.x()) / rect.width()).clamp(0.0, 1.0);
        let comp = self.map.wall_to_comp(self.from_ms)
            + (frac * self.active_duration() as f64).round() as u64;
        let t = self.map.comp_to_wall(comp);
        self.seek_absolute(t);
        self.hide_caption();
    }

    /// Nearest chapter — or collapsed gap — within ~7px of the cursor → tooltip.
    fn hover_timeline(&self, client_x: i32) {
        let rect = self.dom.timeline.get_bounding_client_rect();
        if rect.width() <= 0.0 {
            return;
        }
        let x_of = |t: u64| rect.x() + rect.width() * self.frac_of(t);
        // (distance, label, wall position) — chapters win ties by being checked
        // first with a strict `<`.
        let mut best: Option<(f64, String, u64)> = None;
        for ch in self.chapters.iter() {
            if ch.t_ms < self.from_ms || ch.t_ms > self.to_ms {
                continue;
            }
            let d = (x_of(ch.t_ms) - client_x as f64).abs();
            if d <= 7.0 && best.as_ref().map(|(bd, _, _)| d < *bd).unwrap_or(true) {
                best = Some((d, ch.text.clone(), ch.t_ms));
            }
        }
        for (comp_mid, skipped) in self.map.collapsed_gaps() {
            let wall_mid = self.map.comp_to_wall(comp_mid);
            if wall_mid < self.from_ms || wall_mid > self.to_ms {
                continue;
            }
            let d = (x_of(wall_mid) - client_x as f64).abs();
            if d <= 7.0 && best.as_ref().map(|(bd, _, _)| d < *bd).unwrap_or(true) {
                best = Some((d, format!("skipped {} idle", fmt_idle(skipped)), wall_mid));
            }
        }
        match best {
            Some((_, text, at)) => {
                self.dom.tooltip.set_text_content(Some(&text));
                set_style(
                    &self.dom.tooltip,
                    "left",
                    &format!("{:.4}%", self.frac_of(at) * 100.0),
                );
                let _ = self.dom.tooltip.set_attribute("data-show", "1");
            }
            None => {
                let _ = self.dom.tooltip.set_attribute("data-show", "0");
            }
        }
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
        // FastForward opens paused on the first prompt: the viewer sees the
        // question before the answer scrolls.
        if self.mode == Mode::FastForward && !self.chapters.is_empty() {
            let first = self.chapter_times[0].clamp(self.from_ms, self.to_ms);
            self.seek_absolute(first);
            self.show_caption(0, true);
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
        set_style(
            &self.dom.tiles,
            "grid-template-columns",
            &format!("repeat({cols}, minmax(0, 1fr))"),
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

    // Sidebar
    let sidebar = el(&doc, "aside", "rp-side");
    let side_head = el(&doc, "div", "rp-side-head");
    let side_title = el(&doc, "span", "rp-side-title");
    side_title.set_text_content(Some("prompts"));
    let collapse = el(&doc, "button", "rp-collapse");
    collapse.set_text_content(Some("‹"));
    append(&side_head, &side_title);
    append(&side_head, &collapse);
    let chapter_list = el(&doc, "div", "rp-ch-list");
    append(&sidebar, &side_head);
    append(&sidebar, &chapter_list);

    // Stage
    let stage = el(&doc, "div", "rp-stage");
    let tiles = el(&doc, "div", "rp-tiles");
    let status = el(&doc, "div", "rp-status");
    let _ = status.set_attribute("data-show", "1");
    status.set_text_content(Some("loading recording…"));
    append(&stage, &tiles);
    append(&stage, &status);

    append(&body, &sidebar);
    append(&body, &stage);

    // Prompt caption — docked above the control bar, covering nothing. The
    // terminal stays fully visible while the prompt is up.
    let caption = el(&doc, "div", "rp-cap");
    let _ = caption.set_attribute("data-show", "0");
    let _ = caption.set_attribute("data-pinned", "0");
    let _ = caption.set_attribute("data-expand", "0");
    let caption_main = el(&doc, "div", "rp-cap-main");
    let caption_kicker = el(&doc, "div", "rp-cap-kicker");
    caption_kicker.set_text_content(Some("prompt"));
    let caption_text = el(&doc, "div", "rp-cap-text");
    append(&caption_main, &caption_kicker);
    append(&caption_main, &caption_text);
    let caption_resume = el(&doc, "div", "rp-cap-resume");
    caption_resume.set_text_content(Some("▶ resume"));
    append(&caption, &caption_main);
    append(&caption, &caption_resume);

    // Control bar
    let bar = el(&doc, "div", "rp-bar");
    let timeline = el(&doc, "div", "rp-timeline");
    let played = el(&doc, "div", "rp-played");
    let ticks = el(&doc, "div", "rp-ticks");
    let playhead = el(&doc, "div", "rp-playhead");
    let tooltip = el(&doc, "div", "rp-tip");
    let _ = tooltip.set_attribute("data-show", "0");
    append(&timeline, &played);
    append(&timeline, &ticks);
    append(&timeline, &playhead);
    append(&timeline, &tooltip);

    let controls = el(&doc, "div", "rp-controls");
    let play_btn = el(&doc, "button", "rp-btn rp-play");
    play_btn.set_text_content(Some("▶"));
    let mode_btn = el(&doc, "button", "rp-btn rp-mode");
    mode_btn.set_text_content(Some(Mode::FastForward.label()));
    let speed_btn = el(&doc, "button", "rp-btn rp-speed");
    speed_btn.set_text_content(Some("20×"));
    let time_label = el(&doc, "div", "rp-time");
    time_label.set_text_content(Some("0:00 / 0:00"));
    let time_wall = el(&doc, "div", "rp-time-wall");
    let spacer = el(&doc, "div", "rp-spacer");
    let chip = el(&doc, "div", "rp-chip");
    chip.set_text_content(Some("recorded with seance ✦"));
    append(&controls, &play_btn);
    append(&controls, &mode_btn);
    append(&controls, &speed_btn);
    append(&controls, &time_label);
    append(&controls, &time_wall);
    append(&controls, &spacer);
    append(&controls, &chip);

    append(&bar, &timeline);
    append(&bar, &controls);

    append(&root, &body);
    append(&root, &caption);
    append(&root, &bar);
    append(mount, &root);

    Dom {
        root,
        tiles,
        sidebar,
        chapter_list,
        timeline,
        played,
        playhead,
        ticks,
        tooltip,
        caption,
        caption_text,
        caption_resume,
        play_btn,
        mode_btn,
        speed_btn,
        time_label,
        time_wall,
        status,
    }
}

fn wire_controls(player: &Rc<RefCell<Player>>) {
    let (play_btn, mode_btn, speed_btn, timeline, chapter_list, sidebar, root, caption) = {
        let p = player.borrow();
        (
            p.dom.play_btn.clone(),
            p.dom.mode_btn.clone(),
            p.dom.speed_btn.clone(),
            p.dom.timeline.clone(),
            p.dom.chapter_list.clone(),
            p.dom.sidebar.clone(),
            p.dom.root.clone(),
            p.dom.caption.clone(),
        )
    };

    // play / pause
    on_click(&play_btn, player, |p, _ev| {
        if p.is_playing() {
            p.pause();
        } else {
            p.play();
        }
    });

    // mode cycle
    on_click(&mode_btn, player, |p, _ev| {
        let next = p.mode.next();
        p.set_mode(next);
    });

    // speed: only meaningful in real time (1× → 2× → 4×)
    on_click(&speed_btn, player, |p, _ev| {
        if p.mode != Mode::RealTime {
            p.set_mode(Mode::RealTime);
        }
        p.realtime_mult = match p.realtime_mult as u32 {
            1 => 2.0,
            2 => 4.0,
            _ => 1.0,
        };
        p.sync_controls();
    });

    // Caption bar: the resume hint is its own element and resumes playback;
    // anywhere else on the bar toggles full expansion of the prompt text.
    on_click(&caption, player, |p, ev| {
        let on_resume = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|e| e.closest(".rp-cap-resume").ok().flatten())
            .is_some();
        if on_resume {
            p.caption_pinned = false;
            p.play();
            return;
        }
        let expanded = p.dom.caption.get_attribute("data-expand").as_deref() == Some("1");
        let _ = p
            .dom
            .caption
            .set_attribute("data-expand", if expanded { "0" } else { "1" });
    });

    // sidebar collapse
    if let Ok(Some(btn)) = root.query_selector(".rp-collapse") {
        let side = sidebar.clone();
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |_ev: web_sys::MouseEvent| {
            let collapsed = side.get_attribute("data-collapsed").as_deref() == Some("1");
            let _ = side.set_attribute("data-collapsed", if collapsed { "0" } else { "1" });
        });
        let _ = btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // chapter rows (delegated: the list is rebuilt by set_chapters)
    {
        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            let Some(target) = ev.target() else { return };
            let Ok(el) = target.dyn_into::<web_sys::Element>() else {
                return;
            };
            let Ok(Some(row)) = el.closest(".rp-ch") else {
                return;
            };
            let Some(idx) = row
                .get_attribute("data-idx")
                .and_then(|s| s.parse::<usize>().ok())
            else {
                return;
            };
            if let Ok(mut p) = pl.try_borrow_mut() {
                let Some(t) = p.chapter_times.get(idx).copied() else {
                    return;
                };
                p.pause();
                p.seek_absolute(t);
                // A chapter jump lands the caption pinned: the viewer chose to
                // read this prompt, so it holds until they press play.
                p.show_caption(idx, true);
            }
        });
        let _ = chapter_list.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        cb.forget();
    }

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
                    p.seek_from_client_x(ev.client_x());
                }
            }
        });
        let _ = doc.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref());
        cb.forget();

        let pl = Rc::clone(player);
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |_ev: web_sys::MouseEvent| {
            if let Ok(mut p) = pl.try_borrow_mut() {
                p.dragging = false;
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
--rp-flame-dim:#A97328;--rp-violet:#A790D5;
position:absolute;inset:0;display:flex;flex-direction:column;background:var(--rp-bg);
color:var(--rp-text);font:13px/1.45 ui-sans-serif,system-ui,-apple-system,'Segoe UI',sans-serif;
overflow:hidden;}
.rp-body{flex:1;display:flex;min-height:0;}

.rp-side{width:240px;flex:0 0 240px;display:flex;flex-direction:column;min-height:0;
background:var(--rp-elev);border-right:1px solid var(--rp-border);
transition:width .18s ease,flex-basis .18s ease;}
.rp-side[data-collapsed="1"]{width:34px;flex-basis:34px;}
.rp-side[data-collapsed="1"] .rp-ch-list,
.rp-side[data-collapsed="1"] .rp-side-title{display:none;}
.rp-side[data-collapsed="1"] .rp-collapse{transform:rotate(180deg);}
.rp-side-head{display:flex;align-items:center;justify-content:space-between;
padding:9px 10px;border-bottom:1px solid var(--rp-border);}
.rp-side-title{font-size:11px;letter-spacing:.09em;text-transform:uppercase;color:var(--rp-dim);}
.rp-collapse{background:none;border:0;color:var(--rp-faint);cursor:pointer;font-size:14px;
line-height:1;padding:2px 4px;}
.rp-collapse:hover{color:var(--rp-flame);}
.rp-ch-list{flex:1;overflow-y:auto;padding:6px;}
.rp-ch-empty{color:var(--rp-faint);font-size:12px;padding:10px 8px;}
.rp-ch{display:flex;gap:8px;padding:7px 8px;border-radius:5px;cursor:pointer;
border-left:2px solid transparent;}
.rp-ch:hover{background:var(--rp-surf);}
.rp-ch[data-on="1"]{background:var(--rp-surf);border-left-color:var(--rp-flame);}
.rp-ch[data-on="1"] .rp-ch-t{color:var(--rp-flame);}
.rp-ch-t{flex:0 0 auto;font-variant-numeric:tabular-nums;font-size:11px;color:var(--rp-faint);
padding-top:1px;}
.rp-ch-x{font-size:12px;color:var(--rp-dim);display:-webkit-box;-webkit-line-clamp:3;
-webkit-box-orient:vertical;overflow:hidden;white-space:pre-wrap;word-break:break-word;}
.rp-ch:hover .rp-ch-x{-webkit-line-clamp:unset;color:var(--rp-text);}

.rp-stage{position:relative;flex:1;min-width:0;min-height:0;padding:10px;}
.rp-tiles{display:grid;gap:10px;width:100%;height:100%;grid-auto-rows:1fr;}
.rtile{display:flex;flex-direction:column;min-width:0;min-height:0;background:var(--rp-bg);
border:1px solid var(--rp-border);border-radius:7px;overflow:hidden;
box-shadow:0 6px 22px rgba(0,0,0,.35);transition:border-color .18s ease,box-shadow .18s ease;}
.rtile[data-flash="1"]{border-color:var(--rp-flame);
box-shadow:0 0 0 1px rgba(233,160,58,.35),0 6px 26px rgba(233,160,58,.14);}
.rtile-head{display:flex;align-items:center;gap:7px;padding:5px 9px;background:var(--rp-elev);
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
.rtile-body{flex:1;min-height:0;position:relative;}
.rtile-canvas{display:block;position:absolute;inset:0;}

.rp-status{position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);
color:var(--rp-dim);font-size:12px;letter-spacing:.03em;pointer-events:none;}
.rp-status[data-show="0"]{display:none;}

.rp-cap{flex:0 0 auto;display:flex;align-items:flex-start;gap:12px;
padding:0 14px;max-height:0;opacity:0;overflow:hidden;cursor:pointer;
border-left:3px solid var(--rp-flame);
background:var(--rp-elev);
background:color-mix(in srgb, var(--rp-elev) 96%, transparent);
transition:opacity .3s ease,max-height .3s ease,padding .3s ease;}
.rp-cap[data-show="1"]{opacity:1;max-height:180px;padding:9px 14px;}
.rp-cap-main{flex:1;min-width:0;}
.rp-cap-kicker{font-size:10px;letter-spacing:.16em;text-transform:uppercase;
color:var(--rp-flame);margin-bottom:4px;}
.rp-cap-text{font-size:13px;line-height:1.45;color:var(--rp-text);white-space:pre-wrap;
word-break:break-word;font-family:ui-monospace,'JetBrains Mono',monospace;
display:-webkit-box;-webkit-line-clamp:3;-webkit-box-orient:vertical;
overflow:hidden;text-overflow:ellipsis;}
.rp-cap[data-expand="1"] .rp-cap-text{-webkit-line-clamp:unset;max-height:150px;
overflow-y:auto;}
.rp-cap-resume{flex:0 0 auto;align-self:center;font-size:11px;color:var(--rp-faint);
border:1px solid var(--rp-border);border-radius:5px;padding:3px 9px;white-space:nowrap;}
.rp-cap-resume:hover{color:var(--rp-flame);border-color:var(--rp-flame-dim);}
.rp-cap-resume[data-show="0"]{display:none;}

.rp-bar{flex:0 0 auto;background:var(--rp-elev);border-top:1px solid var(--rp-border);}
.rp-timeline{position:relative;height:16px;cursor:pointer;}
.rp-timeline::after{content:"";position:absolute;left:0;right:0;top:7px;height:3px;
background:var(--rp-surf);border-radius:2px;}
.rp-played{position:absolute;left:0;top:7px;height:3px;width:0;border-radius:2px;
background:var(--rp-flame-dim);z-index:1;}
.rp-ticks{position:absolute;inset:0;z-index:2;pointer-events:none;}
.rp-tick{position:absolute;top:3px;width:2px;height:11px;margin-left:-1px;border-radius:1px;
background:var(--rp-flame);box-shadow:0 0 6px rgba(233,160,58,.55);}
.rp-gap{position:absolute;top:2px;width:2px;height:13px;margin-left:-1px;
background:var(--rp-faint);opacity:.75;}
.rp-playhead{position:absolute;top:2px;width:2px;height:13px;margin-left:-1px;z-index:3;
background:var(--rp-text);border-radius:1px;}
.rp-tip{position:absolute;bottom:20px;transform:translateX(-50%);max-width:320px;
background:var(--rp-surf);border:1px solid var(--rp-border);border-radius:6px;
padding:6px 9px;font-size:11px;color:var(--rp-text);white-space:pre-wrap;z-index:4;
pointer-events:none;box-shadow:0 8px 24px rgba(0,0,0,.5);}
.rp-tip[data-show="0"]{display:none;}

.rp-controls{display:flex;align-items:center;gap:8px;padding:7px 10px 9px;}
.rp-btn{background:var(--rp-surf);border:1px solid var(--rp-border);color:var(--rp-dim);
border-radius:5px;padding:4px 10px;font-size:12px;cursor:pointer;line-height:1.3;}
.rp-btn:hover{color:var(--rp-text);border-color:var(--rp-flame-dim);}
.rp-play{color:var(--rp-flame);min-width:38px;}
.rp-speed[data-live="1"]{color:var(--rp-violet);}
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
    fn speeds_follow_the_mode() {
        assert_eq!(speed_for(Mode::RealTime, 1.0), 1.0);
        assert_eq!(speed_for(Mode::RealTime, 4.0), 4.0);
        assert_eq!(speed_for(Mode::FastForward, 1.0), FF_SPEED);
        assert_eq!(speed_for(Mode::Chapters, 4.0), 0.0);
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
