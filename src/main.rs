mod agency;
mod agents;
mod app;
mod caps;
mod cmdlog;
mod control;
mod ctl;
mod daemon;
mod desktop_notify;
mod events;
mod fileview;
mod gui_client;
mod host;
mod launch;
mod pane;
mod picker;
mod popout;
mod prompts;
mod remote_cache;
mod remote_term;
mod remote_term_view;
mod runtime;
mod scratchpad;
mod state;
mod sysopen;
mod term_font;
mod term_shared;
mod theme;
mod tunnel;

use gpui::*;
use gpui_component::Root;

use crate::app::SeanceApp;

fn main() {
    // Rust ignores SIGPIPE by default, turning `seance ctl … | head` into a
    // broken-pipe panic. Restore the conventional CLI disposition: die quietly.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args: Vec<String> = std::env::args().collect();

    // `seance --version` / `-V` / `version` — never open the GUI.
    if matches!(
        args.get(1).map(String::as_str),
        Some("--version") | Some("-V") | Some("version")
    ) {
        println!("seance {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // `seance ctl ...` — control-plane CLI client, no GUI.
    if args.get(1).map(String::as_str) == Some("ctl") {
        std::process::exit(ctl::run_ctl(args[2..].to_vec()));
    }

    // Hidden helpers used by the thin-client tunnel over ssh.
    // `seance _sockpath` — print the local daemon bind path.
    if args.get(1).map(String::as_str) == Some("_sockpath") {
        println!("{}", control::bind_socket_path().display());
        return;
    }
    // `seance _ensure-daemon` — start the local daemon if absent, wait ready.
    if args.get(1).map(String::as_str) == Some("_ensure-daemon") {
        match daemon::ensure_daemon() {
            Ok(spawned) => {
                eprintln!(
                    "[seance] daemon {}",
                    if spawned {
                        "started"
                    } else {
                        "already running"
                    }
                );
                return;
            }
            Err(e) => {
                eprintln!("[seance] ensure-daemon failed: {e:#}");
                std::process::exit(1);
            }
        }
    }

    // `seance daemon` — long-lived session runtime (no GUI).
    if args.get(1).map(String::as_str) == Some("daemon") {
        daemon::run_daemon(args[2..].to_vec());
    }

    // `seance upgrade` / `seance reload` / `seance daemon-upgrade` —
    // graceful daemon binary swap. Sessions survive. Prefer this over any
    // kill of the daemon process.
    if matches!(
        args.get(1).map(String::as_str),
        Some("upgrade") | Some("reload") | Some("daemon-upgrade")
    ) {
        match daemon::ensure_daemon() {
            Ok(_) => {}
            Err(e) => {
                eprintln!("[seance] {e:#}");
                std::process::exit(1);
            }
        }
        match daemon::request_upgrade() {
            Ok(()) => {
                eprintln!("[seance] daemon upgraded — sessions preserved");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[seance] upgrade failed: {e:#}");
                eprintln!(
                    "[seance] do NOT fall back to killing the daemon — that destroys sessions"
                );
                std::process::exit(1);
            }
        }
    }

    // `seance restart-gui` — kill only the GUI process(es), leave daemon up.
    if args.get(1).map(String::as_str) == Some("restart-gui") {
        let self_pid = std::process::id();
        let mut killed = 0u32;
        if let Ok(out) = std::process::Command::new("pgrep")
            .args(["-x", "seance"])
            .output()
        {
            for pid_s in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                let Ok(pid) = pid_s.parse::<u32>() else {
                    continue;
                };
                if pid == self_pid {
                    continue;
                }
                // Skip daemon processes (cmdline contains "daemon").
                if sysopen::process_cmdline(pid).contains("daemon") {
                    continue;
                }
                let _ = std::process::Command::new("kill").arg(pid_s).status();
                killed += 1;
            }
        }
        eprintln!("[seance] stopped {killed} gui process(es); daemon left running");
        // Relaunch GUI in this process by falling through — but we're still
        // the restart-gui argv. Re-exec as plain seance.
        let bin = std::env::current_exe().expect("current_exe");
        let err = std::process::Command::new(bin).status();
        std::process::exit(err.map(|s| s.code().unwrap_or(1)).unwrap_or(1));
    }

    // Durable GUI stderr + panic backtraces before anything else on this path
    // (desktop launches otherwise discard stderr). See `install_gui_stderr_log`.
    install_gui_stderr_log();

    // Resolve where the daemon lives. CLI overrides rewrite the saved
    // preference; otherwise the preference (or a reachable local daemon)
    // decides silently, and only genuine ambiguity opens the picker.
    let cli_pref = match args.get(1).map(String::as_str) {
        Some("--remote") => match args.get(2) {
            Some(host) => Some(launch::LaunchPref {
                mode: launch::LaunchMode::Remote,
                host: Some(host.clone()),
            }),
            None => {
                eprintln!("usage: seance --remote <host>");
                std::process::exit(2);
            }
        },
        Some("--local") => Some(launch::LaunchPref {
            mode: launch::LaunchMode::Local,
            host: launch::load().and_then(|p| p.host),
        }),
        _ => None,
    };
    if let Some(ref pref) = cli_pref {
        launch::save(pref);
    }
    let pref = cli_pref.or_else(launch::load);

    enum Boot {
        /// Local daemon confirmed up.
        Local,
        /// Tunnel established; connect through it.
        Remote(tunnel::Tunnel),
        /// Ask the human (prefill + optional error from a failed silent path).
        Picker(Option<launch::LaunchPref>, Option<String>),
    }

    let boot = match &pref {
        Some(p) if p.mode == launch::LaunchMode::Remote => {
            let host = p.host.clone().unwrap_or_default();
            if host.is_empty() {
                Boot::Picker(
                    pref.clone(),
                    Some("saved remote preference has no host".into()),
                )
            } else {
                eprintln!("[seance] launch preference: remote → {host}");
                match tunnel::Tunnel::establish(&host) {
                    Ok(t) => Boot::Remote(t),
                    Err(e) => Boot::Picker(pref.clone(), Some(format!("{host}: {e:#}"))),
                }
            }
        }
        Some(_) => match daemon::ensure_daemon() {
            Ok(spawned) => {
                eprintln!(
                    "[seance] {} session daemon",
                    if spawned { "started" } else { "connected to" }
                );
                Boot::Local
            }
            Err(e) => Boot::Picker(pref.clone(), Some(format!("local daemon: {e:#}"))),
        },
        None => {
            // No preference: a running local daemon means business as usual;
            // otherwise ask rather than silently spawning one.
            if std::os::unix::net::UnixStream::connect(control::bind_socket_path()).is_ok() {
                eprintln!("[seance] connected to session daemon");
                Boot::Local
            } else {
                Boot::Picker(None, None)
            }
        }
    };

    // Install the shell-integration rc so default shell panes get command
    // tracking (see docs/SHELL-INTEGRATION.md).
    let rc_path = pane::shell_rc_path();
    if let Some(dir) = rc_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&rc_path, include_str!("../assets/seance.bash"));

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        theme::init(cx);

        // Kill the ssh forward on quit — the tunnel lives and dies with the GUI.
        cx.on_app_quit(|_cx| async {
            if let Some(t) = ACTIVE_TUNNEL.get() {
                t.shutdown();
            }
        })
        .detach();

        match boot {
            Boot::Local => open_main_window(cx),
            Boot::Remote(t) => {
                adopt_tunnel(t);
                open_main_window(cx);
            }
            Boot::Picker(prefill, error) => open_picker_window(prefill, error, cx),
        }
        cx.activate(true);
    });
}

