//! Context-menu primitive: one floating menu at a time, native-menu look
//! (candlelit), dismissed by click-away / Escape / opening another. The DOM
//! contextmenu event is suppressed by callers inside app surfaces — the
//! browser menu survives only in text inputs (deliberate web divergence).

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// A row's trailing affordance: (css class, tooltip, action).
type Trailing = (String, String, Box<dyn FnOnce()>);

pub enum MenuEntry {
    Item {
        label: String,
        danger: bool,
        action: Box<dyn FnOnce()>,
        /// Optional trailing affordance (class, title, action) — a second hit
        /// target on the same row that runs instead of the row action.
        trailing: Option<Trailing>,
    },
    Separator,
}

impl MenuEntry {
    pub fn item(label: impl Into<String>, action: impl FnOnce() + 'static) -> Self {
        Self::Item {
            label: label.into(),
            danger: false,
            action: Box::new(action),
            trailing: None,
        }
    }
    pub fn danger(label: impl Into<String>, action: impl FnOnce() + 'static) -> Self {
        Self::Item {
            label: label.into(),
            danger: true,
            action: Box::new(action),
            trailing: None,
        }
    }

    /// Attach a trailing `✕`-style button to an item row (no-op on a
    /// separator). The row's own action does NOT fire when it is clicked.
    pub fn with_trailing(
        mut self,
        class: impl Into<String>,
        title: impl Into<String>,
        action: impl FnOnce() + 'static,
    ) -> Self {
        if let Self::Item { trailing, .. } = &mut self {
            *trailing = Some((class.into(), title.into(), Box::new(action)));
        }
        self
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
            let _ = doc
                .remove_event_listener_with_callback("keydown", open._key.as_ref().unchecked_ref());
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
                trailing,
            } => {
                let Ok(item) = doc.create_element("div") else {
                    continue;
                };
                item.set_class_name(if danger {
                    "ctx-item danger"
                } else {
                    "ctx-item"
                });
                if trailing.is_none() {
                    item.set_text_content(Some(&label));
                } else {
                    item.set_class_name(if danger {
                        "ctx-item danger has-trailing"
                    } else {
                        "ctx-item has-trailing"
                    });
                    if let Ok(text) = doc.create_element("span") {
                        text.set_class_name("ctx-label");
                        text.set_text_content(Some(&label));
                        let _ = item.append_child(&text);
                    }
                }
                if let Some((class, title, act)) = trailing {
                    if let Ok(btn) = doc.create_element("span") {
                        btn.set_class_name(&class);
                        btn.set_text_content(Some("✕"));
                        let _ = btn.set_attribute("title", &title);
                        let slot = Rc::new(RefCell::new(Some(act)));
                        let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(
                            move |ev: web_sys::MouseEvent| {
                                // Beats the row action: the row's own listener
                                // never sees this mousedown.
                                ev.stop_propagation();
                                close_menu();
                                if let Some(f) = slot.borrow_mut().take() {
                                    f();
                                }
                            },
                        );
                        let _ = btn.add_event_listener_with_callback(
                            "mousedown",
                            cb.as_ref().unchecked_ref(),
                        );
                        closures.push(cb);
                        let _ = item.append_child(&btn);
                    }
                }
                let slot = Rc::new(RefCell::new(Some(action)));
                let cb = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(
                    move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        close_menu();
                        if let Some(f) = slot.borrow_mut().take() {
                            f();
                        }
                    },
                );
                let _ =
                    item.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref());
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
    let vw = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1e9);
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
    let key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
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
.ctx-item.has-trailing{display:flex;align-items:center;gap:10px;}
.ctx-item.has-trailing .ctx-label{flex:1 1 auto;}
.ctx-item.has-trailing>span:last-child{flex:0 0 auto;opacity:0;
color:var(--text-faint,#7A6E68);padding:0 2px;border-radius:3px;}
.ctx-item.has-trailing:hover>span:last-child{opacity:.65;}
.ctx-item.has-trailing>span:last-child:hover{opacity:1;
color:var(--danger,#D0675D);}
.ctx-sep{height:1px;margin:4px 6px;background:var(--border,#352C2E);}"#,
    ));
    if let Some(head) = doc.head() {
        let _ = head.append_child(&style);
    }
}
