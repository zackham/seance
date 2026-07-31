//! Replay **editor** — trim, chapters, review, publish.
//!
//! The player ([`crate::replay`]) knows how to paint a recorded session. This
//! module is the room you sit in *before* you hand that session to someone
//! else: pick the window, fix the chapter titles the lite line-reconstructor
//! mangled, drop the chapters you don't want strangers reading, step through
//! the whole thing once with your own eyes, then publish.
//!
//! Shape, deliberately:
//!
//! - **Load wide, trim narrow.** The bridge fetch happens exactly once, for
//!   the widest window the editor was opened with. Every trim after that is
//!   [`Player::set_range`] over already-loaded bytes — dragging a handle never
//!   re-hits the network. Reaching further back is an explicit *load more
//!   history* button that tears the editor down and reopens it at `from − 2h`,
//!   because that genuinely is a new fetch and pretending otherwise would make
//!   a cheap-looking control expensive.
//! - **The trim strip is ours, not the player's.** An editor-owned bar above
//!   the player's own timeline. Two flame handles, a bright span between them.
//!   No coupling to the player's internal timeline DOM, so the two agents'
//!   files touch only at the documented method boundary.
//! - **Privacy is a standing banner, not a modal.** Everything on those
//!   screens ships — keys, paths, half-typed secrets. A modal you dismiss once
//!   is a modal you never read again; the strip stays up the whole session,
//!   and *review pass* walks the human through every chapter in Chapters mode
//!   so "I looked" is a thing they actually did.
//!
//! **Untrusted text.** Chapter text is reconstructed terminal input. It is
//! never interpolated into markup: every dynamic string in this file lands via
//! `set_text_content` / `HtmlInputElement::set_value`, and `set_inner_html` is
//! called only with the static skeleton below. Same for the published URL.

// NEEDS web-sys feature: Request
//   (`Request::new_with_str_and_init`, and it also gates
//   `Window::fetch_with_request` — verified at web-sys-0.3.103
//   src/features/gen_Window.rs:3121.)
// NEEDS web-sys feature: RequestInit
//   (`RequestInit::new` + `set_method` / `set_headers` / `set_body`.)
// NEEDS web-sys feature: Headers
//   (`Headers::new`, `Headers::set` — content-type on the publish POST.)
// NEEDS web-sys feature: Response
//   (`Response::ok`, `Response::status`, `Response::text`.)
//
// Already enabled in crates/seance-web/Cargo.toml and used here: Window,
// Document, Element, HtmlElement, HtmlInputElement, Node, CssStyleDeclaration,
// DomRect, MouseEvent, KeyboardEvent, FocusEvent, Location, Navigator,
// Clipboard, Performance, HtmlHeadElement, console.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};

use seance_core::replay::{Chapter, Manifest};

// ---------------------------------------------------------------------------
// pure helpers (unit-tested natively — no web_sys below this line and above
// the `Editor` section)
// ---------------------------------------------------------------------------

/// Shortest window the trim handles may collapse to. Below a second the
/// player has nothing to show and the duration readout becomes noise.
const MIN_SPAN_MS: u64 = 1_000;

/// Two hours — the default window, and the step for *load more history*.
const TWO_HOURS_MS: u64 = 2 * 60 * 60 * 1_000;

/// Clamp a proposed `[from, to]` into `[lo, hi]`, preserving a minimum span.
///
/// Handles are independent in the UI, so either one can be dragged past the
/// other; the crossing case resolves by pushing the *other* edge, which is
/// what a dragger expects (the window follows the finger rather than sticking).
fn clamp_range(from: i64, to: i64, lo: u64, hi: u64) -> (u64, u64) {
    let lo_i = lo as i64;
    let hi_i = hi.max(lo + MIN_SPAN_MS) as i64;
    let min = MIN_SPAN_MS as i64;
    let mut f = from.clamp(lo_i, hi_i);
    let mut t = to.clamp(lo_i, hi_i);
    if t - f < min {
        if t + min <= hi_i {
            t = f + min;
        } else {
            t = hi_i;
            f = (t - min).max(lo_i);
        }
    }
    (f as u64, t as u64)
}

/// Fraction of the loaded window a timestamp sits at, clamped to `0.0..=1.0`.
fn ms_to_frac(t_ms: u64, lo: u64, hi: u64) -> f64 {
    if hi <= lo {
        return 0.0;
    }
    let f = (t_ms.saturating_sub(lo) as f64) / ((hi - lo) as f64);
    f.clamp(0.0, 1.0)
}

/// Inverse of [`ms_to_frac`]. `frac` need not be in range; it is clamped.
fn frac_to_ms(frac: f64, lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        return lo;
    }
    let f = frac.clamp(0.0, 1.0);
    lo + (f * ((hi - lo) as f64)).round() as u64
}

