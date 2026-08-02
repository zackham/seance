//! Websocket client to the `seance web` bridge: the wasm mirror of the native
//! [`gui_client`] connection supervisor.
//!
//! Wire: one websocket TEXT message = exactly one JSON line of the unix-socket
//! GUI protocol. The bridge relays verbatim, so the lifecycle is identical to
//! the native supervisor — hello, `Attach`, then stream events — and it MUST be
//! re-run on every reopen: an upgraded/restarted daemon only re-pushes `State`
//! and full grids in response to a fresh `Attach`, so a socket that reconnects
//! without re-attaching paints a frozen window forever.
//!
//! Closure lifetime: every `Closure` handed to the DOM is stored in a struct
//! field. Dropping one detaches the callback mid-flight (dead socket, silent);
//! `forget()`ing them all leaks one set per reconnect. Fields are the middle
//! path — replaced (and thus dropped) exactly when their socket is replaced.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};

use base64::Engine as _;
use seance_core::protocol::{hello_line_with, GuiEvent, GuiRequest};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{CloseEvent, MessageEvent, WebSocket};

/// Delay between reconnect attempts (mirrors the native supervisor).
const RECONNECT_BACKOFF_MS: i32 = 400;
/// Latency probe cadence.
const PING_INTERVAL_MS: i32 = 10_000;
/// Outbound queue cap while disconnected; oldest requests are dropped first
/// (newest input is what the user is actually waiting on).
const MAX_QUEUE: usize = 256;
/// Bridge close code for a rejected token — terminal, never retried.
const CLOSE_AUTH_FAILED: u16 = 4401;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Disconnected,
    /// Bad/expired token (bridge closed with 4401). No reconnect.
    AuthFailed,
}

/// Handlers live in their own cells so a daemon event may call back into
/// [`Conn::send`] without a `RefCell` double-borrow.
struct Handlers {
    on_event: Box<dyn FnMut(GuiEvent)>,
    on_status: Box<dyn FnMut(ConnStatus)>,
}

/// Per-socket DOM callbacks. Held alive here; dropped with the socket.
#[derive(Default)]
struct SocketClosures {
    open: Option<Closure<dyn FnMut()>>,
    message: Option<Closure<dyn FnMut(MessageEvent)>>,
    close: Option<Closure<dyn FnMut(CloseEvent)>>,
    error: Option<Closure<dyn FnMut(JsValue)>>,
}

struct Inner {
    url: String,
    ws: Option<WebSocket>,
    closures: SocketClosures,
    /// Serialized request lines waiting for an open socket.
    queue: VecDeque<String>,
    /// Pending ping send time (performance.now ms).
    ping_at: Option<f64>,
    rtt_ms: Option<f64>,
    ping_timer: Option<i32>,
    ping_closure: Option<Closure<dyn FnMut()>>,
    reconnect_timer: Option<i32>,
    reconnect_closure: Option<Closure<dyn FnMut()>>,
    /// Terminal state: auth failure or explicit teardown.
    stopped: bool,
    /// In-flight fs bridge calls awaiting their FsResult, keyed by id.
    fs_pending: HashMap<u64, Box<dyn FnOnce(Result<serde_json::Value, String>)>>,
    fs_next_id: u64,
}

pub struct Conn {
    inner: RefCell<Inner>,
    handlers: RefCell<Handlers>,
    /// `Attach.subscriptions` seed, owned by the app and re-read on EVERY
    /// open: parking a circle must not come back after a reconnect.
    subscriptions: Rc<RefCell<Option<Vec<String>>>>,
}

/// Open a connection and keep it open. `url` is the full ws URL including
/// `?token=…`. Returns immediately; status arrives via `on_status`.
pub fn connect(
    url: String,
    subscriptions: Rc<RefCell<Option<Vec<String>>>>,
    on_event: Box<dyn FnMut(GuiEvent)>,
    on_status: Box<dyn FnMut(ConnStatus)>,
) -> Rc<Conn> {
    let conn = Rc::new(Conn {
        inner: RefCell::new(Inner {
            url,
            ws: None,
            closures: SocketClosures::default(),
            queue: VecDeque::new(),
            ping_at: None,
            rtt_ms: None,
            ping_timer: None,
            ping_closure: None,
            reconnect_timer: None,
            reconnect_closure: None,
            stopped: false,
            fs_pending: HashMap::new(),
            fs_next_id: 1,
        }),
        handlers: RefCell::new(Handlers {
            on_event,
            on_status,
        }),
        subscriptions,
    });
    conn.start_ping_timer();
    conn.open();
    conn
}

impl Conn {
    /// Serialize one request as a JSON line. Queues (bounded, drop-oldest)
    /// when the socket is not open; the queue flushes after the next
    /// re-attach so the daemon has fresh state before replayed requests land.
    pub fn send(&self, req: &GuiRequest) {
        let Ok(line) = serde_json::to_string(req) else {
            return;
        };
        self.send_line(line);
    }

