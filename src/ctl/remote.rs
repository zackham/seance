//! CLI host routing. Run client-side filesystem/agent operations beside the daemon.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::launch::{LaunchMode, LaunchPref};

pub(super) fn dispatch(args: &[String]) -> Result<()> {
    let bound = ["SEANCE_SOCKET", "SEANCE_SESSION"]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|v| !v.is_empty()));
    if bound || args.is_empty() || matches!(subcommand(args), "help" | "--help" | "-h" | "skill") {
        return Ok(());
    }
    let pref = crate::launch::try_load().context("read seance launch preference")?;
    let Some(host) = remote_host(pref.as_ref())? else {
        return Ok(());
    };
    let args = with_scope(args, std::env::var("SEANCE_WORKSPACE").ok().as_deref());
    let (args, body_file, stdin) = prepare_args(&args)?;
    let input = match body_file {
        Some(path) => Stdio::from(File::open(&path).with_context(|| format!("read {path}"))?),
        None if stdin => Stdio::inherit(),
        None => Stdio::null(),
    };
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "--",
        host,
        &remote_command(&args),
    ])
    .stdin(input);
    // exec preserves streaming output, exit codes, and cancellation without an orphan proxy.
    Err(cmd.exec()).with_context(|| format!("connect to saved host {host}"))
}

fn remote_host(pref: Option<&LaunchPref>) -> Result<Option<&str>> {
    let Some(pref) = pref.filter(|p| p.mode == LaunchMode::Remote) else {
        return Ok(None);
    };
    let host = pref.host.as_deref().unwrap_or("");
    if host.is_empty() || host.starts_with('-') || host.chars().any(char::is_whitespace) {
        bail!("invalid saved remote host; choose a host in seance or use `seance ctl --local …`");
    }
    Ok(Some(host))
}

fn subcommand(args: &[String]) -> &str {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--scope" => {
                it.next();
            }
            "--all" | "--json" => {}
            sub => return sub,
        }
    }
    ""
}

fn with_scope(args: &[String], scope: Option<&str>) -> Vec<String> {
    let mut forwarded = Vec::new();
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        forwarded.extend(["--scope".into(), scope.into()]);
    }
    forwarded.extend_from_slice(args);
    forwarded
}

fn prepare_args(args: &[String]) -> Result<(Vec<String>, Option<String>, bool)> {
    let sub = subcommand(args);
    let body_command = matches!(sub, "send" | "note" | "finish");
    let mut forwarded = Vec::new();
    let mut body_file = None;
    let mut stdin = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if body_command && arg == "--file" {
            body_file = Some(it.next().context("--file needs PATH")?.clone());
            forwarded.push("--stdin".into());
        } else {
            forwarded.push(arg.clone());
            // Values can themselves look like flags; preserve the ctl parser's boundaries.
            if arg == "--scope"
                || (matches!(sub, "note" | "finish") && arg == "--pane")
                || (sub == "finish" && matches!(arg.as_str(), "--note" | "--status" | "--task"))
            {
                if let Some(value) = it.next() {
                    forwarded.push(value.clone());
                }
            } else if body_command && arg == "--stdin" {
                stdin = true;
            }
        }
    }
    Ok((forwarded, body_file, stdin))
}

fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn remote_command(args: &[String]) -> String {
    // Noninteractive ssh doesn't load the user's shell PATH. Never interpolate payloads unquoted.
    format!(
        "PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\" exec seance ctl --local {}",
        args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn host_selection_does_not_fall_back_from_invalid_remote() {
        assert_eq!(remote_host(None).unwrap(), None);
        let mut pref = LaunchPref {
            mode: LaunchMode::Local,
            host: Some("desk".into()),
        };
        assert_eq!(remote_host(Some(&pref)).unwrap(), None);
        pref.mode = LaunchMode::Remote;
        assert_eq!(remote_host(Some(&pref)).unwrap(), Some("desk"));
        for bad in ["", "-oProxyCommand=whoami", "desk\nother"] {
            pref.host = Some(bad.into());
            assert!(remote_host(Some(&pref)).is_err());
        }
        pref.host = None;
        assert!(remote_host(Some(&pref)).is_err());
    }

    #[test]
    fn local_payload_files_become_stdin_but_viewer_paths_stay_remote() {
        for sub in ["send", "note", "finish"] {
            let (forwarded, file, _) = prepare_args(&args(&[
                "--scope",
                "circle",
                sub,
                "pane",
                "--file",
                "/Users/me/task.md",
                "--json",
            ]))
            .unwrap();
            assert_eq!(file.as_deref(), Some("/Users/me/task.md"));
            assert_eq!(
                forwarded,
                args(&["--scope", "circle", sub, "pane", "--stdin", "--json"])
            );
        }
        for original in [
            args(&["new", "--name", "notes", "--file", "/host/notes.md"]),
            args(&["wait", "pane", "--artifact", "/host/result.md"]),
        ] {
            let (forwarded, file, stdin) = prepare_args(&original).unwrap();
            assert_eq!(forwarded, original);
            assert!(file.is_none());
            assert!(!stdin);
        }
    }

    #[test]
    fn body_options_preserve_flag_shaped_values_and_file_precedence() {
        let (forwarded, file, stdin) = prepare_args(&args(&[
            "finish", "--note", "--file", "--pane", "pane", "--stdin", "--file", "first", "--file",
            "last",
        ]))
        .unwrap();
        assert_eq!(file.as_deref(), Some("last"));
        assert!(stdin);
        assert_eq!(
            forwarded,
            args(&[
                "finish", "--note", "--file", "--pane", "pane", "--stdin", "--stdin", "--stdin"
            ])
        );
        assert!(prepare_args(&args(&["send", "pane", "--file"])).is_err());
    }

    #[test]
    fn inherited_scope_precedes_explicit_overrides() {
        let original = args(&["--scope", "chosen", "roster", "--all"]);
        assert_eq!(
            with_scope(&original, Some("inherited")),
            args(&[
                "--scope",
                "inherited",
                "--scope",
                "chosen",
                "roster",
                "--all"
            ])
        );
        assert_eq!(with_scope(&original, Some("")), original);
    }

    #[test]
    fn shell_roundtrip_preserves_literal_arguments() {
        let original = args(&[
            "",
            "spaces and 'quotes'",
            "$HOME $(printf WRONG) `printf WRONG`",
            "a;printf WRONG",
            "line one\nline two",
            "🕯",
        ]);
        let script = format!(
            "printf '%s\\0' {}",
            original
                .iter()
                .map(|a| quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let output = Command::new("sh").args(["-c", &script]).output().unwrap();
        assert!(output.status.success());
        let expected = original.join("\0") + "\0";
        assert_eq!(output.stdout, expected.as_bytes());
    }
}
