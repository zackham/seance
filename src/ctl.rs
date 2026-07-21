//! `seance ctl …` — the control-plane CLI client.
//!
//! A thin, dependency-free command-line front end over the Unix-socket protocol
//! defined in [`crate::control`]. It builds one [`ControlRequest`], opens the
//! socket, writes a single JSON line, reads a single response line, and prints
//! the result — human-readable by default, raw JSON with `--json`.
//!
//! This is what a **pane in the circle** (any agent or shell inside seance
//! pane) shells out to in order to drive its sibling sessions:
//!
//! ```text
//! seance ctl new  --name worker-1 --cwd ~/proj --command claude
//! seance ctl send worker-1 run the full test suite and report failures
//! seance ctl read worker-1 --lines 40
//! seance ctl scratchpad worker-1
//! ```
//!
//! # Exit codes
//! - `0` — the app answered `ok: true`.
//! - `1` — the app answered `ok: false` (the error is printed to stderr).
//! - `2` — could not connect to the socket (with an "is seance running?" hint).
//!
//! Arguments are hand-parsed; no `clap`, no new dependencies. The only cross-
//! module dependency is on [`crate::control`]'s request/response types and
//! [`crate::control::socket_path`].

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::control::{socket_path, ControlRequest, ControlResponse};

/// Entry point wired from `main` when argv[1] is `ctl`.
///
/// `args` is everything *after* `ctl` (the subcommand and its flags). Returns
/// the process exit code — the caller should `std::process::exit(run_ctl(...))`.
pub fn run_ctl(args: Vec<String>) -> i32 {
    // Global flags may appear anywhere; pull them out first so subcommand
    // parsers don't have to each account for them.
    //   --json       raw JSON output
    //   --all        drop workspace scoping (see below)
    //   --scope WS   act as if scoped to workspace WS
    let mut json_out = false;
    let mut all_flag = false;
    let mut scope_override: Option<String> = None;
    let mut rest: Vec<String> = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json_out = true,
            "--all" => all_flag = true,
            "--scope" => match it.next() {
                Some(ws) => scope_override = Some(ws),
                None => {
                    eprintln!("seance ctl: --scope requires a workspace name");
                    return 1;
                }
            },
            _ => rest.push(a),
        }
    }

    // Workspace scoping: a ctl run INSIDE a seance pane inherits that pane's
    // workspace via $SEANCE_WORKSPACE and only sees/affects its own workspace.
    // `--all` lifts the scope; `--scope WS` targets another workspace
    // explicitly. Callers outside seance are unscoped by default.
    let scope: Option<String> = if all_flag {
        None
    } else {
        scope_override.or_else(|| std::env::var("SEANCE_WORKSPACE").ok().filter(|s| !s.is_empty()))
    };
    let from: Option<String> = std::env::var("SEANCE_SESSION").ok().filter(|s| !s.is_empty());

    let mut it = rest.into_iter();
    let sub = match it.next() {
        Some(s) => s,
        None => {
            print_help();
            return 1;
        }
    };
    let mut sub_args: Vec<String> = it.collect();
    // Client-side flag: after successful `new`, block until agent boot-ready.
    let wait_ready = sub_args.iter().any(|a| a == "--wait-ready");
    if wait_ready {
        sub_args.retain(|a| a != "--wait-ready");
    }
    let scratch_cat = sub == "scratchpad" || sub == "pad";
    let scratch_cat = scratch_cat && sub_args.iter().any(|a| a == "--cat");
    if scratch_cat {
        sub_args.retain(|a| a != "--cat");
    }

    // Build the request (or handle help / a parse error).
    let request = match sub.as_str() {
        "help" | "-h" | "--help" => {
            print_help();
            return 0;
        }
        "skill" => {
            print!("{SKILL_TEXT}");
            return 0;
        }
        "list" | "ls" => Ok(ControlRequest::List { scope: None, from: None }),
        "new" => parse_new(sub_args),
        "send" => parse_send(sub_args),
        "send-raw" | "raw" => parse_send_raw(sub_args),
        "read" => parse_read(sub_args),
        "status" => parse_status(sub_args),
        "kill" => parse_kill(sub_args),
        "scratchpad" | "pad" => parse_scratchpad(sub_args),
        "timeline" | "tl" => parse_timeline(sub_args),
        "status-set" => parse_status_set(sub_args),
        "ask" => parse_ask(sub_args),
        "propose" => parse_propose(sub_args),
        "propose-result" => match sub_args.first() {
            Some(id) => Ok(ControlRequest::ProposeResult { id: id.clone(), scope: None, from: None }),
            None => Err("propose-result: expected PROPOSAL_ID".into()),
        },
        "human" | "whereis-human" => Ok(ControlRequest::Human { scope: None, from: None }),
        "fork" => parse_fork(sub_args),
        "cmd-begin" => {
            let mut cwd = None;
            let mut words: Vec<String> = Vec::new();
            let mut it = sub_args.into_iter();
            while let Some(a) = it.next() {
                if a == "--cwd" {
                    cwd = it.next();
                } else {
                    words.push(a);
                }
            }
            if words.is_empty() {
                Err("cmd-begin: expected COMMAND".into())
            } else {
                Ok(ControlRequest::CmdBegin {
                    command: words.join(" "),
                    cwd: cwd.or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .map(|p| p.to_string_lossy().to_string())
                    }),
                    scope: None,
                    from: None,
                })
            }
        }
        "cmd-end" => match sub_args.first().and_then(|v| v.parse::<i32>().ok()) {
            Some(exit) => Ok(ControlRequest::CmdEnd { exit, scope: None, from: None }),
            None => Err("cmd-end: expected EXIT_CODE".into()),
        },
        "commands" => {
            let mut limit = None;
            let mut pane = None;
            let mut it = sub_args.into_iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--limit" => limit = it.next().and_then(|v| v.parse().ok()),
                    _ => pane = Some(a),
                }
            }
            match pane {
                Some(pane) => Ok(ControlRequest::Commands { pane, limit, scope: None, from: None }),
                None => Err("commands: expected PANE".into()),
            }
        }
        "last-command" => {
            let mut failed_only = false;
            let mut pane = None;
            for a in sub_args {
                if a == "--failed" {
                    failed_only = true;
                } else {
                    pane = Some(a);
                }
            }
            match pane {
                Some(pane) => Ok(ControlRequest::LastCommand { pane, failed_only, scope: None, from: None }),
                None => Err("last-command: expected PANE".into()),
            }
        }
        "ask-result" => match sub_args.first() {
            Some(id) => Ok(ControlRequest::AskResult { id: id.clone(), scope: None, from: None }),
            None => Err("ask-result: expected ASK_ID".into()),
        },
        "watch" => parse_watch(sub_args),
        "whoami" => Ok(ControlRequest::Whoami {
            scope: None,
            from: None,
        }),
        "caps" => Ok(ControlRequest::Caps {
            scope: None,
            from: None,
        }),
        "grant" => parse_grant(sub_args),
        "revoke" => parse_revoke(sub_args),
        "policy" => parse_policy(sub_args),
        "seize" => parse_seize(sub_args),
        "release" => parse_release(sub_args),
        "drive" | "drive-mode" => parse_drive(sub_args),
        "doctor" => Ok(ControlRequest::Doctor { scope: None, from: None }),
        "brief" => Ok(ControlRequest::Brief { scope: None, from: None }),
        "roster" | "stage" => Ok(ControlRequest::Roster { scope: None, from: None }),
        "task" | "inbox" => parse_task(sub_args),
        "note" => parse_note(sub_args),
        "finish" => parse_finish(sub_args),
        "wait" => {
            // Client-side wait loop (not a single control op).
            return run_wait(sub_args, scope, from, json_out);
        }
        // Convenience: fan-in wait --status done --cat
        "harvest" => {
            let mut args = sub_args;
            if !args.iter().any(|a| a == "--status") {
                args.push("--status".into());
                args.push("done".into());
            }
            if !args.iter().any(|a| a == "--cat" || a == "--harvest") {
                args.push("--cat".into());
            }
            return run_wait(args, scope, from, json_out);
        }
        // seance ↔ vita seam: open a telegram topic for a pane (zero telegram
        // protocol inside seance — shells out to vita capabilities).
        "phone" | "telegram-topic" | "tg" => {
            return run_phone(sub_args, scope, from, json_out);
        }
        "export-session" | "export" => {
            return run_export_session(sub_args, scope, json_out);
        }
        "prompts" => {
            return run_prompts(sub_args, json_out);
        }
        other => Err(format!("unknown subcommand '{other}' (try `seance ctl help`)")),
    };

    let request = match request {
        Ok(r) => with_identity(r, scope.clone(), from.clone()),
        Err(msg) if msg.starts_with("__help__\n") => {
            print!("{}", msg.trim_start_matches("__help__\n"));
            if !msg.ends_with('\n') {
                println!();
            }
            return 0;
        }
        Err(msg) => {
            eprintln!("seance ctl: {msg}");
            return 1;
        }
    };

    // Streaming watch — special connection lifecycle.
    if matches!(request, ControlRequest::Watch { .. }) {
        return run_watch(&request, json_out);
    }

    // Round-trip it over the socket.
    let response = match send_request(&request) {
        Ok(r) => r,
        Err(ConnectError::Connect(e)) => {
            eprintln!(
                "seance ctl: could not connect to control socket at {} ({e}).\n\
                 is seance running? start the app, then retry.",
                socket_path().display()
            );
            return 2;
        }
        Err(ConnectError::Io(e)) => {
            eprintln!("seance ctl: control IO error: {e}");
            return 2;
        }
        Err(ConnectError::Protocol(e)) => {
            eprintln!("seance ctl: bad response from seance: {e}");
            return 1;
        }
    };

    // Print + exit.
    if json_out {
        // Re-serialize the parsed response for a stable, single-line shape.
        match serde_json::to_string(&response) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("seance ctl: failed to render json: {e}");
                return 1;
            }
        }
        return if response.ok { 0 } else { 1 };
    }

    // `propose` blocks: poll propose-result until the human resolves it.
    if sub == "propose" && response.ok && !json_out {
        if let Some(id) = response
            .data
            .as_ref()
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str())
        {
            let timeout_secs: u64 = std::env::var("SEANCE_ASK_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600);
            let started = std::time::Instant::now();
            loop {
                if started.elapsed().as_secs() > timeout_secs {
                    eprintln!("seance ctl: proposal unresolved after {timeout_secs}s (id {id})");
                    return 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(1200));
                let poll = ControlRequest::ProposeResult {
                    id: id.to_string(),
                    scope: None,
                    from: None,
                };
                if let Ok(r) = send_request(&poll) {
                    if r.ok {
                        let resolved = r
                            .data
                            .as_ref()
                            .and_then(|d| d.get("resolved"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if resolved {
                            let outcome = r
                                .data
                                .as_ref()
                                .and_then(|d| d.get("outcome"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            println!("{outcome}");
                            return if outcome == "accepted" { 0 } else { 1 };
                        }
                    }
                }
            }
        }
    }

    // `ask` blocks: submit returned an id; poll ask-result until answered.
    if sub == "ask" && response.ok && !json_out {
        if let Some(id) = response
            .data
            .as_ref()
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str())
        {
            let timeout_secs: u64 = std::env::var("SEANCE_ASK_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600);
            let started = std::time::Instant::now();
            loop {
                if started.elapsed().as_secs() > timeout_secs {
                    eprintln!("seance ctl: ask timed out after {timeout_secs}s (id {id})");
                    return 1;
                }
                std::thread::sleep(std::time::Duration::from_millis(1200));
                let poll = ControlRequest::AskResult {
                    id: id.to_string(),
                    scope: None,
                    from: None,
                };
                match send_request(&poll) {
                    Ok(r) if r.ok => {
                        let answered = r
                            .data
                            .as_ref()
                            .and_then(|d| d.get("answered"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if answered {
                            let answer = r
                                .data
                                .as_ref()
                                .and_then(|d| d.get("answer"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            println!("{answer}");
                            return 0;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if response.ok {
        // pad --cat: body only (no path line) — one-hop verify.
        if scratch_cat {
            let path = response
                .data
                .as_ref()
                .and_then(|d| {
                    d.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| d.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()))
                });
            if let Some(path) = path {
                match std::fs::read_to_string(&path) {
                    Ok(body) => {
                        print!("{body}");
                        if !body.ends_with('\n') {
                            println!();
                        }
                    }
                    Err(e) => {
                        eprintln!("seance ctl: scratchpad --cat: {e}");
                        return 1;
                    }
                }
            }
        } else if !json_out {
            print_ok_human(&sub, &response);
        }
        // A+ orchestrator: block until agent TUI is ready for inject.
        if sub == "new" && wait_ready {
            let slug = response
                .data
                .as_ref()
                .and_then(|d| {
                    d.get("slug")
                        .and_then(|v| v.as_str())
                        .or_else(|| d.get("name").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                        .or_else(|| d.as_str().map(|s| s.to_string()))
                })
                .unwrap_or_default();
            let command = response
                .data
                .as_ref()
                .and_then(|d| d.get("command").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            if !slug.is_empty() {
                if !json_out {
                    eprintln!("waiting until '{slug}' is boot-ready…");
                }
                let code = run_wait(
                    vec![
                        slug.clone(),
                        "--ready".into(),
                        "--timeout".into(),
                        "120".into(),
                    ],
                    scope.clone(),
                    from.clone(),
                    json_out,
                );
                if code != 0 {
                    return code;
                }
                // Profile boot-clear (trust dialog / update skip).
                boot_clear_pane(&slug, &command, scope.clone(), from.clone(), json_out);
            }
        }
        0
    } else {
        eprintln!(
            "seance ctl: {}",
            response.error.as_deref().unwrap_or("request failed")
        );
        1
    }
}


/// `timeline [--since 10m|2h|30s] [--pane P] [--actor A] [--limit N]`
fn parse_timeline(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut since_secs = None;
    let mut pane = None;
    let mut actor = None;
    let mut limit = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since" => {
                let v = it.next().ok_or("timeline: --since needs a value")?;
                since_secs = Some(parse_duration_secs(&v)?);
            }
            "--pane" => pane = Some(it.next().ok_or("timeline: --pane needs a value")?),
            "--actor" => actor = Some(it.next().ok_or("timeline: --actor needs a value")?),
            "--limit" => {
                limit = Some(
                    it.next()
                        .ok_or("timeline: --limit needs a value")?
                        .parse()
                        .map_err(|_| "timeline: bad --limit")?,
                )
            }
            other => return Err(format!("timeline: unknown arg '{other}'")),
        }
    }
    Ok(ControlRequest::Timeline { since_secs, pane, actor, limit, scope: None, from: None })
}

fn parse_duration_secs(v: &str) -> Result<u64, String> {
    let (num, mult) = match v.chars().last() {
        Some('s') => (&v[..v.len() - 1], 1),
        Some('m') => (&v[..v.len() - 1], 60),
        Some('h') => (&v[..v.len() - 1], 3600),
        Some('d') => (&v[..v.len() - 1], 86400),
        _ => (v, 1),
    };
    num.parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("bad duration '{v}' (use 30s/10m/2h)"))
}

/// `status-set STATE [NOTE...] [--pane P]`
fn parse_status_set(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pane" => pane = Some(it.next().ok_or("status-set: --pane needs a value")?),
            _ => positionals.push(a),
        }
    }
    if positionals.is_empty() {
        return Err("status-set: expected STATE (planning|working|blocked|needs-human|done|idle)".into());
    }
    let state = positionals.remove(0);
    let note = if positionals.is_empty() {
        None
    } else {
        Some(positionals.join(" "))
    };
    Ok(ControlRequest::StatusSet { state, note, pane, scope: None, from: None })
}

/// `propose PANE TEXT... [--reason R]`
fn parse_propose(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut reason = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--reason" => reason = Some(it.next().ok_or("propose: --reason needs a value")?),
            _ => positionals.push(a),
        }
    }
    if positionals.len() < 2 {
        return Err("propose: expected PANE TEXT...".into());
    }
    let pane = positionals.remove(0);
    Ok(ControlRequest::Propose {
        pane,
        text: positionals.join(" "),
        reason,
        scope: None,
        from: None,
    })
}

/// `fork [--workspace W] [--name N]`
fn parse_fork(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut workspace = None;
    let mut name = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" => workspace = Some(it.next().ok_or("fork: --workspace needs a value")?),
            "--name" => name = Some(it.next().ok_or("fork: --name needs a value")?),
            other => return Err(format!("fork: unknown arg '{other}'")),
        }
    }
    Ok(ControlRequest::WorkspaceFork { workspace, name, scope: None, from: None })
}

/// `ask QUESTION... [--choices a,b,c]`
fn parse_ask(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut choices = None;
    let mut words: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--choices" => {
                let v = it.next().ok_or("ask: --choices needs a,b,c")?;
                choices = Some(v.split(',').map(|s| s.trim().to_string()).collect());
            }
            _ => words.push(a),
        }
    }
    if words.is_empty() {
        return Err("ask: expected a question".into());
    }
    Ok(ControlRequest::Ask { question: words.join(" "), choices, scope: None, from: None })
}