/// The live ssh tunnel for this GUI process, if any. Static so the quit hook
/// and any diagnostics can reach it; set exactly once per process.
static ACTIVE_TUNNEL: std::sync::OnceLock<tunnel::Tunnel> = std::sync::OnceLock::new();

/// Point every in-process client (gui socket, shelled `seance ctl`) at the
/// forwarded socket, then stash the tunnel for the process lifetime.
fn adopt_tunnel(t: tunnel::Tunnel) {
    eprintln!(
        "[seance] remote daemon via {} ({})",
        t.host,
        t.local_sock.display()
    );
    // SAFETY: called on the main thread before the GUI connects or spawns.
    unsafe {
        std::env::set_var("SEANCE_SOCKET", &t.local_sock);
    }
    let _ = ACTIVE_TUNNEL.set(t);
}

fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1480.), px(920.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("seance".into()),
                ..Default::default()
            }),
            app_id: Some("seance".into()),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| SeanceApp::new(window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        },
    )
    .expect("failed to open window");
}

fn open_picker_window(prefill: Option<launch::LaunchPref>, error: Option<String>, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(680.), px(360.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("seance — connect".into()),
                ..Default::default()
            }),
            app_id: Some("seance".into()),
            ..Default::default()
        },
        |_window, cx| {
            cx.new(|cx| {
                picker::LaunchPicker::new(
                    prefill,
                    error,
                    cx,
                    Box::new(|outcome, window, cx| {
                        if let picker::PickOutcome::Remote(t) = outcome {
                            adopt_tunnel(t);
                        }
                        open_main_window(cx);
                        window.remove_window();
                    }),
                )
            })
        },
    )
    .expect("failed to open picker window");
}