/// `"1h 04m 09s"` / `"12m 40s"` / `"40s"`. Coarse on purpose — this labels a
/// share window, not a benchmark.
fn fmt_duration(ms: u64) -> String {
    let total = ms / 1_000;
    let (h, m, s) = (total / 3_600, (total % 3_600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Offset-from-start clock for a chapter row: `"4:07"` / `"1:04:07"`.
fn fmt_offset(t_ms: u64, origin_ms: u64) -> String {
    let total = t_ms.saturating_sub(origin_ms) / 1_000;
    let (h, m, s) = (total / 3_600, (total % 3_600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn in_range(t_ms: u64, from_ms: u64, to_ms: u64) -> bool {
    t_ms >= from_ms && t_ms <= to_ms
}

/// One row of the chapters panel: the manifest chapter plus editor state.
#[derive(Clone, Debug, PartialEq)]
struct ChapterRow {
    t_ms: u64,
    pane: String,
    /// Live text — starts at the manifest value, edited in place.
    text: String,
    /// The human's include checkbox. Out-of-range rows are excluded
    /// regardless (see [`effective`]), without clobbering this flag, so
    /// widening the trim brings them back exactly as they were.
    included: bool,
}

impl ChapterRow {
    fn effective(&self, from_ms: u64, to_ms: u64) -> bool {
        self.included && in_range(self.t_ms, from_ms, to_ms)
    }
}

/// The chapter vec that goes to `Player::set_chapters` **and** into the
/// publish body — one function, so the preview cannot disagree with the
/// artifact. Blank titles are dropped: an untitled jump target is worse than
/// no jump target.
fn effective_chapters(rows: &[ChapterRow], from_ms: u64, to_ms: u64) -> Vec<Chapter> {
    rows.iter()
        .filter(|r| r.effective(from_ms, to_ms) && !r.text.trim().is_empty())
        .map(|r| Chapter {
            t_ms: r.t_ms,
            pane: r.pane.clone(),
            text: r.text.trim().to_string(),
        })
        .collect()
}

/// First chapter a review pass should land on.
fn first_effective_ms(rows: &[ChapterRow], from_ms: u64, to_ms: u64) -> Option<u64> {
    rows.iter()
        .filter(|r| r.effective(from_ms, to_ms))
        .map(|r| r.t_ms)
        .min()
}

/// Default window when the route carries no explicit range.
fn default_window(now_ms: u64, from: Option<u64>, to: Option<u64>) -> (u64, u64) {
    let to_ms = to.unwrap_or(now_ms);
    let from_ms = from.unwrap_or_else(|| to_ms.saturating_sub(TWO_HOURS_MS));
    if from_ms + MIN_SPAN_MS > to_ms {
        (to_ms.saturating_sub(TWO_HOURS_MS), to_ms)
    } else {
        (from_ms, to_ms)
    }
}

// ---------------------------------------------------------------------------
// the editor
// ---------------------------------------------------------------------------

thread_local! {
    /// One editor at a time. The route owns it; [`open`] replaces any
    /// previous instance wholesale.
    static EDITOR: RefCell<Option<Rc<RefCell<Editor>>>> = const { RefCell::new(None) };
}

/// Which trim handle a drag is moving.
#[derive(Clone, Copy, PartialEq)]
enum Handle {
    From,
    To,
}

struct Editor {
    workspace: String,
    token: String,
    /// The loaded window — what the bridge actually shipped. Trim lives
    /// inside this and never outside it.
    lo_ms: u64,
    hi_ms: u64,
    /// Current trim.
    from_ms: u64,
    to_ms: u64,
    rows: Vec<ChapterRow>,
    player: Option<Rc<RefCell<crate::replay::Player>>>,
    ready: bool,
    dragging: Option<Handle>,
    /// `performance.now()` of the last drag-driven `set_range`, for throttling.
    last_drag_ms: f64,
    publishing: bool,
    dom: Dom,
    /// Interval driving the playhead marker; cleared on [`close`].
    tick: Option<i32>,
    /// Event closures that must outlive this call stack.
    _keep: Vec<KeepAlive>,
}

/// Type-erased closure storage — the editor holds several closure shapes and
/// only ever needs to keep them alive, never to call them again.
#[allow(dead_code)] // held, never read — dropping these unregisters the DOM handlers
enum KeepAlive {
    Mouse(Closure<dyn FnMut(web_sys::MouseEvent)>),
    Key(Closure<dyn FnMut(web_sys::KeyboardEvent)>),
    Focus(Closure<dyn FnMut(web_sys::FocusEvent)>),
    Tick(Closure<dyn FnMut()>),
}

/// Elements looked up once at build time. `Option` everywhere would be honest
/// but unreadable; instead [`build_dom`] fails as a unit and [`open`] bails.
struct Dom {
    root: web_sys::Element,
    title: web_sys::HtmlInputElement,
    trim: web_sys::Element,
    span: web_sys::HtmlElement,
    h_from: web_sys::HtmlElement,
    h_to: web_sys::HtmlElement,
    playhead: web_sys::HtmlElement,
    duration: web_sys::Element,
    mount: web_sys::Element,
    chapters: web_sys::Element,
    chapter_count: web_sys::Element,
    pub_panel: web_sys::HtmlElement,
    pub_body: web_sys::Element,
    publish_btn: web_sys::Element,
}

/// Mount the editor, replacing whatever `#app` was showing.
///
/// `workspace` and `token` come from the route; `initial_from_ms` /
/// `initial_to_ms` are optional and default to the last two hours. The range
/// passed here is the **widest** window the session will ever have — trims
/// narrow it, *load more history* reopens with a wider one.
pub fn open(
    workspace: String,
    token: String,
    initial_from_ms: Option<u64>,
    initial_to_ms: Option<u64>,
) {
    close();

    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };
    let Some(body) = doc.body() else { return };
    ensure_style(&doc);

    // The app view stays in the DOM (its websocket and canvases are none of
    // our business) but yields the viewport.
    if let Some(app) = doc.get_element_by_id("app") {
        if let Some(el) = app.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.style().set_property("display", "none");
        }
    }

    let now_ms = js_sys::Date::now().max(0.0) as u64;
    let (lo_ms, hi_ms) = default_window(now_ms, initial_from_ms, initial_to_ms);

    let Some(dom) = build_dom(&doc, &workspace) else {
        return;
    };
    let node: &web_sys::Node = dom.root.unchecked_ref();
    if body.append_child(node).is_err() {
        return;
    }

    let editor = Rc::new(RefCell::new(Editor {
        workspace: workspace.clone(),
        token: token.clone(),
        lo_ms,
        hi_ms,
        from_ms: lo_ms,
        to_ms: hi_ms,
        rows: Vec::new(),
        player: None,
        ready: false,
        dragging: None,
        last_drag_ms: 0.0,
        publishing: false,
        dom,
        tick: None,
        _keep: Vec::new(),
    }));
    EDITOR.with(|slot| *slot.borrow_mut() = Some(editor.clone()));

    wire_events(&editor);
    render_trim(&editor.borrow());
    render_chapters(&editor);

    // Player: one Bridge load over the full window. `{slug}` in the pane
    // template is substituted by the player per pane.
    let origin = win
        .location()
        .origin()
        .unwrap_or_else(|_| String::from(""));
    let manifest_url = format!(
        "{origin}/replay/manifest?token={}&workspace={}&from_ms={lo_ms}&to_ms={hi_ms}",
        enc(&token),
        enc(&workspace)
    );
    let pane_url_template = format!(
        "{origin}/replay/pane?token={}&slug={{slug}}&from_ms={lo_ms}&to_ms={hi_ms}",
        enc(&token)
    );

    let mount = editor.borrow().dom.mount.clone();
    let for_ready = editor.clone();
    let player = crate::replay::Player::create(
        mount,
        crate::replay::Source::Bridge {
            manifest_url,
            pane_url_template,
        },
        Box::new(move |res| on_manifest(&for_ready, res)),
    );
    editor.borrow_mut().player = Some(player);

    start_tick(&editor);
}

/// Tear the editor down and give the viewport back to `#app`.
pub fn close() {
    let Some(editor) = EDITOR.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    let ed = editor.borrow();
    if let (Some(win), Some(handle)) = (web_sys::window(), ed.tick) {
        win.clear_interval_with_handle(handle);
    }
    ed.dom.root.remove();
    drop(ed);
    if let Some(app) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("app"))
    {
        if let Some(el) = app.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.style().set_property("display", "");
        }
    }
}

fn on_manifest(editor: &Rc<RefCell<Editor>>, res: Result<Manifest, String>) {
    match res {
        Ok(manifest) => {
            {
                let mut ed = editor.borrow_mut();
                ed.ready = true;
                // Trust the manifest's own bounds when it has them — the
                // bridge may have clamped to what actually exists on disk,
                // and rendering a trim bar wider than the recording is a
                // lying instrument.
                if manifest.to_ms > manifest.from_ms {
                    ed.lo_ms = manifest.from_ms;
                    ed.hi_ms = manifest.to_ms;
                    ed.from_ms = manifest.from_ms;
                    ed.to_ms = manifest.to_ms;
                }
                ed.rows = manifest
                    .chapters
                    .iter()
                    .map(|c| ChapterRow {
                        t_ms: c.t_ms,
                        pane: c.pane.clone(),
                        text: c.text.clone(),
                        included: true,
                    })
                    .collect();
                if let Some(t) = manifest.title.as_deref() {
                    if !t.is_empty() {
                        ed.dom.title.set_value(t);
                    }
                }
                render_trim(&ed);
            }
            render_chapters(editor);
            apply_chapters(editor);
        }
        Err(err) => {
            let ed = editor.borrow();
            ed.dom.chapter_count.set_text_content(Some("—"));
            show_pub_message(&ed, &format!("could not load the recording: {err}"), true);
        }
    }
}

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------

fn build_dom(doc: &web_sys::Document, workspace: &str) -> Option<Dom> {
    let root = doc.create_element("div").ok()?;
    root.set_id("rpe-root");
    root.set_inner_html(SKELETON);

    // Workspace name is untrusted-ish (user-named) — text content, never markup.
    let heading = q(&root, "#rpe-heading")?;
    heading.set_text_content(Some(&format!("share replay ✦ {workspace}")));

    let title: web_sys::HtmlInputElement = q(&root, "#rpe-title")?.dyn_into().ok()?;
    let dom = Dom {
        title,
        trim: q(&root, "#rpe-trim")?,
        span: q(&root, "#rpe-span")?.dyn_into().ok()?,
        h_from: q(&root, "#rpe-h-from")?.dyn_into().ok()?,
        h_to: q(&root, "#rpe-h-to")?.dyn_into().ok()?,
        playhead: q(&root, "#rpe-playhead")?.dyn_into().ok()?,
        duration: q(&root, "#rpe-duration")?,
        mount: q(&root, "#rpe-player")?,
        chapters: q(&root, "#rpe-chapters")?,
        chapter_count: q(&root, "#rpe-chcount")?,
        pub_panel: q(&root, "#rpe-pub")?.dyn_into().ok()?,
        pub_body: q(&root, "#rpe-pub-body")?,
        publish_btn: q(&root, "#rpe-publish")?,
        root,
    };
    Some(dom)
}

fn q(root: &web_sys::Element, sel: &str) -> Option<web_sys::Element> {
    root.query_selector(sel).ok().flatten()
}

/// Percent-encode a query-string value.
fn enc(s: &str) -> String {
    String::from(js_sys::encode_uri_component(s))
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

fn wire_events(editor: &Rc<RefCell<Editor>>) {
    let mut keep: Vec<KeepAlive> = Vec::new();

    // --- one delegated click listener for every button in the chrome -------
    {
        let ed = editor.clone();
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            let Some(target) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            else {
                return;
            };
            let hit = |sel: &str| target.closest(sel).ok().flatten().is_some();
            if hit("#rpe-publish") {
                ev.prevent_default();
                confirm_publish(&ed);
            } else if hit("#rpe-pub-go") {
                ev.prevent_default();
                do_publish(&ed);
            } else if hit("#rpe-pub-cancel") {
                ev.prevent_default();
                hide_pub(&ed.borrow());
            } else if hit("#rpe-pub-copy") {
                ev.prevent_default();
                copy_published_url(&ed.borrow());
            } else if hit("#rpe-review") {
                ev.prevent_default();
                review_pass(&ed);
            } else if hit("#rpe-set-start") {
                ev.prevent_default();
                set_edge_to_playhead(&ed, Handle::From);
            } else if hit("#rpe-set-end") {
                ev.prevent_default();
                set_edge_to_playhead(&ed, Handle::To);
            } else if hit("#rpe-more") {
                ev.prevent_default();
                load_more_history(&ed);
            } else if hit("#rpe-close") {
                ev.prevent_default();
                close();
            }
        });
        let root = editor.borrow().dom.root.clone();
        let _ = root.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        keep.push(KeepAlive::Mouse(cb));
    }

    // --- trim handles: mousedown arms a drag ------------------------------
    {
        let ed = editor.clone();
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            let Some(target) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            else {
                return;
            };
            let which = if target.closest("#rpe-h-from").ok().flatten().is_some() {
                Some(Handle::From)
            } else if target.closest("#rpe-h-to").ok().flatten().is_some() {
                Some(Handle::To)
            } else {
                None
            };
            let Some(which) = which else { return };
            ev.prevent_default();
            ed.borrow_mut().dragging = Some(which);
        });
        let trim = editor.borrow().dom.trim.clone();
        let _ = trim.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
        keep.push(KeepAlive::Mouse(cb));
    }

    // --- document-level drag + release ------------------------------------
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        {
            let ed = editor.clone();
            let cb =
                Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
                    drag_move(&ed, &ev, false);
                });
            let _ = doc.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref());
            keep.push(KeepAlive::Mouse(cb));
        }
        {
            let ed = editor.clone();
            let cb =
                Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
                    if ed.borrow().dragging.is_none() {
                        return;
                    }
                    // Final, unthrottled application so the released handle
                    // and the player agree exactly.
                    drag_move(&ed, &ev, true);
                    ed.borrow_mut().dragging = None;
                    apply_chapters(&ed);
                    render_chapters(&ed);
                });
            let _ = doc.add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref());
            keep.push(KeepAlive::Mouse(cb));
        }
    }

    // --- chapters panel: include toggles, inline edit ----------------------
    {
        let ed = editor.clone();
        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            let Some(target) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            else {
                return;
            };
            if let Some(input) = target.dyn_ref::<web_sys::HtmlInputElement>() {
                // The click already toggled `checked`; read it back.
                let checked = input.checked();
                if let Some(i) = row_index(&target) {
                    if let Some(row) = ed.borrow_mut().rows.get_mut(i) {
                        row.included = checked;
                    }
                    apply_chapters(&ed);
                    render_chapters(&ed);
                }
                return;
            }
            // Click on the seek stamp jumps the player there.
            if target.closest(".rpe-ch-at").ok().flatten().is_some() {
                if let Some(i) = row_index(&target) {
                    // Read everything out first — `seek_ms` is foreign code
                    // and must not run while the editor borrow is live.
                    let seek = {
                        let e = ed.borrow();
                        e.rows.get(i).map(|r| r.t_ms).zip(e.player.clone())
                    };
                    if let Some((t, p)) = seek {
                        if let Ok(mut pb) = p.try_borrow_mut() { pb.seek_ms(t); }
                    }
                }
            }
        });
        let panel = editor.borrow().dom.chapters.clone();
        let _ = panel.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref());
        keep.push(KeepAlive::Mouse(cb));
    }
    {
        // Enter commits an inline edit (and never inserts a newline).
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |ev: web_sys::KeyboardEvent| {
                if ev.key() != "Enter" {
                    return;
                }
                let Some(target) = ev
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                else {
                    return;
                };
                if target.closest(".rpe-ch-text").ok().flatten().is_none() {
                    return;
                }
                ev.prevent_default();
                if let Some(el) = target.dyn_ref::<web_sys::HtmlElement>() {
                    let _ = el.blur(); // → focusout → commit
                }
            },
        );
        let panel = editor.borrow().dom.chapters.clone();
        let _ = panel.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
        keep.push(KeepAlive::Key(cb));
    }
    {
        let ed = editor.clone();
        let cb = Closure::<dyn FnMut(web_sys::FocusEvent)>::new(move |ev: web_sys::FocusEvent| {
            let Some(target) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            else {
                return;
            };
            if target.closest(".rpe-ch-text").ok().flatten().is_none() {
                return;
            }
            let Some(i) = row_index(&target) else { return };
            let text = target.text_content().unwrap_or_default();
            let changed = {
                let mut e = ed.borrow_mut();
                match e.rows.get_mut(i) {
                    Some(row) if row.text != text => {
                        row.text = text;
                        true
                    }
                    _ => false,
                }
            };
            if changed {
                apply_chapters(&ed);
                render_chapters(&ed);
            }
        });
        let panel = editor.borrow().dom.chapters.clone();
        let _ = panel.add_event_listener_with_callback("focusout", cb.as_ref().unchecked_ref());
        keep.push(KeepAlive::Focus(cb));
    }

    editor.borrow_mut()._keep = keep;
}