/// Stamp the caller's workspace scope + identity onto a parsed request.
fn with_identity(
    request: ControlRequest,
    scope: Option<String>,
    from: Option<String>,
) -> ControlRequest {
    use ControlRequest::*;
    match request {
        List { .. } => List { scope, from },
        New { name, cwd, command, workspace, file, .. } => New { name, cwd, command, workspace, file, scope, from },
        Send { pane, text, submit, force, .. } => Send { pane, text, submit, force, scope, from },
        SendRaw { pane, bytes_b64, force, .. } => SendRaw { pane, bytes_b64, force, scope, from },
        Read { pane, lines, .. } => Read { pane, lines, scope, from },
        Status { pane, .. } => Status { pane, scope, from },
        Kill { pane, .. } => Kill { pane, scope, from },
        Scratchpad { pane, .. } => Scratchpad { pane, scope, from },
        Propose { pane, text, reason, .. } => Propose { pane, text, reason, scope, from },
        ProposeResult { id, .. } => ProposeResult { id, scope, from },
        Human { .. } => Human { scope, from },
        CmdBegin { command, cwd, .. } => CmdBegin { command, cwd, scope, from },
        CmdEnd { exit, .. } => CmdEnd { exit, scope, from },
        Commands { pane, limit, .. } => Commands { pane, limit, scope, from },
        LastCommand { pane, failed_only, .. } => LastCommand { pane, failed_only, scope, from },
        WorkspaceFork { workspace, name, .. } => WorkspaceFork { workspace, name, scope, from },
        Timeline { since_secs, pane, actor, limit, .. } => Timeline { since_secs, pane, actor, limit, scope, from },
        StatusSet { state, note, pane, .. } => StatusSet { state, note, pane, scope, from },
        Ask { question, choices, .. } => Ask { question, choices, scope, from },
        AskResult { id, .. } => AskResult { id, scope, from },
        Watch {
            since_seq,
            kinds,
            pane,
            actor,
            catch_up,
            ..
        } => Watch {
            since_seq,
            kinds,
            pane,
            actor,
            catch_up,
            scope,
            from,
        },
        Whoami { .. } => Whoami { scope, from },
        Caps { .. } => Caps { scope, from },
        CapsGrant {
            principal,
            cap,
            workspace,
            ttl_secs,
            ..
        } => CapsGrant {
            principal,
            cap,
            workspace,
            ttl_secs,
            scope,
            from,
        },
        CapsRevoke {
            principal,
            cap,
            workspace,
            ..
        } => CapsRevoke {
            principal,
            cap,
            workspace,
            scope,
            from,
        },
        PolicyGet { workspace, .. } => PolicyGet {
            workspace,
            scope,
            from,
        },
        PolicySet {
            mode, workspace, ..
        } => PolicySet {
            mode,
            workspace,
            scope,
            from,
        },
        Seize { pane, as_owner, .. } => Seize { pane, as_owner, scope, from },
        Release { pane, .. } => Release { pane, scope, from },
        DriveMode { pane, mode, .. } => DriveMode { pane, mode, scope, from },
        Doctor { .. } => Doctor { scope, from },
        Brief { .. } => Brief { scope, from },
        Roster { .. } => Roster { scope, from },
        Note {
            pane,
            text,
            append,
            ..
        } => Note {
            pane,
            text,
            append,
            scope,
            from,
        },
        Finish {
            pane,
            body,
            append,
            status,
            status_note,
            empty_ok,
            task,
            ..
        } => Finish {
            pane,
            body,
            append,
            status,
            status_note,
            empty_ok,
            task,
            scope,
            from,
        },
        Task {
            pane,
            id,
            ..
        } => Task {
            pane,
            id,
            scope,
            from,
        },
    }
}

// ---------------------------------------------------------------------------
// Subcommand parsers

// ---------------------------------------------------------------------------

