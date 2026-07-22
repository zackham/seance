//! `seance ctl …` — the control-plane CLI client.
//!
//! A thin, dependency-free command-line front end over the Unix-socket protocol
//! defined in [`crate::control`]. See crate-level docs on the original module.

mod parse;
mod phone;
mod print;
mod wait;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::control::{socket_path, ControlRequest, ControlResponse};

use parse::*;
use phone::*;
use print::*;
use wait::*;

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

### File / markdown panes (show a document live — NOT a shell)

When the human should **see a file render on the stage** (meeting notes, design
doc, task md), open a **file pane**. This is a first-class pane kind: native
markdown render, 1s mtime refresh, history ◀/▶. **Do not** fake it with bat,
less, watch, or a looping clear+cat in a terminal.

```bash
# RIGHT — native file viewer (markdown if .md)
seance ctl new --name l10-notes --file data/scratch/l10-2026-07-21-notes.md
# or absolute path
seance ctl new --name notes --file "$PWD/docs/PLAN.md"

# WRONG — terminal pane pretending to be a viewer
seance ctl new --name notes --command "bat -p --paging=never file.md"
seance ctl new --name notes --command "bash -c 'while true; do clear; bat f; sleep 1; done'"
```

After the file pane exists:
- **Edit the file on disk** with your normal tools (Write/Edit). The pane updates itself.
- Do **not** `ctl send` into a file pane — there is **no PTY**.
- `ctl read notes` shows a text extract for debugging; the human already sees the render.
- Pad/scratchpad is separate (`$SEANCE_SCRATCHPAD`); file panes are for **named docs**.

`new --file` and `new --agent`/`--command` are mutually exclusive shapes: file
viewer vs process. Roster `kind` is `file` vs `terminal`.

### Commands (rest)

