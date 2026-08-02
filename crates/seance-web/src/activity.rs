//! Activity drawer — the attributed event feed, web side.
//!
//! The native app renders this from `events.jsonl` (`chrome.rs::render_activity`);
//! here the daemon already pushes the same events over the wire and
//! [`ClientState::note_activity`] keeps the tail in `state.activity`
//! (VecDeque, newest LAST, capped). The drawer renders that deque newest
//! FIRST, so what just happened is under your eyes without scrolling.
//!
//! Shape: a right-side drawer, full height under the topbar, elevated ground
//! with a warm hairline on its left edge. It never overlays the tiles' focus
//! ring by accident — it sits above them (z-index) but leaves the topbar
//! alone, so the connection dot stays visible while you read.
//!
//! Cost: the DOM is built once and cached in a thread-local; [`refresh`]
//! rebuilds only the list, only while open (a closed drawer is a single bool
//! test per badge tick). One `set_inner_html` over ≤200 short rows is not a
//! hot path — it happens on badge changes, not per frame.

// NEEDS web-sys feature: Node
//   (`Node::append_child`, via `Element: Deref<Target = Node>`, to mount the
//   drawer into `document.body` and the <style> into <head>.) Everything else
//   used here — `Window`, `Document`, `Element`, `HtmlElement`,
//   `CssStyleDeclaration`, `MouseEvent`, `Performance`, `HtmlHeadElement` —
//   is already enabled in crates/seance-web/Cargo.toml.

use std::cell::RefCell;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::state::ClientState;

thread_local! {
    /// The one drawer. `None` until first open.
    static DRAWER: RefCell<Option<Drawer>> = const { RefCell::new(None) };
}

struct Drawer {
    root: web_sys::Element,
    /// The scrolling <div> that holds the rows (rebuilt by `refresh`).
    list: web_sys::Element,
    open: bool,
    /// ✕ handler, kept alive for the drawer's lifetime.
    _close: Closure<dyn FnMut(web_sys::MouseEvent)>,
}

/// Open the drawer if closed, close it if open.
pub fn toggle(state: &ClientState) {
    let built = DRAWER.with(|slot| slot.borrow().is_some());
    if !built {
        build();
    }
    let now_open = DRAWER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(d) = slot.as_mut() else { return false };
        d.open = !d.open;
        set_display(&d.root, d.open);
        d.open
    });
    if now_open {
        refresh(state);
        DRAWER.with(|slot| {
            if let Some(d) = slot.borrow().as_ref() {
                d.list.set_scroll_top(0);
            }
        });
    }
}

pub fn is_open() -> bool {
    DRAWER.with(|slot| slot.borrow().as_ref().is_some_and(|d| d.open))
}

/// Re-render if open (called when badges/activity change). No-op otherwise.
pub fn refresh(state: &ClientState) {
    let now = now_ms();
    DRAWER.with(|slot| {
        let slot = slot.borrow();
        let Some(d) = slot.as_ref() else { return };
        if !d.open {
            return;
        }
        // Pin to the top when already at the top; otherwise hold the reader's
        // position (an arriving event must not yank the line they're reading).
        let was_at_top = d.list.scroll_top() <= 2;
        let prev = d.list.scroll_top();
        d.list.set_inner_html(&rows_html(state, now));
        d.list.set_scroll_top(if was_at_top { 0 } else { prev });
    });
}

/// Close if open; true when something was closed.
pub fn close() -> bool {
    DRAWER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(d) = slot.as_mut() else {
            return false;
        };
        if !d.open {
            return false;
        }
        d.open = false;
        set_display(&d.root, false);
        true
    })
}

fn set_display(root: &web_sys::Element, open: bool) {
    let Some(el) = root.dyn_ref::<web_sys::HtmlElement>() else {
        return;
    };
    let _ = el
        .style()
        .set_property("display", if open { "flex" } else { "none" });
}

fn build() {
    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };
    let Some(body) = doc.body() else { return };
    ensure_style(&doc);

    let Ok(root) = doc.create_element("div") else {
        return;
    };
    root.set_id("activity-drawer");
    root.set_inner_html(
        r#"<div id="activity-head"><span class="ah-title">activity</span>
<span class="ah-sub">newest first</span><span class="ah-spacer"></span>
<span id="activity-close" title="close (escape)">✕</span></div>
<div id="activity-list"></div>"#,
    );
    set_display(&root, false);

    let Some(list) = doc_child(&root, "activity-list") else {
        return;
    };

    let close_cb =
        Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            let hit = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|e| e.closest("#activity-close").ok().flatten())
                .is_some();
            if hit {
                ev.stop_propagation();
                close();
            }
        });
    let _ = root.add_event_listener_with_callback("mousedown", close_cb.as_ref().unchecked_ref());

    let node: &web_sys::Node = root.unchecked_ref();
    if body.append_child(node).is_err() {
        return;
    }

    DRAWER.with(|slot| {
        *slot.borrow_mut() = Some(Drawer {
            root,
            list,
            open: false,
            _close: close_cb,
        });
    });
}

