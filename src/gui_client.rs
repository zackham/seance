//! GUI ↔ daemon client: persistent unix-socket connection with auto-reconnect.
//!
//! After a daemon upgrade (or any socket drop) the GUI must re-hello, re-attach,
//! and re-register for broadcasts — otherwise `seance ctl new` from an external
//! agent creates panes the daemon knows about but the open window never sees
//! until a full GUI restart. The supervisor loop below owns that lifecycle.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use base64::Engine as _;

use crate::control::{socket_path, ControlRequest, ControlResponse};
use crate::runtime::protocol::{GuiEvent, GuiRequest};

/// How long to wait between reconnect attempts after a disconnect.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(400);
/// Backoff after the daemon *refuses* us (version gate). A refusal stands
/// until a human upgrades one side, so hammering the socket every 400ms just
/// fills the log with the same sentence a hundred times a minute.
const REFUSED_BACKOFF: Duration = Duration::from_secs(5);
/// Preflight is a boot-blocking question — bounded so an unresponsive daemon
/// delays launch rather than hanging it.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(3);
/// Poll interval while connected so we notice a dead reader promptly.
const WRITE_POLL: Duration = Duration::from_millis(200);

pub struct GuiClient {
    /// None after intentional disconnect (window closed) — stops supervisor reconnect.
    tx: Mutex<Option<Sender<GuiRequest>>>,
    /// Shared with supervisor: once set, never open a new socket.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Subscription seed for `Attach`, re-read on every (re)connect so a
    /// daemon restart restores this window's active list rather than
    /// re-subscribing everything. `None` = "give me everything" (migration).
    subscription_seed: SubscriptionSeed,
    /// This connection attached as a blank window (subscribes to nothing).
    empty: bool,
    /// In-flight fs bridge calls awaiting their FsResult, keyed by id.
    fs_pending: FsPending,
    fs_next_id: std::sync::atomic::AtomicU64,
}

/// Result of a daemon-side `sh -lc`. `stderr` is carried because a host
/// command that fails usually explains itself there, and "exit 1" with the
/// reason thrown away is an error message that lies by omission.
pub struct ShellOut {
    pub status: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

/// One fs result: (ok, data, error).
type FsReply = (bool, Option<serde_json::Value>, Option<String>);
type FsPending = Arc<Mutex<std::collections::HashMap<u64, Sender<FsReply>>>>;
/// Shared `Attach` subscription seed (see [`GuiClient::subscription_seed`]).
type SubscriptionSeed = Arc<Mutex<Option<Vec<String>>>>;

impl GuiClient {
    /// Connect, spawn the connection supervisor, return client + event receiver.
    ///
    /// The initial connect is verified synchronously so launch fails loudly when
    /// no daemon is listening. After that the supervisor reconnects forever
    /// (daemon upgrade, brief socket blip, etc.) and re-sends `Attach` so the
    /// GUI re-syncs full state + grids.
    ///
    /// `seed` is this GUI's persisted active list (`subscriptions.json`);
    /// `None` means "no persisted list" → the daemon seeds every workspace and
    /// the GUI adopts that as its first active list.
    pub fn connect(seed: Option<Vec<String>>) -> Result<(Arc<Self>, Receiver<GuiEvent>)> {
        // Second process: `seance` while another GUI is up → empty window.
        // Same-process new window uses connect_empty().
        let empty = std::env::var("SEANCE_EMPTY_WINDOW").ok().as_deref() == Some("1")
            || other_gui_running();
        Self::connect_opts(empty, seed)
    }

    /// Attach as an empty window (subscribes to no workspaces).
    pub fn connect_empty() -> Result<(Arc<Self>, Receiver<GuiEvent>)> {
        Self::connect_opts(true, None)
    }