/// `data-i` off the nearest `.rpe-ch` ancestor.
fn row_index(from: &web_sys::Element) -> Option<usize> {
    from.closest(".rpe-ch")
        .ok()
        .flatten()
        .and_then(|row| row.get_attribute("data-i"))
        .and_then(|s| s.parse::<usize>().ok())
}

fn drag_move(editor: &Rc<RefCell<Editor>>, ev: &web_sys::MouseEvent, force: bool) {
    let (which, rect, lo, hi) = {
        let ed = editor.borrow();
        let Some(which) = ed.dragging else { return };
        (
            which,
            ed.dom.trim.get_bounding_client_rect(),
            ed.lo_ms,
            ed.hi_ms,
        )
    };
    if rect.width() <= 0.0 {
        return;
    }
    let frac = ((ev.client_x() as f64) - rect.left()) / rect.width();
    let t = frac_to_ms(frac, lo, hi);

    // Throttle: dragging fires far faster than a re-render is worth, and
    // `set_range` may make the player rebuild its window.
    let now = web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0);
    let due = force || now - editor.borrow().last_drag_ms >= 40.0;

    {
        let mut ed = editor.borrow_mut();
        let (f, to) = match which {
            Handle::From => clamp_range(t as i64, ed.to_ms as i64, lo, hi),
            Handle::To => clamp_range(ed.from_ms as i64, t as i64, lo, hi),
        };
        ed.from_ms = f;
        ed.to_ms = to;
        render_trim(&ed);
        if !due {
            return;
        }
        ed.last_drag_ms = now;
    }
    push_range(editor);
}

