//! CLI argument parsers → [`ControlRequest`].

use crate::control::ControlRequest;

pub(crate) fn parse_timeline(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn parse_duration_secs(v: &str) -> Result<u64, String> {
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
pub(crate) fn parse_status_set(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_propose(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_fork(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_ask(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn with_identity(
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

/// `new --name NAME [--cwd DIR] [--command CMD|--agent A|--file PATH] [--workspace WS]`
pub(crate) fn parse_new(args: Vec<String>) -> Result<ControlRequest, String> {
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
    if file.is_some() && (agent.is_some() || command.is_some()) {
        return Err(
            "new: --file is a file pane (viewer); do not combine with --agent/--command. \
             Example: seance ctl new --name notes --file path/to/doc.md"
                .into(),
        );
    }
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
pub(crate) fn parse_send(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn parse_note(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn parse_finish(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_send_raw(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_read(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_status(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "status")?;
    Ok(ControlRequest::Status { pane, scope: None, from: None })
}

/// `kill SESSION`
pub(crate) fn parse_kill(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "kill")?;
    Ok(ControlRequest::Kill { pane, scope: None, from: None })
}

/// `scratchpad [SESSION]` — defaults to `$SEANCE_SESSION` when unset.
pub(crate) fn parse_scratchpad(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_task(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn parse_seize(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn parse_release(args: Vec<String>) -> Result<ControlRequest, String> {
    let pane = single_positional(args, "release")?;
    Ok(ControlRequest::Release {
        pane,
        scope: None,
        from: None,
    })
}

pub(crate) fn parse_drive(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn parse_watch(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_grant(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_revoke(args: Vec<String>) -> Result<ControlRequest, String> {
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
pub(crate) fn parse_policy(args: Vec<String>) -> Result<ControlRequest, String> {
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

pub(crate) fn single_positional(args: Vec<String>, cmd: &str) -> Result<String, String> {
    let mut positionals: Vec<String> = args.into_iter().filter(|a| !a.starts_with('-')).collect();
    match positionals.len() {
        0 => Err(format!("{cmd}: expected a SESSION name")),
        1 => Ok(positionals.remove(0)),
        _ => Err(format!("{cmd}: expected exactly one SESSION name")),
    }
}

/// Consume the next iterator item as a flag's value, or explain its absence.
pub(crate) fn take_value(it: &mut std::vec::IntoIter<String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} expects a value"))
}

pub(crate) fn base64_encode(input: &[u8]) -> String {
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