    fn connect_opts(
        empty: bool,
        seed: Option<Vec<String>>,
    ) -> Result<(Arc<Self>, Receiver<GuiEvent>)> {
        let path = socket_path();
        // Probe: fail fast if nothing is listening.
        let probe = UnixStream::connect(&path)
            .with_context(|| format!("connect gui to {}", path.display()))?;
        drop(probe);

        let (req_tx, req_rx) = mpsc::channel::<GuiRequest>();
        let (ev_tx, ev_rx) = mpsc::channel::<GuiEvent>();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_sup = Arc::clone(&stop);
        let fs_pending: FsPending = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let fs_pending_sup = Arc::clone(&fs_pending);
        // A blank window subscribes to nothing regardless of any persisted list.
        let subscription_seed: SubscriptionSeed =
            Arc::new(Mutex::new(if empty { Some(Vec::new()) } else { seed }));
        let seed_sup = Arc::clone(&subscription_seed);

        thread::Builder::new()
            .name("seance-gui-conn".into())
            .spawn(move || connection_supervisor(req_rx, ev_tx, seed_sup, stop_sup, fs_pending_sup))
            .context("spawn gui connection supervisor")?;

        let client = Arc::new(Self {
            tx: Mutex::new(Some(req_tx)),
            stop,
            subscription_seed,
            empty,
            fs_pending,
            fs_next_id: std::sync::atomic::AtomicU64::new(1),
        });
        Ok((client, ev_rx))
    }

    /// This connection attached as a blank window — `connect()` decides that
    /// on its own (second process / `SEANCE_EMPTY_WINDOW`), so the app asks
    /// rather than assumes. Blank windows never persist an active list.
    pub fn is_empty_window(&self) -> bool {
        self.empty
    }

    /// Update the `Attach` seed used by every future reconnect. Blank windows
    /// keep their empty seed.
    pub fn set_subscription_seed(&self, subs: Vec<String>) {
        if self.empty {
            return;
        }
        *self.subscription_seed.lock().unwrap() = Some(subs);
    }

    /// Tell the daemon this window is gone, then kill the supervisor (no reconnect).
    pub fn disconnect(&self) {
        // Best-effort Bye so workspaces reassign before the socket dies.
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(GuiRequest::Bye);
        }
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Drop sender → supervisor sees Disconnected and exits.
        *self.tx.lock().unwrap() = None;
    }

    pub fn send(&self, req: GuiRequest) -> Result<()> {
        self.tx
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gui client disconnected"))?
            .send(req)
            .map_err(|_| anyhow::anyhow!("gui client disconnected"))
    }

    /// Acknowledge grid frames up to `seq`.
    ///
    /// Best-effort on purpose: the daemon stops waiting for acks after a short
    /// stall, so a dropped one costs a beat of extra buffering, never a stall.
    pub fn ack_grid(&self, seq: u64) {
        let _ = self.send(GuiRequest::GridAck { seq });
    }