fn set_edge_to_playhead(editor: &Rc<RefCell<Editor>>, which: Handle) {
    let Some(player) = editor.borrow().player.clone() else {
        return;
    };
    let Ok(pb) = player.try_borrow() else { return };
    let at = pb.current_ms();
    drop(pb);
    {
        let mut ed = editor.borrow_mut();
        let (lo, hi) = (ed.lo_ms, ed.hi_ms);
        let (f, t) = match which {
            Handle::From => clamp_range(at as i64, ed.to_ms as i64, lo, hi),
            Handle::To => clamp_range(ed.from_ms as i64, at as i64, lo, hi),
        };
        ed.from_ms = f;
        ed.to_ms = t;
        render_trim(&ed);
    }
    push_range(editor);
    apply_chapters(editor);
    render_chapters(editor);
}

/// Hand the current trim to the player. Kept separate from the borrow that
/// computed it — `set_range` is foreign code and must never run while the
/// editor's `RefCell` is held.
fn push_range(editor: &Rc<RefCell<Editor>>) {
    let (player, from, to) = {
        let ed = editor.borrow();
        (ed.player.clone(), ed.from_ms, ed.to_ms)
    };
    if let Some(p) = player {
        if let Ok(mut pb) = p.try_borrow_mut() { pb.set_range(from, to); }
    }
}

fn apply_chapters(editor: &Rc<RefCell<Editor>>) {
    let (player, chapters) = {
        let ed = editor.borrow();
        (
            ed.player.clone(),
            effective_chapters(&ed.rows, ed.from_ms, ed.to_ms),
        )
    };
    if let Some(p) = player {
        if let Ok(mut pb) = p.try_borrow_mut() { pb.set_chapters(chapters); }
    }
}

/// Chapters mode, parked on the first included chapter, playing. The point is
/// that the human sees every prompt and its response before publishing.
fn review_pass(editor: &Rc<RefCell<Editor>>) {
    apply_chapters(editor);
    let (player, start) = {
        let ed = editor.borrow();
        (
            ed.player.clone(),
            first_effective_ms(&ed.rows, ed.from_ms, ed.to_ms).unwrap_or(ed.from_ms),
        )
    };
    let Some(p) = player else { return };
    let Ok(mut p) = p.try_borrow_mut() else { return };
    p.set_mode(crate::replay::Mode::Chapters);
    p.seek_ms(start);
    p.play();
}

