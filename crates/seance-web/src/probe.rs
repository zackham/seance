//! Latency probe overlay for the web client — the browser-side counterpart of
//! `src/latency_probe.rs` in the native GUI.
//!
//! Same philosophy, different substrate: the native probe stamps an in-flight
//! keystroke and closes it when the matching grid/paint lands, printing
//! p50/p95/max to stderr. There is no stderr in a tab, so the web probe keeps
//! the aggregates in small rings and *renders* them — the overlay is a shipped
//! feature, so the performance claim is falsifiable by anyone running the app.
//!
//! Model: [`Probe::record_input`] stamps `performance.now()` for an input sent
//! to a pane (unlike the native probe, which keeps only the FIRST unanswered
//! keystroke, the web probe queues every stamp — bursts are common when the
//! websocket coalesces, and a grid frame closes the whole burst). A grid frame
//! for that pane ([`Probe::record_grid`]) closes every pending stamp at or
//! before `now` into an echo sample. Stamps older than [`STALE_MS`] are
//! discarded unpaired: a pane that died, or a keystroke the daemon swallowed,
//! must not poison the aggregate.
//!
//! No timers: [`Probe::tick`] is driven by the app's animation frame, and DOM
//! writes are throttled to [`REFRESH_MS`]. Idle cost when hidden is one bool
//! test per frame.
//!
//! All pure math ([`percentile`], [`RxWindow`]) is `web_sys`-free and unit
//! tested natively.

use std::collections::HashMap;

use web_sys::{window, Document, HtmlElement, Performance};

// NEEDS web-sys feature: Node
//   (`Node::append_child` / the `Element: Deref<Target = Node>` methods used to
//   mount the overlay into `document.body`. Every other API used here —
//   `Window`, `Document`, `Element`, `HtmlElement`, `CssStyleDeclaration`,
//   `Performance` — is already enabled in crates/seance-web/Cargo.toml.)

/// Echo/paint samples retained per ring.
const RING: usize = 200;
/// Per-pane cap on unanswered input stamps.
const PENDING_CAP: usize = 64;
/// Unanswered input stamps older than this are dropped, never paired.
const STALE_MS: f64 = 2000.0;
/// Overlay DOM refresh interval while visible.
const REFRESH_MS: f64 = 500.0;
/// Sliding window for the receive-rate readout.
const RX_WINDOW_MS: f64 = 1000.0;

/// Budgets. Over these, the p95 renders in flame.
const ECHO_BUDGET_MS: f64 = 33.0;
const PAINT_BUDGET_MS: f64 = 8.0;

// Palette (docs/THEME.md). Hex is the canonical spelling in CSS-land.
const BG_ELEVATED: &str = "#1C1718";
const BORDER: &str = "#352C2E";
const TEXT: &str = "#EBE3DB";
const TEXT_DIM: &str = "#A69A91";
const FLAME: &str = "#E9A03A";

// ---------------------------------------------------------------------------
// pure math (no web_sys — unit tested natively)
// ---------------------------------------------------------------------------

/// Nearest-rank percentile over an unsorted slice, in the slice's own units.
///
/// Sorts a copy: the rings are ≤ [`RING`] and this runs at most twice per
/// [`REFRESH_MS`], so the allocation is cheaper than maintaining order on the
/// hot record path. Matches the native probe's index arithmetic
/// (`(n - 1) * q`) so GUI and web numbers are comparable. `None` on empty.
pub fn percentile(samples: &[f64], q: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let q = q.clamp(0.0, 1.0);
    let idx = (((n - 1) as f64) * q) as usize; // truncate — native latency_probe parity
    Some(sorted[idx.min(n - 1)])
}

