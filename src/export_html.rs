//! Session export → scrubable offline HTML (teaching / share medium).
//!
//! # v1 design (adversarial budget)
//!
//! - **Not** full 60fps TUI replay (privacy, size, fidelity cliffs).
//! - Durable signal: attributed events + pads + tasks + cmdlog.
//! - Events embedded as **JSON** once; client virtual-renders rows (no 5k `<tr>`).
//! - Caps + priority sampling for huge logs.
//! - Optional `--share` publishes via vita reports (PIN-capable).

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::events::{self, Event};

/// Hard caps so a day-long swarm session stays shareable.
const MAX_EVENTS: usize = 8_000;
const MAX_PAD_CHARS: usize = 120_000;
const MAX_TASK_CHARS: usize = 12_000;
const TARGET_HTML_WARN_BYTES: usize = 2_500_000;

/// Export options.
#[derive(Clone, Debug)]
pub struct ExportOpts {
    pub workspace: Option<String>,
    pub title: String,
    pub out: Option<PathBuf>,
    /// Drop file paths under /home/ from event details & pads (teaching share).
    pub redact_paths: bool,
    /// Publish via vita-reports after write (requires ~/work/vita).
    pub share: bool,
    pub pin: Option<String>,
    /// Open the HTML with xdg-open after write.
    pub open: bool,
}

