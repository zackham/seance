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
    let sub_args: Vec<String> = it.collect();

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
        other => Err(format!("unknown subcommand '{other}' (try `seance ctl help`)")),
    };

    let request = match request {
        Ok(r) => with_identity(r, scope, from.clone()),
        Err(msg) => {
            eprintln!("seance ctl: {msg}");
            return 1;
        }
    };

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
        print_ok_human(&sub, &response);
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
        Send { pane, text, submit, .. } => Send { pane, text, submit, scope, from },
        SendRaw { pane, bytes_b64, .. } => SendRaw { pane, bytes_b64, scope, from },
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

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => name = Some(take_value(&mut it, "--name")?),
            "--cwd" => cwd = Some(take_value(&mut it, "--cwd")?),
            "--command" => command = Some(take_value(&mut it, "--command")?),
            "--workspace" => workspace = Some(take_value(&mut it, "--workspace")?),
            "--file" => file = Some(take_value(&mut it, "--file")?),
            other => return Err(format!("new: unexpected argument '{other}'")),
        }
    }

    let name = name.ok_or("new: --name is required")?;
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
    let mut positionals: Vec<String> = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--no-submit" => submit = false,
            "--submit" => submit = true,
            _ => positionals.push(arg),
        }
    }

    if positionals.is_empty() {
        return Err("send: expected SESSION and TEXT (e.g. `send worker-1 run the tests`)".into());
    }
    let pane = positionals.remove(0);
    if positionals.is_empty() {
        return Err("send: expected TEXT after the pane name".into());
    }
    let text = positionals.join(" ");

    Ok(ControlRequest::Send {
        pane,
        text,
        submit,
        scope: None,
        from: None,
    })
}

