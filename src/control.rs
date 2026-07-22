//! The seance **control plane**: a Unix-socket server that lets one session
//! (a "master" agent — Claude, codex, grok, or any CLI running in a pane) drive
//! the *other* sessions in the app. Spawn them, inject prompts, read their
//! rendered screens — all while the human watches the terminals get driven live.
//!
//! # Shape
//!
//! - A blocking `std::os::unix::net::UnixListener` bound at
//!   [`socket_path`] (`$XDG_RUNTIME_DIR/seance.sock`, falling back to
//!   `/tmp/seance-$UID.sock`). One thread accepts; a fresh thread handles each
//!   connection. No async runtime, no tokio — just std threads and blocking IO.
//! - **JSON-lines protocol.** One request JSON per `\n`-terminated line in, one
//!   [`ControlResponse`] JSON line out. The connection may stay open and carry
//!   many request/response pairs (a master session keeps a socket and pipelines
//!   `send`/`read` calls without reconnecting).
//! - Requests are forwarded onto the gpui side through
//!   [`ControlHandle::tx`] paired with a [`oneshot::Sender`]. The connection
//!   thread blocks on the oneshot receiver (via `futures::executor::block_on`)
//!   with a **10-second timeout** — a wedged/slow gpui main loop turns into an
//!   `ok: false` error response, never a hung client.
//!
//! # Seam with the app (the gpui side owns the receiver)
//!
//! This module is deliberately **gpui-free** so it compiles independently. It
//! only defines the protocol types and the socket plumbing. The app constructs
//! the channel, hands us the [`ControlHandle`] (the send half), keeps the
//! receive half, and on the gpui foreground loop:
//!
//! 1. drains `(ControlRequest, oneshot::Sender<ControlResponse>)` pairs,
//! 2. applies each request to real sessions (spawn PTYs, bracketed-paste into
//!    the driven terminal, snapshot the alacritty grid, …),
//! 3. answers the `oneshot::Sender` with a [`ControlResponse`].
//!
//! Nothing here touches a terminal or a gpui entity; that all lives app-side.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use futures::channel::{mpsc, oneshot};

