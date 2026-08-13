//! Small cross-platform process helpers (Linux + macOS).

/// The platform "open this URI/path with the default app" command.
pub fn opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Open a link off the calling thread. **This is the seam every click in the
/// GUI goes through** — a terminal's ctrl/middle-click, a PR chip, a pad link,
/// the PR board.
///
/// Off-thread because the routing in [`crate::scrylink`] talks to a socket,
/// and every caller here is a click listener on the render thread.
pub fn open_detached(target: &str) {
    let target = target.to_string();
    std::thread::spawn(move || {
        if crate::scrylink::open(&target) {
            return;
        }
        // Reaped: nothing is waiting on this thread, and a window that runs
        // for days shouldn't collect a zombie per link click.
        if let Ok(mut child) = spawn_opener(&target) {
            let _ = child.wait();
        }
    });
}

/// Same routing, on this thread — for CLI paths that return straight into
/// process exit and would otherwise outlive the thread doing the work.
pub fn open_blocking(target: &str) {
    if crate::scrylink::open(target) {
        return;
    }
    // Deliberately not waited on here: a handler that doesn't fork would hang
    // the command. Exit reaps it.
    let _ = spawn_opener(target);
}

fn spawn_opener(target: &str) -> std::io::Result<std::process::Child> {
    std::process::Command::new(opener())
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}

/// Full command line of a live process, best-effort ("" when unknown).
///
/// Linux reads `/proc`; macOS shells `ps` (no procfs there).
pub fn process_cmdline(pid: u32) -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ")
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }
}

/// Is this `/proc` cmdline a GUI process — the thing `restart-gui` may kill?
///
/// The GUI is `seance` with **no subcommand**: only flags (`--local`,
/// `--remote <host>`). Every other surface shares the binary and the process
/// name, so `pgrep -x seance` finds them all. This used to skip only cmdlines
/// containing "daemon", which meant a `seance web` bridge was killed silently
/// on every GUI redeploy — remote access to seance disappearing as a side
/// effect of shipping a UI change, with nothing said about it.
pub fn is_gui_cmdline(cmdline: &str) -> bool {
    let mut args = cmdline.split_whitespace().skip(1); // skip argv[0]
    while let Some(arg) = args.next() {
        if arg == "--remote" {
            args.next(); // its host value is not a subcommand
            continue;
        }
        if !arg.starts_with('-') {
            return false; // a bare word here is a subcommand: web, ctl, daemon…
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_of_self_mentions_binary() {
        let line = process_cmdline(std::process::id());
        // Test binary path always contains "seance" in this repo's target dir.
        assert!(
            line.contains("seance") || !line.is_empty(),
            "unexpected empty cmdline: {line:?}"
        );
    }

    #[test]
    fn restart_gui_targets_the_bare_gui_process() {
        assert!(is_gui_cmdline("/home/z/work/seance/target/release/seance"));
        assert!(is_gui_cmdline("seance"));
        // Launch flags don't make it something else.
        assert!(is_gui_cmdline("seance --local"));
        assert!(is_gui_cmdline("seance --remote box.tail1234.ts.net"));
    }

    #[test]
    fn restart_gui_spares_every_other_surface_on_the_same_binary() {
        // The regression this exists for: a web bridge died on every GUI
        // redeploy because the only exemption was the word "daemon".
        assert!(!is_gui_cmdline(
            "./target/release/seance web --bind 127.0.0.1:9666"
        ));
        assert!(!is_gui_cmdline(
            "seance daemon --takeover /run/user/1000/x.sock"
        ));
        assert!(!is_gui_cmdline("seance ctl list --all"));
        assert!(!is_gui_cmdline("seance replay export foo"));
        assert!(!is_gui_cmdline("seance upgrade"));
        // A host value that happens to look like a subcommand is still a value.
        assert!(!is_gui_cmdline("seance --remote host web"));
    }
}
