//! Blocking wait / watch / boot-clear for `seance ctl`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::control::{socket_path, ControlRequest, ControlResponse};

use super::parse::{base64_encode, parse_duration_secs, single_positional, with_identity};
use super::{send_request, ConnectError};

pub(crate) fn run_wait(
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
pub(crate) fn harvest_pad_body(pane: &str, scope: &Option<String>, from: &Option<String>) -> Option<String> {
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

pub(crate) fn run_watch(request: &ControlRequest, json_out: bool) -> i32 {
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

pub(crate) fn watch_wake_loop(panes: Vec<String>, tx: std::sync::mpsc::Sender<()>) {
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
pub(crate) fn boot_clear_pane(
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