/// `new --name NAME [--cwd DIR] [--command CMD] [--workspace WS]`
fn parse_new(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut name: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut command: Option<String> = None;
    let mut workspace: Option<String> = None;
    let mut file: Option<String> = None;
    let mut agent: Option<String> = None;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => name = Some(take_value(&mut it, "--name")?),
            "--cwd" => cwd = Some(take_value(&mut it, "--cwd")?),
            "--command" => command = Some(take_value(&mut it, "--command")?),
            "--workspace" => workspace = Some(take_value(&mut it, "--workspace")?),
            "--file" => file = Some(take_value(&mut it, "--file")?),
            "--agent" => agent = Some(take_value(&mut it, "--agent")?),
            other => return Err(format!("new: unexpected argument '{other}'")),
        }
    }

    let name = name.ok_or("new: --name is required")?;
    if let Some(a) = agent {
        if command.is_some() {
            return Err("new: use either --agent or --command, not both".into());
        }
        let profile = crate::agents::resolve(&a)?;
        command = Some(crate::agents::command_line(&profile));
    }
    Ok(ControlRequest::New {
        name,
        cwd,
        command,
        workspace,
        file,
        scope: None,
        from: None,
    })
}

/// `send SESSION TEXT... [--no-submit]`
///
/// The pane is the first positional; everything else that isn't a flag is
/// joined with spaces into the text (so you don't have to quote the prompt).
fn parse_send(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut submit = true;
    let mut force = false;
    let mut file: Option<String> = None;
    let mut stdin = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-submit" => submit = false,
            "--submit" => submit = true,
            "--force" => force = true,
            "--file" => file = Some(it.next().ok_or("send: --file needs PATH")?),
            "--stdin" => stdin = true,
            "--help" | "-h" => {
                return Err(
                    "__help__\nsend PANE TEXT... | send PANE --file PATH | send PANE --stdin\n  \
                     --no-submit  --force\n  \
                     IMPORTANT: shell expands $VARS in TEXT — use --file for verbatim payloads"
                        .into(),
                );
            }
            other => positionals.push(other.to_string()),
        }
    }

    if positionals.is_empty() {
        return Err("send: expected SESSION (and TEXT, or --file/--stdin)".into());
    }
    let pane = positionals.remove(0);
    let text = if let Some(path) = file {
        std::fs::read_to_string(&path).map_err(|e| format!("send: read {path}: {e}"))?
    } else if stdin {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("send: stdin: {e}"))?;
        buf
    } else {
        if positionals.is_empty() {
            return Err("send: expected TEXT after pane (or --file/--stdin)".into());
        }
        positionals.join(" ")
    };

    Ok(ControlRequest::Send {
        pane,
        text,
        submit,
        force,
        scope: None,
        from: None,
    })
}

fn parse_note(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane = None;
    let mut append = true;
    let mut file = None;
    let mut words: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pane" => pane = Some(it.next().ok_or("note: --pane needs value")?),
            "--replace" => append = false,
            "--file" => file = Some(it.next().ok_or("note: --file needs PATH")?),
            other if pane.is_none()
                && !other.starts_with('-')
                && !other.contains('/')
                && words.is_empty()
                && std::env::var("SEANCE_SESSION").is_err() =>
            {
                // first bare token as pane when outside seance
                pane = Some(other.to_string());
            }
            other => words.push(other.to_string()),
        }
    }
    // If first word looks like pane id when multiple words
    if pane.is_none() && !words.is_empty() && words[0].chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        // could be pane or text — if from is set, text only
        if std::env::var("SEANCE_SESSION").ok().filter(|s| !s.is_empty()).is_none() {
            pane = Some(words.remove(0));
        }
    }
    let text = if let Some(path) = file {
        std::fs::read_to_string(&path).map_err(|e| format!("note: {e}"))?
    } else {
        words.join(" ")
    };
    if text.is_empty() {
        return Err("note: expected TEXT or --file PATH".into());
    }
    Ok(ControlRequest::Note {
        pane,
        text,
        append,
        scope: None,
        from: None,
    })
}

fn parse_finish(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane = None;
    let mut body_file = None;
    let mut body = None;
    let mut append = true;
    let mut status = "done".to_string();
    let mut status_note = None;
    let mut empty_ok = false;
    let mut stdin = false;
    let mut task = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pane" => pane = Some(it.next().ok_or("finish: --pane needs value")?),
            "--file" => body_file = Some(it.next().ok_or("finish: --file needs PATH")?),
            "--stdin" => stdin = true,
            "--replace" => append = false,
            "--status" => status = it.next().ok_or("finish: --status needs value")?,
            "--note" => status_note = Some(it.next().ok_or("finish: --note needs value")?),
            "--task" => task = Some(it.next().ok_or("finish: --task needs TASK_ID")?),
            "--empty-ok" => empty_ok = true,
            "--help" | "-h" => {
                return Err(
                    "__help__\nfinish [--pane P] [--file PATH | --stdin] [--status done] [--note N] [--task ID] [--empty-ok]\n  \
                     Writes scratchpad (via daemon) + status-set. status=done requires a body unless --empty-ok.\n  \
                     Prefer --stdin/--file so shell never expands $VARS."
                        .into(),
                );
            }
            // When inside a seance pane, first bare token is body text (not pane).
            // Outside, first bare token can be pane if it looks like a slug.
            other if pane.is_none()
                && !other.starts_with('-')
                && std::env::var("SEANCE_SESSION").ok().filter(|s| !s.is_empty()).is_none()
                && other
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_') =>
            {
                pane = Some(other.to_string());
            }
            other => {
                body = Some(match body {
                    None => other.to_string(),
                    Some(b) => format!("{b} {other}"),
                });
            }
        }
    }
    if let Some(path) = body_file {
        body = Some(std::fs::read_to_string(&path).map_err(|e| format!("finish: {e}"))?);
    } else if stdin {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("finish: stdin: {e}"))?;
        body = Some(buf);
    }
    Ok(ControlRequest::Finish {
        pane,
        body,
        append,
        status,
        status_note,
        empty_ok,
        task,
        scope: None,
        from: None,
    })
}

/// `send-raw SESSION BYTES` — BYTES is interpreted as a UTF-8 string and
/// base64-encoded for transport. Use shell escapes for control chars, e.g.
/// `send-raw w $'\x03'` for Ctrl-C, `send-raw w $'\r'` for a bare Enter.
fn parse_send_raw(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut force = false;
    let mut positionals: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--force" => force = true,
            other => positionals.push(other.to_string()),
        }
    }
    if positionals.len() < 2 {
        return Err(
            "send-raw: expected SESSION and BYTES (e.g. `send-raw worker-1 $'\\x03'` for Ctrl-C)"
                .into(),
        );
    }
    let pane = positionals.remove(0);
    let raw = positionals.join(" ");
    let bytes_b64 = base64_encode(raw.as_bytes());
    Ok(ControlRequest::SendRaw {
        pane,
        bytes_b64,
        force,
        scope: None,
        from: None,
    })
}

/// `read SESSION [--lines N]`
fn parse_read(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane: Option<String> = None;
    let mut lines: Option<usize> = None;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--lines" | "-n" => {
                let v = take_value(&mut it, "--lines")?;
                let n: usize = v
                    .parse()
                    .map_err(|_| format!("read: --lines expects a number, got '{v}'"))?;
                lines = Some(n);
            }
            other if other.starts_with('-') => {
                return Err(format!("read: unexpected flag '{other}'"));
            }
            other => {
                if pane.is_some() {
                    return Err(format!("read: unexpected argument '{other}'"));
                }
                pane = Some(other.to_string());
            }
        }
    }

    let pane = pane.ok_or("read: expected SESSION")?;
    Ok(ControlRequest::Read { pane, lines, scope: None, from: None })
}

/// `status SESSION`
fn parse_status(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "status")?;
    Ok(ControlRequest::Status { pane, scope: None, from: None })
}

/// `kill SESSION`
fn parse_kill(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "kill")?;
    Ok(ControlRequest::Kill { pane, scope: None, from: None })
}

/// `scratchpad [SESSION]` — defaults to `$SEANCE_SESSION` when unset.
fn parse_scratchpad(args: Vec<String>) -> Result<ControlRequest, String> {
    // --cat handled in run_ctl after response (client-side read of path).
    let filtered: Vec<String> = args.into_iter().filter(|a| a != "--cat").collect();
    let pane = if filtered.is_empty() {
        std::env::var("SEANCE_SESSION")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "scratchpad: expected SESSION (or set $SEANCE_SESSION inside a pane)".to_string()
            })?
    } else {
        single_positional(filtered, "scratchpad")?
    };
    Ok(ControlRequest::Scratchpad { pane, scope: None, from: None })
}

/// `task [--id ID] [PANE]` — durable inject inbox / task envelope.
fn parse_task(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane = None;
    let mut id = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--id" | "--task" => id = Some(it.next().ok_or("task: --id needs value")?),
            "--help" | "-h" => {
                return Err(
                    "__help__\ntask [--id TASK_ID] [PANE]\n  \
                     Show active inject inbox for a pane (default $SEANCE_SESSION).\n  \
                     Body is the durable text from the last `send`."
                        .into(),
                );
            }
            other if !other.starts_with('-') => pane = Some(other.to_string()),
            other => return Err(format!("task: unexpected '{other}'")),
        }
    }
    Ok(ControlRequest::Task {
        pane,
        id,
        scope: None,
        from: None,
    })
}

fn parse_seize(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane = None;
    let mut as_owner = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--as" => as_owner = Some(take_value(&mut it, "--as")?),
            other if pane.is_none() => pane = Some(other.to_string()),
            other => return Err(format!("seize: unexpected '{other}'")),
        }
    }
    Ok(ControlRequest::Seize {
        pane: pane.ok_or("seize: expected PANE")?,
        as_owner,
        scope: None,
        from: None,
    })
}

fn parse_release(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "release")?;
    Ok(ControlRequest::Release {
        pane,
        scope: None,
        from: None,
    })
}

fn parse_drive(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut pane = None;
    let mut mode = None;
    for a in args {
        if pane.is_none() && !a.starts_with('-') {
            pane = Some(a);
        } else if mode.is_none() {
            mode = Some(a);
        }
    }
    Ok(ControlRequest::DriveMode {
        pane: pane.ok_or("drive: expected PANE")?,
        mode: mode.ok_or("drive: expected MODE (pair|locked_human|agent_led)")?,
        scope: None,
        from: None,
    })
}

/// Client-side wait: poll human/list/read until condition.