/// `send-raw SESSION BYTES` — BYTES is interpreted as a UTF-8 string and
/// base64-encoded for transport. Use shell escapes for control chars, e.g.
/// `send-raw w $'\x03'` for Ctrl-C, `send-raw w $'\r'` for a bare Enter.
fn parse_send_raw(args: Vec<String>) -> Result<ControlRequest, String> {
    let mut positionals = args;
    if positionals.len() < 2 {
        return Err(
            "send-raw: expected SESSION and BYTES (e.g. `send-raw worker-1 $'\\x03'` for Ctrl-C)"
                .into(),
        );
    }
    let pane = positionals.remove(0);
    // Remaining positionals re-joined with spaces so multi-word raw input works.
    let raw = positionals.join(" ");
    let bytes_b64 = base64_encode(raw.as_bytes());
    Ok(ControlRequest::SendRaw { pane, bytes_b64, scope: None, from: None })
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

/// `scratchpad SESSION`
fn parse_scratchpad(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "scratchpad")?;
    Ok(ControlRequest::Scratchpad { pane, scope: None, from: None })
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
        "list" | "ls" => print_list(data),
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
        let running = s.get("running").and_then(|v| v.as_bool());
        let state = match running {
            Some(true) => "running",
            Some(false) => "exited",
            None => "-",
        };
        let ws = str_field(s, "workspace");
        let cmd = str_field(s, "command");
        let mut line = format!("{name:<24} {state:<8}");
        if let Some(ws) = ws {
            line.push_str(&format!(" [{ws}]"));
        }
        if let Some(cmd) = cmd {
            line.push_str(&format!(" {cmd}"));
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

You are (or will be) inside **seance** — a multi-pane co-working space where
every pane is live on the human's screen. Agents, shells, and the human share
one circle. Work is meant to be visible, interruptible, and attributable — not
hidden in a background job. If `$SEANCE_SESSION` is set, you are in a pane
right now.

Environment:

- `$SEANCE_SESSION`    your pane id
- `$SEANCE_WORKSPACE`  your workspace — your `seance ctl` calls see and affect
                       ONLY panes in this workspace (scoping is automatic)
- `$SEANCE_SCRATCHPAD` markdown shared live with the human (flip notes on this
                       pane) — durable notes go here; screens scroll away
- `$SEANCE_SOCKET`     the control socket (the CLI finds it automatically)

Use `seance ctl` to discover, create, and drive sibling panes. The human
watches every terminal live — that visibility is the product.

### Commands

- `seance ctl list`                          panes in your workspace + state
- `seance ctl new --name N [--cwd D] [--command C]`
      spawn a pane. Default command is a plain shell; use `--command claude`
      (or codex/grok/anything) for an agent worker. Prints the pane id.
- `seance ctl send PANE TEXT...`             paste TEXT + submit (Enter after
      a settle delay). Give tasks, answer prompts. `--no-submit` stages only.
- `seance ctl send-raw PANE $'\x03'`         raw keys: `$'\x03'` Ctrl-C,
      `$'\r'` bare Enter (accept a confirmation), `$'\x1b'` Esc.
- `seance ctl read PANE [--lines N]`         the pane's rendered screen —
      your ONLY view of a worker. Read before you assume.
- `seance ctl status PANE`                   running / exited, title, popped
- `seance ctl scratchpad PANE`               path of that pane's shared notes
- `seance ctl kill PANE`                     terminate when done
- `seance ctl status-set STATE [NOTE]`       self-report: planning|working|
      blocked|needs-human|done|idle — shows as a badge the human sees
- `seance ctl ask "QUESTION" --choices a,b`  ask the human; BLOCKS until they
      click an answer in the UI (prints the answer; default 10min timeout)
- `seance ctl timeline --since 10m`          the attributed event log: every
      human action and agent ctl call. "what happened while I worked?"
- `seance ctl propose PANE CMD --reason R`   GHOST TEXT: the command appears
      dimmed at the pane's prompt; the human hits Enter to run it, Esc to
      dismiss, or types over it. BLOCKS until resolved; prints
      accepted/rejected. PREFER propose over send for anything risky.
- `seance ctl human`                         where is the human? focused pane,
      selected workspace, pending asks. Don't repaint what they're reading.
- `seance ctl fork [--workspace W] [--name N]`  fork a workspace: same panes
      (fresh processes), scratchpads copied — branch reality, try things.
- `seance ctl new --name doc --file PATH`    FILE PANE: live view of a
      document with change history. Open one when working on a file with the
      human — they see your edits appear live and can step back in history.
- `seance ctl last-command PANE [--failed]`  structured {command, cwd, exit,
      duration_ms} from shell integration — no screen-scraping. `commands
      PANE` lists recent ones. (Default shell panes only.)

Exit codes: 0 ok · 1 request failed (read stderr) · 2 seance not reachable.
Scoping: you cannot see or touch panes outside `$SEANCE_WORKSPACE`. If the
human explicitly asks for cross-workspace work, add `--all` to a call.

### The loop that works

1. Spawn:  `seance ctl new --name worker-1 --cwd /path --command claude`
2. Task:   `seance ctl send worker-1 "summarize failures in the test suite"`
3. Poll:   `seance ctl read worker-1 --lines 40` every few seconds. Idle =
   screen stopped changing and shows a prompt (`>`/`❯`) or a question.
   Answer prompts with `send` (menus often want `send-raw PANE $'\r'`).
4. Collect: have workers write results to their scratchpad
   (`echo ... >> $SEANCE_SCRATCHPAD` works inside any worker).
5. Clean up: `seance ctl kill worker-1` when its work is truly done.

Shell panes are tools too: spawn one (default command) and drive real
commands through it with `send` — the human sees exactly what ran and can
reach over, press up-arrow, and tweak your command themselves. Prefer this
over hiding work in your own subshell when the human might want to follow
along or take over.

### Rules

- Report status at transitions: `status-set working "running tests"` when you
  start, `status-set blocked "..."`/`needs-human` when stuck, `done` when
  finished. The human triages many panes by these badges.
- Decisions belong in `ask`, not buried in terminal output: use
  `seance ctl ask "Delete the old migration?" --choices yes,no` and wait.
- Never fire-and-forget: every `send` is followed by `read` until resolved.
- Never assume a worker finished because time passed — the screen is truth.
- Durable results in scratchpads; screens are ephemeral.
- Do not kill panes you did not create unless asked.
"#;

fn print_help() {
    println!(
        "\
seance ctl — engage the shared human+agent space from the command line

USAGE:
    seance ctl <command> [args] [--json]

COMMANDS:
    list                          list panes (name, state, workspace, command)
    new  --name NAME [opts]       spawn a pane
         --cwd DIR                  working directory (default: app default)
         --command CMD              command (default: shell; use claude/codex/…)
         --workspace WS             place in a named workspace
         --file PATH                file pane (live doc + history) instead of PTY
    send SESSION TEXT...          type TEXT into SESSION and submit it
         --no-submit                leave TEXT in the input, do not press Enter
    send-raw SESSION BYTES        inject raw bytes (no paste-wrap, no submit)
                                    e.g. send-raw w $'\\x03'  (Ctrl-C)
    read SESSION [--lines N]      print SESSION's visible screen (tail N lines)
    status SESSION                metadata (running/exited, title, …)
    kill SESSION                  terminate SESSION
    scratchpad SESSION            path to SESSION's shared notes file
    ask \"Q\" [--choices a,b]       ask the human (blocks until they answer)
    propose PANE CMD --reason R   ghost command for human Enter/Esc
    human                         where is the human? focus / workspace / asks
    skill                         print engagement protocol for any agent
    help                          show this help

GLOBAL:
    --json                        emit the raw JSON response instead of text

EXIT CODES:
    0  ok      1  request failed      2  cannot connect (is seance running?)

EXAMPLES:
    seance ctl new --name build --cwd ~/proj --command claude
    seance ctl send build \"run the test suite and summarize failures\"
    seance ctl read build --lines 40
    seance ctl send-raw build $'\\x03'        # Ctrl-C the running command
    cat \"$(seance ctl scratchpad build)\"     # read the shared notes"
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
