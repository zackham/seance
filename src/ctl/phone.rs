//! `ctl phone` and `ctl prompts` client-side commands.

use std::path::PathBuf;

use crate::control::{ControlRequest, ControlResponse};

use super::parse::with_identity;
use super::print::{print_ok_human, str_field};
use super::{send_request, ConnectError};

pub(crate) fn run_phone(
    args: Vec<String>,
    scope: Option<String>,
    from: Option<String>,
    json_out: bool,
) -> i32 {
    let mut pane: Option<String> = None;
    let mut label: Option<String> = None;
    let mut workspace = scope.clone();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--label" | "--name" => label = it.next(),
            "--workspace" | "--ws" => workspace = it.next(),
            "--help" | "-h" => {
                println!(
                    "phone [PANE] [--label L] [--workspace WS]\n  \
                     Open a vita telegram topic and seed it with seance stage context\n  \
                     (workspace, roster, ctl how-to). No participant claim.\n  \
                     Default pane: $SEANCE_SESSION. Default workspace: pane's ws / $SEANCE_WORKSPACE."
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

    // Live stage snapshot for the seed card.
    let brief_req = with_identity(
        ControlRequest::Brief {
            scope: None,
            from: None,
        },
        workspace.clone().or_else(|| scope.clone()),
        from.clone(),
    );
    let brief_data = send_request(&brief_req).ok().and_then(|r| r.data);
    let panes_arr = brief_data
        .as_ref()
        .and_then(|d| d.get("panes"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let focus_row = panes_arr.iter().find(|p| {
        p.get("slug").and_then(|s| s.as_str()) == Some(pane.as_str())
            || p.get("name").and_then(|s| s.as_str()) == Some(pane.as_str())
    });
    let name = focus_row
        .and_then(|p| p.get("name").and_then(|s| s.as_str()))
        .unwrap_or(pane.as_str())
        .to_string();
    let ws = workspace
        .or_else(|| {
            focus_row
                .and_then(|p| p.get("workspace").and_then(|s| s.as_str()))
                .map(|s| s.to_string())
        })
        .or_else(|| std::env::var("SEANCE_WORKSPACE").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "main".into());

    // Roster lines for the seed (workspace-scoped when we can filter).
    let mut roster_lines = String::new();
    for p in &panes_arr {
        let pws = p.get("workspace").and_then(|s| s.as_str()).unwrap_or("");
        if !ws.is_empty() && pws != ws.as_str() && pws != "" {
            continue;
        }
        let slug = p.get("slug").and_then(|s| s.as_str()).unwrap_or("?");
        let nm = p.get("name").and_then(|s| s.as_str()).unwrap_or(slug);
        let st = p.get("status").and_then(|s| s.as_str()).unwrap_or("-");
        let owner = p.get("owner").and_then(|s| s.as_str()).unwrap_or("none");
        let pad_rev = p.get("pad_rev").and_then(|v| v.as_u64()).unwrap_or(0);
        let task = p
            .get("task_id")
            .and_then(|s| s.as_str())
            .unwrap_or("-");
        roster_lines.push_str(&format!(
            "· `{slug}` ({nm}) status={st} owner={owner} pad@r{pad_rev} task={task}\n"
        ));
    }
    if roster_lines.is_empty() {
        roster_lines = format!("· `{pane}` ({name}) — (brief empty; still reachable via ctl)\n");
    }

    let topic_label = label.unwrap_or_else(|| format!("seance · {ws} · {name}"));

    // Bare topic only — no register_participant.
    let open_body = serde_json::json!({
        "name": topic_label,
        "note": format!("seance workspace {ws} · focus pane {pane}"),
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
    // open_topic returns data nested under meta sometimes; unwrap data.
    let data = open_json
        .get("data")
        .cloned()
        .unwrap_or_else(|| open_json.clone());
    let topic_id = data
        .get("topic_id")
        .or_else(|| data.get("id"))
        .or_else(|| open_json.get("topic_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(topic_id) = topic_id else {
        eprintln!(
            "seance ctl phone: could not parse topic_id from: {}",
            open_json
        );
        return 1;
    };

    let link = data
        .get("link")
        .or_else(|| open_json.pointer("/data/link"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("topic_id={topic_id}"));

    let bind_path = PathBuf::from(
        shellexpand::tilde(&format!(
            "~/.local/share/seance/scratch/{pane}.telegram.json"
        ))
        .into_owned(),
    );
    let bind = serde_json::json!({
        "pane": pane,
        "workspace": ws,
        "topic_id": topic_id,
        "link": link,
        "label": topic_label,
        "created_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    if let Ok(s) = serde_json::to_string_pretty(&bind) {
        let _ = std::fs::write(&bind_path, s);
    }

    // Stage card — orientation only. Reader uses seance ctl on this host.
    let seed = format!(
        "✦ seance phone — stage card\n\
         \n\
         **workspace:** `{ws}`\n\
         **focus pane:** `{pane}` ({name})\n\
         \n\
         **roster (this circle)**\n\
         {roster}\
         \n\
         **drive from this host with seance ctl** (no special telegram bridge):\n\
         ```\n\
         seance ctl --scope {ws} roster\n\
         seance ctl --scope {ws} brief\n\
         seance ctl send {pane} --file /tmp/task.md\n\
         seance ctl wait {pane} --status done --timeout 600 --cat\n\
         seance ctl pad {pane} --cat\n\
         seance ctl read {pane} --lines 40   # debug only\n\
         seance ctl seize {pane} / release {pane}\n\
         seance ctl skill                   # full agent contract\n\
         ```\n\
         \n\
         Host has the seance daemon. Scope keeps you in this workspace unless `--all`.\n\
         Optional: status one-liners may post here when a pane goes needs-human.\n\
         This topic is **not** an exclusive participant claim — just a phone surface."
        ,
        roster = roster_lines,
    );
    let _ = run_vita_capability(
        "vita.telegram.send",
        &serde_json::json!({
            "topic_id": topic_id,
            "text": seed,
        }),
    );

    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "pane": pane,
                "workspace": ws,
                "topic_id": topic_id,
                "link": link,
                "label": topic_label,
                "bind": bind_path.to_string_lossy(),
            })
        );
    } else {
        println!("phone {pane} workspace={ws} topic={topic_id}");
        println!("bind {}", bind_path.display());
        if link.starts_with("http") {
            println!("link {link}");
        }
        eprintln!("seeded stage card (no participant claim) — use seance ctl on this host");
    }
    0
}

pub(crate) fn run_vita_capability(name: &str, input: &serde_json::Value) -> Result<String, String> {
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

/// `seance ctl prompts [query]` — list / filter precanned prompts.
pub(crate) fn run_prompts(args: Vec<String>, json_out: bool) -> i32 {
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