impl Default for ExportOpts {
    fn default() -> Self {
        Self {
            workspace: None,
            title: "seance session".into(),
            out: None,
            redact_paths: false,
            share: false,
            pin: None,
            open: false,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ExportMeta {
    pub title: String,
    pub workspace: String,
    pub generated_ms: u64,
    pub event_count: usize,
    pub events_sampled: bool,
    pub pane_count: usize,
    pub cmd_count: usize,
    pub pad_count: usize,
    pub gen_ms: u64,
    pub html_bytes: usize,
    pub seance_version: &'static str,
}

/// Bundle a workspace (or all) into a self-contained HTML file.
pub fn export_session(
    workspace: Option<&str>,
    out: &Path,
    title: &str,
) -> Result<PathBuf, String> {
    export_with_opts(ExportOpts {
        workspace: workspace.map(|s| s.to_string()),
        title: title.to_string(),
        out: Some(out.to_path_buf()),
        ..Default::default()
    })
    .map(|r| r.path)
}

pub struct ExportResult {
    pub path: PathBuf,
    pub meta: ExportMeta,
    pub share_url: Option<String>,
}

pub fn export_with_opts(opts: ExportOpts) -> Result<ExportResult, String> {
    let t0 = Instant::now();
    let workspace = opts.workspace.as_deref();

    // Prefer live ring + disk; cap hard.
    let mut entries = events::read(0, workspace, None, None, MAX_EVENTS.saturating_mul(2));
    let events_sampled = sample_events(&mut entries, MAX_EVENTS);

    let state = crate::state::AppState::load();
    let panes: Vec<_> = state
        .panes
        .iter()
        .filter(|p| workspace.is_none_or(|w| p.workspace == w))
        .cloned()
        .collect();

    let pad_dir = PathBuf::from(shellexpand::tilde("~/.local/share/seance/scratch").into_owned());

    // Pads (truncated).
    let mut pads_json = Vec::new();
    let mut pad_count = 0usize;
    for p in &panes {
        let pad_path = pad_dir.join(format!("{}.md", p.slug));
        let mut body = std::fs::read_to_string(&pad_path).unwrap_or_default();
        if body.trim().is_empty() {
            continue;
        }
        if body.len() > MAX_PAD_CHARS {
            let mut start = body.len().saturating_sub(MAX_PAD_CHARS);
            while start > 0 && !body.is_char_boundary(start) {
                start -= 1;
            }
            body = format!(
                "…[truncated {} chars]\n{}",
                body.len() - (body.len() - start),
                &body[start..]
            );
        }
        if opts.redact_paths {
            body = redact_paths(&body);
        }
        pad_count += 1;
        pads_json.push(serde_json::json!({
            "slug": p.slug,
            "name": p.name,
            "body": body,
        }));
    }

    // Tasks from state (active + recent).
    let mut tasks_json = Vec::new();
    for t in &state.tasks {
        if workspace.is_some_and(|w| {
            !panes.iter().any(|p| p.slug == t.pane && p.workspace == w)
        }) {
            continue;
        }
        let mut body = t.body.clone();
        if body.len() > MAX_TASK_CHARS {
            let mut end = MAX_TASK_CHARS;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            body.push_str("…");
        }
        if opts.redact_paths {
            body = redact_paths(&body);
        }
        tasks_json.push(serde_json::json!({
            "id": t.id,
            "pane": t.pane,
            "status": t.status,
            "body": body,
            "created_ms": t.created_ms,
            "finished_ms": t.finished_ms,
        }));
    }

    // Cmdlog from cold state (handoff-persisted 0.9.11+).
    let mut cmds_json = Vec::new();
    let mut cmd_count = 0usize;
    for slug in state.cmd_log.pane_slugs() {
        if workspace.is_some_and(|w| !panes.iter().any(|p| p.slug == slug && p.workspace == w)) {
            continue;
        }
        for rec in state.cmd_log.list(&slug, 80) {
            cmd_count += 1;
            let mut command = rec.command.clone();
            if opts.redact_paths {
                command = redact_paths(&command);
            }
            cmds_json.push(serde_json::json!({
                "pane": slug,
                "seq": rec.seq,
                "command": command,
                "cwd": if opts.redact_paths { redact_paths(&rec.cwd) } else { rec.cwd.clone() },
                "started_ms": rec.started_ms,
                "ended_ms": rec.ended_ms,
                "exit": rec.exit,
            }));
        }
    }

    let mut events_json = Vec::with_capacity(entries.len());
    for e in &entries {
        let mut detail = e.detail.clone();
        if opts.redact_paths {
            detail = redact_paths(&detail);
        }
        events_json.push(serde_json::json!({
            "ts": e.ts,
            "time": events::fmt_time(e.ts),
            "actor": e.actor,
            "pane": e.pane.clone().unwrap_or_else(|| "-".into()),
            "kind": e.kind,
            "detail": detail,
            "workspace": e.workspace,
        }));
    }

    let mut roster = Vec::new();
    for p in &panes {
        roster.push(serde_json::json!({
            "name": p.name,
            "slug": p.slug,
            "workspace": p.workspace,
            "kind": p.kind,
            "command": if opts.redact_paths { redact_paths(&p.command) } else { p.command.clone() },
            "status": p.status.clone().unwrap_or_else(|| "-".into()),
            "pad_rev": p.pad_rev,
        }));
    }

    let ws_label = workspace.unwrap_or("(all workspaces)");
    let data = serde_json::json!({
        "meta": {
            "title": opts.title,
            "workspace": ws_label,
            "seance_version": env!("CARGO_PKG_VERSION"),
        },
        "roster": roster,
        "events": events_json,
        "pads": pads_json,
        "tasks": tasks_json,
        "commands": cmds_json,
    });
    let data_json = serde_json::to_string(&data).map_err(|e| e.to_string())?;
    // Escape for embedding in <script type="application/json">
    let data_json_safe = data_json.replace("</", "<\\/");

    let html = render_shell(&opts.title, ws_label, &data_json_safe, events_sampled, entries.len());
    let path = opts
        .out
        .clone()
        .unwrap_or_else(|| default_out_path(workspace));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, &html).map_err(|e| e.to_string())?;

    let html_bytes = html.len();
    let gen_ms = t0.elapsed().as_millis() as u64;
    let meta = ExportMeta {
        title: opts.title.clone(),
        workspace: ws_label.to_string(),
        generated_ms: now_ms(),
        event_count: entries.len(),
        events_sampled,
        pane_count: panes.len(),
        cmd_count,
        pad_count,
        gen_ms,
        html_bytes,
        seance_version: env!("CARGO_PKG_VERSION"),
    };

    if html_bytes > TARGET_HTML_WARN_BYTES {
        eprintln!(
            "seance export: warning — HTML is {html_bytes} bytes (> {TARGET_HTML_WARN_BYTES}); consider --workspace or --redact"
        );
    }

    let mut share_url = None;
    if opts.share {
        share_url = Some(publish_via_vita(&path, &opts)?);
    }
    if opts.open {
        let _ = std::process::Command::new("xdg-open")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    Ok(ExportResult {
        path,
        meta,
        share_url,
    })
}

/// Prefer high-signal events when over cap: finish/status/send/ask first, then fill.
fn sample_events(entries: &mut Vec<Event>, max: usize) -> bool {
    if entries.len() <= max {
        return false;
    }
    let priority = |k: &str| -> i32 {
        match k {
            "finish" | "status_set" | "send" | "ask" | "ask_resolved" | "cmd_end" | "cmd_start" => {
                0
            }
            "pane_spawned" | "pane_exited" | "pane_killed" | "agency" => 1,
            _ => 2,
        }
    };
    entries.sort_by(|a, b| {
        priority(&a.kind)
            .cmp(&priority(&b.kind))
            .then_with(|| a.ts.cmp(&b.ts))
    });
    // Keep first `max` after priority sort, then re-sort chronologically for scrubber.
    entries.truncate(max);
    entries.sort_by_key(|e| e.ts);
    true
}

fn redact_paths(s: &str) -> String {
    // Cheap: collapse /home/<user>/… and /Users/…
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/home/") || bytes[i..].starts_with(b"/Users/") {
            out.push_str("~/");
            // skip /home/user or /Users/user
            let rest = if bytes[i..].starts_with(b"/home/") {
                &bytes[i + 6..]
            } else {
                &bytes[i + 7..]
            };
            let mut j = 0;
            while j < rest.len() && rest[j] != b'/' && !rest[j].is_ascii_whitespace() {
                j += 1;
            }
            // skip username
            let after = &rest[j..];
            if after.first() == Some(&b'/') {
                // keep relative path after home
                let mut k = 1;
                while k < after.len()
                    && !after[k].is_ascii_whitespace()
                    && after[k] != b'\''
                    && after[k] != b'"'
                {
                    k += 1;
                }
                out.push_str(std::str::from_utf8(&after[1..k]).unwrap_or("…"));
                i += if bytes[i..].starts_with(b"/home/") {
                    6 + j + k
                } else {
                    7 + j + k
                };
            } else {
                i += if bytes[i..].starts_with(b"/home/") {
                    6 + j
                } else {
                    7 + j
                };
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn publish_via_vita(html_path: &Path, opts: &ExportOpts) -> Result<String, String> {
    let vita = PathBuf::from(shellexpand::tilde("~/work/vita").into_owned());
    let reports = vita.join("data/reports");
    std::fs::create_dir_all(&reports).map_err(|e| e.to_string())?;
    let stamp = chrono_day();
    let slug = format!(
        "{}-seance-{}",
        stamp,
        opts.workspace
            .as_deref()
            .map(slugify_simple)
            .unwrap_or_else(|| "session".into())
    );
    let dest = reports.join(format!("{slug}.html"));
    std::fs::copy(html_path, &dest).map_err(|e| format!("copy to reports: {e}"))?;
    // Minimal companion markdown so report registry can find a slug.
    let md = reports.join(format!("{slug}.md"));
    if !md.exists() {
        let body = format!(
            "---\ntitle: \"{}\"\ndate: {}\nproject: seance\ntags: [seance, session-export]\n---\n\n# {}\n\nExported seance session HTML: `{slug}.html`\n",
            opts.title, stamp, opts.title
        );
        let _ = std::fs::write(&md, body);
    }
    let mut cmd = std::process::Command::new("uv");
    cmd.current_dir(&vita)
        .env("PYTHONPATH", "scripts")
        .args([
            "run",
            "python",
            "-m",
            "reports.publish",
            &slug,
            "--format",
            "html",
        ]);
    if let Some(pin) = &opts.pin {
        cmd.args(["--pin", pin]);
    }
    let out = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("spawn publish: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!(
            "vita publish failed (exit {:?}): {stderr}{stdout}",
            out.status.code()
        ));
    }
    // Extract https://vita-reports.ham.xyz/s/… from output
    for token in stdout.split_whitespace().chain(stderr.split_whitespace()) {
        if token.contains("vita-reports.ham.xyz/s/") {
            return Ok(token.trim_matches(|c: char| c == '"' || c == '\'').to_string());
        }
    }
    // Fallback: search for /s/TOKEN
    if let Some(idx) = stdout.find("/s/") {
        let rest = &stdout[idx..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(rest.len());
        return Ok(format!("https://vita-reports.ham.xyz{}", &rest[..end]));
    }
    Ok(format!("published slug={slug} (see publish output)"))
}

fn chrono_day() -> String {
    // Local date YYYY-MM-DD without chrono crate.
    let secs = now_ms() / 1000;
    // Approximate UTC date; good enough for slug.
    let days = secs / 86400;
    // 1970-01-01 + days — use a small algorithm
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn render_shell(
    title: &str,
    ws: &str,
    data_json: &str,
    sampled: bool,
    n_events: usize,
) -> String {
    let sample_note = if sampled {
        format!(" · sampled to {n_events} high-signal events")
    } else {
        format!(" · {n_events} events")
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>{title}</title>
<style>
:root {{
  --bg:#1a1410; --fg:#e8dcc8; --dim:#8a7a68; --flame:#e8a060;
  --violet:#a890d0; --line:#3a3028; --surf:#241c16;
  font-family: "Iosevka", "JetBrains Mono", ui-monospace, monospace;
}}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--bg); color:var(--fg); line-height:1.45; }}
header {{ padding:1rem 1.25rem; border-bottom:1px solid var(--line);
  background:linear-gradient(180deg,#221810,var(--bg)); position:sticky; top:0; z-index:2; }}
header h1 {{ margin:0; font-size:1.1rem; color:var(--flame); font-weight:600; }}
header p {{ margin:0.3rem 0 0; color:var(--dim); font-size:0.8rem; }}
.controls {{ display:flex; gap:0.75rem; flex-wrap:wrap; margin-top:0.55rem; align-items:center; }}
input[type=range] {{ width:min(420px, 70vw); }}
label {{ color:var(--dim); font-size:0.78rem; }}
.scrub-info {{ color:var(--flame); font-size:0.82rem; min-width:10rem; }}
main {{ display:grid; grid-template-columns: minmax(280px,1fr) 1.5fr; gap:0; min-height:calc(100vh - 5.5rem); }}
@media (max-width: 900px) {{ main {{ grid-template-columns: 1fr; }} }}
section {{ padding:0.9rem 1.1rem; border-bottom:1px solid var(--line); }}
h2 {{ margin:0 0 0.6rem; font-size:0.75rem; letter-spacing:0.08em;
  text-transform:uppercase; color:var(--violet); }}
table {{ width:100%; border-collapse:collapse; font-size:0.76rem; }}
th, td {{ text-align:left; padding:0.22rem 0.35rem; border-bottom:1px solid var(--line);
  vertical-align:top; }}
th {{ color:var(--dim); font-weight:500; }}
td.t {{ color:var(--dim); white-space:nowrap; }}
td.a {{ color:var(--flame); max-width:7rem; overflow:hidden; }}
td.p {{ color:var(--violet); }}
td.k {{ color:var(--dim); }}
pre {{ white-space:pre-wrap; word-break:break-word; background:var(--surf);
  padding:0.65rem 0.85rem; border-radius:6px; border:1px solid var(--line);
  font-size:0.78rem; max-height:22rem; overflow:auto; }}
.pad h3 {{ font-size:0.88rem; margin:0 0 0.4rem; }}
.pad h3 code {{ color:var(--dim); font-size:0.72rem; }}
#timeline-body {{ display:block; max-height:calc(100vh - 8rem); overflow:auto; }}
footer {{ padding:0.85rem 1.25rem; color:var(--dim); font-size:0.72rem; border-top:1px solid var(--line); }}
.stat {{ color:var(--dim); font-size:0.75rem; }}
</style>
</head>
<body>
<header>
  <h1>✦ {title_esc}</h1>
  <p>seance session export · workspace <strong>{ws_esc}</strong>{sample_note} · offline scrubber v1</p>
  <div class="controls">
    <label>scrub <input id="scrub" type="range" min="0" max="0" value="0"/></label>
    <span class="scrub-info" id="scrub-label">—</span>
    <label><input type="checkbox" id="only-agents"/> agents only</label>
    <label><input type="checkbox" id="only-hi" checked/> high-signal kinds</label>
    <input id="filter" type="search" placeholder="filter detail…" style="background:var(--surf);border:1px solid var(--line);color:var(--fg);padding:0.25rem 0.5rem;border-radius:4px;min-width:10rem"/>
  </div>
</header>
<main>
  <div>
    <section>
      <h2>roster</h2>
      <div id="roster"></div>
    </section>
    <section>
      <h2>tasks</h2>
      <div id="tasks"></div>
    </section>
    <section>
      <h2>commands</h2>
      <div id="commands"></div>
    </section>
    <section>
      <h2>pads</h2>
      <div id="pads"></div>
    </section>
  </div>
  <section>
    <h2>timeline</h2>
    <table>
      <thead><tr><th>time</th><th>actor</th><th>pane</th><th>kind</th><th>detail</th></tr></thead>
    </table>
    <div id="timeline-body"><table><tbody id="tl"></tbody></table></div>
  </section>
</main>
<footer>seance export-session v1 · not full TUI replay · events JSON embedded once · virtual scrub</footer>
<script id="data" type="application/json">{data}</script>
<script>
const DATA = JSON.parse(document.getElementById('data').textContent);
const HI = new Set(['finish','status_set','send','ask','ask_resolved','cmd_end','cmd_start','pane_exited','pane_spawned']);
const events = DATA.events || [];
const scrub = document.getElementById('scrub');
const label = document.getElementById('scrub-label');
const onlyAgents = document.getElementById('only-agents');
const onlyHi = document.getElementById('only-hi');
const filter = document.getElementById('filter');
const tl = document.getElementById('tl');

function esc(s) {{
  return String(s ?? '').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}}

// Roster
document.getElementById('roster').innerHTML = `
<table><thead><tr><th>name</th><th>slug</th><th>ws</th><th>status</th><th>cmd</th></tr></thead>
<tbody>${{(DATA.roster||[]).map(r => `<tr>
<td>${{esc(r.name)}}</td><td><code>${{esc(r.slug)}}</code></td><td>${{esc(r.workspace)}}</td>
<td>${{esc(r.status)}}</td><td class="cmd">${{esc(r.command)}}</td></tr>`).join('')}}
</tbody></table>`;

// Tasks
document.getElementById('tasks').innerHTML = (DATA.tasks||[]).length
  ? (DATA.tasks||[]).map(t => `<div class="pad"><h3>${{esc(t.id)}} · ${{esc(t.pane)}} · ${{esc(t.status)}}</h3><pre>${{esc(t.body)}}</pre></div>`).join('')
  : '<p class="stat">(no tasks)</p>';

// Commands
document.getElementById('commands').innerHTML = (DATA.commands||[]).length
  ? `<table><thead><tr><th>pane</th><th>exit</th><th>command</th></tr></thead><tbody>
${{(DATA.commands||[]).map(c => `<tr><td>${{esc(c.pane)}}</td><td>${{c.exit ?? '…'}}</td><td>${{esc(c.command)}}</td></tr>`).join('')}}
</tbody></table>`
  : '<p class="stat">(no cmdlog — needs 0.9.11+ handoff persist)</p>';

// Pads
document.getElementById('pads').innerHTML = (DATA.pads||[]).length
  ? (DATA.pads||[]).map(p => `<div class="pad" id="pad-${{esc(p.slug)}}"><h3>${{esc(p.name)}} <code>${{esc(p.slug)}}</code></h3><pre>${{esc(p.body)}}</pre></div>`).join('')
  : '<p class="stat">(no pad content)</p>';

scrub.max = Math.max(0, events.length - 1);
scrub.value = scrub.max;

function visibleIndices() {{
  const max = +scrub.value;
  const agents = onlyAgents.checked;
  const hi = onlyHi.checked;
  const q = (filter.value || '').toLowerCase();
  const out = [];
  for (let i = 0; i <= max && i < events.length; i++) {{
    const e = events[i];
    if (agents && !(e.actor||'').startsWith('agent:') && e.actor !== 'cli') continue;
    if (hi && !HI.has(e.kind)) continue;
    if (q && !(e.detail||'').toLowerCase().includes(q) && !(e.kind||'').toLowerCase().includes(q)) continue;
    out.push(i);
  }}
  return out;
}}

function apply() {{
  const idx = visibleIndices();
  // Virtual-ish: render at most 400 rows (tail of filter) for DOM budget.
  const slice = idx.length > 400 ? idx.slice(idx.length - 400) : idx;
  const rows = slice.map(i => {{
    const e = events[i];
    return `<tr><td class="t">${{esc(e.time)}}</td><td class="a">${{esc(e.actor)}}</td>
<td class="p">${{esc(e.pane)}}</td><td class="k">${{esc(e.kind)}}</td><td class="d">${{esc(e.detail)}}</td></tr>`;
  }}).join('');
  tl.innerHTML = rows || '<tr><td colspan="5" class="stat">(no events match)</td></tr>';
  const e = events[+scrub.value];
  label.textContent = e
    ? `${{e.time}} · showing ${{slice.length}}${{idx.length > 400 ? ' (tail)' : ''}} / ${{idx.length}} filtered`
    : `${{idx.length}} rows`;
}}
scrub.addEventListener('input', apply);
onlyAgents.addEventListener('change', apply);
onlyHi.addEventListener('change', apply);
filter.addEventListener('input', apply);
apply();
</script>
</body>
</html>
"#,
        title = esc(title),
        title_esc = esc(title),
        ws_esc = esc(ws),
        sample_note = sample_note,
        data = data_json,
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Default output path under data dir.
pub fn default_out_path(workspace: Option<&str>) -> PathBuf {
    let stamp = now_ms() / 1000;
    let name = match workspace {
        Some(w) => format!("seance-export-{}-{stamp}.html", slugify_simple(w)),
        None => format!("seance-export-all-{stamp}.html"),
    };
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/exports").into_owned()).join(name)
}

fn slugify_simple(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_collapses_home() {
        let s = redact_paths("error in /home/zack/work/seance/src/app.rs line 1");
        assert!(s.contains("~/"), "{s}");
        assert!(!s.contains("/home/zack"), "{s}");
    }

    #[test]
    fn sample_prefers_finish() {
        let mut v = vec![
            Event {
                id: String::new(),
                seq: 1,
                ts: 1,
                actor: "cli".into(),
                workspace: None,
                pane: Some("a".into()),
                kind: "touch".into(),
                detail: "x".into(),
                caused_by: None,
                span: None,
                origin: None,
            },
            Event {
                id: String::new(),
                seq: 2,
                ts: 2,
                actor: "agent:a".into(),
                workspace: None,
                pane: Some("a".into()),
                kind: "finish".into(),
                detail: "done".into(),
                caused_by: None,
                span: None,
                origin: None,
            },
        ];
        // Force sample with max 1
        let sampled = sample_events(&mut v, 1);
        assert!(sampled);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, "finish");
    }
}