    /// PTY bytes → base64 → `Input`.
    pub fn input(&self, pane: &str, bytes: &[u8]) {
        self.send(&GuiRequest::Input {
            pane: pane.to_string(),
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }

    /// Round-trip time from the last resolved ping, if any.
    pub fn rtt_ms(&self) -> Option<f64> {
        self.inner.borrow().rtt_ms
    }

    /// Async daemon fs-bridge call (files/config/host-select live on the
    /// DAEMON's machine — same seam the native thin client uses). The callback
    /// fires with `Ok(data)` on success or `Err(message)` on failure; if the
    /// connection drops before the reply arrives the callback is simply never
    /// invoked (callers must tolerate silence — reload-on-reconnect covers it).
    pub fn fs_call(
        &self,
        op: seance_core::protocol::FsOp,
        cb: Box<dyn FnOnce(Result<serde_json::Value, String>)>,
    ) {
        let id = {
            let mut inner = self.inner.borrow_mut();
            let id = inner.fs_next_id;
            inner.fs_next_id += 1;
            inner.fs_pending.insert(id, cb);
            id
        };
        self.send(&GuiRequest::Fs { id, fs: op });
    }

    /// Best-effort `Bye` + permanent teardown (page unload / logout). After
    /// this the conn never reconnects and all closures are released.
    pub fn shutdown(&self) {
        self.send(&GuiRequest::Bye);
        let mut inner = self.inner.borrow_mut();
        inner.stopped = true;
        if let Some(win) = web_sys::window() {
            if let Some(h) = inner.ping_timer.take() {
                win.clear_interval_with_handle(h);
            }
            if let Some(h) = inner.reconnect_timer.take() {
                win.clear_timeout_with_handle(h);
            }
        }
        if let Some(ws) = inner.ws.take() {
            let _ = ws.close();
        }
        inner.closures = SocketClosures::default();
        inner.ping_closure = None;
        inner.reconnect_closure = None;
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn send_line(&self, line: String) {
        let mut inner = self.inner.borrow_mut();
        if inner.stopped {
            return;
        }
        let open = inner
            .ws
            .as_ref()
            .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
        if open {
            let ws = inner.ws.as_ref().expect("open implies present");
            if ws.send_with_str(&line).is_ok() {
                return;
            }
            // Send failed → socket is dying; fall through and queue.
        }
        if inner.queue.len() >= MAX_QUEUE {
            inner.queue.pop_front();
        }
        inner.queue.push_back(line);
    }

    fn status(self: &Rc<Self>, s: ConnStatus) {
        if let Ok(mut h) = self.handlers.try_borrow_mut() {
            (h.on_status)(s);
        }
    }

    fn open(self: &Rc<Self>) {
        if self.inner.borrow().stopped {
            return;
        }
        let url = self.inner.borrow().url.clone();
        let ws = match WebSocket::new(&url) {
            Ok(ws) => ws,
            Err(_) => {
                self.status(ConnStatus::Disconnected);
                self.schedule_reconnect();
                return;
            }
        };
        self.status(ConnStatus::Connecting);

        let weak_open = Rc::downgrade(self);
        let on_open = Closure::<dyn FnMut()>::new(move || {
            if let Some(this) = weak_open.upgrade() {
                this.handle_open();
            }
        });
        let weak_msg = Rc::downgrade(self);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(this) = weak_msg.upgrade() {
                this.handle_message(ev);
            }
        });
        let weak_close = Rc::downgrade(self);
        let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |ev: CloseEvent| {
            if let Some(this) = weak_close.upgrade() {
                this.handle_close(ev.code());
            }
        });
        let weak_err: Weak<Conn> = Rc::downgrade(self);
        let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
            // `error` is always followed by `close`; status flips there so we
            // never schedule two reconnects for one drop.
            let _ = &weak_err;
        });

        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let mut inner = self.inner.borrow_mut();
        // Replacing the previous socket's closures drops them — correct, that
        // socket is gone; doing it while it still lives would mute it.
        if let Some(old) = inner.ws.take() {
            old.set_onopen(None);
            old.set_onmessage(None);
            old.set_onclose(None);
            old.set_onerror(None);
            let _ = old.close();
        }
        inner.ws = Some(ws);
        inner.closures = SocketClosures {
            open: Some(on_open),
            message: Some(on_message),
            close: Some(on_close),
            error: Some(on_error),
        };
    }

    /// hello → Attach → flush. Mirrors `connection_supervisor`: re-attach on
    /// EVERY open so the daemon re-pushes `State` + full grids.
    fn handle_open(self: &Rc<Self>) {
        let hello = hello_line_with("gui", env!("CARGO_PKG_VERSION"));
        {
            let inner = self.inner.borrow();
            let Some(ws) = inner.ws.as_ref() else { return };
            if ws.send_with_str(hello.trim_end()).is_err() {
                return;
            }
        }
        self.send(&GuiRequest::Attach {
            selected_workspace: None,
            focused_pane: None,
            // The persisted active list (localStorage `seance_active`), or
            // None on a virgin client — the daemon then subscribes to every
            // circle and the first State seeds the list.
            subscriptions: self.subscriptions.borrow().clone(),
        });
        // Queued requests replay only after the re-attach above.
        let queued: Vec<String> = {
            let mut inner = self.inner.borrow_mut();
            inner.queue.drain(..).collect()
        };
        for line in queued {
            self.send_line(line);
        }
        self.status(ConnStatus::Connected);
    }

    fn handle_message(self: &Rc<Self>, ev: MessageEvent) {
        let Some(text) = ev.data().as_string() else {
            // Binary frames are not part of this protocol.
            return;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<GuiEvent>(line) else {
                continue;
            };
            if matches!(event, GuiEvent::Pong) {
                self.resolve_ping();
                continue; // never forwarded — the probe is ours
            }
            // fs bridge replies route to their waiter, mirroring the native
            // gui_client — the app event stream never sees them.
            if let GuiEvent::FsResult {
                id,
                ok,
                data,
                error,
            } = &event
            {
                let cb = self.inner.borrow_mut().fs_pending.remove(id);
                if let Some(cb) = cb {
                    if *ok {
                        cb(Ok(data.clone().unwrap_or(serde_json::Value::Null)));
                    } else {
                        cb(Err(error
                            .clone()
                            .unwrap_or_else(|| "fs op failed".to_string())));
                    }
                }
                continue;
            }
            if let Ok(mut h) = self.handlers.try_borrow_mut() {
                (h.on_event)(event);
            }
        }
    }

    fn handle_close(self: &Rc<Self>, code: u16) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.ws = None;
            inner.ping_at = None;
            if code == CLOSE_AUTH_FAILED {
                inner.stopped = true;
            }
        }
        if code == CLOSE_AUTH_FAILED {
            self.status(ConnStatus::AuthFailed);
            return;
        }
        self.status(ConnStatus::Disconnected);
        self.schedule_reconnect();
    }

    fn schedule_reconnect(self: &Rc<Self>) {
        if self.inner.borrow().stopped || self.inner.borrow().reconnect_timer.is_some() {
            return;
        }
        let Some(win) = web_sys::window() else { return };
        let weak = Rc::downgrade(self);
        let cb = Closure::<dyn FnMut()>::new(move || {
            if let Some(this) = weak.upgrade() {
                {
                    let mut inner = this.inner.borrow_mut();
                    inner.reconnect_timer = None;
                    inner.reconnect_closure = None;
                }
                this.open();
            }
        });
        let handle = win.set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            RECONNECT_BACKOFF_MS,
        );
        let mut inner = self.inner.borrow_mut();
        match handle {
            Ok(h) => {
                inner.reconnect_timer = Some(h);
                inner.reconnect_closure = Some(cb);
            }
            // No timer → no reconnect; drop the closure rather than leak it.
            Err(_) => drop(cb),
        }
    }

    /// One interval for the life of the conn; pings are no-ops while closed.
    fn start_ping_timer(self: &Rc<Self>) {
        let Some(win) = web_sys::window() else { return };
        let weak = Rc::downgrade(self);
        let cb = Closure::<dyn FnMut()>::new(move || {
            if let Some(this) = weak.upgrade() {
                this.ping();
            }
        });
        let handle = win.set_interval_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            PING_INTERVAL_MS,
        );
        let mut inner = self.inner.borrow_mut();
        match handle {
            Ok(h) => {
                inner.ping_timer = Some(h);
                inner.ping_closure = Some(cb);
            }
            Err(_) => drop(cb),
        }
    }

    fn ping(self: &Rc<Self>) {
        {
            let inner = self.inner.borrow();
            let connected = inner
                .ws
                .as_ref()
                .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
            if !connected || inner.stopped {
                return;
            }
        }
        // Un-answered previous ping just gets overwritten: the next Pong
        // resolves against the newest send, which is the honest measurement.
        self.inner.borrow_mut().ping_at = now_ms();
        self.send(&GuiRequest::Ping);
    }

    fn resolve_ping(&self) {
        let mut inner = self.inner.borrow_mut();
        if let (Some(sent), Some(now)) = (inner.ping_at.take(), now_ms()) {
            inner.rtt_ms = Some((now - sent).max(0.0));
        }
    }
}

/// `performance.now()` in ms; `None` when the API is unavailable (never on a
/// live page, but no unwraps on web APIs).
fn now_ms() -> Option<f64> {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
}
