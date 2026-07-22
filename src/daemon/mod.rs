//! seance daemon process — owns the session engine and serves ctl + GUI + handoff.

use std::io::{BufRead, BufReader, IoSlice, IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};

use crate::control::{self, ControlRequest, ControlResponse};
use crate::runtime::engine::{Engine, OwnedFdAdopt};
use crate::runtime::protocol::{GuiEvent, GuiRequest, HandoffBundle, Hello};
use crate::runtime::{daemon_pid_path, SessionEvent, SharedEngine};

/// Entry: `seance daemon` or `seance daemon --takeover PATH`.
pub fn run_daemon(args: Vec<String>) -> ! {
    let code = match run_daemon_inner(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[seance daemon] fatal: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run_daemon_inner(args: Vec<String>) -> Result<()> {
    // Install shell integration rc.
    let rc = crate::pane::shell_rc_path();
    if let Some(dir) = rc.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&rc, include_str!("../../assets/seance.bash"));

    let takeover = args
        .windows(2)
        .find(|w| w[0] == "--takeover")
        .map(|w| PathBuf::from(&w[1]));
    let is_takeover = takeover.is_some();

    let (engine, event_rx) = if let Some(ref handoff_sock) = takeover {
        eprintln!("[seance daemon] takeover from {}", handoff_sock.display());
        receive_handoff(handoff_sock)?
    } else {
        Engine::new()?
    };

    let engine = Arc::new(Mutex::new(engine));

    // Session event pump → broadcast grids.
    {
        let eng = Arc::clone(&engine);
        thread::Builder::new()
            .name("seance-events".into())
            .spawn(move || {
                while let Ok(ev) = event_rx.recv() {
                    if let Ok(mut e) = eng.lock() {
                        e.handle_session_event(ev);
                    }
                }
            })
            .ok();
    }

    // Write pid file.
    let pid_path = daemon_pid_path();
    if let Some(dir) = pid_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&pid_path, format!("{}\n", std::process::id()));

    let sock_path = control::socket_path();
    // Remove stale socket if we own the world.
    // On takeover, the old daemon may still be closing — wait briefly for it
    // to drop the control socket, then bind.
    if is_takeover {
        for _ in 0..50 {
            if !sock_path.exists() {
                break;
            }
            // If the path exists but nothing listens, remove it.
            if UnixStream::connect(&sock_path).is_err() {
                let _ = std::fs::remove_file(&sock_path);
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }
        if sock_path.exists() {
            // Force: old process should have exited by now.
            let _ = std::fs::remove_file(&sock_path);
        }
    } else if sock_path.exists() {
        match UnixStream::connect(&sock_path) {
            Ok(_) => bail!(
                "another seance daemon is already listening at {}",
                sock_path.display()
            ),
            Err(_) => {
                let _ = std::fs::remove_file(&sock_path);
            }
        }
    }
    if let Some(parent) = sock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let listener =
        UnixListener::bind(&sock_path).with_context(|| format!("bind {}", sock_path.display()))?;
    eprintln!("[seance daemon] listening on {}", sock_path.display());
    eprintln!("[seance daemon] pid {}", std::process::id());

    // Accept loop.
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let eng = Arc::clone(&engine);
                thread::Builder::new()
                    .name("seance-conn".into())
                    .spawn(move || {
                        if let Err(e) = handle_connection(stream, eng) {
                            eprintln!("[seance daemon] connection error: {e:#}");
                        }
                    })
                    .ok();
            }
            Err(e) => eprintln!("[seance daemon] accept: {e}"),
        }
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, engine: SharedEngine) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 || line.trim().is_empty() {
        // Probe connect (ensure_daemon / single-instance check) — silent.
        return Ok(());
    }
    let hello: Hello = serde_json::from_str(line.trim()).context("hello parse")?;
    match hello.role.as_str() {
        "ctl" => serve_ctl(reader, writer, engine),
        "gui" => serve_gui(reader, writer, engine),
        "handoff" => {
            // Only accepted when we're the old daemon exporting state.
            serve_handoff_export(writer, engine)
        }
        "upgrade" => {
            // GUI or CLI asked this daemon to upgrade itself.
            serve_upgrade_request(writer, engine)
        }
        other => {
            let _ = writeln!(
                writer,
                "{}",
                serde_json::to_string(&ControlResponse::err(format!("unknown role '{other}'")))?
            );
            Ok(())
        }
    }
}