- `new --agent claude|grok|codex|shell`  (+ `--wait-ready`)
- `new --file PATH` — **file pane** (live markdown/text viewer; no shell)
- `send --file|--stdin` · `send-raw` · `read` (debug)
- `pad [PANE] --cat` · `note` · `finish` · `status-set` · `task`/`inbox`
- `roster`/`stage` · `brief` · `human` · `wait` · `watch` · `doctor`
- `propose` (ghost cmd) · `ask` · `seize`/`release`/`drive`
- `whoami` · `caps` · `policy` · `grant`/`revoke`
- `phone` / `telegram-topic` — open vita telegram topic + seed stage card (no participant claim)
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
- Live docs for the human → **`new --file`**, never bat/watch loops.
"#;

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
        scope_override.or_else(|| {
            std::env::var("SEANCE_WORKSPACE")
                .ok()
                .filter(|s| !s.is_empty())
        })
    };
    let from: Option<String> = std::env::var("SEANCE_SESSION")
        .ok()
        .filter(|s| !s.is_empty());

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
        "list" | "ls" => Ok(ControlRequest::List {
            scope: None,
            from: None,
        }),
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
            Some(id) => Ok(ControlRequest::ProposeResult {
                id: id.clone(),
                scope: None,
                from: None,
            }),
            None => Err("propose-result: expected PROPOSAL_ID".into()),
        },
        "human" | "whereis-human" => Ok(ControlRequest::Human {
            scope: None,
            from: None,
        }),
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
            Some(exit) => Ok(ControlRequest::CmdEnd {
                exit,
                scope: None,
                from: None,
            }),
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
                Some(pane) => Ok(ControlRequest::Commands {
                    pane,
                    limit,
                    scope: None,
                    from: None,
                }),
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
                Some(pane) => Ok(ControlRequest::LastCommand {
                    pane,
                    failed_only,
                    scope: None,
                    from: None,
                }),
                None => Err("last-command: expected PANE".into()),
            }
        }
        "ask-result" => match sub_args.first() {
            Some(id) => Ok(ControlRequest::AskResult {
                id: id.clone(),
                scope: None,
                from: None,
            }),
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
        "doctor" => Ok(ControlRequest::Doctor {
            scope: None,
            from: None,
        }),
        "brief" => Ok(ControlRequest::Brief {
            scope: None,
            from: None,
        }),
        "roster" | "stage" => Ok(ControlRequest::Roster {
            scope: None,
            from: None,
        }),
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
        "prompts" => {
            return run_prompts(sub_args, json_out);
        }
        other => Err(format!(
            "unknown subcommand '{other}' (try `seance ctl help`)"
        )),
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
            let path = response.data.as_ref().and_then(|d| {
                d.as_str().map(|s| s.to_string()).or_else(|| {
                    d.get("path")
                        .and_then(|p| p.as_str())
                        .map(|s| s.to_string())
                })
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

pub(crate) enum ConnectError {
    /// Couldn't open the socket at all — app almost certainly not running.
    Connect(std::io::Error),
    /// Connected, but IO failed mid-exchange.
    Io(std::io::Error),
    /// Got bytes, but they weren't a valid response line.
    Protocol(String),
}

/// Open the socket, send one request line, read one response line.
pub(crate) fn send_request(request: &ControlRequest) -> Result<ControlResponse, ConnectError> {
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
    let n = reader.read_line(&mut resp_line).map_err(ConnectError::Io)?;
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
                pane, text, submit, ..
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
        let req =
            parse_finish(vec!["--status".into(), "done".into(), "--empty-ok".into()]).unwrap();
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
            ControlRequest::SendRaw {
                pane, bytes_b64, ..
            } => {
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

    #[test]
    fn parse_duration_secs_units() {
        assert_eq!(parse_duration_secs("30").unwrap(), 30);
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("10m").unwrap(), 600);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("1d").unwrap(), 86400);
        assert!(parse_duration_secs("xm").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    #[test]
    fn parse_timeline_flags() {
        let req = parse_timeline(vec![
            "--since".into(),
            "10m".into(),
            "--pane".into(),
            "w".into(),
            "--actor".into(),
            "cli".into(),
            "--limit".into(),
            "5".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Timeline {
                since_secs,
                pane,
                actor,
                limit,
                ..
            } => {
                assert_eq!(since_secs, Some(600));
                assert_eq!(pane.as_deref(), Some("w"));
                assert_eq!(actor.as_deref(), Some("cli"));
                assert_eq!(limit, Some(5));
            }
            _ => panic!("expected timeline"),
        }
        assert!(parse_timeline(vec!["--bogus".into()]).is_err());
        assert!(parse_timeline(vec!["--since".into()]).is_err());
    }

    #[test]
    fn parse_status_set_state_and_note() {
        let req = parse_status_set(vec![
            "blocked".into(),
            "waiting".into(),
            "on".into(),
            "review".into(),
            "--pane".into(),
            "w1".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::StatusSet {
                state, note, pane, ..
            } => {
                assert_eq!(state, "blocked");
                assert_eq!(note.as_deref(), Some("waiting on review"));
                assert_eq!(pane.as_deref(), Some("w1"));
            }
            _ => panic!("expected status_set"),
        }
        assert!(parse_status_set(vec![]).is_err());
    }

    #[test]
    fn parse_propose_and_ask() {
        let req = parse_propose(vec![
            "w".into(),
            "run".into(),
            "tests".into(),
            "--reason".into(),
            "ci".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Propose {
                pane, text, reason, ..
            } => {
                assert_eq!(pane, "w");
                assert_eq!(text, "run tests");
                assert_eq!(reason.as_deref(), Some("ci"));
            }
            _ => panic!("expected propose"),
        }
        assert!(parse_propose(vec!["only-pane".into()]).is_err());

        let ask = parse_ask(vec![
            "ship".into(),
            "it?".into(),
            "--choices".into(),
            "yes, no, later".into(),
        ])
        .unwrap();
        match ask {
            ControlRequest::Ask {
                question, choices, ..
            } => {
                assert_eq!(question, "ship it?");
                assert_eq!(
                    choices.as_ref().map(|c| c.as_slice()),
                    Some(["yes".to_string(), "no".to_string(), "later".to_string()].as_slice())
                );
            }
            _ => panic!("expected ask"),
        }
        assert!(parse_ask(vec![]).is_err());
    }

    #[test]
    fn parse_fork_flags() {
        let req = parse_fork(vec![
            "--workspace".into(),
            "lab".into(),
            "--name".into(),
            "lab-2".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::WorkspaceFork {
                workspace, name, ..
            } => {
                assert_eq!(workspace.as_deref(), Some("lab"));
                assert_eq!(name.as_deref(), Some("lab-2"));
            }
            _ => panic!("expected fork"),
        }
        assert!(parse_fork(vec!["stray".into()]).is_err());
    }

    #[test]
    fn parse_note_replace_and_file() {
        let dir = std::env::temp_dir().join(format!("seance-note-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("n.md");
        std::fs::write(&path, "file body").unwrap();
        // Ensure outside-pane path so first token can be pane.
        let prev = std::env::var("SEANCE_SESSION").ok();
        std::env::remove_var("SEANCE_SESSION");
        let req = parse_note(vec![
            "w".into(),
            "inline".into(),
            "text".into(),
            "--replace".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Note {
                pane, text, append, ..
            } => {
                assert_eq!(pane.as_deref(), Some("w"));
                assert_eq!(text, "inline text");
                assert!(!append);
            }
            _ => panic!("expected note"),
        }
        let req = parse_note(vec!["--file".into(), path.to_string_lossy().into()]).unwrap();
        match req {
            ControlRequest::Note { text, .. } => assert_eq!(text, "file body"),
            _ => panic!("expected note"),
        }
        assert!(parse_note(vec![]).is_err());
        match prev {
            Some(v) => std::env::set_var("SEANCE_SESSION", v),
            None => std::env::remove_var("SEANCE_SESSION"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_seize_release_drive() {
        let seize = parse_seize(vec!["w".into(), "--as".into(), "human".into()]).unwrap();
        match seize {
            ControlRequest::Seize { pane, as_owner, .. } => {
                assert_eq!(pane, "w");
                assert_eq!(as_owner.as_deref(), Some("human"));
            }
            _ => panic!("expected seize"),
        }
        assert!(parse_seize(vec![]).is_err());

        let rel = parse_release(vec!["w".into()]).unwrap();
        assert!(matches!(rel, ControlRequest::Release { pane, .. } if pane == "w"));

        let drive = parse_drive(vec!["w".into(), "locked_human".into()]).unwrap();
        match drive {
            ControlRequest::DriveMode { pane, mode, .. } => {
                assert_eq!(pane, "w");
                assert_eq!(mode, "locked_human");
            }
            _ => panic!("expected drive"),
        }
        assert!(parse_drive(vec!["w".into()]).is_err());
    }

    #[test]
    fn parse_task_id_and_pane() {
        let req = parse_task(vec!["--id".into(), "t42".into(), "worker".into()]).unwrap();
        match req {
            ControlRequest::Task { pane, id, .. } => {
                assert_eq!(pane.as_deref(), Some("worker"));
                assert_eq!(id.as_deref(), Some("t42"));
            }
            _ => panic!("expected task"),
        }
        let empty = parse_task(vec![]).unwrap();
        match empty {
            ControlRequest::Task { pane, id, .. } => {
                assert!(pane.is_none());
                assert!(id.is_none());
            }
            _ => panic!("expected task"),
        }
    }

    #[test]
    fn parse_watch_flags() {
        let req = parse_watch(vec![
            "--since-seq".into(),
            "10".into(),
            "--kinds".into(),
            "status_set,ctl_send".into(),
            "--pane".into(),
            "w".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Watch {
                since_seq,
                kinds,
                pane,
                catch_up,
                ..
            } => {
                assert_eq!(since_seq, Some(10));
                assert_eq!(
                    kinds.as_ref().map(|k| k.as_slice()),
                    Some(["status_set".to_string(), "ctl_send".to_string()].as_slice())
                );
                assert_eq!(pane.as_deref(), Some("w"));
                assert!(catch_up); // default
            }
            _ => panic!("expected watch"),
        }
        let no_cu = parse_watch(vec!["--no-catch-up".into()]).unwrap();
        match no_cu {
            ControlRequest::Watch { catch_up, .. } => assert!(!catch_up),
            _ => panic!("expected watch"),
        }
        assert!(parse_watch(vec!["--bogus".into()]).is_err());
    }

    #[test]
    fn parse_grant_revoke_policy() {
        let g = parse_grant(vec![
            "agent:w".into(),
            "send".into(),
            "--workspace".into(),
            "lab".into(),
            "--ttl".into(),
            "3600".into(),
        ])
        .unwrap();
        match g {
            ControlRequest::CapsGrant {
                principal,
                cap,
                workspace,
                ttl_secs,
                ..
            } => {
                assert_eq!(principal, "agent:w");
                assert_eq!(cap, "send");
                assert_eq!(workspace.as_deref(), Some("lab"));
                assert_eq!(ttl_secs, Some(3600));
            }
            _ => panic!("expected grant"),
        }

        let r = parse_revoke(vec!["agent:w".into(), "send".into()]).unwrap();
        match r {
            ControlRequest::CapsRevoke { principal, cap, .. } => {
                assert_eq!(principal, "agent:w");
                assert_eq!(cap, "send");
            }
            _ => panic!("expected revoke"),
        }

        let p = parse_policy(vec![
            "propose_required".into(),
            "--workspace".into(),
            "lab".into(),
        ])
        .unwrap();
        match p {
            ControlRequest::PolicySet {
                mode, workspace, ..
            } => {
                assert_eq!(mode, "propose_required");
                assert_eq!(workspace.as_deref(), Some("lab"));
            }
            _ => panic!("expected policy set"),
        }
    }

    #[test]
    fn parse_new_rejects_file_with_agent() {
        assert!(parse_new(vec![
            "--name".into(),
            "n".into(),
            "--file".into(),
            "x.md".into(),
            "--agent".into(),
            "claude".into(),
        ])
        .is_err());
        assert!(parse_new(vec![
            "--name".into(),
            "n".into(),
            "--agent".into(),
            "claude".into(),
            "--command".into(),
            "bash".into(),
        ])
        .is_err());
    }

    #[test]
    fn with_identity_stamps_scope_and_from() {
        let req = ControlRequest::Send {
            pane: "w".into(),
            text: "hi".into(),
            submit: true,
            force: false,
            scope: None,
            from: None,
        };
        let stamped = with_identity(req, Some("lab".into()), Some("orch".into()));
        match stamped {
            ControlRequest::Send { scope, from, .. } => {
                assert_eq!(scope.as_deref(), Some("lab"));
                assert_eq!(from.as_deref(), Some("orch"));
            }
            _ => panic!("expected send"),
        }
    }

    #[test]
    fn parse_finish_task_and_note() {
        // Outside a pane, a bare slug-like token is the pane id; remaining
        // positionals become body. Multi-word body after --pane is explicit.
        let req = parse_finish(vec![
            "--pane".into(),
            "w".into(),
            "--status".into(),
            "blocked".into(),
            "--note".into(),
            "stuck".into(),
            "--task".into(),
            "t1".into(),
            "body".into(),
            "text".into(),
        ])
        .unwrap();
        match req {
            ControlRequest::Finish {
                pane,
                status,
                status_note,
                task,
                body,
                empty_ok,
                ..
            } => {
                assert_eq!(pane.as_deref(), Some("w"));
                assert_eq!(status, "blocked");
                assert_eq!(status_note.as_deref(), Some("stuck"));
                assert_eq!(task.as_deref(), Some("t1"));
                assert_eq!(body.as_deref(), Some("body text"));
                assert!(!empty_ok);
            }
            _ => panic!("expected finish"),
        }
    }

    #[test]
    fn parse_kill_and_status() {
        let k = parse_kill(vec!["w".into()]).unwrap();
        assert!(matches!(k, ControlRequest::Kill { pane, .. } if pane == "w"));
        let s = parse_status(vec!["w".into()]).unwrap();
        assert!(matches!(s, ControlRequest::Status { pane, .. } if pane == "w"));
        assert!(parse_kill(vec![]).is_err());
    }
}