/// Reopen the editor two hours further back — the one control that really is
/// a new bridge fetch, so it says so and rebuilds rather than pretending.
fn load_more_history(editor: &Rc<RefCell<Editor>>) {
    let (ws, token, lo, hi) = {
        let ed = editor.borrow();
        (
            ed.workspace.clone(),
            ed.token.clone(),
            ed.lo_ms.saturating_sub(TWO_HOURS_MS),
            ed.hi_ms,
        )
    };
    open(ws, token, Some(lo), Some(hi));
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

fn render_trim(ed: &Editor) {
    let (lo, hi) = (ed.lo_ms, ed.hi_ms);
    let a = ms_to_frac(ed.from_ms, lo, hi) * 100.0;
    let b = ms_to_frac(ed.to_ms, lo, hi) * 100.0;
    let style = ed.dom.span.style();
    let _ = style.set_property("left", &format!("{a:.3}%"));
    let _ = style.set_property("width", &format!("{:.3}%", (b - a).max(0.0)));
    let _ = ed
        .dom
        .h_from
        .style()
        .set_property("left", &format!("{a:.3}%"));
    let _ = ed.dom.h_to.style().set_property("left", &format!("{b:.3}%"));
    ed.dom.duration.set_text_content(Some(&format!(
        "{} selected",
        fmt_duration(ed.to_ms.saturating_sub(ed.from_ms))
    )));
}

fn render_chapters(editor: &Rc<RefCell<Editor>>) {
    let ed = editor.borrow();
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let panel = &ed.dom.chapters;
    panel.set_inner_html("");

    if ed.rows.is_empty() {
        if let Ok(empty) = doc.create_element("div") {
            empty.set_class_name("rpe-empty");
            empty.set_text_content(Some(if ed.ready {
                "no prompts found in this window."
            } else {
                "loading…"
            }));
            let _ = panel.append_child(empty.unchecked_ref::<web_sys::Node>());
        }
        ed.dom.chapter_count.set_text_content(Some("—"));
        return;
    }

    let mut kept = 0usize;
    for (i, row) in ed.rows.iter().enumerate() {
        let inside = in_range(row.t_ms, ed.from_ms, ed.to_ms);
        let on = row.effective(ed.from_ms, ed.to_ms);
        if on {
            kept += 1;
        }
        let Ok(el) = doc.create_element("div") else {
            continue;
        };
        el.set_class_name(match (inside, row.included) {
            (true, true) => "rpe-ch",
            (true, false) => "rpe-ch off",
            (false, _) => "rpe-ch out",
        });
        let _ = el.set_attribute("data-i", &i.to_string());
        // Static skeleton only — the untrusted text goes in via text_content.
        el.set_inner_html(CHAPTER_HTML);

        if let Some(cb) = q(&el, ".rpe-ch-box").and_then(|e| {
            e.dyn_into::<web_sys::HtmlInputElement>().ok()
        }) {
            cb.set_checked(row.included && inside);
            if !inside {
                let _ = cb.set_attribute("disabled", "disabled");
                let _ = cb.set_attribute(
                    "title",
                    "outside the trim window — widen the trim to include it",
                );
            }
        }
        if let Some(at) = q(&el, ".rpe-ch-at") {
            at.set_text_content(Some(&fmt_offset(row.t_ms, ed.lo_ms)));
        }
        if let Some(pane) = q(&el, ".rpe-ch-pane") {
            pane.set_text_content(Some(&row.pane));
        }
        if let Some(text) = q(&el, ".rpe-ch-text") {
            text.set_text_content(Some(&row.text));
        }
        let _ = panel.append_child(el.unchecked_ref::<web_sys::Node>());
    }
    ed.dom
        .chapter_count
        .set_text_content(Some(&format!("{kept}/{}", ed.rows.len())));
}

fn start_tick(editor: &Rc<RefCell<Editor>>) {
    let Some(win) = web_sys::window() else { return };
    let ed_ref = editor.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        let (player, lo, hi) = {
            let ed = ed_ref.borrow();
            (ed.player.clone(), ed.lo_ms, ed.hi_ms)
        };
        let Some(p) = player else { return };
        let Ok(pb) = p.try_borrow() else { return };
        let at = pb.current_ms();
        drop(pb);
        let frac = ms_to_frac(at, lo, hi) * 100.0;
        let ed = ed_ref.borrow();
        let _ = ed
            .dom
            .playhead
            .style()
            .set_property("left", &format!("{frac:.3}%"));
    });
    let handle = win
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 250)
        .ok();
    let mut ed = editor.borrow_mut();
    ed.tick = handle;
    ed._keep.push(KeepAlive::Tick(cb));
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct PublishBody<'a> {
    workspace: &'a str,
    from_ms: u64,
    to_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    chapters: Vec<Chapter>,
}

#[derive(serde::Deserialize, Default)]
struct PublishReply {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn confirm_publish(editor: &Rc<RefCell<Editor>>) {
    let ed = editor.borrow();
    if ed.publishing {
        return;
    }
    let n = effective_chapters(&ed.rows, ed.from_ms, ed.to_ms).len();
    let summary = format!(
        "publish to your configured publisher? {} · {n} chapter{}",
        fmt_duration(ed.to_ms.saturating_sub(ed.from_ms)),
        if n == 1 { "" } else { "s" }
    );
    ed.dom.pub_body.set_inner_html(CONFIRM_HTML);
    if let Some(line) = q(&ed.dom.pub_body, "#rpe-pub-q") {
        line.set_text_content(Some(&summary));
    }
    show_pub(&ed);
}

fn do_publish(editor: &Rc<RefCell<Editor>>) {
    if editor.borrow().publishing {
        return;
    }
    let (url, body_json) = {
        let mut ed = editor.borrow_mut();
        ed.publishing = true;
        let title = ed.dom.title.value();
        let title = title.trim().to_string();
        let body = PublishBody {
            workspace: &ed.workspace,
            from_ms: ed.from_ms,
            to_ms: ed.to_ms,
            title: if title.is_empty() {
                None
            } else {
                Some(title.as_str())
            },
            chapters: effective_chapters(&ed.rows, ed.from_ms, ed.to_ms),
        };
        let json = match serde_json::to_string(&body) {
            Ok(j) => j,
            Err(e) => {
                let msg = format!("could not encode the publish request: {e}");
                ed.publishing = false;
                show_pub_message(&ed, &msg, true);
                return;
            }
        };
        let origin = web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default();
        (
            format!("{origin}/replay/publish?token={}", enc(&ed.token)),
            json,
        )
    };

    {
        let ed = editor.borrow();
        ed.dom.pub_body.set_inner_html(SPINNER_HTML);
        show_pub(&ed);
        let _ = ed.dom.publish_btn.set_attribute("disabled", "disabled");
    }

    let Some(win) = web_sys::window() else { return };
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&JsValue::from_str(&body_json));
    if let Ok(headers) = web_sys::Headers::new() {
        if headers.set("content-type", "application/json").is_ok() {
            opts.set_headers(&headers);
        }
    }
    let req = match web_sys::Request::new_with_str_and_init(&url, &opts) {
        Ok(r) => r,
        Err(_) => {
            finish_publish(editor, Err("could not build the publish request".into()));
            return;
        }
    };

    let on_response = {
        let editor = editor.clone();
        Closure::<dyn FnMut(JsValue)>::new(move |val: JsValue| {
            let Some(resp) = val.dyn_ref::<web_sys::Response>() else {
                finish_publish(&editor, Err("bridge returned a non-response".into()));
                return;
            };
            let status = resp.status();
            let text = match resp.text() {
                Ok(p) => p,
                Err(_) => {
                    finish_publish(&editor, Err(format!("bridge replied {status}, unreadable")));
                    return;
                }
            };
            // Second hop: body text → {url} | {error}.
            let ok_cb = {
                let editor = editor.clone();
                Closure::<dyn FnMut(JsValue)>::new(move |body: JsValue| {
                    let body = body.as_string().unwrap_or_default();
                    let reply: PublishReply = serde_json::from_str(&body).unwrap_or_default();
                    let outcome = match (reply.url, reply.error) {
                        (Some(u), _) if !u.is_empty() => Ok(u),
                        (_, Some(e)) if !e.is_empty() => Err(e),
                        _ => Err(format!("bridge replied {status} with no url")),
                    };
                    finish_publish(&editor, outcome);
                })
            };
            let err_cb = {
                let editor = editor.clone();
                Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
                    finish_publish(&editor, Err(format!("bridge replied {status}, body lost")));
                })
            };
            let _ = text.then2(&ok_cb, &err_cb);
            // One-shot: the promise owns the callbacks from here.
            ok_cb.forget();
            err_cb.forget();
        })
    };
    let on_error = {
        let editor = editor.clone();
        Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
            let msg = e
                .as_string()
                .or_else(|| {
                    e.dyn_ref::<js_sys::Error>()
                        .map(|err| String::from(err.message()))
                })
                .unwrap_or_else(|| "network error reaching the bridge".into());
            finish_publish(&editor, Err(msg));
        })
    };
    let _ = win.fetch_with_request(&req).then2(&on_response, &on_error);
    on_response.forget();
    on_error.forget();
}

