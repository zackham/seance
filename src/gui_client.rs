//! GUI ↔ daemon client: persistent unix-socket connection with auto-reconnect.
//!
//! After a daemon upgrade (or any socket drop) the GUI must re-hello, re-attach,
//! and re-register for broadcasts — otherwise `seance ctl new` from an external
//! agent creates panes the daemon knows about but the open window never sees
//! until a full GUI restart. The supervisor loop below owns that lifecycle.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use base64::Engine as _;

use crate::control::socket_path;
use crate::runtime::protocol::{GuiEvent, GuiRequest};

/// How long to wait between reconnect attempts after a disconnect.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(400);
/// Poll interval while connected so we notice a dead reader promptly.
const WRITE_POLL: Duration = Duration::from_millis(200);

pub struct GuiClient {
    /// None after intentional disconnect (window closed) — stops supervisor reconnect.
    tx: Mutex<Option<Sender<GuiRequest>>>,
    /// Shared with supervisor: once set, never open a new socket.
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl GuiClient {
    /// Connect, spawn the connection supervisor, return client + event receiver.
    ///
    /// The initial connect is verified synchronously so launch fails loudly when
    /// no daemon is listening. After that the supervisor reconnects forever
    /// (daemon upgrade, brief socket blip, etc.) and re-sends `Attach` so the
    /// GUI re-syncs full state + grids.
    pub fn connect() -> Result<(Arc<Self>, Receiver<GuiEvent>)> {
        // Second process: `seance` while another GUI is up → empty window.
        // Same-process new window uses connect_empty().
        let empty = std::env::var("SEANCE_EMPTY_WINDOW").ok().as_deref() == Some("1")
            || other_gui_running();
        Self::connect_opts(empty)
    }

    /// Attach as an empty window (claims no existing workspaces).
    pub fn connect_empty() -> Result<(Arc<Self>, Receiver<GuiEvent>)> {
        Self::connect_opts(true)
    }

    fn connect_opts(empty: bool) -> Result<(Arc<Self>, Receiver<GuiEvent>)> {
        let path = socket_path();
        // Probe: fail fast if nothing is listening.
        let probe = UnixStream::connect(&path)
            .with_context(|| format!("connect gui to {}", path.display()))?;
        drop(probe);

        let (req_tx, req_rx) = mpsc::channel::<GuiRequest>();
        let (ev_tx, ev_rx) = mpsc::channel::<GuiEvent>();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_sup = Arc::clone(&stop);

        thread::Builder::new()
            .name("seance-gui-conn".into())
            .spawn(move || connection_supervisor(req_rx, ev_tx, empty, stop_sup))
            .context("spawn gui connection supervisor")?;

        let client = Arc::new(Self {
            tx: Mutex::new(Some(req_tx)),
            stop,
        });
        Ok((client, ev_rx))
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
    #[allow(dead_code)] // multi-window API — protocol-ready, awaiting UI wiring
    pub fn refresh_grid(&self, pane: &str) -> Result<()> {
        self.send(GuiRequest::RefreshGrid {
            pane: pane.to_string(),
        })
    }

    pub fn transfer_workspace(&self, workspace: &str, to_window: &str) -> Result<()> {
        self.send(GuiRequest::TransferWorkspace {
            workspace: workspace.to_string(),
            to_window: to_window.to_string(),
        })
    }

    pub fn collect_all(&self) -> Result<()> {
        self.send(GuiRequest::CollectAll)
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

    /// Place workspace `moved` immediately before `before` in the sidebar.
    pub fn reorder_workspace(&self, moved: &str, before: &str) -> Result<()> {
        self.send(GuiRequest::ReorderWorkspace {
            moved: moved.to_string(),
            before: before.to_string(),
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

    pub fn fork_workspace(&self, workspace: &str, name: Option<String>) -> Result<()> {
        self.send(GuiRequest::ForkWorkspace {
            workspace: workspace.to_string(),
            name,
        })
    }

    pub fn answer_ask(&self, id: &str, answer: &str) -> Result<()> {
        self.send(GuiRequest::AnswerAsk {
            id: id.to_string(),
            answer: answer.to_string(),
        })
    }
}

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
        let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        if cmdline.contains("daemon") {
            continue;
        }
        return true;
    }
    false
}

fn connection_supervisor(
    req_rx: Receiver<GuiRequest>,
    ev_tx: Sender<GuiEvent>,
    empty: bool,
    stop: Arc<std::sync::atomic::AtomicBool>,
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
                        Ok(ev) => {
                            if ev_tx_reader.send(ev).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
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
                empty,
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
        eprintln!("[seance gui] disconnected from daemon; reconnecting…");
        // Give the old reader a moment to exit before we open a new socket.
        let _ = death_rx.recv_timeout(Duration::from_millis(50));
        thread::sleep(RECONNECT_BACKOFF);
    }
}

fn open_gui_stream() -> Result<UnixStream> {
    let path = socket_path();
    let stream =
        UnixStream::connect(&path).with_context(|| format!("connect gui to {}", path.display()))?;
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut writer = stream.try_clone()?;
    writeln!(writer, r#"{{"role":"gui"}}"#)?;
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