fn serve_ctl(
    reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    engine: SharedEngine,
) -> Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: ControlRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = ControlResponse::err(format!("bad request: {e}"));
                writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
                continue;
            }
        };
        // Streaming watch: ack then push matching events until disconnect.
        if let ControlRequest::Watch {
            since_seq,
            kinds,
            pane,
            actor,
            catch_up,
            scope,
            from: _,
        } = req
        {
            return serve_watch(
                writer,
                engine,
                since_seq.unwrap_or(0),
                kinds,
                pane,
                actor,
                scope,
                catch_up,
            );
        }
        let resp = engine
            .lock()
            .map(|mut e| e.handle_control(req))
            .unwrap_or_else(|_| ControlResponse::err("engine lock poisoned"));
        writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
    }
    Ok(())
}

/// Stream events matching filters as JSON lines after an initial ack.
fn serve_watch(
    mut writer: UnixStream,
    _engine: SharedEngine,
    since_seq: u64,
    kinds: Option<Vec<String>>,
    pane: Option<String>,
    actor: Option<String>,
    scope: Option<String>,
    catch_up: bool,
) -> Result<()> {
    use crate::events;

    let (rx, cursor) = events::subscribe();
    let ack = ControlResponse::ok(serde_json::json!({
        "watching": true,
        "cursor": cursor,
        "since_seq": since_seq,
    }));
    writeln!(writer, "{}", serde_json::to_string(&ack)?)?;
    writer.flush()?;

    let kinds_ref = kinds.as_deref();
    let write_ev = |writer: &mut UnixStream, e: &events::Event| -> Result<()> {
        if !events::matches_filter(
            e,
            scope.as_deref(),
            pane.as_deref(),
            actor.as_deref(),
            kinds_ref,
        ) {
            return Ok(());
        }
        // Wrap as ControlResponse so clients can share the same decoder.
        let resp = ControlResponse::ok(serde_json::to_value(e)?);
        writeln!(writer, "{}", serde_json::to_string(&resp)?)?;
        writer.flush()?;
        Ok(())
    };

    if catch_up {
        // Prefer in-memory ring; fall back to disk for deeper history.
        let mut backlog = events::ring_since(since_seq);
        if backlog.is_empty() && since_seq > 0 {
            backlog = events::read_ex(
                0,
                since_seq,
                scope.as_deref(),
                pane.as_deref(),
                actor.as_deref(),
                kinds.as_deref(),
                500,
            );
        } else if backlog.is_empty() && since_seq == 0 {
            // Fresh subscriber with no cursor: last 50 matching from disk.
            backlog = events::read_ex(
                0,
                0,
                scope.as_deref(),
                pane.as_deref(),
                actor.as_deref(),
                kinds.as_deref(),
                50,
            );
        }
        for e in backlog {
            write_ev(&mut writer, &e)?;
        }
    }

    // Live stream until client disconnects or write fails.
    while let Ok(e) = rx.recv() {
        if write_ev(&mut writer, &e).is_err() {
            break;
        }
    }
    Ok(())
}

fn serve_gui(
    mut reader: BufReader<UnixStream>,
    writer: UnixStream,
    engine: SharedEngine,
) -> Result<()> {
    let writer = Arc::new(Mutex::new(writer));
    let (tx, rx) = mpsc::channel::<GuiEvent>();

    // Register for broadcasts — one connection = one window.
    let window_id = {
        let mut eng = engine.lock().unwrap();
        eng.register_gui(tx.clone())
    };

    // Writer thread for push events.
    {
        let writer = Arc::clone(&writer);
        thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                let Ok(json) = serde_json::to_string(&ev) else {
                    continue;
                };
                let mut w = writer.lock().unwrap();
                if writeln!(w, "{json}").is_err() {
                    break;
                }
            }
        });
    }

    // Read requests.
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: GuiRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                let ev = GuiEvent::Error {
                    message: format!("bad gui request: {e}"),
                };
                let _ = tx.send(ev);
                continue;
            }
        };
        let is_bye = matches!(req, GuiRequest::Bye);
        let reply = engine.lock().unwrap().handle_gui(req, &window_id);
        if let Some(ev) = reply {
            let _ = tx.send(ev);
        }
        if is_bye {
            // Bye already ran unregister_gui; drop the connection.
            break;
        }
    }
    // Socket EOF (or post-Bye) — free / reassign if still registered.
    if let Ok(mut eng) = engine.lock() {
        eng.unregister_gui(&window_id);
    }
    Ok(())
}

