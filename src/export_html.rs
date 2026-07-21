//! Session export → scrubable offline HTML (teaching / share medium).
//!
//! v0: timeline events + roster snapshot + scratchpad bodies. Not full 60fps
//! TUI replay — the flight recorder + pads are the durable signal.

use std::path::{Path, PathBuf};

use crate::events;

/// Bundle a workspace (or all) into a self-contained HTML file.
pub fn export_session(
    workspace: Option<&str>,
    out: &Path,
    title: &str,
) -> Result<PathBuf, String> {
    let entries = events::read(0, workspace, None, None, 5000);
    let state = crate::state::AppState::load();
    let panes: Vec<_> = state
        .panes
        .iter()
        .filter(|p| workspace.is_none_or(|w| p.workspace == w))
        .cloned()
        .collect();

    let pad_dir = PathBuf::from(shellexpand::tilde("~/.local/share/seance/scratch").into_owned());
    let mut pad_sections = String::new();
    for p in &panes {
        let pad_path = pad_dir.join(format!("{}.md", p.slug));
        let body = std::fs::read_to_string(&pad_path).unwrap_or_default();
        if body.trim().is_empty() {
            continue;
        }
        pad_sections.push_str(&format!(
            r#"<section class="pad" id="pad-{slug}">
<h3>{name} <code>{slug}</code></h3>
<pre>{body}</pre>
</section>
"#,
            slug = esc(&p.slug),
            name = esc(&p.name),
            body = esc(&body),
        ));
    }

    let mut event_rows = String::new();
    for e in &entries {
        event_rows.push_str(&format!(
            r#"<tr data-ts="{ts}" data-actor="{actor}" data-pane="{pane}">
<td class="t">{time}</td>
<td class="a">{actor}</td>
<td class="p">{pane}</td>
<td class="k">{kind}</td>
<td class="d">{detail}</td>
</tr>
"#,
            ts = e.ts,
            time = esc(&events::fmt_time(e.ts)),
            actor = esc(&e.actor),
            pane = esc(e.pane.as_deref().unwrap_or("-")),
            kind = esc(&e.kind),
            detail = esc(&e.detail),
        ));
    }

    let mut roster_rows = String::new();
    for p in &panes {
        roster_rows.push_str(&format!(
            r#"<tr>
<td>{name}</td><td><code>{slug}</code></td><td>{ws}</td>
<td>{kind}</td><td class="cmd">{cmd}</td><td>{status}</td>
</tr>
"#,
            name = esc(&p.name),
            slug = esc(&p.slug),
            ws = esc(&p.workspace),
            kind = esc(&p.kind),
            cmd = esc(&p.command),
            status = esc(p.status.as_deref().unwrap_or("-")),
        ));
    }

    let ws_label = workspace.unwrap_or("(all workspaces)");
    let html = format!(
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
header {{ padding:1.25rem 1.5rem; border-bottom:1px solid var(--line);
  background:linear-gradient(180deg,#221810,var(--bg)); position:sticky; top:0; z-index:2; }}
header h1 {{ margin:0; font-size:1.15rem; color:var(--flame); font-weight:600; }}
header p {{ margin:0.35rem 0 0; color:var(--dim); font-size:0.85rem; }}
main {{ display:grid; grid-template-columns: 1fr 1.4fr; gap:0; min-height:calc(100vh - 5rem); }}
@media (max-width: 900px) {{ main {{ grid-template-columns: 1fr; }} }}
section {{ padding:1rem 1.25rem; border-bottom:1px solid var(--line); }}
h2 {{ margin:0 0 0.75rem; font-size:0.8rem; letter-spacing:0.08em;
  text-transform:uppercase; color:var(--violet); }}
table {{ width:100%; border-collapse:collapse; font-size:0.78rem; }}
th, td {{ text-align:left; padding:0.28rem 0.4rem; border-bottom:1px solid var(--line);
  vertical-align:top; }}
th {{ color:var(--dim); font-weight:500; position:sticky; top:4.5rem; background:var(--bg); }}
td.t {{ color:var(--dim); white-space:nowrap; }}
td.a {{ color:var(--flame); max-width:7rem; overflow:hidden; }}
td.p {{ color:var(--violet); }}
td.k {{ color:var(--dim); }}
td.cmd {{ max-width:14rem; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }}
pre {{ white-space:pre-wrap; word-break:break-word; background:var(--surf);
  padding:0.75rem 1rem; border-radius:6px; border:1px solid var(--line);
  font-size:0.8rem; max-height:28rem; overflow:auto; }}
.controls {{ display:flex; gap:0.75rem; flex-wrap:wrap; margin-top:0.6rem; align-items:center; }}
input[type=range] {{ width:min(420px, 70vw); }}
label {{ color:var(--dim); font-size:0.8rem; }}
.scrub-info {{ color:var(--flame); font-size:0.85rem; min-width:8rem; }}
.hidden-row {{ display:none; }}
.pad h3 {{ font-size:0.9rem; color:var(--fg); margin:0 0 0.5rem; }}
.pad h3 code {{ color:var(--dim); font-size:0.75rem; }}
footer {{ padding:1rem 1.5rem; color:var(--dim); font-size:0.75rem; border-top:1px solid var(--line); }}
</style>
</head>
<body>
<header>
  <h1>✦ {title}</h1>
  <p>seance session export · workspace <strong>{ws}</strong> · {n_events} events · {n_panes} panes</p>
  <div class="controls">
    <label>scrub <input id="scrub" type="range" min="0" max="{max_i}" value="{max_i}"/></label>
    <span class="scrub-info" id="scrub-label">all events</span>
    <label><input type="checkbox" id="only-agents"/> agents only</label>
  </div>
</header>
<main>
  <div>
    <section>
      <h2>roster</h2>
      <table>
        <thead><tr><th>name</th><th>slug</th><th>ws</th><th>kind</th><th>command</th><th>status</th></tr></thead>
        <tbody>{roster}</tbody>
      </table>
    </section>
    <section>
      <h2>pads</h2>
      {pads}
    </section>
  </div>
  <section>
    <h2>timeline</h2>
    <table id="timeline">
      <thead><tr><th>time</th><th>actor</th><th>pane</th><th>kind</th><th>detail</th></tr></thead>
      <tbody>
{events}
      </tbody>
    </table>
  </section>
</main>
<footer>generated by seance export-session · offline scrubber v0 · not full TUI replay</footer>
<script>
const rows = [...document.querySelectorAll('#timeline tbody tr')];
const scrub = document.getElementById('scrub');
const label = document.getElementById('scrub-label');
const onlyAgents = document.getElementById('only-agents');
function apply() {{
  const max = +scrub.value;
  const agents = onlyAgents.checked;
  let shown = 0;
  rows.forEach((tr, i) => {{
    const actor = tr.dataset.actor || '';
    const hideIdx = i > max;
    const hideAgent = agents && !(actor.startsWith('agent:') || actor === 'cli');
    tr.classList.toggle('hidden-row', hideIdx || hideAgent);
    if (!hideIdx && !hideAgent) shown++;
  }});
  const tr = rows[Math.min(max, rows.length-1)];
  label.textContent = tr ? (tr.querySelector('.t')?.textContent || '') + ' · ' + shown + ' rows' : shown + ' rows';
}}
scrub.addEventListener('input', apply);
onlyAgents.addEventListener('change', apply);
apply();
</script>
</body>
</html>
"#,
        title = esc(title),
        ws = esc(ws_label),
        n_events = entries.len(),
        n_panes = panes.len(),
        max_i = entries.len().saturating_sub(1),
        roster = roster_rows,
        pads = if pad_sections.is_empty() {
            "<p style=\"color:var(--dim)\">(no pad content)</p>".into()
        } else {
            pad_sections
        },
        events = event_rows,
    );

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(out, html).map_err(|e| e.to_string())?;
    Ok(out.to_path_buf())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Default output path under data dir.
pub fn default_out_path(workspace: Option<&str>) -> PathBuf {
    let stamp = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        secs
    };
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