/// Fixed-capacity FIFO of `f64` samples. Push is O(1) amortized; the oldest
/// sample falls off the front once full.
#[derive(Debug, Default)]
pub struct Ring {
    buf: Vec<f64>,
    next: usize,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring {
            buf: Vec::with_capacity(cap),
            next: 0,
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, v: f64) {
        if self.buf.len() < self.cap {
            self.buf.push(v);
        } else {
            self.buf[self.next] = v;
            self.next = (self.next + 1) % self.cap;
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Samples in arbitrary order — callers percentile them, order is irrelevant.
    pub fn samples(&self) -> &[f64] {
        &self.buf
    }
}

/// Sliding-window byte accounting for the rx rate readout.
///
/// Keeps `(timestamp, bytes)` for the last [`RX_WINDOW_MS`] and reports
/// bytes/second. Eviction happens on both write and read so a quiet socket
/// decays to zero instead of freezing on its last burst (a stopped clock that
/// reads "1.2 KiB/s" is worse than one that reads 0).
#[derive(Debug, Default)]
pub struct RxWindow {
    events: Vec<(f64, usize)>,
    window_ms: f64,
}

impl RxWindow {
    pub fn new(window_ms: f64) -> RxWindow {
        RxWindow {
            events: Vec::new(),
            window_ms,
        }
    }

    pub fn record(&mut self, now: f64, bytes: usize) {
        self.events.push((now, bytes));
        self.evict(now);
    }

    pub fn evict(&mut self, now: f64) {
        let cutoff = now - self.window_ms;
        self.events.retain(|(t, _)| *t >= cutoff);
    }

    /// Bytes per second over the window (windowed total scaled to 1s).
    pub fn bytes_per_sec(&self, now: f64) -> f64 {
        let cutoff = now - self.window_ms;
        let total: usize = self
            .events
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, b)| *b)
            .sum();
        if self.window_ms <= 0.0 {
            return 0.0;
        }
        total as f64 * (1000.0 / self.window_ms)
    }
}

/// Close every stamp at or before `now` into echo samples, drop stamps older
/// than [`STALE_MS`], and leave anything stamped in the future alone.
///
/// Returns the closed durations; `pending` is left holding only live stamps.
/// Split out from [`Probe::record_grid`] so the pairing rule — the one piece of
/// logic that can silently corrupt every number on the overlay — is testable
/// without a DOM.
pub fn drain_pending(pending: &mut Vec<f64>, now: f64) -> Vec<f64> {
    let mut echoes = Vec::new();
    let mut live = Vec::with_capacity(pending.len());
    for &stamp in pending.iter() {
        if stamp > now {
            live.push(stamp);
            continue;
        }
        let age = now - stamp;
        if age > STALE_MS {
            continue; // unpaired: dropped, not counted
        }
        echoes.push(age);
    }
    *pending = live;
    echoes
}

/// `12.3` / `—` when there is no sample yet. Never renders a missing metric as
/// zero — an absent sample and a fast sample are different facts.
fn fmt_ms(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.1}"),
        None => "—".to_string(),
    }
}

fn fmt_kib(bytes_per_sec: f64) -> String {
    format!("{:.1}", bytes_per_sec / 1024.0)
}

/// Pad `s` to `w` columns (monospace, so column count == character count).
fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.to_string()
    } else {
        let mut out = String::with_capacity(w);
        out.push_str(s);
        for _ in n..w {
            out.push(' ');
        }
        out
    }
}

/// Escape the few characters that could break out of a text node. All values
/// are numbers we format ourselves, but pane ids are caller-supplied and the
/// counter row interpolates strings, so escape on principle.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

/// The overlay. One per app; created hidden, toggled by the app's keybinding.
///
/// Every DOM handle is optional: a probe on a document without a body, or in a
/// worker, degrades to pure accounting rather than panicking. Recording never
/// touches the DOM.
pub struct Probe {
    perf: Option<Performance>,
    root: Option<HtmlElement>,
    visible: bool,

    /// Unanswered input stamps per pane, oldest first.
    pending: HashMap<String, Vec<f64>>,
    echo: Ring,
    paint: Ring,
    rx: RxWindow,
    rtt_ms: Option<f64>,
    frames: u64,
    /// Inputs dropped unpaired — shown so a broken pairing is visible, not silent.
    unpaired: u64,

    last_render: f64,
}