/// Client-side wait — orchestrator A+ primitive. Prefer this over `read` loops.
///
/// ```text
/// seance ctl wait PANE --status done --timeout 300
/// seance ctl wait PANE --scratchpad --min-bytes 500
/// seance ctl wait PANE --artifact /path/to/out.md --min-bytes 100
/// seance ctl wait PANE --owner none
/// seance ctl wait PANE --ready
/// seance ctl wait --any w1 w2 w3 --status done
/// ```
fn run_wait(
    args: Vec<String>,
    scope: Option<String>,
    from: Option<String>,
    json_out: bool,
) -> i32 {
    let mut panes: Vec<String> = Vec::new();
    let mut any = false;
    let mut owner: Option<String> = None;
    let mut status: Option<String> = None;
    let mut contains: Option<String> = None;
    let mut artifact: Option<String> = None;
    let mut scratchpad = false;
    let mut min_bytes: u64 = 1;
    let mut ready = false;
    let mut stable_ms: u64 = 1200;
    let mut timeout_secs: u64 = 300;
    // Default true when --scratchpad: require growth since last inject.
    let mut since_inject = true;
    // --status done requires pad evidence by default (0.9.6); --badge-only skips.
    let mut evidence = true;
    // After success, dump each satisfied pane's pad body (master harvest path).
    let mut cat_pads = false;
    let mut task_id: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--any" => any = true,
            "--owner" => owner = it.next(),
            "--status" => status = it.next(),
            "--contains" => contains = it.next(),
            "--artifact" => artifact = it.next(),
            "--scratchpad" | "--pad" => scratchpad = true,
            "--since-inject" => since_inject = true,
            "--any-pad" => since_inject = false, // absolute pad size (legacy)
            "--badge-only" => evidence = false,
            "--cat" | "--harvest" => cat_pads = true,
            "--task" => task_id = it.next(),
            "--min-bytes" => {
                min_bytes = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
            }
            "--ready" => ready = true,
            "--stable-ms" => {
                stable_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1200);
            }
            "--timeout" => {
                timeout_secs = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300);
            }
            "--help" | "-h" => {
                println!(
                    "wait PANE [PANE...] [opts]\n  \
                     --status done   (default: also require pad growth since inject)\n  \
                     --badge-only    status badge only (legacy / no evidence)\n  \
                     --cat|--harvest print each pad body after success (fan-in harvest)\n  \
                     --task ID       wait until that task_id is done\n  \
                     --scratchpad [--since-inject|--any-pad] --min-bytes N\n  \
                     --artifact PATH  --owner none  --ready  --any  --timeout S"
                );
                return 0;
            }
            other if !other.starts_with('-') => panes.push(other.to_string()),
            other => {
                eprintln!("seance ctl wait: unexpected '{other}'");
                return 1;
            }
        }
    }

    if panes.is_empty() {
        eprintln!(
            "seance ctl wait: expected PANE [PANE...] [--status done] [--scratchpad] [--artifact PATH] [--owner none] [--ready] [--timeout S]"
        );
        return 1;
    }
    if !any && panes.len() > 1 {
        // multiple panes without --any means wait-all
    }

    let started = std::time::Instant::now();
    let mut last_screens: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut stable_since: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();

    // Event-driven wake: background watch thread pokes a channel so we re-check
    // brief immediately on status/finish/pad activity (still fall back to poll).
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
    {
        let panes_for_watch = panes.clone();
        std::thread::spawn(move || {
            watch_wake_loop(panes_for_watch, wake_tx);
        });
    }
    let mut last_rows: Vec<serde_json::Value> = Vec::new();

    loop {
        if started.elapsed().as_secs() > timeout_secs {
            // Timeout diagnostics: last roster rows + pad_rev deltas.
            if json_out {
                println!(
                    "{{\"ok\":false,\"error\":\"timeout\",\"timeout_secs\":{timeout_secs},\"panes\":{},\"last\":{}}}",
                    serde_json::to_string(&panes).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(&last_rows).unwrap_or_else(|_| "[]".into())
                );
            } else {
                eprintln!(
                    "seance ctl wait: timeout after {timeout_secs}s on {}",
                    panes.join(",")
                );
                for row in &last_rows {
                    let slug = row.get("slug").and_then(|v| v.as_str()).unwrap_or("?");
                    let st = row.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                    let rev = row.get("pad_rev").and_then(|v| v.as_u64()).unwrap_or(0);
                    let inj = row.get("inject_pad_rev").and_then(|v| v.as_u64());
                    let bytes = row
                        .get("scratchpad_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let tid = row.get("task_id").and_then(|v| v.as_str()).unwrap_or("-");
                    eprintln!(
                        "  {slug}: status={st} pad={bytes}B@r{rev} inj={inj:?} task={tid}"
                    );
                }
            }
            return 1;
        }

        // One dense brief for all panes (token-cheap).
        let brief_req = with_identity(
            ControlRequest::Brief {
                scope: None,
                from: None,
            },
            scope.clone(),
            from.clone(),
        );
        let brief = match send_request(&brief_req) {
            Ok(r) if r.ok => r,
            Ok(r) => {
                eprintln!("seance ctl wait: {}", r.error.unwrap_or_else(|| "brief failed".into()));
                return 1;
            }
            Err(_) => {
                eprintln!("seance ctl wait: cannot connect");
                return 2;
            }
        };
        let rows = brief
            .data
            .as_ref()
            .and_then(|d| d.get("panes"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        // Keep a filtered diagnostic snapshot of panes we care about.
        last_rows = rows
            .iter()
            .filter(|p| {
                let slug = p.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                let name = p.get("name").and_then(|s| s.as_str()).unwrap_or("");
                panes.iter().any(|want| want == slug || want == name)
            })
            .cloned()
            .collect();

        let mut satisfied: Vec<String> = Vec::new();

        for pane in &panes {
            let row = rows.iter().find(|p| {
                p.get("slug").and_then(|s| s.as_str()) == Some(pane.as_str())
                    || p.get("name").and_then(|s| s.as_str()) == Some(pane.as_str())
            });
            let Some(row) = row else {
                continue;
            };

            let running = row.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
            let exited = row.get("exited").and_then(|v| v.as_bool()).unwrap_or(false);
            let o = row.get("owner").and_then(|v| v.as_str()).unwrap_or("none");
            let st = row.get("status").and_then(|v| v.as_str());
            let _pad = row
                .get("scratchpad")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let pad_bytes = row
                .get("scratchpad_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            // Default condition if nothing specified: free for inject (running, owner none)
            let mut want_default = owner.is_none()
                && status.is_none()
                && contains.is_none()
                && artifact.is_none()
                && !scratchpad
                && !ready;

            let mut ok = true;

            if let Some(want) = &owner {
                let match_o = o == want.as_str()
                    || (want == "agent" && o.starts_with("agent:"))
                    || (want == "idle" && o == "none");
                ok &= match_o;
                want_default = false;
            }
            if let Some(want_st) = &status {
                ok &= st == Some(want_st.as_str());
                want_default = false;
                // Evidence-bound done (0.9.6): badge alone is not enough when
                // an inject baseline exists.
                if evidence && want_st == "done" {
                    let inj_rev = row.get("inject_pad_rev").and_then(|v| v.as_u64());
                    let inj_bytes = row
                        .get("inject_pad_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let pad_rev = row.get("pad_rev").and_then(|v| v.as_u64()).unwrap_or(0);
                    if let Some(r) = inj_rev {
                        let rev_ok = pad_rev > r;
                        let bytes_ok = pad_bytes > inj_bytes;
                        ok &= rev_ok || bytes_ok;
                    }
                }
            }
            if let Some(want_tid) = &task_id {
                let got = row.get("task_id").and_then(|v| v.as_str());
                let tstat = row.get("task_status").and_then(|v| v.as_str());
                // Match this dispatch: done only when THIS id reports done
                // (and preferably with pad evidence when evidence=true).
                ok &= got == Some(want_tid.as_str()) && tstat == Some("done");
                if evidence && ok {
                    let inj_rev = row.get("inject_pad_rev").and_then(|v| v.as_u64());
                    let inj_bytes = row
                        .get("inject_pad_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let pad_rev = row.get("pad_rev").and_then(|v| v.as_u64()).unwrap_or(0);
                    if let Some(r) = inj_rev {
                        ok &= pad_rev > r || pad_bytes > inj_bytes;
                    }
                }
                want_default = false;
            }
            if ready {
                // Boot-ready: running, not exited, no known blocking dialogs on screen.
                ok &= running && !exited;
                want_default = false;
            }
            // Fail-fast: waiting for done but pane already exited/tombstoned.
            if exited && status.as_deref() == Some("done") {
                ok = false;
                // Leave unsatisfied so timeout reports clearly; orchestrator
                // can see status=idle from exit handler.
            }
            if scratchpad {
                if since_inject {
                    let inj_rev = row
                        .get("inject_pad_rev")
                        .and_then(|v| v.as_u64());
                    let inj_bytes = row
                        .get("inject_pad_bytes")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let pad_rev = row.get("pad_rev").and_then(|v| v.as_u64()).unwrap_or(0);
                    // Prefer rev growth; also accept byte growth ≥ min_bytes since inject.
                    let rev_ok = inj_rev.map(|r| pad_rev > r).unwrap_or(false);
                    let bytes_ok = pad_bytes.saturating_sub(inj_bytes) >= min_bytes;
                    ok &= rev_ok || bytes_ok;
                } else {
                    ok &= pad_bytes >= min_bytes;
                }
                want_default = false;
            }
            if let Some(path) = &artifact {
                let sz = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                ok &= sz >= min_bytes;
                want_default = false;
            }

            // Screen contains / ready dialog check — only when needed.
            if contains.is_some() || ready {
                let read = with_identity(
                    ControlRequest::Read {
                        pane: pane.clone(),
                        lines: Some(30),
                        scope: None,
                        from: None,
                    },
                    scope.clone(),
                    from.clone(),
                );
                if let Ok(r) = send_request(&read) {
                    if r.ok {
                        let screen = r
                            .data
                            .as_ref()
                            .and_then(|d| {
                                d.as_str()
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        d.get("screen")
                                            .and_then(|s| s.as_str())
                                            .map(|s| s.to_string())
                                    })
                            })
                            .unwrap_or_default();
                        if let Some(c) = &contains {
                            ok &= screen.contains(c.as_str());
                            want_default = false;
                        }
                        if ready {
                            // Only *blocking* dialogs — splash tips ("Grok 4.5
                            // is here", "New worktree") can coexist with a live
                            // prompt and must not false-deny ready (meta-demo3).
                            let blocked = screen.contains("trust this folder")
                                || screen.contains("Trusting the directory")
                                || screen.contains("Do you trust")
                                || (screen.contains("Update available")
                                    && (screen.contains("Skip until next version")
                                        || screen.contains("Yes, I trust")));
                            let promptish = screen.contains('❯')
                                || screen.contains('›')
                                || screen.contains("bypass permissions")
                                || screen.contains("always-approve")
                                || screen.contains("? for shortcuts")
                                || screen.contains("ctrl+c to interrupt")
                                || screen.contains("esc to interrupt");
                            ok &= !blocked && promptish;
                            // stable screen
                            let prev = last_screens.get(pane).cloned().unwrap_or_default();
                            if screen == prev && !screen.is_empty() {
                                let since = stable_since
                                    .entry(pane.clone())
                                    .or_insert_with(std::time::Instant::now);
                                ok &= since.elapsed().as_millis() as u64 >= stable_ms;
                            } else {
                                last_screens.insert(pane.clone(), screen);
                                stable_since.insert(pane.clone(), std::time::Instant::now());
                                ok = false;
                            }
                        }
                    }
                }
            }

            if want_default {
                ok = running && !exited && o == "none";
            }

            // Exited pane with status wait fails (unless waiting on exited intentionally)
            if exited && status.is_none() && !scratchpad && artifact.is_none() {
                // still allow pad/artifact waits on tombstones
            }

            if ok {
                satisfied.push(pane.clone());
            }
        }

        let success = if any {
            !satisfied.is_empty()
        } else {
            satisfied.len() == panes.len()
        };

        if success {
            let label = if status.as_deref() == Some("done") {
                "done"
            } else if ready {
                "ready"
            } else {
                "ok"
            };
            let list = if any {
                satisfied.as_slice()
            } else {
                panes.as_slice()
            };
            if json_out {
                // Optionally attach pad bodies when harvesting.
                if cat_pads {
                    let mut pads = serde_json::Map::new();
                    for p in list {
                        if let Some(body) = harvest_pad_body(p, &scope, &from) {
                            pads.insert(p.clone(), serde_json::Value::String(body));
                        }
                    }
                    println!(
                        "{{\"ok\":true,\"{label}\":{},\"elapsed_ms\":{},\"pads\":{}}}",
                        serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()),
                        started.elapsed().as_millis(),
                        serde_json::Value::Object(pads)
                    );
                } else {
                    println!(
                        "{{\"ok\":true,\"{label}\":{},\"elapsed_ms\":{}}}",
                        serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()),
                        started.elapsed().as_millis()
                    );
                }
            } else {
                println!("{label} {}", list.join(" "));
                if cat_pads {
                    for p in list {
                        println!("----- {p} -----");
                        match harvest_pad_body(p, &scope, &from) {
                            Some(body) => {
                                print!("{body}");
                                if !body.ends_with('\n') {
                                    println!();
                                }
                            }
                            None => println!("(no pad)"),
                        }
                    }
                }
            }
            return 0;
        }

        // Event-driven: wake on bus activity, with a short poll ceiling so we
        // still make progress if watch is unavailable.
        match wake_rx.recv_timeout(std::time::Duration::from_millis(400)) {
            Ok(()) => {
                // Drain extra wakes so we don't thrash.
                while wake_rx.try_recv().is_ok() {}
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
}

/// Resolve a pane's scratchpad path via ctl and read the body (harvest helper).
fn harvest_pad_body(pane: &str, scope: &Option<String>, from: &Option<String>) -> Option<String> {
    let req = with_identity(
        ControlRequest::Scratchpad {
            pane: pane.to_string(),
            scope: None,
            from: None,
        },
        scope.clone(),
        from.clone(),
    );
    let r = send_request(&req).ok()?;
    if !r.ok {
        return None;
    }
    let path = r.data.as_ref().and_then(|d| {
        d.as_str()
            .map(|s| s.to_string())
            .or_else(|| d.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()))
    })?;
    std::fs::read_to_string(path).ok()
}

/// `watch [--kinds a,b] [--pane P] [--actor A] [--since-seq N] [--no-catch-up]`
fn parse_watch(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut kinds = None;
    let mut pane = None;
    let mut actor = None;
    let mut since_seq = None;
    let mut catch_up = true;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--kinds" | "--types" | "--events" => {
                let v = take_value(&mut it, "--kinds")?;
                kinds = Some(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "--pane" => pane = Some(take_value(&mut it, "--pane")?),
            "--actor" => actor = Some(take_value(&mut it, "--actor")?),
            "--since-seq" | "--cursor" => {
                let v = take_value(&mut it, "--since-seq")?;
                since_seq = Some(
                    v.parse()
                        .map_err(|_| format!("--since-seq: not a number: {v}"))?,
                );
            }
            "--no-catch-up" => catch_up = false,
            other => return Err(format!("watch: unexpected argument '{other}'")),
        }
    }
    Ok(ControlRequest::Watch {
        since_seq,
        kinds,
        pane,
        actor,
        catch_up,
        scope: None,
        from: None,
    })
}

/// `grant PRINCIPAL CAP [--workspace WS] [--ttl SECS]`
fn parse_grant(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut principal = None;
    let mut cap = None;
    let mut workspace = None;
    let mut ttl_secs = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" | "--ws" => workspace = Some(take_value(&mut it, "--workspace")?),
            "--ttl" => {
                let v = take_value(&mut it, "--ttl")?;
                ttl_secs = Some(v.parse().map_err(|_| format!("--ttl: not a number: {v}"))?);
            }
            other if principal.is_none() => principal = Some(other.to_string()),
            other if cap.is_none() => cap = Some(other.to_string()),
            other => return Err(format!("grant: unexpected '{other}'")),
        }
    }
    Ok(ControlRequest::CapsGrant {
        principal: principal.ok_or("grant: expected PRINCIPAL (e.g. agent:worker-1 or cli)")?,
        cap: cap.ok_or("grant: expected CAP (e.g. send, kill, new, *)")?,
        workspace,
        ttl_secs,
        scope: None,
        from: None,
    })
}

