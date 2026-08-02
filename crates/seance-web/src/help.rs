//! Grimoire overlay — the web counterpart of the native `HelpWindow`
//! (`src/app/chrome.rs::render_help`).
//!
//! Same book, different room. The native grimoire is a scrolling window; here
//! it is a dimmed full-viewport overlay with one centered, scrollable card.
//! Content is **static** — the DOM is built once on first open, cached in a
//! thread-local, and toggled by `display`. There is no per-frame cost and no
//! rebuild path.
//!
//! Where the web client diverges from the native app the grimoire says so out
//! loud — the chord table documents BOTH spellings (native `ctrl+shift+X` and
//! its `alt+X` twin, which is the reliable one in a browser), and native-only
//! features get their own dim section instead of being quietly omitted. A help
//! page that documents keys the page can never receive is a lying instrument.
//!
//! Dismissal: click the backdrop (or ✕), or press Escape — Escape arrives
//! through the app keymap (`ChromeCommand::Escape` → `App::escape_topmost` →
//! [`close`]), not through a listener of our own, so overlay precedence stays
//! in one place.

// NEEDS web-sys feature: Node
//   (`Node::append_child`, reached through `Element: Deref<Target = Node>`,
//   to mount the overlay into `document.body` and the <style> into <head>.)
//   Everything else used here — `Window`, `Document`, `Element`,
//   `HtmlElement`, `CssStyleDeclaration`, `MouseEvent`, `HtmlHeadElement` —
//   is already enabled in crates/seance-web/Cargo.toml.

use std::cell::RefCell;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

thread_local! {
    /// The one grimoire. `None` until first open; afterwards the node stays in
    /// the DOM and only its `display` changes.
    static GRIMOIRE: RefCell<Option<Grimoire>> = const { RefCell::new(None) };
}

struct Grimoire {
    root: web_sys::Element,
    open: bool,
    /// Backdrop/✕ click handler, kept alive for the overlay's lifetime.
    _click: Closure<dyn FnMut(web_sys::MouseEvent)>,
}

/// Open the grimoire if closed, close it if open.
pub fn toggle() {
    let built = GRIMOIRE.with(|slot| slot.borrow().is_some());
    if !built {
        build();
    }
    GRIMOIRE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(g) = slot.as_mut() else { return };
        g.open = !g.open;
        set_display(&g.root, g.open);
        if g.open {
            // Reopen at the top of the book, like the native window.
            g.root.set_scroll_top(0);
            if let Some(card) = g.root.first_element_child() {
                card.set_scroll_top(0);
            }
        }
    });
}

pub fn is_open() -> bool {
    GRIMOIRE.with(|slot| slot.borrow().as_ref().is_some_and(|g| g.open))
}

/// Close if open; true when something was closed.
pub fn close() -> bool {
    GRIMOIRE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(g) = slot.as_mut() else {
            return false;
        };
        if !g.open {
            return false;
        }
        g.open = false;
        set_display(&g.root, false);
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
    root.set_id("grimoire-overlay");
    root.set_inner_html(CARD_HTML);
    set_display(&root, false);

    // Backdrop (or ✕) click closes; clicks inside the card are ignored.
    let click = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        let Some(target) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let on_close_btn = target.closest("#grimoire-close").ok().flatten().is_some();
        let inside_card = target.closest("#grimoire-card").ok().flatten().is_some();
        if on_close_btn || !inside_card {
            ev.stop_propagation();
            close();
        }
    });
    let _ = root.add_event_listener_with_callback("mousedown", click.as_ref().unchecked_ref());

    let node: &web_sys::Node = root.unchecked_ref();
    if body.append_child(node).is_err() {
        return;
    }

    GRIMOIRE.with(|slot| {
        *slot.borrow_mut() = Some(Grimoire {
            root,
            open: false,
            _click: click,
        });
    });
}

fn ensure_style(doc: &web_sys::Document) {
    if doc.get_element_by_id("grimoire-style").is_some() {
        return;
    }
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    style.set_id("grimoire-style");
    style.set_text_content(Some(STYLE));
    if let Some(head) = doc.head() {
        let node: &web_sys::Node = style.unchecked_ref();
        let _ = head.append_child(node);
    }
}