impl Default for Probe {
    fn default() -> Probe {
        Probe::new()
    }
}

impl Probe {
    /// Build the overlay and append it to `document.body`, hidden.
    pub fn new() -> Probe {
        let win = window();
        let perf = win.as_ref().and_then(|w| w.performance());
        let doc: Option<Document> = win.as_ref().and_then(|w| w.document());
        let root = doc.as_ref().and_then(build_root);

        Probe {
            perf,
            root,
            visible: false,
            pending: HashMap::new(),
            echo: Ring::new(RING),
            paint: Ring::new(RING),
            rx: RxWindow::new(RX_WINDOW_MS),
            rtt_ms: None,
            frames: 0,
            unpaired: 0,
            last_render: f64::NEG_INFINITY,
        }
    }

    fn now(&self) -> f64 {
        self.perf.as_ref().map(|p| p.now()).unwrap_or(0.0)
    }

    /// Show/hide the overlay. Forces a render on the next [`tick`](Probe::tick)
    /// so a freshly shown overlay is never up to 500ms stale.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        self.last_render = f64::NEG_INFINITY;
        if let Some(root) = &self.root {
            let _ = root
                .style()
                .set_property("display", if self.visible { "block" } else { "none" });
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Stamp an input sent to `pane`. Capped at [`PENDING_CAP`] per pane; the
    /// oldest stamp is evicted (and counted unpaired) when the cap is hit, so a
    /// pane that never echoes cannot grow without bound.
    pub fn record_input(&mut self, pane: &str) {
        let now = self.now();
        let q = self
            .pending
            .entry(pane.to_string())
            .or_insert_with(|| Vec::with_capacity(8));
        q.push(now);
        while q.len() > PENDING_CAP {
            q.remove(0);
            self.unpaired += 1;
        }
    }

    /// A grid frame landed for `pane`: close every pending stamp ≤ now into echo
    /// samples, discarding stamps older than [`STALE_MS`] as unpaired.
    pub fn record_grid(&mut self, pane: &str) {
        let now = self.now();
        let Some(q) = self.pending.get_mut(pane) else {
            return;
        };
        let before = q.len();
        let echoes = drain_pending(q, now);
        // Anything that neither echoed nor survived was dropped as stale.
        self.unpaired += (before - q.len() - echoes.len()) as u64;
        if q.is_empty() {
            self.pending.remove(pane);
        }
        for e in echoes {
            self.echo.push(e);
        }
    }

    /// Renderer paint duration, in milliseconds.
    pub fn record_frame(&mut self, paint_ms: f64) {
        self.frames = self.frames.saturating_add(1);
        self.paint.push(paint_ms);
    }

    /// Account an inbound websocket message.
    pub fn record_rx(&mut self, bytes: usize) {
        let now = self.now();
        self.rx.record(now, bytes);
    }

    /// Latest measured websocket round-trip (`None` clears the readout).
    pub fn set_rtt(&mut self, rtt_ms: Option<f64>) {
        self.rtt_ms = rtt_ms;
    }

    /// Called every animation frame. Refreshes the overlay at most once per
    /// [`REFRESH_MS`]; a no-op beyond one branch while hidden.
    pub fn tick(&mut self) {
        if !self.visible {
            return;
        }
        let now = self.now();
        if now - self.last_render < REFRESH_MS {
            return;
        }
        self.last_render = now;
        self.render(now);
    }

    fn render(&mut self, now: f64) {
        let Some(root) = &self.root else {
            return;
        };
        self.rx.evict(now);

        let echo_p50 = percentile(self.echo.samples(), 0.50);
        let echo_p95 = percentile(self.echo.samples(), 0.95);
        let paint_p50 = percentile(self.paint.samples(), 0.50);
        let paint_p95 = percentile(self.paint.samples(), 0.95);

        let echo_hot = echo_p95.map(|v| v > ECHO_BUDGET_MS).unwrap_or(false);
        let paint_hot = paint_p95.map(|v| v > PAINT_BUDGET_MS).unwrap_or(false);

        let mut html = String::with_capacity(512);
        html.push_str(&stat_row(
            "echo",
            echo_p50,
            echo_p95,
            echo_hot,
            self.echo.len(),
        ));
        html.push_str(&stat_row(
            "paint",
            paint_p50,
            paint_p95,
            paint_hot,
            self.paint.len(),
        ));
        html.push_str(&plain_row("rtt", &fmt_ms(self.rtt_ms), "ms"));
        html.push_str(&plain_row(
            "rx",
            &fmt_kib(self.rx.bytes_per_sec(now)),
            "KiB/s",
        ));
        html.push_str(&plain_row("frames", &self.frames.to_string(), ""));
        if self.unpaired > 0 {
            html.push_str(&plain_row("unpaired", &self.unpaired.to_string(), ""));
        }

        root.set_inner_html(&html);
    }
}

/// `echo   p50 3.1  p95 8.4` — p95 in flame when over budget.
fn stat_row(label: &str, p50: Option<f64>, p95: Option<f64>, hot: bool, n: usize) -> String {
    let color = if hot { FLAME } else { TEXT };
    format!(
        "<div>{label}<span style=\"color:{TEXT_DIM}\">p50 </span>\
         <span style=\"color:{TEXT}\">{p50}</span>  \
         <span style=\"color:{TEXT_DIM}\">p95 </span>\
         <span style=\"color:{color}\">{p95}</span>\
         <span style=\"color:{TEXT_DIM}\">  n={n}</span></div>",
        label = esc(&pad(label, 7)),
        p50 = esc(&pad(&fmt_ms(p50), 6)),
        p95 = esc(&fmt_ms(p95)),
        n = n,
    )
}

/// `rtt    12.4 ms` — single-value rows share the label column with `stat_row`.
fn plain_row(label: &str, value: &str, unit: &str) -> String {
    let unit = if unit.is_empty() {
        String::new()
    } else {
        format!("<span style=\"color:{TEXT_DIM}\"> {}</span>", esc(unit))
    };
    format!(
        "<div>{label}<span style=\"color:{TEXT}\">{value}</span>{unit}</div>",
        label = esc(&pad(label, 7)),
        value = esc(value),
    )
}

/// Create the hidden overlay element and mount it. `None` when the document has
/// no body or element creation fails — the probe then runs headless.
fn build_root(doc: &Document) -> Option<HtmlElement> {
    let el = doc.create_element("div").ok()?;
    let el: HtmlElement = el.dyn_into_html()?;
    let style = el.style();
    for (k, v) in [
        ("position", "fixed"),
        ("right", "8px"),
        ("bottom", "8px"),
        ("z-index", "2147483000"),
        ("display", "none"),
        ("pointer-events", "none"),
        ("padding", "6px 8px"),
        ("border-radius", "4px"),
        (
            "font",
            "11px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
        ),
        ("white-space", "pre"),
        ("background", BG_ELEVATED),
        ("opacity", "0.92"),
        ("border", &format!("1px solid {BORDER}")),
        ("color", TEXT_DIM),
        ("user-select", "none"),
    ] {
        let _ = style.set_property(k, v);
    }
    let body = doc.body()?;
    body.append_child(&el).ok()?;
    Some(el)
}

/// `Element` → `HtmlElement` without pulling `wasm_bindgen::JsCast` into the
/// module's public surface. Kept as a trait so the cast site reads as one call
/// and stays the only `unchecked` in the file.
trait DynIntoHtml {
    fn dyn_into_html(self) -> Option<HtmlElement>;
}

impl DynIntoHtml for web_sys::Element {
    fn dyn_into_html(self) -> Option<HtmlElement> {
        use wasm_bindgen::JsCast as _;
        self.dyn_into::<HtmlElement>().ok()
    }
}

// ---------------------------------------------------------------------------
// tests — pure math only, no web_sys, so `cargo test` runs on the host
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_empty_is_none() {
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn percentile_single_sample() {
        assert_eq!(percentile(&[4.2], 0.5), Some(4.2));
        assert_eq!(percentile(&[4.2], 0.95), Some(4.2));
    }

    #[test]
    fn percentile_nearest_rank() {
        // 1..=100, index = trunc((n-1) * q) — same arithmetic as the native probe.
        let s: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        assert_eq!(percentile(&s, 0.5), Some(50.0)); // trunc(99 * 0.50) = 49 -> s[49]
        assert_eq!(percentile(&s, 0.95), Some(95.0)); // trunc(99 * 0.95) = 94 -> s[94]
        assert_eq!(percentile(&s, 0.0), Some(1.0));
        assert_eq!(percentile(&s, 1.0), Some(100.0));
    }

    #[test]
    fn percentile_ignores_input_order() {
        let a = [9.0, 1.0, 5.0, 3.0, 7.0];
        let b = [1.0, 3.0, 5.0, 7.0, 9.0];
        assert_eq!(percentile(&a, 0.5), percentile(&b, 0.5));
        assert_eq!(percentile(&a, 0.95), percentile(&b, 0.95));
    }

    #[test]
    fn percentile_clamps_out_of_range_q() {
        let s = [1.0, 2.0, 3.0];
        assert_eq!(percentile(&s, 2.0), Some(3.0));
        assert_eq!(percentile(&s, -1.0), Some(1.0));
    }

    #[test]
    fn ring_keeps_last_n() {
        let mut r = Ring::new(3);
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            r.push(v);
        }
        assert_eq!(r.len(), 3);
        let mut got = r.samples().to_vec();
        got.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(got, vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn ring_under_capacity() {
        let mut r = Ring::new(200);
        r.push(1.0);
        r.push(2.0);
        assert_eq!(r.len(), 2);
        assert_eq!(percentile(r.samples(), 0.5), Some(1.0));
    }

    #[test]
    fn rx_window_scales_to_per_second() {
        let mut w = RxWindow::new(1000.0);
        w.record(0.0, 1024);
        w.record(500.0, 1024);
        assert_eq!(w.bytes_per_sec(900.0), 2048.0);
    }

    #[test]
    fn rx_window_decays_to_zero() {
        let mut w = RxWindow::new(1000.0);
        w.record(0.0, 4096);
        assert_eq!(w.bytes_per_sec(5000.0), 0.0);
        w.evict(5000.0);
        assert_eq!(w.events.len(), 0);
    }

    #[test]
    fn drain_pending_closes_all_stamps_at_or_before_now() {
        let mut p = vec![100.0, 110.0, 120.0];
        let echoes = drain_pending(&mut p, 130.0);
        assert_eq!(echoes, vec![30.0, 20.0, 10.0]);
        assert!(p.is_empty());
    }

    #[test]
    fn drain_pending_drops_stale_unpaired() {
        let mut p = vec![0.0, 2500.0];
        let echoes = drain_pending(&mut p, 3000.0);
        assert_eq!(echoes, vec![500.0]); // 3000ms-old stamp discarded
        assert!(p.is_empty());
    }

    #[test]
    fn drain_pending_keeps_future_stamps() {
        let mut p = vec![100.0, 900.0];
        let echoes = drain_pending(&mut p, 500.0);
        assert_eq!(echoes, vec![400.0]);
        assert_eq!(p, vec![900.0]);
    }

    #[test]
    fn drain_pending_empty_is_noop() {
        let mut p: Vec<f64> = Vec::new();
        assert!(drain_pending(&mut p, 1000.0).is_empty());
    }

    #[test]
    fn fmt_ms_missing_is_dash_not_zero() {
        assert_eq!(fmt_ms(None), "—");
        assert_eq!(fmt_ms(Some(0.0)), "0.0");
        assert_eq!(fmt_ms(Some(3.14159)), "3.1");
    }

    #[test]
    fn pad_aligns_columns() {
        assert_eq!(pad("echo", 7), "echo   ");
        assert_eq!(pad("unpaired", 7), "unpaired");
    }

    #[test]
    fn esc_neutralizes_markup() {
        assert_eq!(esc("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
    }
}