/// `revoke PRINCIPAL [CAP] [--workspace WS]`
fn parse_revoke(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut principal = None;
    let mut cap = "*".to_string();
    let mut workspace = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" | "--ws" => workspace = Some(take_value(&mut it, "--workspace")?),
            other if principal.is_none() => principal = Some(other.to_string()),
            other => cap = other.to_string(),
        }
    }
    Ok(ControlRequest::CapsRevoke {
        principal: principal.ok_or("revoke: expected PRINCIPAL")?,
        cap,
        workspace,
        scope: None,
        from: None,
    })
}

/// `policy [get|set MODE] [--workspace WS]`
fn parse_policy(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut workspace = None;
    let mut mode = None;
    let mut action = "get";
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "get" => action = "get",
            "set" => {
                action = "set";
                mode = Some(take_value(&mut it, "set")?);
            }
            "--workspace" | "--ws" => workspace = Some(take_value(&mut it, "--workspace")?),
            other if action == "set" && mode.is_none() => mode = Some(other.to_string()),
            other => {
                // bare MODE → set
                if crate::caps::PolicyMode::parse(other).is_some() {
                    action = "set";
                    mode = Some(other.to_string());
                } else {
                    return Err(format!("policy: unexpected '{other}'"));
                }
            }
        }
    }
    if action == "set" {
        Ok(ControlRequest::PolicySet {
            mode: mode.ok_or("policy set: expected MODE (open|propose_required|locked)")?,
            workspace,
            scope: None,
            from: None,
        })
    } else {
        Ok(ControlRequest::PolicyGet {
            workspace,
            scope: None,
            from: None,
        })
    }
}

/// Stream events until interrupted.
fn run_watch(request: &ControlRequest, json_out: bool) -> i32 {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let path = socket_path();
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "seance ctl: could not connect to {} ({e}). is seance running?",
                path.display()
            );
            return 2;
        }
    };
    // Watch stays open indefinitely — no read timeout.
    let _ = stream.set_read_timeout(None);
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("seance ctl: {e}");
            return 2;
        }
    };
    if writer.write_all(b"{\"role\":\"ctl\"}\n").is_err() {
        eprintln!("seance ctl: failed to say hello");
        return 2;
    }
    let mut line = match serde_json::to_string(request) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("seance ctl: {e}");
            return 1;
        }
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
        eprintln!("seance ctl: failed to send watch");
        return 2;
    }

    let mut reader = BufReader::new(stream);
    let mut first = true;
    loop {
        let mut resp_line = String::new();
        match reader.read_line(&mut resp_line) {
            Ok(0) => {
                if first {
                    eprintln!("seance ctl: watch connection closed immediately");
                    return 2;
                }
                return 0;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("seance ctl: watch read error: {e}");
                return 2;
            }
        }
        let resp: ControlResponse = match serde_json::from_str(resp_line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("seance ctl: bad watch line: {e}");
                continue;
            }
        };
        if first {
            first = false;
            if !resp.ok {
                eprintln!(
                    "seance ctl: watch failed: {}",
                    resp.error.as_deref().unwrap_or("unknown")
                );
                return 1;
            }
            if !json_out {
                let cursor = resp
                    .data
                    .as_ref()
                    .and_then(|d| d.get("cursor"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                eprintln!("watching events (cursor={cursor}) · ctrl-c to stop");
            } else {
                println!("{}", resp_line.trim_end());
            }
            continue;
        }
        if json_out {
            println!("{}", resp_line.trim_end());
            continue;
        }
        // Human-readable event line.
        if let Some(data) = &resp.data {
            let seq = data.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
            let ts = data.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
            let actor = data
                .get("actor")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let kind = data.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let detail = data.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            let pane = data
                .get("pane")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let origin = data
                .get("origin")
                .and_then(|v| v.as_str())
                .map(|o| format!(" origin={o}"))
                .unwrap_or_default();
            let time = crate::events::fmt_time(ts);
            println!("{time}  #{seq:<6}  {actor:<16}  {kind:<16}  [{pane}]  {detail}{origin}");
        }
    }
}

/// Pull exactly one positional (a pane name) or explain what was wrong.
fn single_positional(args: Vec<String>, cmd: &str) -> Result<String, String> {
    let mut positionals: Vec<String> = args.into_iter().filter(|a| !a.starts_with('-')).collect();
    match positionals.len() {
        0 => Err(format!("{cmd}: expected a SESSION name")),
        1 => Ok(positionals.remove(0)),
        _ => Err(format!("{cmd}: expected exactly one SESSION name")),
    }
}

/// Consume the next iterator item as a flag's value, or explain its absence.
fn take_value(it: &mut std::vec::IntoIter<String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} expects a value"))
}

// ---------------------------------------------------------------------------
// Socket round-trip
// ---------------------------------------------------------------------------

/// Failure modes distinct enough to warrant different exit codes / messages.
enum ConnectError {
    /// Couldn't open the socket at all — app almost certainly not running.
    Connect(std::io::Error),
    /// Connected, but IO failed mid-exchange.
    Io(std::io::Error),
    /// Got bytes, but they weren't a valid response line.
    Protocol(String),
}