fn serve_handoff_export(mut writer: UnixStream, engine: SharedEngine) -> Result<()> {
    // This is the OLD daemon exporting to a new one — actually handoff is
    // initiated by serve_upgrade_request which spawns the new daemon.
    let _ = writeln!(
        writer,
        "{}",
        serde_json::json!({"ok": false, "error": "use role upgrade on the live daemon"})
    );
    let _ = engine;
    Ok(())
}

/// Serialize upgrades: only one `serve_upgrade_request` may run per daemon.
/// Two racing requests would each spawn a new daemon, bind-race the handoff
/// socket, and double-run `prepare_upgrade` — whichever exits first kills the
/// other mid-handoff.
static UPGRADE_SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Resets [`UPGRADE_SERVING`] on any early return from a failed upgrade so a
/// later attempt can proceed. The happy path exits the process (skipping this
/// drop) — fine, the successor daemon starts with a fresh flag.
struct UpgradeGate;
impl Drop for UpgradeGate {
    fn drop(&mut self) {
        UPGRADE_SERVING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn serve_upgrade_request(mut writer: UnixStream, engine: SharedEngine) -> Result<()> {
    eprintln!("[seance daemon] upgrade requested");
    // Reject a concurrent upgrade rather than racing two teardowns.
    if UPGRADE_SERVING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        eprintln!("[seance daemon] upgrade rejected: another upgrade already in flight");
        let _ = writeln!(
            writer,
            "{}",
            serde_json::json!({"ok": false, "error": "upgrade already in progress"})
        );
        let _ = writer.flush();
        return Ok(());
    }
    let _gate = UpgradeGate;
    let handoff_path = {
        let mut p = control::socket_path();
        p.set_extension("handoff.sock");
        p
    };
    if handoff_path.exists() {
        let _ = std::fs::remove_file(&handoff_path);
    }
    let handoff_listener = UnixListener::bind(&handoff_path)?;

    // Spawn new daemon binary with --takeover. Log to a side file so
    // upgrade failures are diagnosable (stdout/stderr of the old daemon
    // may already be redirected).
    //
    // IMPORTANT: after `cargo build --release`, the running daemon's inode is
    // unlinked — `current_exe()` becomes `…/seance (deleted)` and spawn fails
    // with ENOENT. Prefer the on-disk path at the same location (the new
    // binary), then SEANCE_BIN, then PATH.
    let bin = resolve_upgrade_bin().context("resolve upgrade binary")?;
    eprintln!("[seance daemon] upgrade spawning {}", bin.display());
    let log_path = crate::runtime::state_data_dir().join("daemon-upgrade.log");
    if let Some(dir) = log_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let log_file = std::fs::File::create(&log_path).context("daemon-upgrade.log")?;
    let mut child = std::process::Command::new(&bin)
        .arg("daemon")
        .arg("--takeover")
        .arg(&handoff_path)
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()
        .with_context(|| format!("spawn upgraded daemon from {}", bin.display()))?;

    // Accept handoff connection from new daemon.
    handoff_listener.set_nonblocking(false)?;
    // Timeout accept.
    let (conn, _) = handoff_listener
        .accept()
        .context("waiting for new daemon handoff connect")?;

    // Prepare state + FDs.
    let (bundle, fds) = {
        let mut eng = engine.lock().unwrap();
        eng.prepare_upgrade()?
    };

    // Send bundle JSON + SCM_RIGHTS.
    send_handoff(conn, &bundle, &fds)?;

    // Reply *before* tearing down the control socket so the client never
    // races an unlink into EAGAIN/EOF on a half-closed stream.
    //
    // Best-effort on purpose: the handoff already happened and the new daemon
    // is live, so we MUST fall through to teardown + exit even if the client
    // vanished (e.g. Ctrl-C mid-upgrade → EPIPE). Bailing here would strand the
    // old daemon holding the control socket while the new daemon can never bind.
    {
        let line = serde_json::json!({"ok": true, "pid": child.id()}).to_string();
        if let Err(e) = writeln!(writer, "{line}") {
            eprintln!("[seance daemon] upgrade: client write failed, tearing down anyway: {e}");
        }
        let _ = writer.flush();
        // Half-close write so the client sees EOF after the line, not a hang.
        let _ = writer.shutdown(std::net::Shutdown::Write);
    }

    // Drop control socket FIRST so the new daemon can bind, then exit.
    // Children stay alive: prepare_upgrade already released master FDs
    // without SIGHUP.
    let _ = std::fs::remove_file(control::socket_path());
    let _ = std::fs::remove_file(&handoff_path);
    eprintln!("[seance daemon] upgrade complete, exiting old process");
    // Small delay so sendmsg flush / client response lands.
    thread::sleep(Duration::from_millis(80));
    std::process::exit(0);
}

fn receive_handoff(handoff_sock: &PathBuf) -> Result<(Engine, mpsc::Receiver<SessionEvent>)> {
    // Connect to old daemon's handoff listener.
    let mut last_err = None;
    let mut stream = None;
    for _ in 0..50 {
        match UnixStream::connect(handoff_sock) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let stream = stream.with_context(|| format!("connect handoff: {last_err:?}"))?;

    let (bundle, fds) = recv_handoff(stream)?;
    let adopted: Vec<(usize, OwnedFdAdopt)> = fds
        .into_iter()
        .enumerate()
        .map(|(i, fd)| (i, OwnedFdAdopt { fd }))
        .collect();
    Engine::from_handoff(bundle, adopted)
}

/// Send handoff bundle + file descriptors via SCM_RIGHTS.
fn send_handoff(mut stream: UnixStream, bundle: &HandoffBundle, fds: &[OwnedFd]) -> Result<()> {
    let json = serde_json::to_string(bundle)?;
    let len = (json.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(json.as_bytes())?;

    if fds.is_empty() {
        return Ok(());
    }

    let raws: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
    // Send a single dummy byte with the FDs.
    let dummy = [0u8];
    let iov = [IoSlice::new(&dummy)];
    // cmsg buffer
    let fd_bytes = unsafe {
        std::slice::from_raw_parts(
            raws.as_ptr() as *const u8,
            raws.len() * std::mem::size_of::<RawFd>(),
        )
    };
    // Use nix-less sendmsg via libc.
    let mut cbuf = vec![0u8; unsafe { libc::CMSG_SPACE((fd_bytes.len()) as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    let mut iov_c = libc::iovec {
        iov_base: dummy.as_ptr() as *mut _,
        iov_len: 1,
    };
    msg.msg_iov = &mut iov_c;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cbuf.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            bail!("CMSG_FIRSTHDR null");
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN((fd_bytes.len()) as u32) as _;
        std::ptr::copy_nonoverlapping(
            fd_bytes.as_ptr(),
            libc::CMSG_DATA(cmsg) as *mut u8,
            fd_bytes.len(),
        );
        let n = libc::sendmsg(stream.as_raw_fd(), &msg, 0);
        if n < 0 {
            bail!("sendmsg: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_handoff(stream: UnixStream) -> Result<(HandoffBundle, Vec<OwnedFd>)> {
    use std::io::Read;
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut json = vec![0u8; len];
    stream.read_exact(&mut json)?;
    let bundle: HandoffBundle = serde_json::from_slice(&json)?;

    let n_fds = bundle.panes.iter().filter_map(|p| p.fd_index).count();
    if n_fds == 0 {
        return Ok((bundle, Vec::new()));
    }

    let mut dummy = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: dummy.as_mut_ptr() as *mut _,
        iov_len: 1,
    };
    let mut cbuf =
        vec![
            0u8;
            unsafe { libc::CMSG_SPACE((n_fds * std::mem::size_of::<RawFd>()) as u32) as usize }
        ];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cbuf.len() as _;

    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        bail!("recvmsg: {}", std::io::Error::last_os_error());
    }

    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                let bytes = (*cmsg).cmsg_len as usize - (data as usize - cmsg as usize);
                let count = bytes / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    let fd = *data.add(i);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((bundle, fds))
}

/// Resolve the binary to exec for `seance upgrade`.
///
/// When a running daemon's binary has been replaced by `cargo build`, Linux
/// leaves the old inode mapped and `current_exe()` reports
/// `…/seance (deleted)`. Spawning that path fails. The *new* binary lives at
/// the original path without the suffix — use that when present.
fn resolve_upgrade_bin() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SEANCE_BIN") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Ok(pb);
        }
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let s = exe.to_string_lossy();
    // Kernel may append " (deleted)" when the file was unlinked under us.
    let cleaned = s
        .strip_suffix(" (deleted)")
        .or_else(|| s.strip_suffix(" (deleted)"))
        .unwrap_or(s.as_ref());
    let cleaned = PathBuf::from(cleaned);
    if cleaned.is_file() {
        return Ok(cleaned);
    }
    if exe.is_file() {
        return Ok(exe);
    }
    // PATH lookup.
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join("seance");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "cannot find seance binary to upgrade to (current_exe={})",
        exe.display()
    );
}

/// Ensure a daemon is running; spawn one if needed. Returns true if we spawned.
pub fn ensure_daemon() -> Result<bool> {
    let path = control::socket_path();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Ok(false);
        }
        let _ = std::fs::remove_file(&path);
    }
    let bin = std::env::current_exe()?;
    std::process::Command::new(bin)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn seance daemon")?;
    // Wait for socket.
    for _ in 0..100 {
        if path.exists() && UnixStream::connect(&path).is_ok() {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("daemon did not become ready");
}

/// Ask the live daemon to upgrade to this binary.
pub fn request_upgrade() -> Result<()> {
    let mut stream =
        UnixStream::connect(control::socket_path()).context("connect to daemon for upgrade")?;
    // Blocking + long timeout: handoff with many panes can take >30s, and a
    // nonblocking socket races into EAGAIN (os error 11) on read_line.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    writeln!(stream, r#"{{"role":"upgrade"}}"#).context("write upgrade hello")?;
    let _ = stream.flush();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // Retry Interrupted / WouldBlock — observed under load when the daemon is
    // mid-handoff and the kernel returns EAGAIN despite SO_RCVTIMEO.
    let n = {
        let mut last_err = None;
        let mut got = 0usize;
        for attempt in 0..40 {
            // Do NOT clear `line` between retries: read_line *appends*, so a
            // partial line read before a WouldBlock/timeout is resumed on the
            // next attempt rather than silently discarded (which would corrupt
            // the JSON we parse below).
            match reader.read_line(&mut line) {
                Ok(n) => {
                    got = n;
                    last_err = None;
                    break;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted
                        || e.raw_os_error() == Some(11) /* EAGAIN */ =>
                {
                    last_err = Some(e);
                    // First attempts: brief yield. Later: slightly longer.
                    let ms = if attempt < 10 { 25 } else { 100 };
                    thread::sleep(Duration::from_millis(ms));
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    bail!(
                        "upgrade timed out waiting for daemon reply \
                         (check ~/.local/share/seance/daemon-upgrade.log)"
                    );
                }
                Err(e) => return Err(e).context("read upgrade response"),
            }
        }
        if let Some(e) = last_err {
            return Err(e).context(
                "read upgrade response: still WouldBlock/EAGAIN after retries \
                 (daemon busy or upgrade already in flight? check daemon-upgrade.log)",
            );
        }
        got
    };
    if n == 0 {
        bail!(
            "daemon closed upgrade connection without reply \
             (check ~/.local/share/seance/daemon-upgrade.log)"
        );
    }
    eprintln!("[seance] upgrade response: {}", line.trim());
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).context("parse upgrade response")?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown upgrade failure");
        bail!("{err}");
    }
    // Wait for new daemon to bind the control socket (handoff can lag).
    for _ in 0..100 {
        if UnixStream::connect(control::socket_path()).is_ok() {
            // Brief settle so first ctl/GUI attach doesn't race empty accept.
            thread::sleep(Duration::from_millis(50));
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("upgraded daemon did not become ready on control socket");
}

/// Connect as ctl client (hello first). Used by seance ctl after protocol change.
pub fn ctl_connect() -> Result<UnixStream> {
    let mut stream = UnixStream::connect(control::socket_path())
        .with_context(|| format!("connect {}", control::socket_path().display()))?;
    writeln!(stream, r#"{{"role":"ctl"}}"#)?;
    Ok(stream)
}