fn finish_publish(editor: &Rc<RefCell<Editor>>, outcome: Result<String, String>) {
    // The whole body runs under one shared borrow, released before the final
    // `borrow_mut` — a publish callback that panicked on a double borrow would
    // leave the button disabled forever.
    {
        let ed = editor.borrow();
        let _ = ed.dom.publish_btn.remove_attribute("disabled");
        match outcome {
            Ok(url) => {
                ed.dom.pub_body.set_inner_html(SUCCESS_HTML);
                if let Some(input) = q(&ed.dom.pub_body, "#rpe-pub-url")
                    .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
                {
                    input.set_value(&url);
                }
                if let Some(a) = q(&ed.dom.pub_body, "#rpe-pub-open") {
                    let _ = a.set_attribute("href", &url);
                }
                show_pub(&ed);
            }
            Err(err) => show_pub_message(&ed, &err, true),
        }
    }
    editor.borrow_mut().publishing = false;
}

fn copy_published_url(ed: &Editor) {
    let Some(input) = q(&ed.dom.pub_body, "#rpe-pub-url")
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    else {
        return;
    };
    let value = input.value();
    input.select();
    let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) else {
        return;
    };
    let _ = clipboard.write_text(&value);
    if let Some(btn) = q(&ed.dom.pub_body, "#rpe-pub-copy") {
        btn.set_text_content(Some("copied"));
    }
}

fn show_pub(ed: &Editor) {
    let _ = ed.dom.pub_panel.style().set_property("display", "block");
}

fn hide_pub(ed: &Editor) {
    let _ = ed.dom.pub_panel.style().set_property("display", "none");
}

/// A one-line message in the publish strip. `danger` marks failures; the
/// publish-command hint is attached there because a missing publisher config
/// is by far the most common cause and the fix lives in one file.
fn show_pub_message(ed: &Editor, msg: &str, danger: bool) {
    ed.dom.pub_body.set_inner_html(if danger {
        ERROR_HTML
    } else {
        NOTE_HTML
    });
    if let Some(line) = q(&ed.dom.pub_body, "#rpe-pub-msg") {
        line.set_text_content(Some(msg));
    }
    show_pub(ed);
}

// ---------------------------------------------------------------------------
// static markup + styling (candlelit — docs/THEME.md)
// ---------------------------------------------------------------------------

const SKELETON: &str = r##"<div class="rpe-head">
  <div id="rpe-heading" class="rpe-h1"></div>
  <input id="rpe-title" class="rpe-title" type="text" placeholder="untitled session"
         maxlength="120" spellcheck="false" autocomplete="off" />
  <div id="rpe-publish" class="rpe-btn rpe-btn-flame" title="publish this window">publish ✦</div>
  <div id="rpe-close" class="rpe-x" title="leave the editor">✕</div>
</div>
<div class="rpe-warn">
  <span class="rpe-warn-mark">⚠</span>
  <span class="rpe-warn-text">everything on these screens ships — scrub before you share</span>
  <div id="rpe-review" class="rpe-btn rpe-btn-violet" title="step through every chapter in chapters mode">review pass</div>
</div>
<div id="rpe-pub" class="rpe-pub"><div id="rpe-pub-body"></div></div>
<div class="rpe-body">
  <div class="rpe-main">
    <div class="rpe-trimwrap">
      <div id="rpe-trim" class="rpe-trim">
        <div class="rpe-track"></div>
        <div id="rpe-span" class="rpe-span"></div>
        <div id="rpe-playhead" class="rpe-playhead"></div>
        <div id="rpe-h-from" class="rpe-handle" title="drag: start of the shared window">✦</div>
        <div id="rpe-h-to" class="rpe-handle" title="drag: end of the shared window">✦</div>
      </div>
      <div class="rpe-trimctl">
        <span id="rpe-duration" class="rpe-dur"></span>
        <div id="rpe-set-start" class="rpe-btn" title="move the start handle to the playhead">set start to playhead</div>
        <div id="rpe-set-end" class="rpe-btn" title="move the end handle to the playhead">set end to playhead</div>
        <div id="rpe-more" class="rpe-btn rpe-btn-ghost" title="reload the editor two hours further back (refetches)">load more history</div>
      </div>
    </div>
    <div id="rpe-player" class="rpe-player"></div>
  </div>
  <div class="rpe-side">
    <div class="rpe-side-head">chapters <span id="rpe-chcount" class="rpe-count"></span></div>
    <div class="rpe-side-note">click a title to fix it · enter commits · unchecked chapters are dropped from the share</div>
    <div id="rpe-chapters" class="rpe-chapters"></div>
  </div>
</div>"##;

const CHAPTER_HTML: &str = r##"<div class="rpe-ch-top">
<input class="rpe-ch-box" type="checkbox" />
<span class="rpe-ch-at" title="seek here"></span>
<span class="rpe-ch-pane"></span>
</div>
<div class="rpe-ch-text" contenteditable="true" spellcheck="false"></div>"##;

const CONFIRM_HTML: &str = r##"<div id="rpe-pub-q" class="rpe-pub-q"></div>
<div class="rpe-pub-row">
<div id="rpe-pub-go" class="rpe-btn rpe-btn-flame">publish</div>
<div id="rpe-pub-cancel" class="rpe-btn rpe-btn-ghost">cancel</div>
</div>"##;

const SPINNER_HTML: &str =
    r##"<div class="rpe-pub-q"><span class="rpe-spin"></span> publishing…</div>"##;

const SUCCESS_HTML: &str = r##"<div class="rpe-pub-q">published.</div>
<div class="rpe-pub-row">
<input id="rpe-pub-url" class="rpe-url" type="text" readonly />
<div id="rpe-pub-copy" class="rpe-btn">copy</div>
<a id="rpe-pub-open" class="rpe-btn rpe-btn-violet" target="_blank" rel="noopener noreferrer">open</a>
<div id="rpe-pub-cancel" class="rpe-btn rpe-btn-ghost">done</div>
</div>"##;

const ERROR_HTML: &str = r##"<div id="rpe-pub-msg" class="rpe-pub-err"></div>
<div class="rpe-pub-hint">the publisher runs <code>publish_command</code> from
<code>~/.config/seance/publish.json</code> — if that file is missing or the command
exits non-zero, this is what you see.</div>
<div class="rpe-pub-row"><div id="rpe-pub-cancel" class="rpe-btn rpe-btn-ghost">dismiss</div></div>"##;