/// Open the socket, send one request line, read one response line.
fn send_request(request: &ControlRequest) -> Result<ControlResponse, ConnectError> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).map_err(ConnectError::Connect)?;
    // Guard against a hung server: cap read/write waits. Generous vs. the
    // server's own 10s per-request budget so we don't time out a valid slow op.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut writer = stream.try_clone().map_err(ConnectError::Io)?;
    // Protocol v2: first line is a role hello so the daemon can multiplex
    // ctl / gui / upgrade on one socket.
    writer
        .write_all(b"{\"role\":\"ctl\"}\n")
        .map_err(ConnectError::Io)?;
    let mut line = serde_json::to_string(request)
        .map_err(|e| ConnectError::Protocol(format!("could not serialize request: {e}")))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(ConnectError::Io)?;
    writer.flush().map_err(ConnectError::Io)?;

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    let n = reader
        .read_line(&mut resp_line)
        .map_err(ConnectError::Io)?;
    if n == 0 {
        return Err(ConnectError::Protocol(
            "connection closed before a response was received".into(),
        ));
    }

    serde_json::from_str::<ControlResponse>(resp_line.trim_end())
        .map_err(|e| ConnectError::Protocol(format!("{e}: {}", resp_line.trim_end())))
}

// ---------------------------------------------------------------------------
// Human-readable output
// ---------------------------------------------------------------------------

/// Pretty-print a successful response for the given subcommand. Falls back to
/// dumping the JSON payload when we don't have a bespoke renderer.
fn print_ok_human(sub: &str, response: &ControlResponse) {
    let data = match &response.data {
        Some(d) => d,
        None => {
            // No payload: a bare success (kill, or an empty list). Say so.
            match sub {
                "kill" => println!("killed"),
                _ => println!("ok"),
            }
            return;
        }
    };

    match sub {
        "list" | "ls" | "brief" | "roster" | "stage" => print_list(data),
        "task" | "inbox" => {
            if let Some(body) = data.get("body").and_then(|v| v.as_str()) {
                let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let st = data.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let pane = data.get("pane").and_then(|v| v.as_str()).unwrap_or("?");
                eprintln!("# task {id}  pane={pane}  status={st}");
                print!("{body}");
                if !body.ends_with('\n') {
                    println!();
                }
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
                );
            }
        }
        "whoami" => {
            if let Some(obj) = data.as_object() {
                for k in [
                    "principal",
                    "session",
                    "workspace",
                    "policy",
                    "task_id",
                    "task_status",
                    "hint",
                ] {
                    if let Some(v) = obj.get(k) {
                        if let Some(s) = v.as_str() {
                            println!("{k:<14} {s}");
                        } else if !v.is_null() {
                            println!("{k:<14} {v}");
                        }
                    }
                }
            } else {
                println!("{data}");
            }
        }
        "send" | "finish" | "status-set" | "note" => {
            // Dense one-liners for orchestrator feedback (task_id, pad_rev).
            if let Some(obj) = data.as_object() {
                let mut parts = Vec::new();
                if let Some(s) = obj.get("slug").and_then(|v| v.as_str()) {
                    parts.push(s.to_string());
                }
                if let Some(t) = obj.get("task_id").and_then(|v| v.as_str()) {
                    parts.push(format!("task={t}"));
                }
                if let Some(s) = obj.get("status").and_then(|v| v.as_str()) {
                    parts.push(format!("status={s}"));
                }
                if let Some(r) = obj.get("pad_rev").and_then(|v| v.as_u64()) {
                    parts.push(format!("rev={r}"));
                }
                if let Some(b) = obj.get("scratchpad_bytes").and_then(|v| v.as_u64()) {
                    parts.push(format!("pad={b}B"));
                }
                if parts.is_empty() {
                    println!("ok");
                } else {
                    println!("{}", parts.join(" "));
                }
            } else if data.is_null() {
                println!("ok");
            } else {
                println!("{data}");
            }
        }
        "timeline" | "tl" => {
            if let Some(events) = data.get("events").and_then(|v| v.as_array()) {
                if events.is_empty() {
                    println!("(no events)");
                }
                for e in events {
                    println!(
                        "{}  {:<18} {:<14} {}",
                        e.get("time").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("actor").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("detail").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                }
            }
        }
        "status" => print_status(data),
        "read" => print_read(data),
        "scratchpad" | "pad" => print_scratchpad(data),
        "new" => print_new(data),
        _ => {
            // Unknown-but-ok: show the payload compactly.
            println!(
                "{}",
                serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
            );
        }
    }
}

/// Render `list` as a compact table. Expects `data` to be an array of pane
/// objects (fields are best-effort: name, workspace, command, running, title).
fn print_list(data: &serde_json::Value) {
    if let Some(scope) = data.get("scope").and_then(|v| v.as_str()) {
        println!("workspace: {scope}   (use --all to list every workspace)");
    }
    let Some(arr) = data.as_array() else {
        // Wrapped payloads: {"panes":[...]} (current) / {"sessions":[...]} (v0.1).
        for key in ["panes", "sessions"] {
            if let Some(arr) = data.get(key).and_then(|v| v.as_array()) {
                return print_session_rows(arr);
            }
        }
        println!("{data}");
        return;
    };
    print_session_rows(arr);
}

fn print_session_rows(arr: &[serde_json::Value]) {
    if arr.is_empty() {
        println!("(no panes)");
        return;
    }
    for s in arr {
        let name = str_field(s, "name").unwrap_or_else(|| "?".into());
        let slug = str_field(s, "slug").unwrap_or_default();
        // Always prefer slug for the left column when it differs — ctl needs slug.
        let label = if !slug.is_empty() && slug != name {
            format!("{slug} ({name})")
        } else if !slug.is_empty() {
            slug.clone()
        } else {
            name.clone()
        };
        let running = s.get("running").and_then(|v| v.as_bool());
        let exited = s.get("exited").and_then(|v| v.as_bool()).unwrap_or(false);
        let owner = str_field(s, "owner").unwrap_or_else(|| "-".into());
        let status = str_field(s, "status").unwrap_or_default();
        let pad_b = s
            .get("scratchpad_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let title = str_field(s, "title").unwrap_or_default();
        let task = str_field(s, "task_id").unwrap_or_default();
        let state = if exited {
            "tombstone"
        } else {
            match running {
                Some(true) => "running",
                Some(false) => "stopped",
                None => "-",
            }
        };
        let ws = str_field(s, "workspace").unwrap_or_default();
        let mut line = format!("{label:<22} {state:<10} owner={owner:<18}");
        if !status.is_empty() {
            line.push_str(&format!(" status={status}"));
        }
        if !task.is_empty() {
            line.push_str(&format!(" task={task}"));
        }
        let pad_rev = s.get("pad_rev").and_then(|v| v.as_u64()).unwrap_or(0);
        if pad_b > 0 || pad_rev > 0 {
            if pad_rev > 0 {
                line.push_str(&format!(" pad={pad_b}B@r{pad_rev}"));
            } else {
                line.push_str(&format!(" pad={pad_b}B"));
            }
        }
        if !ws.is_empty() {
            line.push_str(&format!(" [{ws}]"));
        }
        if !title.is_empty() {
            let t = if title.len() > 40 {
                format!("{}…", &title[..40])
            } else {
                title
            };
            line.push_str(&format!(" · {t}"));
        }
        println!("{}", line.trim_end());
    }
}

/// Render `status` as aligned key/value lines.
fn print_status(data: &serde_json::Value) {
    for key in ["name", "workspace", "command", "running", "title"] {
        if let Some(v) = data.get(key) {
            let rendered = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Null => continue,
                other => other.to_string(),
            };
            println!("{key:<10} {rendered}");
        }
    }
}

/// Render `read`: print the screen text verbatim. Accepts either a bare string
/// payload or `{"screen":"..."}` / `{"text":"..."}`.
fn print_read(data: &serde_json::Value) {
    if let Some(s) = data.as_str() {
        print!("{s}");
        ensure_trailing_newline(s);
        return;
    }
    for key in ["screen", "text", "content", "lines"] {
        if let Some(s) = data.get(key).and_then(|v| v.as_str()) {
            print!("{s}");
            ensure_trailing_newline(s);
            return;
        }
    }
    // Array of lines?
    if let Some(arr) = data.as_array() {
        for l in arr {
            if let Some(s) = l.as_str() {
                println!("{s}");
            }
        }
        return;
    }
    println!("{data}");
}

/// Render `scratchpad`: just the path, so it's pipe-friendly
/// (`cat "$(seance ctl scratchpad w)"`). Accepts a string or `{"path":"..."}`.
fn print_scratchpad(data: &serde_json::Value) {
    if let Some(s) = data.as_str() {
        println!("{s}");
        return;
    }
    if let Some(s) = data.get("path").and_then(|v| v.as_str()) {
        println!("{s}");
        return;
    }
    println!("{data}");
}

/// Render `new`: report the created pane's id/name so the caller can drive
/// it. Accepts a string (the slug) or an object with name/slug.
fn print_new(data: &serde_json::Value) {
    if let Some(s) = data.as_str() {
        println!("created {s}");
        return;
    }
    let slug = str_field(data, "slug");
    let name = str_field(data, "name");
    match (name, slug) {
        (Some(name), Some(slug)) if name != slug => println!("created {name} ({slug})"),
        (Some(name), _) => println!("created {name}"),
        (_, Some(slug)) => println!("created {slug}"),
        _ => println!("created"),
    }
}

/// Best-effort string field extraction.
fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// Print a newline only if the text didn't already end with one — keeps
/// `read` output clean whether or not the screen snapshot is newline-terminated.
fn ensure_trailing_newline(s: &str) {
    if !s.ends_with('\n') {
        println!();
    }
}

// ---------------------------------------------------------------------------
// Event-driven wait wake + boot clear + phone/export/prompts
// ---------------------------------------------------------------------------