    pub fn input(&self, pane: &str, bytes: &[u8]) -> Result<()> {
        self.send(GuiRequest::Input {
            pane: pane.to_string(),
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    pub fn resize(&self, pane: &str, cols: u16, rows: u16) -> Result<()> {
        self.send(GuiRequest::Resize {
            pane: pane.to_string(),
            cols,
            rows,
        })
    }

    pub fn scroll(&self, pane: &str, delta: i32) -> Result<()> {
        self.send(GuiRequest::Scroll {
            pane: pane.to_string(),
            delta,
        })
    }

    pub fn scroll_bottom(&self, pane: &str) -> Result<()> {
        self.send(GuiRequest::ScrollBottom {
            pane: pane.to_string(),
        })
    }

    pub fn inject(&self, pane: &str, text: &str, submit: bool) -> Result<()> {
        self.send(GuiRequest::Inject {
            pane: pane.to_string(),
            text: text.to_string(),
            submit,
        })
    }

    pub fn ghost_accept(&self, pane: &str) -> Result<()> {
        self.send(GuiRequest::GhostAccept {
            pane: pane.to_string(),
        })
    }

    pub fn ghost_reject(&self, pane: &str) -> Result<()> {
        self.send(GuiRequest::GhostReject {
            pane: pane.to_string(),
        })
    }

    pub fn spawn_pane(
        &self,
        name: &str,
        cwd: Option<String>,
        command: Option<String>,
        workspace: Option<String>,
        file: Option<String>,
    ) -> Result<()> {
        self.send(GuiRequest::Spawn {
            name: name.to_string(),
            cwd,
            command,
            workspace,
            file,
            tiled: true,
        })
    }

    pub fn kill(&self, pane: &str) -> Result<()> {
        self.send(GuiRequest::Kill {
            pane: pane.to_string(),
        })
    }

    pub fn set_tiled(&self, pane: &str, tiled: bool) -> Result<()> {
        self.send(GuiRequest::SetTiled {
            pane: pane.to_string(),
            tiled,
        })
    }

    pub fn set_focus(&self, pane: Option<String>, workspace: Option<String>) -> Result<()> {
        self.send(GuiRequest::SetFocus { pane, workspace })
    }

    /// Enable/disable multi-workspace live grid streaming (overview mode).
    pub fn set_overview(&self, enabled: bool) -> Result<()> {
        self.send(GuiRequest::SetOverview { enabled })
    }

    /// Request a FULL grid frame for one pane (repair after damage desync).
    pub fn refresh_grid(&self, pane: &str) -> Result<()> {
        self.send(GuiRequest::RefreshGrid {
            pane: pane.to_string(),
        })
    }

    /// Add a workspace to this window's subscription set ("add to active").
    pub fn subscribe(&self, workspace: &str) -> Result<()> {
        self.send(GuiRequest::Subscribe {
            workspace: workspace.to_string(),
        })
    }

    /// Drop a workspace from this window's subscription set ("park").
    pub fn unsubscribe(&self, workspace: &str) -> Result<()> {
        self.send(GuiRequest::Unsubscribe {
            workspace: workspace.to_string(),
        })
    }

    /// Sleep a whole circle — daemon-side, so every window sees it.
    pub fn sleep_workspace(&self, workspace: &str) -> Result<()> {
        self.send(GuiRequest::SleepWorkspace {
            workspace: workspace.to_string(),
        })
    }

    /// Wake it back onto its own conversations.
    pub fn wake_workspace(&self, workspace: &str) -> Result<()> {
        self.send(GuiRequest::WakeWorkspace {
            workspace: workspace.to_string(),
        })
    }

    pub fn kill_workspace(&self, workspace: &str) -> Result<()> {
        self.send(GuiRequest::KillWorkspace {
            workspace: workspace.to_string(),
        })
    }

    pub fn create_workspace(&self, name: &str) -> Result<()> {
        self.send(GuiRequest::CreateWorkspace {
            name: name.to_string(),
        })
    }

    /// Move `pane` into `workspace`. When `before` is set, insert immediately
    /// before that slug (sidebar / tile order); otherwise append.
    pub fn move_pane(&self, pane: &str, workspace: &str, before: Option<&str>) -> Result<()> {
        self.send(GuiRequest::MovePane {
            pane: pane.to_string(),
            workspace: workspace.to_string(),
            before: before.map(str::to_string),
        })
    }

    pub fn rename_pane(&self, pane: &str, name: &str) -> Result<()> {
        self.send(GuiRequest::RenamePane {
            pane: pane.to_string(),
            name: name.to_string(),
        })
    }

    pub fn rename_workspace(&self, old: &str, new: &str) -> Result<()> {
        self.send(GuiRequest::RenameWorkspace {
            old: old.to_string(),
            new: new.to_string(),
        })
    }

    pub fn answer_ask(&self, id: &str, answer: &str) -> Result<()> {
        self.send(GuiRequest::AnswerAsk {
            id: id.to_string(),
            answer: answer.to_string(),
        })
    }

    /// Fire-and-forget event-log write in the DAEMON's flight recorder.
    pub fn log_event(
        &self,
        actor: &str,
        workspace: Option<&str>,
        pane: Option<&str>,
        kind: &str,
        detail: String,
    ) {
        let _ = self.send(GuiRequest::Event {
            actor: actor.to_string(),
            workspace: workspace.map(str::to_string),
            pane: pane.map(str::to_string),
            kind: kind.to_string(),
            detail,
        });
    }

    // ── fs bridge (thin client: daemon-machine files/pads/layout/host) ──────

    /// Synchronous daemon fs call. Blocks the calling thread up to `timeout`
    /// (do not call on the render path with a long timeout). Ok(data) on
    /// success, Err with the daemon's message otherwise.
    pub fn fs_call(
        &self,
        op: crate::runtime::protocol::FsOp,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let id = self
            .fs_next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (reply_tx, reply_rx) = mpsc::channel::<FsReply>();
        self.fs_pending.lock().unwrap().insert(id, reply_tx);
        if let Err(e) = self.send(GuiRequest::Fs { id, fs: op }) {
            self.fs_pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match reply_rx.recv_timeout(timeout) {
            Ok((true, data, _)) => Ok(data.unwrap_or(serde_json::Value::Null)),
            Ok((false, _, error)) => Err(anyhow::anyhow!(
                "{}",
                error.unwrap_or_else(|| "fs op failed".into())
            )),
            Err(_) => {
                self.fs_pending.lock().unwrap().remove(&id);
                Err(anyhow::anyhow!("fs op timed out after {timeout:?}"))
            }
        }
    }

    /// Read a daemon-side file as bytes (`{contents_b64, mtime_ms}` → bytes).
    pub fn fs_read(&self, path: &str) -> Result<(Vec<u8>, Option<u64>)> {
        let v = self.fs_call(
            crate::runtime::protocol::FsOp::Read {
                path: path.to_string(),
            },
            FS_TIMEOUT,
        )?;
        let b64 = v
            .get("contents_b64")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("fs read: missing contents"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| anyhow::anyhow!("fs read: bad base64: {e}"))?;
        Ok((bytes, v.get("mtime_ms").and_then(|m| m.as_u64())))
    }

    /// Read a daemon-side file as UTF-8 text.
    pub fn fs_read_string(&self, path: &str) -> Result<(String, Option<u64>)> {
        let (bytes, mtime) = self.fs_read(path)?;
        Ok((String::from_utf8_lossy(&bytes).into_owned(), mtime))
    }

    /// Atomically write a daemon-side file; returns its new mtime_ms.
    pub fn fs_write(&self, path: &str, contents: &[u8]) -> Result<Option<u64>> {
        let v = self.fs_call(
            crate::runtime::protocol::FsOp::Write {
                path: path.to_string(),
                contents_b64: base64::engine::general_purpose::STANDARD.encode(contents),
            },
            FS_TIMEOUT,
        )?;
        Ok(v.get("mtime_ms").and_then(|m| m.as_u64()))
    }

    /// Stat a daemon-side path: `None` when missing, `Some(mtime_ms)` when
    /// present (mtime may itself be None on exotic filesystems).
    pub fn fs_stat(&self, path: &str) -> Result<Option<Option<u64>>> {
        let v = self.fs_call(
            crate::runtime::protocol::FsOp::Stat {
                path: path.to_string(),
            },
            FS_TIMEOUT,
        )?;
        if v.get("exists").and_then(|e| e.as_bool()) == Some(true) {
            Ok(Some(v.get("mtime_ms").and_then(|m| m.as_u64())))
        } else {
            Ok(None)
        }
    }

    /// List a daemon-side directory: `(name, is_dir)` pairs.
    pub fn fs_list(&self, path: &str) -> Result<Vec<(String, bool)>> {
        let v = self.fs_call(
            crate::runtime::protocol::FsOp::List {
                path: path.to_string(),
            },
            FS_TIMEOUT,
        )?;
        Ok(v.get("entries")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        Some((
                            e.get("name")?.as_str()?.to_string(),
                            e.get("is_dir").and_then(|d| d.as_bool()).unwrap_or(false),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn fs_remove(&self, path: &str) -> Result<()> {
        self.fs_call(
            crate::runtime::protocol::FsOp::Remove {
                path: path.to_string(),
            },
            FS_TIMEOUT,
        )
        .map(|_| ())
    }

    /// Load the shared layout JSON from the daemon (None = never saved).
    pub fn layout_load(&self) -> Result<Option<String>> {
        let v = self.fs_call(crate::runtime::protocol::FsOp::LayoutLoad, FS_TIMEOUT)?;
        Ok(v.get("json").and_then(|j| j.as_str()).map(str::to_string))
    }

    pub fn layout_save(&self, json: &str) -> Result<()> {
        self.fs_call(
            crate::runtime::protocol::FsOp::LayoutSave {
                json: json.to_string(),
            },
            FS_TIMEOUT,
        )
        .map(|_| ())
    }

    /// Load the shared rail arrangement from the daemon (None = never saved,
    /// i.e. this daemon predates the move or has not been seeded yet).
    pub fn subs_load(&self) -> Result<Option<String>> {
        let v = self.fs_call(crate::runtime::protocol::FsOp::SubsLoad, FS_TIMEOUT)?;
        Ok(v.get("json").and_then(|j| j.as_str()).map(str::to_string))
    }

    pub fn subs_save(&self, json: &str) -> Result<()> {
        self.fs_call(
            crate::runtime::protocol::FsOp::SubsSave {
                json: json.to_string(),
            },
            FS_TIMEOUT,
        )
        .map(|_| ())
    }

    /// Run a shell command daemon-side (`sh -lc`).
    /// 30s budget — background threads only, never the UI thread.
    pub fn shell(&self, cmd: &str) -> Result<ShellOut> {
        let v = self.fs_call(
            crate::runtime::protocol::FsOp::Shell {
                cmd: cmd.to_string(),
            },
            Duration::from_secs(30),
        )?;
        let text = |k: &str| {
            v.get(k)
                .and_then(|o| o.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(ShellOut {
            status: v.get("status").and_then(|c| c.as_i64()),
            stdout: text("stdout"),
            stderr: text("stderr"),
        })
    }

    /// Run a host widget select command daemon-side; returns its stdout.
    pub fn host_select(&self, widget: &str, item: &str) -> Result<String> {
        let v = self.fs_call(
            crate::runtime::protocol::FsOp::HostSelect {
                widget: widget.to_string(),
                item: item.to_string(),
            },
            Duration::from_secs(30),
        )?;
        Ok(v.get("output")
            .and_then(|o| o.as_str())
            .unwrap_or_default()
            .to_string())
    }
}

/// Default budget for small fs bridge ops (reads, writes, stats).
const FS_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// connection supervisor
// ---------------------------------------------------------------------------

/// Owns the socket lifecycle: connect → hello → attach → pump requests +
/// events → on drop, backoff and retry. `req_rx` is the app's outbound queue;
/// `ev_tx` feeds the GUI event bridge.
/// True if another non-daemon seance process is already running.
fn other_gui_running() -> bool {
    let self_pid = std::process::id();
    let Ok(out) = std::process::Command::new("pgrep")
        .args(["-x", "seance"])
        .output()
    else {
        return false;
    };
    for pid_s in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        // Only a bare `seance` invocation is a GUI. Anything with a
        // subcommand (`daemon`, `web`, `restart-gui`, `ctl`, …) is not —
        // under the 0.11 ownership model miscounting was masked by the
        // daemon's sole-window vacuum, but with subscriptions an Attach as
        // "second window" means a blank sidebar, so the check must be exact.
        let cmdline = crate::sysopen::process_cmdline(pid);
        if cmdline.split_whitespace().nth(1).is_some() {
            continue;
        }
        return true;
    }
    false
}

fn connection_supervisor(
    req_rx: Receiver<GuiRequest>,
    ev_tx: Sender<GuiEvent>,
    subscription_seed: SubscriptionSeed,
    stop: Arc<std::sync::atomic::AtomicBool>,
    fs_pending: FsPending,
) {
    use std::sync::atomic::Ordering;
    let mut pending: Option<GuiRequest> = None;
    let mut first = true;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let stream = match open_gui_stream() {
            Ok(s) => s,
            Err(e) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if first {
                    // connect() already probed successfully; this is a race.
                    eprintln!("[seance gui] connect failed: {e:#}; retrying…");
                } else {
                    eprintln!("[seance gui] reconnect failed: {e:#}; retrying…");
                }
                thread::sleep(RECONNECT_BACKOFF);
                continue;
            }
        };
        first = false;

        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[seance gui] clone stream: {e}; retrying…");
                thread::sleep(RECONNECT_BACKOFF);
                continue;
            }
        };
        let reader = BufReader::new(stream);

        // Reader thread → ev_tx; signals death on EOF/error.
        let (death_tx, death_rx) = mpsc::channel::<()>();
        let ev_tx_reader = ev_tx.clone();
        let fs_pending_reader = Arc::clone(&fs_pending);
        // Set by the reader when the daemon refuses this connection, read by
        // the loop below to pick a backoff — a refusal is a standing condition,
        // not a blip.
        let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let refused_reader = Arc::clone(&refused);
        thread::Builder::new()
            .name("seance-gui-read".into())
            .spawn(move || {
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<GuiEvent>(&line) {
                        // fs bridge replies go straight to their waiter — the
                        // app event stream never sees them.
                        Ok(GuiEvent::FsResult {
                            id,
                            ok,
                            data,
                            error,
                        }) => {
                            let waiter = fs_pending_reader.lock().unwrap().remove(&id);
                            if let Some(w) = waiter {
                                let _ = w.send((ok, data, error));
                            }
                        }
                        Ok(ev) => {
                            if ev_tx_reader.send(ev).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            // A ControlResponse error here is the daemon's
                            // hello refusal (e.g. strict version mismatch) —
                            // surface it verbatim instead of "bad event".
                            if let Ok(resp) =
                                serde_json::from_str::<crate::control::ControlResponse>(&line)
                            {
                                if let Some(err) = resp.error {
                                    eprintln!("[seance gui] daemon refused connection: {err}");
                                    // Push it at the app too. A refusal means
                                    // no State and no grids will ever arrive
                                    // on this connection, so the window it
                                    // leaves behind is empty — and an empty
                                    // window that doesn't say why reads as
                                    // "seance is broken" instead of "these two
                                    // builds disagree".
                                    let _ = ev_tx_reader.send(GuiEvent::Error { message: err });
                                    refused_reader.store(true, Ordering::SeqCst);
                                    continue;
                                }
                            }
                            eprintln!("[seance gui] bad event: {e}: {line}");
                        }
                    }
                }
                let _ = death_tx.send(());
            })
            .ok();

        // Attach so the daemon pushes full State + grids and registers this
        // connection for broadcasts (register happens in serve_gui before
        // Attach is processed — hello already registered the writer).
        if write_request(
            &mut writer,
            &GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                // This window's persisted active list. Blank window → empty
                // list; no persisted list yet → None, i.e. "everything", which
                // the GUI then adopts and persists (migration).
                subscriptions: subscription_seed.lock().unwrap().clone(),
            },
        )
        .is_err()
        {
            eprintln!("[seance gui] attach failed; reconnecting…");
            thread::sleep(RECONNECT_BACKOFF);
            continue;
        }

        // Flush any request that failed mid-write on the previous connection.
        if let Some(req) = pending.take() {
            if write_request(&mut writer, &req).is_err() {
                pending = Some(req);
                thread::sleep(RECONNECT_BACKOFF);
                continue;
            }
        }

        // Pump outbound requests until the reader dies or the app drops us.
        let mut alive = true;
        let mut intentional = false;
        while alive {
            if stop.load(Ordering::SeqCst) {
                intentional = true;
                break;
            }
            match req_rx.recv_timeout(WRITE_POLL) {
                Ok(req) => {
                    let is_bye = matches!(req, GuiRequest::Bye);
                    if write_request(&mut writer, &req).is_err() {
                        if !is_bye {
                            pending = Some(req);
                        }
                        alive = false;
                    } else if is_bye {
                        // Daemon reassigns workspaces; do not reconnect.
                        intentional = true;
                        alive = false;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if death_rx.try_recv().is_ok() {
                        alive = false;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // App dropped GuiClient — shut down supervisor.
                    return;
                }
            }
        }
        if intentional || stop.load(Ordering::SeqCst) {
            return;
        }
        if !refused.load(Ordering::SeqCst) {
            eprintln!("[seance gui] disconnected from daemon; reconnecting…");
        }
        // Give the old reader a moment to exit before we open a new socket.
        let _ = death_rx.recv_timeout(Duration::from_millis(50));
        thread::sleep(if refused.load(Ordering::SeqCst) {
            REFUSED_BACKOFF
        } else {
            RECONNECT_BACKOFF
        });
    }
}

/// Ask the daemon whether it will have us, before the GUI commits to a window.
///
/// The daemon's version gate refuses a `ctl`/`gui` hello with a
/// [`ControlResponse`] error and hangs up. From inside the app that refusal
/// looked like *nothing*: the supervisor reconnected forever and the window sat
/// there with an empty rail, no error, no workspaces (2026-08-20 — a mac thin
/// client left on 0.24.0 against a 0.25.4 daemon). The daemon was shouting; the
/// GUI had no ears. Ask here, at boot, where the answer still reaches a human —
/// a refusal routes to the launch picker, which renders it.
///
/// Probes as `ctl`, not `gui`: same version gate either way, but a ctl
/// connection that asks one question and leaves is routine, where a `gui` hello
/// registers a window for broadcasts we'd immediately abandon.
pub fn preflight(path: &Path) -> Result<()> {
    let stream = UnixStream::connect(path)
        .with_context(|| format!("connect to daemon at {}", path.display()))?;
    let _ = stream.set_read_timeout(Some(PREFLIGHT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PREFLIGHT_TIMEOUT));
    let mut writer = stream.try_clone().context("clone preflight stream")?;
    writer.write_all(crate::runtime::protocol::hello_line("ctl").as_bytes())?;
    // Cheapest request that always answers. We don't care what it says when
    // it works — only that something comes back, and what it says when it
    // doesn't.
    let mut req = serde_json::to_string(&ControlRequest::Whoami {
        scope: None,
        from: None,
    })?;
    req.push('\n');
    writer.write_all(req.as_bytes())?;
    writer.flush()?;

    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        // Closed without a word. Not the version gate — that answers first —
        // so don't invent a diagnosis for it; let the GUI try.
        Ok(0) => Ok(()),
        Ok(_) => match serde_json::from_str::<ControlResponse>(line.trim()) {
            Ok(resp) if !resp.ok => Err(anyhow!(resp
                .error
                .unwrap_or_else(|| "daemon refused the connection".into()))),
            _ => Ok(()),
        },
        // A slow daemon is not a broken one. Only a refusal is a refusal.
        Err(_) => Ok(()),
    }
}

fn open_gui_stream() -> Result<UnixStream> {
    let path = socket_path();
    let stream =
        UnixStream::connect(&path).with_context(|| format!("connect gui to {}", path.display()))?;
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut writer = stream.try_clone()?;
    writer.write_all(crate::runtime::protocol::hello_line("gui").as_bytes())?;
    writer.flush()?;
    Ok(stream)
}

fn write_request(writer: &mut UnixStream, req: &GuiRequest) -> std::io::Result<()> {
    let mut line = serde_json::to_string(req)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A daemon stand-in: reads the hello + one request, answers with `reply`,
    /// hangs up. Mirrors what `daemon::handle_connection` does on the version
    /// gate (answer, then close).
    fn fake_daemon(reply: &'static str) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let path = std::env::temp_dir().join(format!(
            "seance-preflight-test-{}-{:?}.sock",
            std::process::id(),
            thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind fake daemon");
        let handle = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut hello = String::new();
                let _ = reader.read_line(&mut hello);
                // The version gate answers on the hello alone; a healthy
                // daemon waits for the request first. Reading one line either
                // way keeps the stand-in honest about both.
                let mut req = String::new();
                let _ = reader.read_line(&mut req);
                let mut w = stream;
                let _ = w.write_all(reply.as_bytes());
                let _ = w.flush();
            }
        });
        (path, handle)
    }

    #[test]
    fn preflight_surfaces_a_refusal_verbatim() {
        let (path, h) = fake_daemon(
            r#"{"ok":false,"error":"version mismatch: daemon is seance 0.25.4, client is 0.24.0"}"#,
        );
        let err = preflight(&path).expect_err("a refusal must not read as success");
        assert!(
            err.to_string().contains("version mismatch"),
            "refusal text lost on the way to the human: {err}"
        );
        assert!(err.to_string().contains("0.24.0"), "got: {err}");
        let _ = h.join();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preflight_accepts_a_daemon_that_answers() {
        let (path, h) = fake_daemon(r#"{"ok":true,"data":{"principal":"human"}}"#);
        preflight(&path).expect("a daemon that answers must boot the GUI");
        let _ = h.join();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preflight_fails_when_nothing_is_listening() {
        let path = std::env::temp_dir().join("seance-preflight-test-absent.sock");
        let _ = std::fs::remove_file(&path);
        assert!(preflight(&path).is_err());
    }

    /// A daemon that hangs up wordlessly isn't the version gate — that one
    /// always answers first. Don't invent a diagnosis for it.
    #[test]
    fn preflight_tolerates_a_silent_close() {
        let (path, h) = fake_daemon("");
        preflight(&path).expect("a wordless close is not a refusal");
        let _ = h.join();
        let _ = std::fs::remove_file(&path);
    }
}