/// Redirect GUI process stderr to a durable append log so panics / GPUI noise
/// survive desktop launches (`Terminal=false` otherwise swallows them).
///
/// Path: `$SEANCE_STATE_DIR/gui.stderr.log` (default `~/.local/share/seance/`).
/// Rotates to `gui.stderr.log.1` above ~5 MiB. Sets `RUST_BACKTRACE=1` when
/// unset so panics include stacks. No-op for `ctl` / `daemon` (those paths
/// never call this). Best-effort — failures never block launch.
fn install_gui_stderr_log() {
    // Prefer full backtraces on panic for post-mortem (user can override).
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // SAFETY: single-threaded at main startup; no other threads yet.
        unsafe {
            std::env::set_var("RUST_BACKTRACE", "1");
        }
    }

    let dir = match gui_log_dir() {
        Some(d) => d,
        None => return,
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("gui.stderr.log");
    rotate_gui_log_if_large(&path);

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return,
    };

    use std::io::Write;
    let mut file = file;
    let ts = chrono_lite_stamp();
    let banner = format!(
        "\n--- seance gui pid={} version={} started={} ---\n",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        ts
    );
    let _ = file.write_all(banner.as_bytes());
    let _ = file.flush();

    // If the user launched from a real terminal, leave a breadcrumb on the TTY
    // before we steal fd 2.
    let stderr_is_tty = unsafe { libc::isatty(libc::STDERR_FILENO) == 1 };
    if stderr_is_tty {
        let _ = writeln!(
            std::io::stderr(),
            "[seance] gui stderr → {}",
            path.display()
        );
    }

    // Point fd 2 at the log file so `eprintln!`, panic hooks, and C libraries
    // all land in the durable path.
    let fd = {
        use std::os::fd::AsRawFd;
        file.as_raw_fd()
    };
    unsafe {
        if libc::dup2(fd, libc::STDERR_FILENO) < 0 {
            return;
        }
    }
    // Keep `file` open for the process lifetime via leak — the fd is what
    // matters after dup2; dropping would close our only handle if dup failed
    // mid-way. Leak is intentional.
    std::mem::forget(file);

    eprintln!("[seance] gui log ready (RUST_BACKTRACE=1)");
}

fn gui_log_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            let expanded = shellexpand::full(&dir).ok()?;
            return Some(std::path::PathBuf::from(expanded.as_ref()));
        }
    }
    Some(std::path::PathBuf::from(
        shellexpand::tilde("~/.local/share/seance").as_ref(),
    ))
}

fn rotate_gui_log_if_large(path: &std::path::Path) {
    const MAX: u64 = 5 * 1024 * 1024; // 5 MiB
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX {
        return;
    }
    let bak = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("gui.stderr.log.1");
    let _ = std::fs::rename(path, bak);
}

fn chrono_lite_stamp() -> String {
    // Local wall time without pulling chrono into main — match daemon style.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Prefer libc localtime for a readable stamp.
    unsafe {
        let mut t = secs as libc::time_t;
        let tm = libc::localtime(&mut t);
        if tm.is_null() {
            return format!("unix:{secs}");
        }
        let tm = *tm;
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}