/// Background: subscribe to the event bus and poke `tx` when something changes
/// on the panes we're waiting on. Best-effort — disconnect = poll-only wait.
fn watch_wake_loop(panes: Vec<String>, tx: std::sync::mpsc::Sender<()>) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let path = socket_path();
    let Ok(stream) = UnixStream::connect(&path) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    if writer.write_all(b"{\"role\":\"ctl\"}\n").is_err() {
        return;
    }
    // Catch status, finish, note, send, pane lifecycle — anything that can
    // change wait conditions.
    let req = ControlRequest::Watch {
        since_seq: None,
        kinds: Some(vec![
            "status_set".into(),
            "finish".into(),
            "note".into(),
            "send".into(),
            "pane_exited".into(),
            "pane_spawned".into(),
            "agency".into(),
            "ask".into(),
        ]),
        pane: None,
        actor: None,
        catch_up: false,
        scope: None,
        from: None,
    };
    let Ok(mut line) = serde_json::to_string(&req) else {
        return;
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).is_err() || writer.flush().is_err() {
        return;
    }
    let mut reader = BufReader::new(stream);
    loop {
        let mut resp_line = String::new();
        match reader.read_line(&mut resp_line) {
            Ok(0) => return,
            Ok(_) => {
                // Any event wakes wait; filter lightly by pane name in JSON text.
                let interesting = panes.is_empty()
                    || panes.iter().any(|p| resp_line.contains(p.as_str()))
                    || resp_line.contains("\"ok\"");
                if interesting {
                    if tx.send(()).is_err() {
                        return;
                    }
                }
            }
            Err(_) => {
                // timeout on read — keep watching
                continue;
            }
        }
    }
}

/// After `--wait-ready`, clear known agent boot dialogs.
fn boot_clear_pane(
    slug: &str,
    command: &str,
    scope: Option<String>,
    from: Option<String>,
    json_out: bool,
) {
    let profile = crate::agents::guess_profile_from_command(command)
        .or_else(|| {
            // also try agent field if new returns it
            None
        })
        .unwrap_or("");
    if profile.is_empty() {
        return;
    }
    let seq = crate::agents::boot_clear_sequence(profile);
    if seq.is_empty() {
        return;
    }
    if !json_out {
        eprintln!("boot-clear '{slug}' ({profile})…");
    }
    for bytes in seq {
        // settle so the TUI paints the dialog before we answer
        std::thread::sleep(Duration::from_millis(350));
        let b64 = base64_encode(&bytes);
        let req = with_identity(
            ControlRequest::SendRaw {
                pane: slug.to_string(),
                bytes_b64: b64,
                force: true,
                scope: None,
                from: None,
            },
            scope.clone(),
            from.clone(),
        );
        let _ = send_request(&req);
    }
    // re-check ready briefly after clear
    std::thread::sleep(Duration::from_millis(400));
}

/// `seance ctl phone [PANE] [--label L] [--ttl HOURS]`
/// Opens a vita telegram topic and registers a participant that can drive the pane.
fn run_phone(
    args: Vec<String>,
    scope: Option<String>,
    from: Option<String>,
    json_out: bool,
) -> i32 {
    let mut pane: Option<String> = None;
    let mut label: Option<String> = None;
    let mut ttl_hours: f64 = 4.0;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--label" | "--name" => label = it.next(),
            "--ttl" => {
                ttl_hours = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(4.0);
            }
            "--help" | "-h" => {
                println!(
                    "phone [PANE] [--label L] [--ttl HOURS]\n  \
                     Open a vita telegram topic bound to a seance pane.\n  \
                     Requires `./run capabilities` / vita on PATH from ~/work/vita.\n  \
                     Default pane: $SEANCE_SESSION."
                );
                return 0;
            }
            other if !other.starts_with('-') => pane = Some(other.to_string()),
            other => {
                eprintln!("seance ctl phone: unexpected '{other}'");
                return 1;
            }
        }
    }
    let pane = pane
        .or_else(|| from.clone())
        .or_else(|| std::env::var("SEANCE_SESSION").ok())
        .filter(|s| !s.is_empty());
    let Some(pane) = pane else {
        eprintln!("seance ctl phone: need PANE or $SEANCE_SESSION");
        return 1;
    };

    // Resolve display name via brief if possible.
    let brief_req = with_identity(
        ControlRequest::Brief {
            scope: None,
            from: None,
        },
        scope.clone(),
        from.clone(),
    );
    let name = send_request(&brief_req)
        .ok()
        .and_then(|r| r.data)
        .and_then(|d| {
            d.get("panes")
                .and_then(|p| p.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|p| {
                            p.get("slug").and_then(|s| s.as_str()) == Some(pane.as_str())
                                || p.get("name").and_then(|s| s.as_str()) == Some(pane.as_str())
                        })
                        .and_then(|p| p.get("name").and_then(|s| s.as_str()).map(|s| s.to_string()))
                })
        })
        .unwrap_or_else(|| pane.clone());

    let topic_label = label.unwrap_or_else(|| format!("seance · {name}"));

    // Prefer vita capabilities CLI from the monorepo; fall back to bare `vita`.
    let open_body = serde_json::json!({
        "title": topic_label,
        "note": format!("seance pane {pane} — replies inject via participant bridge"),
    });
    let open_out = run_vita_capability("vita.telegram.open_topic", &open_body);
    let open_json: serde_json::Value = match open_out {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({"raw": s})),
        Err(e) => {
            eprintln!("seance ctl phone: open_topic failed: {e}");
            eprintln!("hint: run from a host with vita (`~/work/vita ./run capabilities …`)");
            return 1;
        }
    };
    let topic_id = open_json
        .pointer("/topic_id")
        .or_else(|| open_json.pointer("/id"))
        .or_else(|| open_json.pointer("/data/topic_id"))
        .or_else(|| open_json.get("topic_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            open_json
                .get("raw")
                .and_then(|v| v.as_str())
                .and_then(|s| {
                    // last-resort: find a uuid-ish token
                    s.split_whitespace()
                        .find(|t| t.len() >= 8 && t.contains('-'))
                        .map(|s| s.to_string())
                })
        });
    let Some(topic_id) = topic_id else {
        eprintln!(
            "seance ctl phone: could not parse topic_id from: {}",
            open_json
        );
        return 1;
    };

    let reg_body = serde_json::json!({
        "topic_id": topic_id,
        "label": format!("seance:{pane}"),
        "mode": "mailbox",
        "ttl_hours": ttl_hours,
        "note": format!("EXCLUSIVE claim: messages inject into seance pane {pane} via `seance ctl send {pane}`"),
    });
    let reg_out = run_vita_capability("vita.telegram.register_participant", &reg_body);
    if let Err(e) = &reg_out {
        eprintln!("seance ctl phone: register_participant warning: {e}");
    }

    // Persist binding next to scratchpad for bridge scripts / future GUI.
    let bind_path = PathBuf::from(
        shellexpand::tilde(&format!(
            "~/.local/share/seance/scratch/{pane}.telegram.json"
        ))
        .into_owned(),
    );
    let link = open_json
        .pointer("/link")
        .or_else(|| open_json.pointer("/data/link"))
        .or_else(|| open_json.get("link"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://t.me/c/3864532297/{topic_id}"));
    let bind = serde_json::json!({
        "pane": pane,
        "topic_id": topic_id,
        "link": link,
        "label": topic_label,
        "ttl_hours": ttl_hours,
        "created_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    if let Ok(s) = serde_json::to_string_pretty(&bind) {
        let _ = std::fs::write(&bind_path, s);
    }

    // Best-effort status line into the topic.
    let _ = run_vita_capability(
        "vita.telegram.send",
        &serde_json::json!({
            "topic_id": topic_id,
            "text": format!(
                "✦ seance pane `{pane}` linked.\nReplies here inject as tasks. Status updates land here when the agent needs you."
            ),
        }),
    );

    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "pane": pane,
                "topic_id": topic_id,
                "label": topic_label,
                "bind": bind_path.to_string_lossy(),
            })
        );
    } else {
        println!("phone {pane} topic={topic_id}");
        println!("bind {}", bind_path.display());
        eprintln!("tip: await replies with vita participant await; inject via `seance ctl send {pane} --file …`");
    }
    0
}

fn run_vita_capability(name: &str, input: &serde_json::Value) -> Result<String, String> {
    let input_s = serde_json::to_string(input).map_err(|e| e.to_string())?;
    // Prefer vita monorepo runner when present.
    let vita_root = PathBuf::from(shellexpand::tilde("~/work/vita").into_owned());
    let run_script = vita_root.join("run");
    let mut cmd = if run_script.exists() {
        let mut c = std::process::Command::new(&run_script);
        c.current_dir(&vita_root);
        c.args([
            "capabilities",
            "call",
            name,
            "--input",
            &input_s,
        ]);
        c
    } else {
        let mut c = std::process::Command::new("vita");
        c.args(["capabilities", "call", name, "--input", &input_s]);
        c
    };
    let out = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "exit {} — {err}{stdout}",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `seance ctl export-session [--workspace WS] [--out PATH] [--title T] [--share] [--pin N] [--open] [--redact]`
fn run_export_session(args: Vec<String>, scope: Option<String>, json_out: bool) -> i32 {
    let mut opts = crate::export_html::ExportOpts {
        workspace: scope.clone(),
        ..Default::default()
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" | "--ws" => opts.workspace = it.next(),
            "--out" | "-o" => opts.out = it.next().map(PathBuf::from),
            "--title" => opts.title = it.next().unwrap_or(opts.title),
            "--share" => opts.share = true,
            "--pin" => opts.pin = it.next(),
            "--open" => opts.open = true,
            "--redact" | "--redact-paths" => opts.redact_paths = true,
            "--help" | "-h" => {
                println!(
                    "export-session [opts]\n  \
                     --workspace WS   limit to workspace (default: $SEANCE_WORKSPACE)\n  \
                     --out PATH       output HTML (default: ~/.local/share/seance/exports/)\n  \
                     --title T        document title\n  \
                     --redact         scrub /home/$USER paths for teaching shares\n  \
                     --share          publish via vita-reports (~/work/vita)\n  \
                     --pin N          PIN-gate the share (with --share)\n  \
                     --open           xdg-open the HTML after write\n  \
                     Offline scrubber v1: events JSON + virtual timeline + pads/tasks/cmdlog."
                );
                return 0;
            }
            other => {
                eprintln!("seance ctl export-session: unexpected '{other}'");
                return 1;
            }
        }
    }
    match crate::export_html::export_with_opts(opts) {
        Ok(r) => {
            if json_out {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "path": r.path.to_string_lossy(),
                        "html_bytes": r.meta.html_bytes,
                        "gen_ms": r.meta.gen_ms,
                        "event_count": r.meta.event_count,
                        "events_sampled": r.meta.events_sampled,
                        "share_url": r.share_url,
                    })
                );
            } else {
                println!(
                    "exported {} ({} events, {} bytes, {}ms)",
                    r.path.display(),
                    r.meta.event_count,
                    r.meta.html_bytes,
                    r.meta.gen_ms
                );
                if let Some(url) = r.share_url {
                    println!("share {url}");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("seance ctl export-session: {e}");
            1
        }
    }
}