const NOTE_HTML: &str = r##"<div id="rpe-pub-msg" class="rpe-pub-q"></div>
<div class="rpe-pub-row"><div id="rpe-pub-cancel" class="rpe-btn rpe-btn-ghost">dismiss</div></div>"##;

fn ensure_style(doc: &web_sys::Document) {
    if doc.get_element_by_id("rpe-style").is_some() {
        return;
    }
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    style.set_id("rpe-style");
    style.set_text_content(Some(STYLE));
    if let Some(head) = doc.head() {
        let node: &web_sys::Node = style.unchecked_ref();
        let _ = head.append_child(node);
    }
}

/// Literal hex fallbacks per docs/THEME.md wherever a CSS var may be absent —
/// the editor must never render unreadable.
const STYLE: &str = r#"#rpe-root{position:fixed;inset:0;z-index:1100;display:flex;
flex-direction:column;background:var(--bg,#131111);color:var(--text-dim,#A69A91);
font-size:12px;line-height:1.5;user-select:none;}
#rpe-root .rpe-head{display:flex;align-items:center;gap:12px;padding:10px 14px;
border-bottom:1px solid var(--border,#352C2E);background:var(--bg-elevated,#1C1718);}
#rpe-root .rpe-h1{font-size:13px;font-weight:600;color:var(--text,#EBE3DB);
letter-spacing:.02em;white-space:nowrap;}
#rpe-root .rpe-title{flex:1;min-width:120px;padding:5px 9px;font:inherit;
color:var(--text,#EBE3DB);background:var(--surface,#211C1D);
border:1px solid var(--border,#352C2E);border-radius:5px;outline:none;user-select:text;}
#rpe-root .rpe-title:focus{border-color:var(--flame,#E9A03A);}
#rpe-root .rpe-title::placeholder{color:var(--text-faint,#69605D);}
#rpe-root .rpe-x{padding:2px 7px;border-radius:4px;color:var(--text-faint,#69605D);cursor:pointer;}
#rpe-root .rpe-x:hover{color:var(--flame,#E9A03A);background:var(--surface,#211C1D);}
#rpe-root .rpe-btn{padding:4px 10px;border-radius:5px;cursor:pointer;
white-space:nowrap;color:var(--text-dim,#A69A91);background:var(--surface,#211C1D);
border:1px solid var(--border,#352C2E);text-decoration:none;display:inline-block;}
#rpe-root .rpe-btn:hover{color:var(--text,#EBE3DB);border-color:var(--flame-dim,#A97328);}
#rpe-root .rpe-btn-flame{color:var(--flame,#E9A03A);border-color:var(--flame-dim,#A97328);
box-shadow:0 0 18px rgba(233,160,58,.10);}
#rpe-root .rpe-btn-flame:hover{color:var(--bg,#131111);background:var(--flame,#E9A03A);}
#rpe-root .rpe-btn-violet{color:var(--violet,#A790D5);border-color:var(--violet-dim,#6C5B95);}
#rpe-root .rpe-btn-violet:hover{color:var(--text,#EBE3DB);border-color:var(--violet,#A790D5);}
#rpe-root .rpe-btn-ghost{background:transparent;color:var(--text-faint,#69605D);}
#rpe-root [disabled]{opacity:.45;pointer-events:none;}
#rpe-root .rpe-warn{display:flex;align-items:center;gap:10px;padding:6px 14px;
background:rgba(233,160,58,.07);border-bottom:1px solid var(--border,#352C2E);}
#rpe-root .rpe-warn-mark{color:var(--flame,#E9A03A);}
#rpe-root .rpe-warn-text{flex:1;color:var(--flame,#E9A03A);}
#rpe-root .rpe-pub{display:none;padding:10px 14px;
border-bottom:1px solid var(--border,#352C2E);background:var(--bg-elevated,#1C1718);}
#rpe-root .rpe-pub-q{color:var(--text,#EBE3DB);}
#rpe-root .rpe-pub-err{color:var(--danger,#D0675D);}
#rpe-root .rpe-pub-hint{margin-top:4px;color:var(--text-faint,#69605D);}
#rpe-root .rpe-pub-hint code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
font-size:11px;color:var(--text-dim,#A69A91);}
#rpe-root .rpe-pub-row{display:flex;align-items:center;gap:8px;margin-top:8px;}
#rpe-root .rpe-url{flex:1;padding:5px 9px;font:inherit;user-select:text;
font-family:ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--flame,#E9A03A);
background:var(--surface,#211C1D);border:1px solid var(--border,#352C2E);
border-radius:5px;outline:none;}
#rpe-root .rpe-spin{display:inline-block;width:9px;height:9px;margin-right:5px;
border:1.5px solid var(--flame-dim,#A97328);border-top-color:transparent;
border-radius:50%;animation:rpe-spin .7s linear infinite;vertical-align:-1px;}
@keyframes rpe-spin{to{transform:rotate(360deg)}}
#rpe-root .rpe-body{flex:1;display:flex;min-height:0;}
#rpe-root .rpe-main{flex:1;display:flex;flex-direction:column;min-width:0;padding:12px 14px;gap:10px;}
#rpe-root .rpe-trimwrap{display:flex;flex-direction:column;gap:8px;}
#rpe-root .rpe-trim{position:relative;height:22px;cursor:default;}
#rpe-root .rpe-track{position:absolute;left:0;right:0;top:9px;height:4px;
border-radius:2px;background:var(--surface,#211C1D);
border:1px solid var(--border,#352C2E);box-sizing:border-box;}
#rpe-root .rpe-span{position:absolute;top:8px;height:6px;border-radius:3px;
background:var(--flame,#E9A03A);box-shadow:0 0 14px rgba(233,160,58,.35);}
#rpe-root .rpe-playhead{position:absolute;top:3px;width:1px;height:16px;
background:var(--violet,#A790D5);opacity:.85;}
#rpe-root .rpe-handle{position:absolute;top:0;transform:translateX(-50%);
width:18px;text-align:center;color:var(--flame,#E9A03A);cursor:ew-resize;
font-size:13px;line-height:22px;text-shadow:0 0 10px rgba(233,160,58,.6);}
#rpe-root .rpe-handle:hover{color:#F4BE62;}
#rpe-root .rpe-trimctl{display:flex;align-items:center;gap:8px;flex-wrap:wrap;}
#rpe-root .rpe-dur{margin-right:4px;color:var(--flame,#E9A03A);}
#rpe-root .rpe-player{flex:1;min-height:0;border-radius:6px;overflow:hidden;position:relative;
border:1px solid var(--border,#352C2E);background:var(--bg-elevated,#1C1718);}
#rpe-root .rpe-side{width:300px;flex:0 0 300px;display:flex;flex-direction:column;
min-height:0;border-left:1px solid var(--border,#352C2E);
background:var(--bg-elevated,#1C1718);}
#rpe-root .rpe-side-head{padding:10px 12px 4px;color:var(--text,#EBE3DB);font-weight:600;}
#rpe-root .rpe-count{color:var(--text-faint,#69605D);font-weight:400;}
#rpe-root .rpe-side-note{padding:0 12px 8px;color:var(--text-faint,#69605D);font-size:11px;}
#rpe-root .rpe-chapters{flex:1;overflow-y:auto;overscroll-behavior:contain;padding:0 8px 12px;}
#rpe-root .rpe-chapters::-webkit-scrollbar{width:8px}
#rpe-root .rpe-chapters::-webkit-scrollbar-thumb{background:var(--border,#352C2E);border-radius:4px}
#rpe-root .rpe-empty{padding:14px 4px;color:var(--text-faint,#69605D);}
#rpe-root .rpe-ch{padding:7px 8px;margin:3px 0;border-radius:5px;
border:1px solid transparent;}
#rpe-root .rpe-ch:hover{background:var(--surface,#211C1D);border-color:var(--border,#352C2E);}
#rpe-root .rpe-ch.off{opacity:.45;}
#rpe-root .rpe-ch.out{opacity:.28;}
#rpe-root .rpe-ch-top{display:flex;align-items:center;gap:7px;font-size:11px;}
#rpe-root .rpe-ch-box{accent-color:#E9A03A;cursor:pointer;margin:0;}
#rpe-root .rpe-ch-at{color:var(--flame,#E9A03A);cursor:pointer;
font-family:ui-monospace,SFMono-Regular,Menlo,monospace;}
#rpe-root .rpe-ch-pane{color:var(--violet,#A790D5);overflow:hidden;
text-overflow:ellipsis;white-space:nowrap;}
#rpe-root .rpe-ch-text{margin-top:3px;padding:2px 4px;border-radius:4px;
color:var(--text,#EBE3DB);white-space:pre-wrap;overflow-wrap:anywhere;
user-select:text;cursor:text;outline:none;max-height:6.5em;overflow-y:auto;}
#rpe-root .rpe-ch-text:hover{background:rgba(0,0,0,.25);}
#rpe-root .rpe-ch-text:focus{background:var(--bg,#131111);
box-shadow:inset 0 0 0 1px var(--flame-dim,#A97328);}"#;

// ---------------------------------------------------------------------------
// tests — pure logic only, no web_sys, so `cargo test -p seance-web` runs natively
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t_ms: u64, text: &str, included: bool) -> ChapterRow {
        ChapterRow {
            t_ms,
            pane: "w-1".into(),
            text: text.into(),
            included,
        }
    }

    #[test]
    fn clamp_keeps_min_span_and_bounds() {
        assert_eq!(clamp_range(0, 10_000, 1_000, 9_000), (1_000, 9_000));
        // Dragging `from` past `to` pushes `to` along.
        let (f, t) = clamp_range(8_500, 8_000, 0, 10_000);
        assert_eq!((f, t), (8_500, 9_500));
        // …unless there's no room, in which case the far edge pins and `from`
        // gives way.
        let (f, t) = clamp_range(9_900, 9_950, 0, 10_000);
        assert_eq!((f, t), (9_000, 10_000));
        // Negative proposals clamp to lo rather than wrapping.
        assert_eq!(clamp_range(-5_000, 4_000, 1_000, 9_000), (1_000, 4_000));
    }

    #[test]
    fn frac_round_trips_and_clamps() {
        assert_eq!(frac_to_ms(0.5, 1_000, 3_000), 2_000);
        assert_eq!(frac_to_ms(-3.0, 1_000, 3_000), 1_000);
        assert_eq!(frac_to_ms(9.0, 1_000, 3_000), 3_000);
        assert!((ms_to_frac(2_000, 1_000, 3_000) - 0.5).abs() < 1e-9);
        assert_eq!(ms_to_frac(0, 1_000, 3_000), 0.0);
        // Degenerate window must not divide by zero.
        assert_eq!(ms_to_frac(5, 7, 7), 0.0);
        assert_eq!(frac_to_ms(0.5, 7, 7), 7);
    }

    #[test]
    fn durations_and_offsets_format() {
        assert_eq!(fmt_duration(40_000), "40s");
        assert_eq!(fmt_duration(760_000), "12m 40s");
        assert_eq!(fmt_duration(3_849_000), "1h 04m 09s");
        assert_eq!(fmt_offset(1_247_000, 1_000_000), "4:07");
        assert_eq!(fmt_offset(4_847_000, 1_000_000), "1:04:07");
        // Before the origin reads as zero, never as an underflowed epoch.
        assert_eq!(fmt_offset(5, 1_000_000), "0:00");
    }

    #[test]
    fn effective_chapters_drop_excluded_out_of_range_and_blank() {
        let rows = vec![
            row(100, "before the window", true),
            row(200, "kept", true),
            row(250, "   ", true),
            row(300, "unchecked", false),
            row(400, "also kept", true),
            row(900, "after the window", true),
        ];
        let got = effective_chapters(&rows, 150, 500);
        assert_eq!(
            got.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["kept", "also kept"]
        );
        assert_eq!(first_effective_ms(&rows, 150, 500), Some(200));
        // Widening restores rows whose `included` flag was never touched.
        assert_eq!(effective_chapters(&rows, 0, 10_000).len(), 4);
        assert_eq!(first_effective_ms(&rows, 0, 10_000), Some(100));
        // Nothing in range → no start hint (caller falls back to `from_ms`).
        assert_eq!(first_effective_ms(&rows, 600, 800), None);
    }

    #[test]
    fn effective_trims_whitespace_into_the_published_text() {
        let rows = vec![row(10, "  fix the parser \n", true)];
        assert_eq!(effective_chapters(&rows, 0, 100)[0].text, "fix the parser");
    }

    #[test]
    fn default_window_falls_back_to_two_hours() {
        let now = 10 * TWO_HOURS_MS;
        assert_eq!(default_window(now, None, None), (now - TWO_HOURS_MS, now));
        assert_eq!(default_window(now, Some(5), Some(9_000)), (5, 9_000));
        // A degenerate explicit range is widened rather than rendered as a
        // zero-width trim bar.
        let (f, t) = default_window(now, Some(9_000), Some(9_000));
        assert_eq!(t, 9_000);
        assert!(t - f >= MIN_SPAN_MS);
        // No `to` → anchored at now.
        assert_eq!(default_window(now, Some(now - 60_000), None).1, now);
    }

    #[test]
    fn row_effectiveness_respects_both_gates() {
        let r = row(500, "x", true);
        assert!(r.effective(0, 1_000));
        assert!(!r.effective(600, 1_000));
        assert!(!row(500, "x", false).effective(0, 1_000));
        // Boundaries are inclusive — a chapter exactly on a handle ships.
        assert!(row(500, "x", true).effective(500, 500));
    }
}