/// Find a just-built child by id without a document round-trip failing when
/// the node isn't mounted yet.
fn doc_child(root: &web_sys::Element, id: &str) -> Option<web_sys::Element> {
    root.query_selector(&format!("#{id}")).ok().flatten()
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

fn rows_html(state: &ClientState, now: f64) -> String {
    if state.activity.is_empty() {
        return r#"<div class="act-empty">nothing yet — the circle is quiet</div>"#.to_string();
    }
    let mut out = String::with_capacity(state.activity.len() * 96);
    for item in state.activity.iter().rev() {
        let cls = actor_class(&item.actor);
        out.push_str(r#"<div class="act-row"><div class="act-when">"#);
        out.push_str(&esc(&rel_time(now - item.at_ms)));
        out.push_str(
            r#"</div><div class="act-body"><div class="act-meta"><span class="act-actor "#,
        );
        out.push_str(cls);
        out.push_str(r#"">"#);
        out.push_str(&esc(&item.actor));
        out.push_str("</span>");
        if let Some(pane) = item.pane.as_deref() {
            out.push_str(r#"<span class="act-pane">"#);
            out.push_str(&esc(pane));
            out.push_str("</span>");
        }
        out.push_str(r#"</div><div class="act-text">"#);
        out.push_str(&esc(&item.text));
        out.push_str("</div></div></div>");
    }
    out
}

/// human = flame (you), agent* = violet (them), daemon/anything else = faint.
fn actor_class(actor: &str) -> &'static str {
    if actor == "human" {
        "human"
    } else if actor.starts_with("agent") {
        "agent"
    } else {
        "other"
    }
}

/// Coarse "how long ago", from a millisecond delta on the `performance.now`
/// clock. Deliberately one unit wide — this column is a glance, not a stopwatch.
fn rel_time(delta_ms: f64) -> String {
    let s = (delta_ms / 1000.0).max(0.0);
    if s < 5.0 {
        return "now".into();
    }
    if s < 60.0 {
        return format!("{}s", s as u64);
    }
    let m = s / 60.0;
    if m < 60.0 {
        return format!("{}m", m as u64);
    }
    let h = m / 60.0;
    if h < 24.0 {
        return format!("{}h", h as u64);
    }
    format!("{}d", (h / 24.0) as u64)
}

/// Minimal HTML escape — activity text is daemon/agent-authored and must never
/// be able to author DOM.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn ensure_style(doc: &web_sys::Document) {
    if doc.get_element_by_id("activity-drawer-style").is_some() {
        return;
    }
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    style.set_id("activity-drawer-style");
    style.set_text_content(Some(STYLE));
    if let Some(head) = doc.head() {
        let node: &web_sys::Node = style.unchecked_ref();
        let _ = head.append_child(node);
    }
}

const STYLE: &str = r#"#activity-drawer{position:fixed;right:0;top:var(--topbar-h,36px);
bottom:0;width:300px;max-width:88vw;z-index:900;display:none;flex-direction:column;
background:var(--bg-elevated,#1C1718);border-left:1px solid var(--border,#352C2E);
box-shadow:-14px 0 34px rgba(0,0,0,.42);font-size:11.5px;line-height:1.45;
color:var(--text-dim,#A69A91);}
#activity-head{flex:0 0 auto;display:flex;align-items:baseline;gap:8px;
padding:8px 10px;border-bottom:1px solid var(--border,#352C2E);}
#activity-head .ah-title{color:var(--text,#EBE3DB);font-weight:600;}
#activity-head .ah-sub{color:var(--text-faint,#69605D);font-size:10px;}
#activity-head .ah-spacer{flex:1 1 auto;}
#activity-close{cursor:pointer;user-select:none;padding:0 4px;border-radius:3px;
color:var(--text-faint,#69605D);}
#activity-close:hover{color:var(--flame,#E9A03A);background:var(--surface,#211C1D);}
#activity-list{flex:1 1 auto;min-height:0;overflow-y:auto;overscroll-behavior:contain;
padding:4px 0 12px;}
#activity-list::-webkit-scrollbar{width:8px}
#activity-list::-webkit-scrollbar-thumb{background:var(--border,#352C2E);border-radius:4px}
.act-row{display:flex;gap:8px;padding:4px 10px;align-items:baseline;}
.act-row:hover{background:var(--surface,#211C1D);}
.act-when{flex:0 0 30px;text-align:right;color:var(--text-faint,#69605D);
font-variant-numeric:tabular-nums;font-size:10.5px;}
.act-body{flex:1 1 auto;min-width:0;}
.act-meta{display:flex;gap:6px;align-items:baseline;}
.act-actor{font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.act-actor.human{color:var(--flame,#E9A03A);}
.act-actor.agent{color:var(--violet,#A790D5);}
.act-actor.other{color:var(--text-faint,#69605D);}
.act-pane{color:var(--text-faint,#69605D);font-size:10.5px;overflow:hidden;
text-overflow:ellipsis;white-space:nowrap;}
.act-text{color:var(--text-dim,#A69A91);word-break:break-word;}
.act-empty{padding:14px 12px;color:var(--text-faint,#69605D);}"#;

#[cfg(test)]
mod tests {
    use super::{actor_class, esc, rel_time};

    #[test]
    fn rel_time_buckets() {
        assert_eq!(rel_time(0.0), "now");
        assert_eq!(rel_time(-500.0), "now");
        assert_eq!(rel_time(4_000.0), "now");
        assert_eq!(rel_time(12_000.0), "12s");
        assert_eq!(rel_time(120_000.0), "2m");
        assert_eq!(rel_time(3.0 * 3_600_000.0), "3h");
        assert_eq!(rel_time(50.0 * 3_600_000.0), "2d");
    }

    #[test]
    fn actor_colors() {
        assert_eq!(actor_class("human"), "human");
        assert_eq!(actor_class("agent:worker-1"), "agent");
        assert_eq!(actor_class("daemon"), "other");
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(esc("<b>&x</b>"), "&lt;b&gt;&amp;x&lt;/b&gt;");
    }
}
