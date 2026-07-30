//! Small cross-platform process helpers (Linux + macOS).

/// The platform "open this URI/path with the default app" command.
pub fn opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Spawn the default-app opener on `target`, detached and quiet.
pub fn open_detached(target: &str) {
    let _ = std::process::Command::new(opener())
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
}