/// `seance ctl prompts [query]` — list / filter precanned prompts.
fn run_prompts(args: Vec<String>, json_out: bool) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "prompts [query]\n  List precanned prompts (builtins + ~/.config/seance/prompts.json).\n  \
             GUI: ctrl+shift+k opens the palette."
        );
        return 0;
    }
    let _ = crate::prompts::ensure_user_file();
    let q = args.join(" ");
    let all = crate::prompts::load_all();
    let hits = crate::prompts::filter(&all, &q);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&hits).unwrap_or_else(|_| "[]".into()));
    } else {
        for p in &hits {
            println!("{:<18} {}", p.id, p.title);
        }
        if hits.is_empty() {
            println!("(no prompts match)");
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Minimal base64 (encode-only) — avoids adding a dependency for send-raw.
// ---------------------------------------------------------------------------

/// Standard base64 alphabet, `=`-padded. Encode-only; the app decodes app-side.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

/// Print the `seance ctl help` text.
/// Agent-facing instructions printed by `seance ctl skill`. The single
/// source of truth — docs/CONTROL.md points here rather than duplicating.
const SKILL_TEXT: &str = r#"## Working in seance (human + agent shared space)

You are inside **seance** — multi-pane live terminals on the human's screen.
Visibility is the product. If `$SEANCE_SESSION` is set, you are in a pane now.

### Environment

- `$SEANCE_SESSION`    your pane **slug** (use this id in ctl, not display name)
- `$SEANCE_WORKSPACE`  auto-scopes ctl to this circle
- `$SEANCE_SCRATCHPAD` shared notes path (screens scroll away)
- `$SEANCE_SOCKET`     control socket (ctl finds it)

### Hot path — worker (you received a task)

1. Re-read your assignment (durable, not scrollback):
     `seance ctl task`          # or `ctl inbox`
     `seance ctl whoami`        # shows task_id when set
   Sidecars next to the pad: `$SEANCE_SCRATCHPAD` with extension `.taskid` / `.task.json`
2. Do the work. Prefer intermediate `status-set working|blocked|needs-human`.
3. Complete with **one call** (pad body + status + task close):
     `seance ctl finish --stdin --status done --note … <<'EOF'
     …answer…
     EOF`
   or `finish --file PATH --status done`.
   `status=done` **requires a body**. Returns `task=… status=done rev=N pad=…B`.

### Hot path — orchestrator (you drive siblings)

Prefer structure over screens. **Never** poll `read` in a sleep loop.

```bash
seance ctl doctor
seance ctl roster                              # slug, status, task, pad@rev
seance ctl new --name w --cwd "$PWD" --agent claude --wait-ready
# NOTE: created slug may be w-2 if w exists — use the id `created` prints
# FOOTGUN: shell expands $VARS in bare send — always --file for tasks:
seance ctl send w-2 --file /tmp/task.md        # returns task=task-N status=working
seance ctl wait w-2 --status done --timeout 600 --cat   # evidence-bound + harvest
# fan-in harvest:
seance ctl wait w-claude-4 w-grok-4 w-codex-4 --status done --cat
```

- `wait --status done` requires **pad growth since inject** (not badge-only).
  Use `--badge-only` only if you intentionally skip evidence.
- `--cat` / `--harvest` prints each pad body after success (one round-trip fan-in).
- `send` returns `task_id`; roster shows `task=task-N`.

### Commands (rest)

- `new --agent claude|grok|codex|shell`  (+ `--wait-ready`)
- `send --file|--stdin` · `send-raw` · `read` (debug)
- `pad [PANE] --cat` · `note` · `finish` · `status-set` · `task`/`inbox`
- `roster`/`stage` · `brief` · `human` · `wait` · `watch` · `doctor`
- `propose` (ghost cmd) · `ask` · `seize`/`release`/`drive`
- `whoami` · `caps` · `policy` · `grant`/`revoke`
- `phone` / `telegram-topic` — open vita telegram topic for a pane (seance↔vita)
- `export-session` — scrubable HTML (timeline + pads)
- `prompts [q]` — precanned prompt library

Exit: 0 ok · 1 failed · 2 not reachable. Scope: `$SEANCE_WORKSPACE`; `--all` only if asked.

### Co-presence

Human keys always steal. Inject denied 3s after human input unless `release`/`--force`.
Exit → tombstone + status idle until `kill`.

### Rules

- Every task has a `wait` condition (or you are the worker finishing).
- Decisions via `ask`; durable text via pad/`finish`; screens are ephemeral.
- Prefer `propose` for risky shell. Don't kill panes you didn't create.
- Prefer `send --file` whenever the text has `$` or multi-line body.
"#;

fn print_help() {
    println!(
        "\
seance ctl — engage the shared human+agent space from the command line

USAGE:
    seance ctl <command> [args] [--json] [--all|--scope WS]

COMMANDS:
    list                          list panes (name, state, workspace, command)
    new  --name NAME [opts]       spawn a pane
         --cwd DIR  --agent NAME  --command CMD  --workspace WS  --file PATH
         --wait-ready             block until agent TUI accepts inject
    send PANE TEXT...             paste + submit
         --file PATH | --stdin    verbatim body (avoids shell $ expansion)
         --no-submit  --force
    send-raw PANE BYTES           raw PTY bytes (Ctrl-C = $'\\x03')
    read PANE [--lines N]         rendered screen (debug)
    status / kill / scratchpad|pad PANE
         pad PANE --cat           print pad body
    note [PANE] TEXT...           append attributed pad note
         --file PATH  --replace
    finish [PANE]                 pad body + status-set (worker bridge)
         --file PATH | --stdin  --status done  --note N  --empty-ok  --replace
    timeline [--since 10m]        attributed event log
    status-set STATE [NOTE]       agent badge
    ask \"Q\" [--choices a,b]       blocking human question
    propose PANE CMD --reason R   ghost command (Enter/Esc)
    human / brief / roster|stage  focus + dense workspace snapshot
    fork [--workspace W] [--name N]
    commands PANE / last-command PANE [--failed]
    watch [opts]                  stream events
         --kinds a,b  --pane P  --actor A  --since-seq N  --no-catch-up
    whoami / caps / grant / revoke / policy
    seize / release / drive PANE  co-presence ownership
    wait PANE [opts]              block until condition
         --status done [--cat|--harvest]  --badge-only  --task ID
         --scratchpad [--since-inject|--any-pad] --min-bytes N
         --artifact PATH  --owner none  --ready  --any  --timeout S
    task|inbox [--id ID] [PANE]   durable inject body (default: self)
    doctor                        agent profiles + binary health
    phone [PANE]                  vita telegram topic for pane (seance↔vita seam)
    export-session [--workspace W] [--out PATH]   scrubable HTML report
    prompts [query]               precanned prompt library
    skill                         full agent contract (paste into workers)
    help

GLOBAL: --json  --all  --scope WS

EXAMPLES:
    seance ctl new --name w --agent claude --wait-ready
    seance ctl send w --file /tmp/task.md          # → task=task-N
    seance ctl wait w --status done --timeout 600 --cat
    seance ctl wait a b c --status done --cat      # fan-in harvest
    seance ctl task                                # re-read my inject
    seance ctl finish --stdin --status done < /tmp/ans.md
    seance ctl roster
    seance ctl doctor"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // Ctrl-C (0x03) and CR (0x0d), the common send-raw payloads.
        assert_eq!(base64_encode(&[0x03]), "Aw==");
        assert_eq!(base64_encode(&[0x0d]), "DQ==");
    }

    #[test]
    fn parse_send_joins_text_and_defaults_submit() {
        let req = parse_send(vec![
            "worker".into(),
            "run".into(),
            "the".into(),
            "tests".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Send {
                pane,
                text,
                submit,
                ..
            } => {
                assert_eq!(pane, "worker");
                assert_eq!(text, "run the tests");
                assert!(submit);
            }
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn parse_send_file_reads_body() {
        let dir = std::env::temp_dir().join(format!("seance-send-file-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("task.md");
        std::fs::write(&path, "hello $SEANCE_SCRATCHPAD world\n").unwrap();
        let req = parse_send(vec![
            "w".into(),
            "--file".into(),
            path.to_string_lossy().into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Send { pane, text, .. } => {
                assert_eq!(pane, "w");
                assert!(text.contains("$SEANCE_SCRATCHPAD"));
                assert!(text.starts_with("hello"));
            }
            _ => panic!("expected send"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_finish_defaults() {
        let req = parse_finish(vec!["--status".into(), "done".into(), "--empty-ok".into()]).unwrap();
        match req {
            ControlRequest::Finish {
                status,
                body,
                append,
                empty_ok,
                ..
            } => {
                assert_eq!(status, "done");
                assert!(body.is_none());
                assert!(append);
                assert!(empty_ok);
            }
            _ => panic!("expected finish"),
        }
    }

    #[test]
    fn parse_send_no_submit_flag() {
        let req = parse_send(vec!["w".into(), "hi".into(), "--no-submit".into()]).unwrap();
        match req {
            ControlRequest::Send { submit, text, .. } => {
                assert!(!submit);
                assert_eq!(text, "hi");
            }
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn parse_send_requires_text() {
        assert!(parse_send(vec!["w".into()]).is_err());
        assert!(parse_send(vec![]).is_err());
    }

    #[test]
    fn parse_new_requires_name() {
        assert!(parse_new(vec![]).is_err());
        let req = parse_new(vec![
            "--name".into(),
            "w".into(),
            "--command".into(),
            "grok".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::New { name, command, .. } => {
                assert_eq!(name, "w");
                assert_eq!(command.as_deref(), Some("grok"));
            }
            _ => panic!("expected new"),
        }
    }

    #[test]
    fn parse_read_lines() {
        let req = parse_read(vec!["w".into(), "--lines".into(), "40".into()]).unwrap();
        match req {
            ControlRequest::Read { pane, lines, .. } => {
                assert_eq!(pane, "w");
                assert_eq!(lines, Some(40));
            }
            _ => panic!("expected read"),
        }
        assert!(parse_read(vec!["w".into(), "--lines".into(), "xx".into()]).is_err());
    }

    #[test]
    fn parse_send_raw_encodes() {
        let req = parse_send_raw(vec!["w".into(), "\u{3}".into()]).unwrap();
        match req {
            ControlRequest::SendRaw { pane, bytes_b64, .. } => {
                assert_eq!(pane, "w");
                assert_eq!(bytes_b64, "Aw==");
            }
            _ => panic!("expected send_raw"),
        }
    }

    #[test]
    fn single_positional_rejects_extra() {
        assert!(single_positional(vec!["a".into(), "b".into()], "kill").is_err());
        assert!(single_positional(vec![], "kill").is_err());
        assert_eq!(single_positional(vec!["a".into()], "kill").unwrap(), "a");
    }
}