/// A request from a control client (the CLI or a master pane) to the app.
///
/// Tagged on the wire by an `"op"` field with snake_case variant names, e.g.
/// `{"op":"send","pane":"worker-1","text":"run the tests"}`.
///
/// **Scoping:** every op carries an optional `scope` (a workspace name). When
/// set, the op only sees/affects panes in that workspace. The CLI fills it
/// automatically from `$SEANCE_WORKSPACE` — so a CLI run *inside* a pane is
/// confined to its own workspace unless it explicitly passes `--all` or
/// `--workspace`. Callers outside seance have no scope and see everything.
///
/// Wire compat: pane ids are accepted under both `pane` and the v0.1 `session`
/// key. Terminals are the first pane kind — the protocol is kind-agnostic.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// List every known pane (tiled and shelved) with its status.
    List {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Spawn a new pane. `name` is the human-facing label (the app slugifies
    /// it for the pane id). `cwd` defaults to the app's default working dir;
    /// `command` defaults to a plain shell; `workspace` places the pane in a
    /// named workspace (defaults to `scope` when scoped).
    New {
        name: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        workspace: Option<String>,
        /// When set, spawn a FILE pane monitoring this path (no PTY).
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Inject `text` into a pane. With `submit` (the default) the app
    /// bracketed-pastes the text, waits a short settle delay, then sends a
    /// carriage return to submit — the delay lets a TUI agent finish handling
    /// the paste before the Enter keystroke lands. With `submit: false` the
    /// text is left sitting in the input, unsent.
    Send {
        #[serde(alias = "session")]
        pane: String,
        text: String,
        #[serde(default = "default_true")]
        submit: bool,
        /// Bypass human ownership (emergency). Prefer `seize`/`release`.
        #[serde(default)]
        force: bool,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Inject raw bytes (base64-encoded) straight into the pane's PTY, no
    /// bracketed-paste wrapping and no submit. The escape hatch for control
    /// characters and key sequences: Ctrl-C (`"Aw=="`), arrow keys, a bare
    /// carriage return (`"DQ=="`), etc.
    SendRaw {
        #[serde(alias = "session")]
        pane: String,
        bytes_b64: String,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Read a pane's **rendered** screen — the visible text a human sees.
    /// With `lines` set, return only the tail N lines (reaching into scrollback
    /// as needed); omitted returns the full visible screen. This is how a master
    /// pane *observes* a driven pane.
    Read {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        lines: Option<usize>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Report one pane's metadata: name, workspace, command, whether it is
    /// still running or has exited, and its current terminal title.
    Status {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Terminate a pane (kill its PTY / child process).
    Kill {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Return the filesystem path of a pane's shared scratchpad file — the
    /// durable side-channel a master and its workers exchange notes through.
    Scratchpad {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Query the event log (the flight recorder): who did what, when.
    Timeline {
        #[serde(default)]
        since_secs: Option<u64>,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        actor: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Self-report agent status. `pane` defaults to the calling pane (`from`).
    /// States by convention: planning|working|blocked|needs-human|done|idle.
    StatusSet {
        state: String,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Ask the human a question. Returns an ask id immediately; poll
    /// AskResult for the answer (the CLI does this automatically).
    Ask {
        question: String,
        #[serde(default)]
        choices: Option<Vec<String>>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Propose a command into a pane as GHOST TEXT: the human sees it dimmed
    /// at the prompt and accepts (Enter/Tab), rejects (Esc), or types over
    /// it. Returns `{id}`; poll ProposeResult. Nothing touches the PTY until
    /// the human (or a future trust policy) accepts.
    Propose {
        #[serde(alias = "session")]
        pane: String,
        text: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Poll a proposal: `{resolved: bool, outcome?: accepted|rejected|superseded}`.
    ProposeResult {
        id: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Where is the human? `{focused_pane, selected_workspace, pending_asks}`.
    /// The politeness API: don't repaint the pane the human is reading.
    Human {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Fork a workspace: respawn its panes (same name/cwd/command) into a new
    /// workspace, copying scratchpads. PTY state does not fork; layout,
    /// commands, and notes do.
    WorkspaceFork {
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Shell integration: a hooked shell reports a command starting.
    /// Attributed to the calling pane via `from`.
    CmdBegin {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Shell integration: the command finished with `exit`.
    CmdEnd {
        exit: i32,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Structured command history for a pane (newest last).
    Commands {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// The most recent (optionally failed) command in a pane — structured,
    /// no screen-scraping: `{command, cwd, exit, duration_ms}`.
    LastCommand {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        failed_only: bool,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Poll an ask for its answer. `{answered: bool, answer?: string}`.
    AskResult {
        id: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Stream events matching filters. After the initial ack, the connection
    /// emits one JSON event object per line until the client disconnects.
    /// Handled specially by the daemon (not a one-shot request).
    Watch {
        /// Only events with seq > since_seq (catch-up from cursor).
        #[serde(default)]
        since_seq: Option<u64>,
        /// Comma-separated kind prefixes or exact kinds (also accepted as array).
        #[serde(default)]
        kinds: Option<Vec<String>>,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        actor: Option<String>,
        /// If true, replay matching ring/disk events after since_seq before live.
        #[serde(default = "default_true")]
        catch_up: bool,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Who am I under the control plane? `{principal, workspace, policy, grants}`.
    Whoami {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// List policy + grants. `{default_policy, workspace_policy, grants}`.
    Caps {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Grant a capability to a principal. `principal` like `agent:slug` or `cli`.
    CapsGrant {
        principal: String,
        /// Op name (`send`, `kill`, `new`, …) or `*`.
        cap: String,
        #[serde(default)]
        workspace: Option<String>,
        /// TTL seconds from now; omit for permanent.
        #[serde(default)]
        ttl_secs: Option<u64>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Revoke grants. `cap` of `*` revokes all caps for the principal.
    CapsRevoke {
        principal: String,
        #[serde(default = "default_star")]
        cap: String,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Get policy for a workspace (or global default).
    PolicyGet {
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Set policy: `open` | `propose_required` | `locked`.
    PolicySet {
        mode: String,
        #[serde(default)]
        workspace: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Human (or force) claims keyboard ownership of a pane.
    Seize {
        #[serde(alias = "session")]
        pane: String,
        /// `human` (default) or principal string.
        #[serde(default)]
        as_owner: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Release ownership → `none` so either may drive next.
    Release {
        #[serde(alias = "session")]
        pane: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Set drive mode: `pair` | `locked_human` | `agent_led`.
    DriveMode {
        #[serde(alias = "session")]
        pane: String,
        mode: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Agent launch profiles + binary health (`seance doctor agents`).
    Doctor {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// One-shot workspace brief for orchestrators (dense pane rows + focus).
    Brief {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Append (or overwrite) a note on a pane's scratchpad with attribution.
    Note {
        #[serde(alias = "session")]
        #[serde(default)]
        pane: Option<String>,
        text: String,
        /// Default true — append with author stamp. false = replace body.
        #[serde(default = "default_true")]
        append: bool,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Worker completion bridge: write scratchpad body + set status in one op.
    /// Sandboxed agents that can reach the socket but not the FS still get
    /// durable completion (body travels on the wire).
    Finish {
        #[serde(alias = "session")]
        #[serde(default)]
        pane: Option<String>,
        /// Optional body to write to scratchpad.
        #[serde(default)]
        body: Option<String>,
        #[serde(default = "default_true")]
        append: bool,
        #[serde(default = "default_done")]
        status: String,
        #[serde(default)]
        status_note: Option<String>,
        /// Allow `status=done` with no body (default false — evidence-bound).
        #[serde(default)]
        empty_ok: bool,
        /// Bind completion to a dispatch task id (from `send` response).
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Stage/roster projection — dense pane rows for humans + orchestrators.
    Roster {
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },

    /// Durable inject inbox / task envelope (0.9.6).
    /// No args → active task for `$SEANCE_SESSION` / pane. `--id` for a task id.
    Task {
        #[serde(alias = "session")]
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        from: Option<String>,
    },
}

fn default_done() -> String {
    "done".into()
}

fn default_star() -> String {
    "*".into()
}

/// The reply to a [`ControlRequest`]. `ok` is the success flag; exactly one of
/// `data` (on success) or `error` (on failure) is typically populated.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlRequest {
    /// Attribution principal field (`from`) when present.
    pub fn from_field(&self) -> &Option<String> {
        match self {
            Self::List { from, .. }
            | Self::New { from, .. }
            | Self::Send { from, .. }
            | Self::SendRaw { from, .. }
            | Self::Read { from, .. }
            | Self::Status { from, .. }
            | Self::Kill { from, .. }
            | Self::Scratchpad { from, .. }
            | Self::Timeline { from, .. }
            | Self::StatusSet { from, .. }
            | Self::Ask { from, .. }
            | Self::AskResult { from, .. }
            | Self::Propose { from, .. }
            | Self::ProposeResult { from, .. }
            | Self::Human { from, .. }
            | Self::WorkspaceFork { from, .. }
            | Self::CmdBegin { from, .. }
            | Self::CmdEnd { from, .. }
            | Self::Commands { from, .. }
            | Self::LastCommand { from, .. }
            | Self::Watch { from, .. }
            | Self::Whoami { from, .. }
            | Self::Caps { from, .. }
            | Self::CapsGrant { from, .. }
            | Self::CapsRevoke { from, .. }
            | Self::PolicyGet { from, .. }
            | Self::PolicySet { from, .. }
            | Self::Seize { from, .. }
            | Self::Release { from, .. }
            | Self::DriveMode { from, .. }
            | Self::Doctor { from, .. }
            | Self::Brief { from, .. }
            | Self::Note { from, .. }
            | Self::Finish { from, .. }
            | Self::Roster { from, .. }
            | Self::Task { from, .. } => from,
        }
    }

    /// Best-effort workspace scope for policy checks.
    pub fn workspace_hint(&self) -> Option<&str> {
        match self {
            Self::New {
                workspace, scope, ..
            }
            | Self::WorkspaceFork {
                workspace, scope, ..
            }
            | Self::PolicyGet {
                workspace, scope, ..
            }
            | Self::PolicySet {
                workspace, scope, ..
            }
            | Self::CapsGrant {
                workspace, scope, ..
            }
            | Self::CapsRevoke {
                workspace, scope, ..
            } => workspace.as_deref().or(scope.as_deref()),
            Self::List { scope, .. }
            | Self::Send { scope, .. }
            | Self::SendRaw { scope, .. }
            | Self::Read { scope, .. }
            | Self::Status { scope, .. }
            | Self::Kill { scope, .. }
            | Self::Scratchpad { scope, .. }
            | Self::Timeline { scope, .. }
            | Self::StatusSet { scope, .. }
            | Self::Ask { scope, .. }
            | Self::AskResult { scope, .. }
            | Self::Propose { scope, .. }
            | Self::ProposeResult { scope, .. }
            | Self::Human { scope, .. }
            | Self::CmdBegin { scope, .. }
            | Self::CmdEnd { scope, .. }
            | Self::Commands { scope, .. }
            | Self::LastCommand { scope, .. }
            | Self::Watch { scope, .. }
            | Self::Whoami { scope, .. }
            | Self::Caps { scope, .. }
            | Self::Seize { scope, .. }
            | Self::Release { scope, .. }
            | Self::DriveMode { scope, .. }
            | Self::Doctor { scope, .. }
            | Self::Brief { scope, .. }
            | Self::Note { scope, .. }
            | Self::Finish { scope, .. }
            | Self::Roster { scope, .. }
            | Self::Task { scope, .. } => scope.as_deref(),
        }
    }
}

impl ControlResponse {
    /// A successful response carrying `data`.
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// A successful response with no payload (e.g. `kill`).
    pub fn ok_empty() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }

    /// A failure response with a human-readable message.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// The app-side handle the server uses to hand requests to the gpui loop.
///
/// The app builds `mpsc::unbounded()`, keeps the receiver, and passes a
/// `ControlHandle { tx }` (the sender) to [`start_server`]. Cloneable because
/// the underlying `UnboundedSender` is; each connection thread clones it.
#[derive(Clone)]
pub struct ControlHandle {
    pub tx: mpsc::UnboundedSender<(ControlRequest, oneshot::Sender<ControlResponse>)>,
}

/// How long a connection thread waits for the gpui side to answer one request
/// before giving up and returning an error response. Guards against a wedged or
/// backed-up main loop hanging control clients indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the control socket path.
///
/// Prefers `$XDG_RUNTIME_DIR/seance.sock` (the user's runtime dir, cleaned on
/// logout). Falls back to `/tmp/seance-$UID.sock` when `XDG_RUNTIME_DIR` is
/// unset — the `$UID` suffix keeps it per-user on a shared `/tmp`.
pub fn socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("seance.sock");
        }
    }
    let uid = current_uid();
    PathBuf::from(format!("/tmp/seance-{uid}.sock"))
}

/// Best-effort real UID for the `/tmp` fallback socket name.
///
/// Reads `$UID` if the shell exported it, else parses `/proc/self/loginuid`,
/// else `0`. We only need *a* stable per-user token, not a security boundary
/// (the socket file's own permissions are the boundary).
fn current_uid() -> u32 {
    if let Ok(uid) = std::env::var("UID") {
        if let Ok(n) = uid.trim().parse::<u32>() {
            return n;
        }
    }
    // /proc/self/loginuid is the login UID; good enough for a filename token.
    if let Ok(s) = std::fs::read_to_string("/proc/self/loginuid") {
        if let Ok(n) = s.trim().parse::<u32>() {
            if n != u32::MAX {
                return n;
            }
        }
    }
    0
}

/// Bind the control socket and start serving on background std threads.
///
/// Returns the bound socket path (also available from [`socket_path`]).
///
/// **Stale-socket handling.** If the socket file already exists we first try to
/// connect to it. A live server answering means another seance is already
/// running — we error out rather than steal the socket. If the connect fails
/// (leftover file from a crash), we remove the stale file and bind fresh.
///
/// Spawns a detached acceptor thread; each accepted connection gets its own
/// detached handler thread. The threads live for the process's lifetime.
pub fn start_server(handle: ControlHandle) -> Result<PathBuf> {
    let path = socket_path();

    // If something is already listening, don't clobber it.
    if path.exists() {
        match UnixStream::connect(&path) {
            Ok(_) => {
                return Err(anyhow!(
                    "a seance control server is already listening at {}",
                    path.display()
                ));
            }
            Err(_) => {
                // Stale file from a crashed instance — safe to remove.
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing stale control socket {}", path.display()))?;
            }
        }
    }

    // Ensure the parent dir exists (XDG_RUNTIME_DIR normally does; /tmp always).
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating control socket dir {}", parent.display()))?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket {}", path.display()))?;

    let bound = path.clone();
    std::thread::Builder::new()
        .name("seance-control".into())
        .spawn(move || accept_loop(listener, handle))
        .context("spawning control acceptor thread")?;

    Ok(bound)
}

/// Accept connections forever, spinning a handler thread per connection.
fn accept_loop(listener: UnixListener, handle: ControlHandle) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let handle = handle.clone();
                // Detached: a slow/wedged client can't back up the acceptor.
                let spawned = std::thread::Builder::new()
                    .name("seance-control-conn".into())
                    .spawn(move || {
                        if let Err(err) = handle_connection(stream, handle) {
                            eprintln!("control: connection error: {err:#}");
                        }
                    });
                if let Err(err) = spawned {
                    eprintln!("control: failed to spawn connection handler: {err:#}");
                }
            }
            Err(err) => {
                eprintln!("control: accept error: {err:#}");
                // Transient accept errors shouldn't kill the whole server.
            }
        }
    }
}

/// Serve one connection: read request lines until EOF, answer each in order.
///
/// A malformed line (bad JSON) produces an error response but keeps the
/// connection open — one fat-fingered request doesn't drop the session.
fn handle_connection(stream: UnixStream, handle: ControlHandle) -> Result<()> {
    let write_stream = stream.try_clone().context("cloning control stream")?;
    let reader = BufReader::new(stream);
    let mut writer = write_stream;

    for line in reader.lines() {
        let line = line.context("reading control request line")?;
        // Tolerate blank lines / keepalives.
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(request) => dispatch(&handle, request),
            Err(err) => ControlResponse::err(format!("invalid request json: {err}")),
        };

        let mut out = serde_json::to_string(&response).unwrap_or_else(|e| {
            // Serializing our own response should never fail; if it somehow
            // does, emit a hand-rolled error line so the client sees *some*
            // valid JSON rather than a dropped connection.
            format!("{{\"ok\":false,\"error\":\"failed to serialize response: {e}\"}}")
        });
        out.push('\n');
        writer
            .write_all(out.as_bytes())
            .context("writing control response")?;
        writer.flush().context("flushing control response")?;
    }

    Ok(())
}

/// Forward one request to the gpui side and block for its answer.
///
/// Sends `(request, oneshot_sender)` through the handle, then blocks on the
/// oneshot receiver with [`REQUEST_TIMEOUT`]. Three failure modes, each mapped
/// to an `ok: false` response:
/// - the mpsc send failed → the app dropped the receiver (shutting down),
/// - the oneshot was dropped without a reply → the app abandoned the request,
/// - the timeout elapsed → the main loop is wedged or badly backed up.
fn dispatch(handle: &ControlHandle, request: ControlRequest) -> ControlResponse {
    let (reply_tx, reply_rx) = oneshot::channel();

    if handle.tx.unbounded_send((request, reply_tx)).is_err() {
        return ControlResponse::err(
            "seance app is not accepting control requests (shutting down?)",
        );
    }

    // Block this connection thread on the oneshot with a timeout. We race the
    // receiver against a timer future; whichever resolves first wins.
    match block_on_with_timeout(reply_rx, REQUEST_TIMEOUT) {
        Ok(Ok(response)) => response,
        Ok(Err(_canceled)) => {
            ControlResponse::err("seance app dropped the request without replying")
        }
        Err(_timeout) => ControlResponse::err(format!(
            "seance app did not answer within {}s (main loop wedged?)",
            REQUEST_TIMEOUT.as_secs()
        )),
    }
}

/// Block on a `oneshot::Receiver` with a wall-clock timeout, using only
/// `futures` + a std timer thread (no async runtime).
///
/// Returns `Ok(result)` when the oneshot resolves in time (result is
/// `Ok(value)` or `Err(Canceled)`), or `Err(())` on timeout.
///
/// Implementation: a throwaway std thread sleeps for `timeout` then fires a
/// second oneshot. We `block_on` a `select` over the real receiver and the
/// timeout receiver. `futures::executor::block_on` drives both to completion on
/// this thread — no reactor needed because the real receiver is woken by the
/// gpui side's `Sender::send`, and the timeout receiver by the timer thread.
fn block_on_with_timeout(
    reply_rx: oneshot::Receiver<ControlResponse>,
    timeout: Duration,
) -> std::result::Result<std::result::Result<ControlResponse, oneshot::Canceled>, ()> {
    use futures::future::{self, Either};

    let (timeout_tx, timeout_rx) = oneshot::channel::<()>();
    // Detached timer thread: sleeps, then trips the timeout oneshot. If the real
    // reply lands first, `timeout_tx` is simply dropped when this thread ends.
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        let _ = timeout_tx.send(());
    });

    futures::executor::block_on(async move {
        match future::select(reply_rx, timeout_rx).await {
            // Real reply resolved first (Ok=value, Err=canceled — both "not a timeout").
            Either::Left((result, _timeout_fut)) => Ok(result),
            // Timeout fired first (or the timer sender was dropped — treat as timeout).
            Either::Right((_timeout_result, _reply_fut)) => Err(()),
        }
    })
}

/// Default for `Send { submit }`: submit unless explicitly told not to.
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_defaults_submit_true() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"op":"send","session":"w","text":"hi"}"#).unwrap();
        match req {
            ControlRequest::Send { submit, .. } => assert!(submit),
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn send_respects_explicit_no_submit() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"op":"send","session":"w","text":"hi","submit":false}"#)
                .unwrap();
        match req {
            ControlRequest::Send { submit, .. } => assert!(!submit),
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn new_defaults_optional_fields_to_none() {
        let req: ControlRequest = serde_json::from_str(r#"{"op":"new","name":"worker"}"#).unwrap();
        match req {
            ControlRequest::New {
                name,
                cwd,
                command,
                workspace,
                ..
            } => {
                assert_eq!(name, "worker");
                assert!(cwd.is_none());
                assert!(command.is_none());
                assert!(workspace.is_none());
            }
            _ => panic!("expected new"),
        }
    }

    #[test]
    fn read_lines_optional() {
        let full: ControlRequest = serde_json::from_str(r#"{"op":"read","session":"w"}"#).unwrap();
        match full {
            ControlRequest::Read { lines, .. } => assert!(lines.is_none()),
            _ => panic!("expected read"),
        }
        let tail: ControlRequest =
            serde_json::from_str(r#"{"op":"read","session":"w","lines":40}"#).unwrap();
        match tail {
            ControlRequest::Read { lines, .. } => assert_eq!(lines, Some(40)),
            _ => panic!("expected read"),
        }
    }

    #[test]
    fn list_tag_only() {
        let req: ControlRequest = serde_json::from_str(r#"{"op":"list"}"#).unwrap();
        assert!(matches!(req, ControlRequest::List { .. }));
    }

    #[test]
    fn response_omits_none_fields() {
        let ok = ControlResponse::ok_empty();
        let s = serde_json::to_string(&ok).unwrap();
        assert_eq!(s, r#"{"ok":true}"#);

        let err = ControlResponse::err("nope");
        let s = serde_json::to_string(&err).unwrap();
        assert_eq!(s, r#"{"ok":false,"error":"nope"}"#);
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        // Note: this test reads the ambient env; it asserts the shape, not a
        // specific dir, to stay hermetic across machines.
        let p = socket_path();
        assert!(p.to_string_lossy().ends_with("seance.sock"));
    }

    #[test]
    fn session_alias_accepts_legacy_key() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"op":"kill","session":"worker-1"}"#).unwrap();
        match req {
            ControlRequest::Kill { pane, .. } => assert_eq!(pane, "worker-1"),
            _ => panic!("expected kill"),
        }
    }

    #[test]
    fn finish_and_note_roundtrip() {
        let finish = ControlRequest::Finish {
            pane: Some("w".into()),
            body: Some("done body".into()),
            append: true,
            status: "done".into(),
            status_note: Some("ok".into()),
            empty_ok: false,
            task: Some("t1".into()),
            scope: Some("lab".into()),
            from: Some("w".into()),
        };
        let s = serde_json::to_string(&finish).unwrap();
        let back: ControlRequest = serde_json::from_str(&s).unwrap();
        match back {
            ControlRequest::Finish {
                pane,
                body,
                status,
                task,
                empty_ok,
                ..
            } => {
                assert_eq!(pane.as_deref(), Some("w"));
                assert_eq!(body.as_deref(), Some("done body"));
                assert_eq!(status, "done");
                assert_eq!(task.as_deref(), Some("t1"));
                assert!(!empty_ok);
            }
            _ => panic!("expected finish"),
        }

        let note = ControlRequest::Note {
            pane: None,
            text: "hi".into(),
            append: false,
            scope: None,
            from: None,
        };
        let s = serde_json::to_string(&note).unwrap();
        assert!(s.contains(r#""op":"note""#));
        let back: ControlRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ControlRequest::Note { append: false, .. }));
    }

    #[test]
    fn status_set_and_seize_wire() {
        let req: ControlRequest = serde_json::from_str(
            r#"{"op":"status_set","state":"working","note":"busy","pane":"w"}"#,
        )
        .unwrap();
        match req {
            ControlRequest::StatusSet {
                state, note, pane, ..
            } => {
                assert_eq!(state, "working");
                assert_eq!(note.as_deref(), Some("busy"));
                assert_eq!(pane.as_deref(), Some("w"));
            }
            _ => panic!("expected status_set"),
        }

        let seize: ControlRequest =
            serde_json::from_str(r#"{"op":"seize","pane":"w","as_owner":"human"}"#).unwrap();
        match seize {
            ControlRequest::Seize { pane, as_owner, .. } => {
                assert_eq!(pane, "w");
                assert_eq!(as_owner.as_deref(), Some("human"));
            }
            _ => panic!("expected seize"),
        }
    }

    #[test]
    fn watch_defaults_catch_up_true() {
        let req: ControlRequest = serde_json::from_str(r#"{"op":"watch"}"#).unwrap();
        match req {
            ControlRequest::Watch { catch_up, .. } => assert!(catch_up),
            _ => panic!("expected watch"),
        }
    }

    #[test]
    fn from_field_and_workspace_hint() {
        let req = ControlRequest::Send {
            pane: "w".into(),
            text: "x".into(),
            submit: true,
            force: false,
            scope: Some("lab".into()),
            from: Some("orch".into()),
        };
        assert_eq!(req.from_field().as_deref(), Some("orch"));
        assert_eq!(req.workspace_hint(), Some("lab"));

        let list = ControlRequest::List {
            scope: None,
            from: None,
        };
        assert!(list.from_field().is_none());
        assert!(list.workspace_hint().is_none());
    }

    #[test]
    fn task_and_roster_ops() {
        let task: ControlRequest =
            serde_json::from_str(r#"{"op":"task","pane":"w","id":"t9"}"#).unwrap();
        match task {
            ControlRequest::Task { pane, id, .. } => {
                assert_eq!(pane.as_deref(), Some("w"));
                assert_eq!(id.as_deref(), Some("t9"));
            }
            _ => panic!("expected task"),
        }
        let roster: ControlRequest = serde_json::from_str(r#"{"op":"roster"}"#).unwrap();
        assert!(matches!(roster, ControlRequest::Roster { .. }));
        let brief: ControlRequest = serde_json::from_str(r#"{"op":"brief"}"#).unwrap();
        assert!(matches!(brief, ControlRequest::Brief { .. }));
        let doctor: ControlRequest = serde_json::from_str(r#"{"op":"doctor"}"#).unwrap();
        assert!(matches!(doctor, ControlRequest::Doctor { .. }));
    }
}