/// Candlelit styling. Falls back to literal hex from docs/THEME.md wherever a
/// CSS var isn't defined — the overlay must never render unreadable.
const STYLE: &str = r#"#grimoire-overlay{position:fixed;inset:0;z-index:1200;display:none;
align-items:flex-start;justify-content:center;padding:5vh 16px;
background:rgba(9,7,8,.72);backdrop-filter:blur(2px);
font-size:12px;line-height:1.55;}
#grimoire-card{position:relative;width:880px;max-width:100%;max-height:85vh;
overflow-y:auto;overscroll-behavior:contain;padding:20px 24px 28px;
background:var(--bg-elevated,#1C1718);border:1px solid var(--border,#352C2E);
border-radius:8px;box-shadow:0 24px 70px rgba(0,0,0,.65),
0 0 60px rgba(233,160,58,.05);color:var(--text-dim,#A69A91);cursor:default;}
#grimoire-card::-webkit-scrollbar{width:8px}
#grimoire-card::-webkit-scrollbar-thumb{background:var(--border,#352C2E);border-radius:4px}
#grimoire-close{position:absolute;top:12px;right:14px;padding:2px 7px;
border-radius:4px;color:var(--text-faint,#69605D);cursor:pointer;
user-select:none;font-size:13px;}
#grimoire-close:hover{color:var(--flame,#E9A03A);background:var(--surface,#211C1D);}
#grimoire-card .g-title{font-size:16px;font-weight:600;
color:var(--text,#EBE3DB);letter-spacing:.02em;}
#grimoire-card .g-lede{padding:6px 0 2px;max-width:70ch;}
#grimoire-card h1{margin:18px 0 4px;font-size:12.5px;font-weight:600;
color:var(--text,#EBE3DB);letter-spacing:.01em;}
#grimoire-card h2{margin:12px 0 3px;font-size:11px;font-weight:600;
color:var(--violet,#A790D5);letter-spacing:.04em;}
#grimoire-card p{margin:4px 0;max-width:74ch;}
#grimoire-card ul{margin:2px 0;padding:0;list-style:none;}
#grimoire-card li{position:relative;padding-left:14px;margin:2px 0;max-width:74ch;}
#grimoire-card li::before{content:"\00b7";position:absolute;left:3px;
color:var(--flame-dim,#A97328);}
#grimoire-card table{width:100%;border-collapse:collapse;margin:3px 0 6px;}
#grimoire-card td{padding:2px 8px 2px 0;vertical-align:top;}
#grimoire-card td.k{width:170px;color:var(--flame,#E9A03A);}
#grimoire-card td.alt{width:118px;color:var(--flame-dim,#A97328);}
#grimoire-card td.d{color:var(--text-dim,#A69A91);}
#grimoire-card code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
font-size:11px;color:var(--text,#EBE3DB);background:var(--surface,#211C1D);
padding:0 4px;border-radius:3px;}
#grimoire-card .note{color:var(--text-faint,#69605D);}
#grimoire-card .glyph{color:var(--violet,#A790D5);}
#grimoire-card .g-native{margin-top:20px;padding:12px 14px;border-radius:6px;
border:1px dashed var(--border,#352C2E);background:rgba(0,0,0,.18);opacity:.62;}
#grimoire-card .g-native h1{margin-top:0;color:var(--text-dim,#A69A91);}
#grimoire-card .g-native td.k{color:var(--text-dim,#A69A91);}
#grimoire-card .g-foot{margin-top:20px;padding-top:10px;
border-top:1px solid var(--border,#352C2E);font-size:11px;
color:var(--text-faint,#69605D);}"#;

/// The book. Static, hand-authored — mirrors `render_help()` section for
/// section, rewritten for what a browser can actually do.
const CARD_HTML: &str = r##"<div id="grimoire-card">
<div id="grimoire-close" title="close (escape)">✕</div>

<div class="g-title">grimoire ✦ seance</div>
<p class="g-lede">a circle of agents and a human in one shared room. every pane
is live in front of you; every agent action and every human click flows through
one control plane we fully own. this tab is a full seat at that circle — the
panes run on the daemon, the pixels arrive here.</p>

<h1>what seance is</h1>
<ul>
<li>panes — terminal sessions (claude / codex / grok / shell) or live file viewers</li>
<li>workspaces — named circles; the tiling region shows only the selected one</li>
<li>control plane — <code>seance ctl</code> over a unix socket, so agents drive sibling panes</li>
<li>notes — each pane carries a shared markdown scratchpad the agent inside it reads and writes</li>
<li>attribution — human and agent actions land in one event log (the activity drawer, <code>alt+a</code>)</li>
</ul>
<p class="note">this client attaches to the same daemon as the native app.
circles are global and unowned — every window holds its own <em>subscription
set</em>, so the same circle can be open here and on the desktop at once. that
is the "seance from anywhere" affordance: attach from a browser, work, and the
daemon keeps it durable.</p>

<h1>keys — the web keymap</h1>
<p>browsers reserve <code>ctrl+shift+n</code> (incognito) and
<code>ctrl+shift+w</code> (close window) at the window level — the page never
sees them — and often swallow <code>ctrl+shift+j/k/r</code> for devtools and
reload. so every chord also binds an <strong>alt twin</strong>, and
<strong>alt is the reliable spelling on the web</strong>. both are listed;
prefer the middle column.</p>
<table>
<tr><td class="k note">chord</td><td class="alt note">alt twin</td><td class="d note">action</td></tr>
<tr><td class="k">ctrl+shift+n</td><td class="alt">alt+n</td><td class="d">summon a new shell pane in this circle <span class="note">— chord reserved by the browser, use alt</span></td></tr>
<tr><td class="k">ctrl+shift+w</td><td class="alt">alt+w</td><td class="d">kill the active pane; killing the last one banishes the circle <span class="note">— chord reserved, use alt</span></td></tr>
<tr><td class="k">ctrl+shift+z</td><td class="alt">alt+z</td><td class="d">focus-zoom the active pane (escape unzooms)</td></tr>
<tr><td class="k">ctrl+shift+m</td><td class="alt">alt+m</td><td class="d">the same zoom, second spelling — for keyboards where z is taken</td></tr>
<tr><td class="k">ctrl+shift+r</td><td class="alt">alt+r</td><td class="d">rename the selected workspace <span class="note">— chord may hard-reload the tab, use alt</span></td></tr>
<tr><td class="k">ctrl+shift+p</td><td class="alt">alt+p</td><td class="d">latency probe overlay <span class="note">— diverges from native, where this key pops a pane out</span></td></tr>
<tr><td class="k">ctrl+shift+a</td><td class="alt">alt+a</td><td class="d">activity drawer — the attributed event feed</td></tr>
<tr><td class="k">ctrl+shift+? &nbsp;/&nbsp; ctrl+shift+/</td><td class="alt">alt+? / alt+/</td><td class="d">this grimoire</td></tr>
<tr><td class="k">ctrl+pgup / pgdn</td><td class="alt">—</td><td class="d">previous / next workspace (sidebar order, wraps)</td></tr>
<tr><td class="k">ctrl+shift+pgup / pgdn</td><td class="alt">—</td><td class="d">previous / next pane in this circle</td></tr>
<tr><td class="k">escape</td><td class="alt">—</td><td class="d">close the topmost layer — menu → grimoire → activity → zoom → selection; with nothing open it belongs to the terminal</td></tr>
</table>
<ul>
<li>anything holding <code>cmd</code> (meta) is left to the browser on purpose — we never steal it</li>
<li>every key this table does not claim goes to the focused pane's pty verbatim</li>
</ul>

<h1>mouse</h1>
<table>
<tr><td class="k">right-click</td><td class="d">context menus on app surfaces — workspace rows, pane rows, tiles. the browser's own menu is suppressed there and deliberately kept inside text inputs</td></tr>
<tr><td class="k">wheel</td><td class="d">scrollback in the pane under the cursor</td></tr>
<tr><td class="k">drag</td><td class="d">select text in a pane; escape clears the selection</td></tr>
<tr><td class="k">ctrl+c / cmd+c</td><td class="d">copy the selection — the ordinary browser clipboard, no chord to learn</td></tr>
<tr><td class="k">ctrl+v / cmd+v</td><td class="d">paste into the focused pane as a bracketed paste</td></tr>
<tr><td class="k">click a pane</td><td class="d">focus it — the candle ring moves, keys follow</td></tr>
<tr><td class="k">click a sidebar row</td><td class="d">select that circle / focus that pane</td></tr>
</table>

<h1>the sidebar — legend</h1>
<table>
<tr><td class="k"><span class="glyph">◆</span></td><td class="d">a live pane — process running, someone home</td></tr>
<tr><td class="k"><span class="glyph">◈</span></td><td class="d">idle or exited — the process has left the circle</td></tr>
<tr><td class="k"><span class="glyph">⠿</span> spinner</td><td class="d">a braille spinner in the pane title means the agent in there is working right now; circles with working agents float to the top of the sidebar</td></tr>
<tr><td class="k">needs</td><td class="d">that pane wants a human — a pending ask, a block, or an agent-reported <code>needs-human</code></td></tr>
<tr><td class="k">done</td><td class="d">the agent reported <code>done</code> and is waiting to be read</td></tr>
<tr><td class="k">× banish</td><td class="d">kill. the first click arms it (turns red), a second within two seconds does it. banishing the last pane banishes the circle — ptys shut down, scratchpad files on disk are kept</td></tr>
<tr><td class="k">quicklaunch</td><td class="d">saved pane recipes (name + cwd + command) — one click summons a pane already pointed at the right project</td></tr>
<tr><td class="k">accounts</td><td class="d">the host-bridge strip: which claude account a new agent pane runs as. it fails closed — a quiet bridge shows an empty strip rather than a wrong one</td></tr>
</table>

<h1>asks + ghost proposals</h1>
<p>two agent→human surfaces, and both block the agent until you answer — that
is the point. an agent does not get to guess about you.</p>
<ul>
<li><strong>ask</strong> — <code>seance ctl ask "Q" --choices a,b</code> raises a toast above the tiles with buttons. you click; the agent's blocking call returns your answer</li>
<li><strong>ghost propose</strong> — <code>seance ctl propose PANE CMD</code> types a dimmed command into the pane and waits. <em>enter</em> or <em>tab</em> accepts and runs it, <em>escape</em> dismisses, and simply typing overrides it</li>
<li>prefer propose over a silent send for anything destructive — the dim text is the consent boundary</li>
</ul>

<h1>the activity drawer</h1>
<p><code>alt+a</code> slides out the event feed: newest first, each line stamped
with how long ago it happened, who did it (you in flame, agents in violet, the
daemon dim) and which pane it touched. spawns, kills, status changes, asks and
human touches all land in the same list — one log, both species.</p>

<h1>the probe</h1>
<p><code>alt+p</code> opens the latency probe. it stamps an input on its way to
a pane and closes that stamp when the answering grid frame paints, then shows
p50/p95/max echo latency alongside the websocket round-trip. it ships as a
feature rather than a debug flag so the performance claim is falsifiable by
anyone running the app. unanswered stamps are discarded, never counted — a dead
pane must not flatter the numbers.</p>

<h1>where things live</h1>
<table>
<tr><td class="k">panes + state</td><td class="d">the daemon's <code>~/.local/share/seance/state.json</code> — this tab holds no truth of its own</td></tr>
<tr><td class="k">notes</td><td class="d"><code>~/.local/share/seance/scratch/&lt;slug&gt;.md</code>, reachable inside a pane as <code>$SEANCE_SCRATCHPAD</code></td></tr>
<tr><td class="k">events</td><td class="d"><code>~/.local/share/seance/events.jsonl</code> — the activity drawer is a live tail of this</td></tr>
<tr><td class="k">socket</td><td class="d"><code>$XDG_RUNTIME_DIR/seance.sock</code> — one owner at a time</td></tr>
</table>

<div class="g-native">
<h1>native app only (for now)</h1>
<p>these exist in the desktop seance and have no web surface yet. they are
listed so the absence is legible rather than mysterious.</p>
<table>
<tr><td class="k">ctrl+shift+space</td><td class="d">overview — every circle at once</td></tr>
<tr><td class="k">ctrl+shift+k</td><td class="d">prompt palette — precanned prompts into the active agent</td></tr>
<tr><td class="k">ctrl+shift+j</td><td class="d">jump palette — fuzzy jump to a pane or circle</td></tr>
<tr><td class="k">ctrl+shift+s</td><td class="d">notes flip — turn the pane over onto its scratchpad face</td></tr>
<tr><td class="k">ctrl+shift+p</td><td class="d">popout — a pane in its own OS window (on the web this key is the probe)</td></tr>
<tr><td class="k">ctrl+shift+f</td><td class="d">jump to the last failed shell command</td></tr>
</table>
<p class="note">also native-only today: whisper (💬), one-click arm (⚡), the pad
drawer (▤), the phone/telegram card (☎) and the file-history scrubber. the
notes themselves are not gone — the agent still reads and writes
<code>$SEANCE_SCRATCHPAD</code>; you just cannot flip a pane over here yet.</p>
</div>

<div class="g-foot">the grimoire grows with the app — if a surface isn't here, that's a bug.</div>
</div>"##;
