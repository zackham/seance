mod agency;
mod agents;
mod app;
mod caps;
mod control;
mod ctl;
mod daemon;
mod desktop_notify;
mod scratchpad;
mod events;
mod cmdlog;
mod fileview;
mod gui_client;
mod host;
mod pane;
mod popout;
mod prompts;
mod remote_term;
mod remote_term_view;
mod runtime;
mod term_font;
mod state;
mod terminal;
mod terminal_view;
mod theme;

use gpui::*;
use gpui_component::Root;

use crate::app::SeanceApp;

fn main() {
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
                let Ok(pid) = pid_s.parse::<u32>() else { continue };
                if pid == self_pid {
                    continue;
                }
                // Skip daemon processes (cmdline contains "daemon").
                let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                    .unwrap_or_default()
                    .replace('\0', " ");
                if cmdline.contains("daemon") {
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

    // Ensure the session daemon is up, then open the GUI client.
    match daemon::ensure_daemon() {
        Ok(spawned) => {
            if spawned {
                eprintln!("[seance] started session daemon");
            } else {
                eprintln!("[seance] connected to session daemon");
            }
        }
        Err(e) => {
            eprintln!("[seance] daemon unavailable: {e:#}");
            std::process::exit(1);
        }
    }

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
        cx.activate(true);
    });
}
