//! Context-menu primitive: one floating menu at a time, native-menu look
//! (candlelit), dismissed by click-away / Escape / opening another. The DOM
//! contextmenu event is suppressed by callers inside app surfaces — the
//! browser menu survives only in text inputs (deliberate web divergence).

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

pub enum MenuEntry {
    Item {
        label: String,
        danger: bool,
        action: Box<dyn FnOnce()>,
    },
    Separator,
}

impl MenuEntry {
    pub fn item(label: impl Into<String>, action: impl FnOnce() + 'static) -> Self {
        Self::Item {
            label: label.into(),
            danger: false,
            action: Box::new(action),
        }
    }
    pub fn danger(label: impl Into<String>, action: impl FnOnce() + 'static) -> Self {
        Self::Item {
            label: label.into(),
            danger: true,
            action: Box::new(action),
        }
    }
}

thread_local! {
    static OPEN: RefCell<Option<OpenMenu>> = const { RefCell::new(None) };
}

struct OpenMenu {
    root: web_sys::Element,
    /// Kept alive for the menu's lifetime.
    _closures: Vec<Closure<dyn FnMut(web_sys::MouseEvent)>>,
    _key: Closure<dyn FnMut(web_sys::KeyboardEvent)>,
    _dismiss: Closure<dyn FnMut(web_sys::MouseEvent)>,
}

/// Close any open menu. Returns true when a menu was actually open.
pub fn close_menu() -> bool {
    OPEN.with(|slot| {
        let Some(open) = slot.borrow_mut().take() else {
            return false;
        };
        open.root.remove();
        if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
            let _ = doc.remove_event_listener_with_callback(
                "mousedown",
                open._dismiss.as_ref().unchecked_ref(),
            );
            let _ = doc.remove_event_listener_with_callback(
                "keydown",
                open._key.as_ref().unchecked_ref(),
            );
        }
        true
    })
}

/// Open a menu at viewport coords (client x/y from the triggering event).
pub fn open_menu(x: f64, y: f64, entries: Vec<MenuEntry>) {
    let _ = close_menu();
    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };
    let Some(body) = doc.body() else { return };
    ensure_style(&doc);

    let Ok(root) = doc.create_element("div") else {
        return;
    };
    root.set_class_name("ctx-menu");
    let mut closures: Vec<Closure<dyn FnMut(web_sys::MouseEvent)>> = Vec::new();

    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                if let Ok(sep) = doc.create_element("div") {
                    sep.set_class_name("ctx-sep");
                    let _ = root.append_child(&sep);
                }
            }
            MenuEntry::Item {
                label,
                danger,
                action,
            } => {
                let Ok(item) = doc.create_element("div") else {
                    continue;
                };
                item.set_class_name(if danger { "ctx-item danger" } else { "ctx-item" });
                item.set_text_content(Some(&label));
                let slot = Rc::new(RefCell::new(Some(action)));
                let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    close_menu();
                    if let Some(f) = slot.borrow_mut().take() {
                        f();
                    }
                });
                let _ = item
                    .add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
                closures.push(cb);
                let _ = root.append_child(&item);
            }
        }
    }

    let el: &web_sys::HtmlElement = match root.dyn_ref() {
        Some(e) => e,
        None => return,
    };
    // Mount off-screen first, then clamp into the viewport once we know size.
    let _ = el.style().set_property("left", "-9999px");
    let _ = el.style().set_property("top", "-9999px");
    let _ = body.append_child(&root);
    let rect = root.get_bounding_client_rect();
    let vw = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1e9);
    let vh = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1e9);
    let cx = x.min((vw - rect.width() - 4.0).max(0.0));
    let cy = y.min((vh - rect.height() - 4.0).max(0.0));
    let _ = el.style().set_property("left", &format!("{cx}px"));
    let _ = el.style().set_property("top", &format!("{cy}px"));

    // Dismissers: any mousedown outside (capture phase) or Escape.
    let dismiss = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|e| e.closest(".ctx-menu").ok().flatten())
            .is_some();
        if !inside {
            close_menu();
        }
    });
    let key = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" {
            close_menu();
        }
    });
    let _ = doc.add_event_listener_with_callback("mousedown", dismiss.as_ref().unchecked_ref());
    let _ = doc.add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref());

    OPEN.with(|slot| {
        *slot.borrow_mut() = Some(OpenMenu {
            root,
            _closures: closures,
            _key: key,
            _dismiss: dismiss,
        });
    });
}

fn ensure_style(doc: &web_sys::Document) {
    if doc.get_element_by_id("ctx-menu-style").is_some() {
        return;
    }
    let Ok(style) = doc.create_element("style") else {
        return;
    };
    style.set_id("ctx-menu-style");
    style.set_text_content(Some(
        r#".ctx-menu{position:fixed;z-index:1000;min-width:200px;padding:4px;
background:var(--bg-elevated,#1C1718);border:1px solid var(--border,#352C2E);
border-radius:6px;box-shadow:0 8px 28px rgba(0,0,0,.55);font-size:12px;
user-select:none;}
.ctx-item{padding:5px 10px;border-radius:4px;color:var(--text-dim,#A69A91);
cursor:pointer;white-space:nowrap;}
.ctx-item:hover{background:var(--surface,#211C1D);color:var(--text,#EBE3DB);}
.ctx-item.danger:hover{color:var(--danger,#D0675D);}
.ctx-sep{height:1px;margin:4px 6px;background:var(--border,#352C2E);}"#,
    ));
    if let Some(head) = doc.head() {
        let _ = head.append_child(&style);
    }
}
