//! Human-readable ctl response formatting + help text.

use crate::control::ControlResponse;

pub(crate) fn print_ok_human(sub: &str, response: &ControlResponse) {
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
pub(crate) fn print_list(data: &serde_json::Value) {
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

pub(crate) fn print_session_rows(arr: &[serde_json::Value]) {
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
pub(crate) fn print_status(data: &serde_json::Value) {
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
pub(crate) fn print_read(data: &serde_json::Value) {
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
pub(crate) fn print_scratchpad(data: &serde_json::Value) {
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
pub(crate) fn print_new(data: &serde_json::Value) {
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
pub(crate) fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Print a newline only if the text didn't already end with one — keeps
/// `read` output clean whether or not the screen snapshot is newline-terminated.
pub(crate) fn ensure_trailing_newline(s: &str) {
    if !s.ends_with('\n') {
        println!();
    }
}

// ---------------------------------------------------------------------------
// Event-driven wait wake + boot clear + phone/export/prompts
// ---------------------------------------------------------------------------

/// Background: subscribe to the event bus and poke `tx` when something changes
/// on the panes we're waiting on. Best-effort — disconnect = poll-only wait.

pub(crate) fn print_help() {
    println!(
        "\
seance ctl — engage the shared human+agent space from the command line

USAGE:
    seance ctl <command> [args] [--json] [--all|--scope WS]

COMMANDS:
    list                          list panes (name, state, workspace, command)
    new  --name NAME [opts]       spawn a pane
         --cwd DIR  --agent NAME  --command CMD  --workspace WS
         --file PATH              file pane (live md/text viewer; no PTY)
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
    phone [PANE]                  open telegram topic + seed roster/ctl how-to
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
