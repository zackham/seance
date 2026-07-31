//! Replay **exporter**: turn the daemon's on-disk recording ring into a
//! shareable bundle — and hand that bundle to a publish command.
//!
//! # Shape
//!
//! - **The ring is the source of truth.** The recorder writes
//!   `<state_dir>/replay/<pane-slug>/<unix_hour>.srr` for the current hour and
//!   gzips finished hours to `<unix_hour>.srr.gz` (48h retention). Nothing here
//!   mutates the ring; the exporter is a pure reader.
//! - **Slicing is keyframe-correct or it is worthless.** A slice that starts on
//!   a DAMAGE frame paints garbage, so [`slice_pane`] always leads with the last
//!   `KIND_FULL` at-or-before `from_ms` — walking back one segment (1h) when the
//!   keyframe lives in the previous hour. Only when no earlier keyframe exists
//!   do we start at the first FULL *after* `from_ms` and drop the unappliable
//!   DAMAGE before it (EVENT records survive: chapters need no grid).
//! - **Two bundle modes, one shell.** Self-contained copies the player next to
//!   the recording (works from any dumb static host); shared-assets writes only
//!   `index.html` + `recording/` and points at a player hosted once. Both set
//!   `window.__SEANCE_REPLAY__` to the bundle's own
//!   `./recording/manifest.json` *before* the wasm module initialises — which
//!   is why the shell uses a dynamic `import()`: a static `import` is hoisted
//!   above the assignment and the player would boot blind.
//! - **Publishing is a seam, not a service.** `~/.config/seance/publish.json`
//!   names a shell command; we hand it the bundle dir and take the last line of
//!   its stdout as the URL. Whatever moves the bytes — rsync, s3, a tunnel —
//!   stays the human's business.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use seance_core::replay::{
    encode_record, extract_chapters, records, Chapter, Manifest, PaneMeta, ReplayEvent, KIND_DAMAGE,
    KIND_EVENT, KIND_FULL, MAGIC,
};

/// One ring segment covers exactly this many ms (its filename is `t_ms / this`).
pub const SEGMENT_MS: u64 = 3_600_000;

/// Manifest format version this exporter emits.
const MANIFEST_VERSION: u32 = 1;

/// Cap on a publish request body (the bridge reads no more than this).
pub const MAX_PUBLISH_BODY: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// ring layout
// ---------------------------------------------------------------------------

/// `<state_dir>/replay` — the recording ring root.
pub fn ring_root() -> PathBuf {
    crate::runtime::state_data_dir().join("replay")
}

/// Where bundles are staged before a publish command runs.
pub fn out_root() -> PathBuf {
    crate::runtime::state_data_dir().join("replay-out")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
struct Segment {
    hour: u64,
    path: PathBuf,
    gz: bool,
}

/// Every segment of one pane, ascending by hour. A finished (`.srr.gz`) segment
/// wins over a same-hour `.srr` — the raw file is the live tail the gzip
/// replaced, so preferring the gz keeps us from emitting an hour twice.
fn segments_for(root: &Path, slug: &str) -> Result<Vec<Segment>> {
    let dir = root.join(slug);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(Vec::new()), // no recordings for this pane
    };
    let mut by_hour: BTreeMap<u64, Segment> = BTreeMap::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let (stem, gz) = match name.strip_suffix(".srr.gz") {
            Some(s) => (s, true),
            None => match name.strip_suffix(".srr") {
                Some(s) => (s, false),
                None => continue,
            },
        };
        let Ok(hour) = stem.parse::<u64>() else {
            continue;
        };
        let seg = Segment { hour, path, gz };
        match by_hour.get(&hour) {
            Some(prev) if prev.gz => {}
            _ => {
                by_hour.insert(hour, seg);
            }
        }
    }
    Ok(by_hour.into_values().collect())
}

fn read_segment(seg: &Segment) -> Result<Vec<u8>> {
    let raw =
        std::fs::read(&seg.path).with_context(|| format!("reading segment {}", seg.path.display()))?;
    if !seg.gz {
        return Ok(raw);
    }
    let mut out = Vec::with_capacity(raw.len() * 4);
    flate2::read::MultiGzDecoder::new(&raw[..])
        .read_to_end(&mut out)
        .with_context(|| format!("decompressing {}", seg.path.display()))?;
    Ok(out)
}

/// Gzip a byte stream (bundle recordings and the `/replay/pane` body).
pub fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).context("gzip write")?;
    enc.finish().context("gzip finish")
}

/// An owned record — segment buffers drop as we go, so we can't borrow.
struct OwnedRec {
    kind: u8,
    t_ms: u64,
    payload: Vec<u8>,
}

/// Read every record from the segments covering `[from_ms, to_ms]`, plus (when
/// `lookback`) the newest segment strictly before them — that's where the
/// keyframe for a mid-hour `from_ms` usually lives.
fn collect_records(
    root: &Path,
    slug: &str,
    from_ms: u64,
    to_ms: u64,
    lookback: bool,
) -> Result<Vec<OwnedRec>> {
    let segs = segments_for(root, slug)?;
    let from_hour = from_ms / SEGMENT_MS;
    let to_hour = to_ms / SEGMENT_MS;

    let mut wanted: Vec<Segment> = Vec::new();
    if lookback {
        if let Some(prev) = segs.iter().filter(|s| s.hour < from_hour).next_back() {
            wanted.push(prev.clone());
        }
    }
    wanted.extend(
        segs.iter()
            .filter(|s| s.hour >= from_hour && s.hour <= to_hour)
            .cloned(),
    );

    let mut out: Vec<OwnedRec> = Vec::new();
    for seg in &wanted {
        let buf = read_segment(seg)?;
        // No magic: a zero-length or foreign file. Skip it — one bad segment
        // must not lose the whole recording.
        let Some(iter) = records(&buf) else { continue };
        for r in iter {
            out.push(OwnedRec { kind: r.kind, t_ms: r.t_ms, payload: r.payload.to_vec() });
        }
    }
    out.sort_by_key(|r| r.t_ms);
    Ok(out)
}

/// Choose the slice start index over time-ordered records.
///
/// `None` when nothing at-or-after `from_ms` is worth emitting. Pure — the seek
/// contract is tested against synthetic streams.
fn slice_start(recs: &[OwnedRec], from_ms: u64) -> Option<usize> {
    // Preferred: the last keyframe at-or-before `from_ms`.
    if let Some(i) = recs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == KIND_FULL && r.t_ms <= from_ms)
        .map(|(i, _)| i)
        .next_back()
    {
        return Some(i);
    }
    // Fallback: the first keyframe at-or-after it (earlier DAMAGE is
    // unappliable and the caller drops it).
    if let Some(i) = recs.iter().position(|r| r.kind == KIND_FULL && r.t_ms >= from_ms) {
        return Some(i);
    }
    // No keyframe at all — events only.
    recs.iter().position(|r| r.t_ms >= from_ms)
}

/// A concatenated `SRR1` stream (with magic) for one pane over
/// `[from_ms, to_ms]`, keyframe-led whenever a keyframe exists.
///
/// A pane with no records in range yields a magic-only stream — a valid empty
/// recording, not an error.
pub fn slice_pane(state_replay_root: &Path, slug: &str, from_ms: u64, to_ms: u64) -> Result<Vec<u8>> {
    let recs = collect_records(state_replay_root, slug, from_ms, to_ms, true)?;
    let mut out = MAGIC.to_vec();
    let Some(start) = slice_start(&recs, from_ms) else {
        return Ok(out);
    };
    let mut seen_full = false;
    for r in &recs[start..] {
        if r.t_ms > to_ms {
            break;
        }
        if r.kind == KIND_FULL {
            seen_full = true;
        }
        // DAMAGE ahead of the first keyframe would paint onto nothing.
        if r.kind == KIND_DAMAGE && !seen_full {
            continue;
        }
        out.extend_from_slice(&encode_record(r.kind, r.t_ms, &r.payload));
    }
    Ok(out)
}

/// Decoded EVENT records in range — the input to chapter extraction.
///
/// Unlike [`slice_pane`] this never walks back a segment: events carry no
/// state, so the window is exactly the window.
pub fn pane_events(root: &Path, slug: &str, from_ms: u64, to_ms: u64) -> Result<Vec<(u64, ReplayEvent)>> {
    let recs = collect_records(root, slug, from_ms, to_ms, false)?;
    let mut out = Vec::new();
    for r in recs {
        if r.kind != KIND_EVENT || r.t_ms < from_ms || r.t_ms > to_ms {
            continue;
        }
        // A malformed event line is a recorder bug, not an export failure.
        if let Ok(ev) = serde_json::from_slice::<ReplayEvent>(&r.payload) {
            out.push((r.t_ms, ev));
        }
    }
    Ok(out)
}

/// Recording coverage of one pane, derived from segment filenames alone
/// (cheap: no segment is opened).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Coverage {
    pub slug: String,
    pub from_ms: u64,
    pub to_ms: u64,
}

/// Every pane in the ring with at least one segment, ascending by slug.
pub fn coverage(root: &Path) -> Result<Vec<Coverage>> {
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return Ok(Vec::new()),
    };
    let mut slugs: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    slugs.sort();

    let mut out = Vec::new();
    for slug in slugs {
        let segs = segments_for(root, &slug)?;
        let (Some(first), Some(last)) = (segs.first(), segs.last()) else {
            continue;
        };
        out.push(Coverage {
            slug,
            from_ms: first.hour * SEGMENT_MS,
            // The last segment covers through the end of its hour.
            to_ms: (last.hour + 1) * SEGMENT_MS,
        });
    }
    Ok(out)
}

/// Panes whose coverage overlaps `[from_ms, to_ms]`.
pub fn panes_with_records(root: &Path, from_ms: u64, to_ms: u64) -> Result<Vec<String>> {
    Ok(coverage(root)?
        .into_iter()
        .filter(|c| c.to_ms > from_ms && c.from_ms <= to_ms)
        .map(|c| c.slug)
        .collect())
}

// ---------------------------------------------------------------------------
// daemon lookup (best effort — the exporter works offline)
// ---------------------------------------------------------------------------

/// `(slug, display name)` for one workspace, via a one-shot ctl `List`.
///
/// Best effort by design: the ring outlives the daemon, so exporting a finished
/// session must not require a live socket. Callers fall back to `name = slug`.
pub fn workspace_panes(workspace: &str) -> Result<Vec<(String, String)>> {
    let req = seance_core::control::ControlRequest::List {
        scope: Some(workspace.to_string()),
        from: Some("replay-export".to_string()),
    };
    let resp = crate::ctl::send_request(&req).map_err(|_| anyhow!("daemon unreachable"))?;
    if !resp.ok {
        bail!("ctl list failed: {}", resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    let data = resp.data.ok_or_else(|| anyhow!("ctl list returned no data"))?;
    let panes = data
        .get("panes")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("ctl list payload has no panes array"))?;
    Ok(panes
        .iter()
        .filter_map(|p| {
            let slug = p.get("slug")?.as_str()?.to_string();
            let name = p.get("name").and_then(|n| n.as_str()).unwrap_or(&slug).to_string();
            Some((slug, name))
        })
        .collect())
}

/// Slugs of `workspace` that also have recordings in range.
///
/// With no daemon there is no workspace→pane map: `allow_all` decides whether
/// that degrades to "every recorded pane in range" (bridge, where the caller
/// already chose a range) or hard-fails asking for explicit `--panes` (CLI).
pub fn resolve_panes(
    root: &Path,
    workspace: &str,
    from_ms: u64,
    to_ms: u64,
    allow_all: bool,
) -> Result<Vec<String>> {
    let recorded = panes_with_records(root, from_ms, to_ms)?;
    // Live panes from the daemon, PLUS dead panes attributed from the ring's
    // own Spawned events — sharing a session AFTER its panes exited is the
    // normal case, and the daemon has already forgotten those panes.
    let mut members: Vec<String> = Vec::new();
    let daemon = workspace_panes(workspace);
    if let Ok(list) = &daemon {
        members.extend(list.iter().map(|(slug, _)| slug.clone()));
    }
    for slug in &recorded {
        if !members.contains(slug) && ring_pane_workspace(root, slug, to_ms) == Some(workspace.to_string()) {
            members.push(slug.clone());
        }
    }
    if !members.is_empty() {
        return Ok(recorded.into_iter().filter(|s| members.contains(s)).collect());
    }
    match daemon {
        Ok(_) => Ok(Vec::new()),
        Err(e) if allow_all => {
            eprintln!("seance replay: {e} — falling back to every recorded pane in range");
            Ok(recorded)
        }
        Err(e) => Err(anyhow!(
            "cannot map workspace {workspace} to panes ({e}); pass --panes explicitly"
        )),
    }
}

/// Workspace a recorded pane belonged to, from the LAST `Spawned` event at or
/// before `at_ms` in its ring — recordings outlive the daemon's memory.
fn ring_pane_workspace(root: &Path, slug: &str, at_ms: u64) -> Option<String> {
    let events = pane_events(root, slug, 0, at_ms).ok()?;
    events
        .iter()
        .rev()
        .find_map(|(_, ev)| match ev {
            seance_core::replay::ReplayEvent::Spawned { workspace, .. } => {
                Some(workspace.clone())
            }
            _ => None,
        })
}

// ---------------------------------------------------------------------------
// bundle export
// ---------------------------------------------------------------------------

/// What to export. `chapters_override` is the editor's output — when present it
/// is written verbatim (the human already fixed the titles).
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub workspace: String,
    pub panes: Vec<String>,
    pub from_ms: u64,
    pub to_ms: u64,
    pub title: Option<String>,
    pub chapters_override: Option<Vec<Chapter>>,
}

/// Build the manifest for a spec.
///
/// Also serves the bridge's manifest endpoint, where the same `<slug>.srr.gz`
/// names are fetched from `/replay/pane` instead of from bundled files.
pub fn build_manifest(root: &Path, spec: &ExportSpec) -> Result<Manifest> {
    let names: BTreeMap<String, String> = workspace_panes(&spec.workspace)
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut panes = Vec::new();
    let mut extracted = Vec::new();
    for slug in &spec.panes {
        panes.push(PaneMeta {
            slug: slug.clone(),
            name: names.get(slug).cloned().unwrap_or_else(|| slug.clone()),
            file: format!("{slug}.srr.gz"),
        });
        if spec.chapters_override.is_none() {
            let events = pane_events(root, slug, spec.from_ms, spec.to_ms)?;
            extracted.extend(extract_chapters(slug, &events));
        }
    }
    let mut chapters = spec.chapters_override.clone().unwrap_or(extracted);
    chapters.sort_by_key(|c| c.t_ms);

    Ok(Manifest {
        version: MANIFEST_VERSION,
        workspace: spec.workspace.clone(),
        title: spec.title.clone(),
        created_ms: now_ms(),
        from_ms: spec.from_ms,
        to_ms: spec.to_ms,
        panes,
        chapters,
    })
}

/// Write a bundle at `out_dir` and return it.
///
/// Layout: `index.html`, `recording/manifest.json`, `recording/<slug>.srr.gz`
/// — plus the player files at the root when `assets_url` is `None`.
pub fn export_bundle(spec: &ExportSpec, out_dir: &Path, assets_url: Option<&str>) -> Result<PathBuf> {
    if spec.panes.is_empty() {
        bail!("nothing to export: no panes with recordings in the requested range");
    }
    if spec.to_ms <= spec.from_ms {
        bail!("empty time range: to_ms must be after from_ms");
    }
    let root = ring_root();
    let rec_dir = out_dir.join("recording");
    std::fs::create_dir_all(&rec_dir)
        .with_context(|| format!("creating bundle dir {}", rec_dir.display()))?;

    let manifest = build_manifest(&root, spec)?;
    let json = serde_json::to_vec_pretty(&manifest).context("serializing manifest")?;
    let mpath = rec_dir.join("manifest.json");
    std::fs::write(&mpath, &json).with_context(|| format!("writing {}", mpath.display()))?;

    for pane in &manifest.panes {
        let stream = slice_pane(&root, &pane.slug, spec.from_ms, spec.to_ms)?;
        let gz = gzip(&stream)?;
        let path = rec_dir.join(&pane.file);
        std::fs::write(&path, &gz).with_context(|| format!("writing {}", path.display()))?;
    }

    if assets_url.is_none() {
        copy_player_assets(out_dir)?;
    }
    let html = index_html(spec.title.as_deref(), assets_url);
    let ipath = out_dir.join("index.html");
    std::fs::write(&ipath, html.as_bytes()).with_context(|| format!("writing {}", ipath.display()))?;

    Ok(out_dir.to_path_buf())
}

/// The player files a self-contained bundle needs. `index.html` is deliberately
/// absent — the bundle generates its own shell.
const PLAYER_ASSETS: [&str; 3] = ["seance_web.js", "seance_web_bg.wasm", "style.css"];

fn copy_player_assets(out_dir: &Path) -> Result<()> {
    let dist = crate::webbridge::resolve_dist(None)?;
    for name in PLAYER_ASSETS {
        let src = dist.join(name);
        if !src.is_file() {
            bail!(
                "self-contained export needs {name} in the web dist ({}) — build the web client \
                 or configure assets_url for a shared-assets bundle",
                dist.display()
            );
        }
        std::fs::copy(&src, out_dir.join(name))
            .with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

/// The bundle shell.
///
/// `window.__SEANCE_REPLAY__` is the whole contract with the player: a URL to a
/// manifest, always bundle-relative so the recording travels with the page in
/// both modes, and independent of `location.hash` (a shared link must replay
/// with no fragment at all). The dynamic `import()` is load-bearing — a static
/// `import` is hoisted above the assignment.
pub fn index_html(title: Option<&str>, assets_url: Option<&str>) -> String {
    let base = match assets_url {
        Some(u) => format!("{}/", u.trim_end_matches('/')),
        None => "./".to_string(),
    };
    let title = html_escape(title.unwrap_or("seance replay"));
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <link rel=\"stylesheet\" href=\"{base}style.css\">\n\
         </head>\n\
         <body>\n\
         <div id=\"app\"></div>\n\
         <div id=\"replay-root\"></div>\n\
         <script type=\"module\">\n\
         window.__SEANCE_REPLAY__ = \"./recording/manifest.json\";\n\
         import(\"{base}seance_web.js\")\n\
         \x20\x20.then((m) => m.default())\n\
         \x20\x20.catch((e) => {{ document.body.textContent = \"replay failed to load: \" + e; }});\n\
         </script>\n\
         </body>\n\
         </html>\n"
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// publish seam
// ---------------------------------------------------------------------------

/// `~/.config/seance/publish.json`. Missing file = every field `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublishConfig {
    /// Host serving the player once, for shared-assets bundles.
    #[serde(default)]
    pub assets_url: Option<String>,
    /// Shell command receiving the bundle dir as `$1`; last stdout line = URL.
    #[serde(default)]
    pub publish_command: Option<String>,
}

pub fn publish_config_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("seance/publish.json");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.config/seance/publish.json").into_owned())
}

pub fn load_publish_config() -> Result<PublishConfig> {
    let path = publish_config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return Ok(PublishConfig::default()),
    };
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Run the configured publish command on a bundle; return the URL it printed.
///
/// The command runs under `sh -lc` (a login shell: the human's PATH, cloud
/// creds, rsync aliases) with the bundle dir passed as a real argv element —
/// `$1`, with no quoting for us to get wrong.
pub fn publish(bundle_dir: &Path, cfg: &PublishConfig) -> Result<String> {
    let Some(cmd) = cfg.publish_command.as_deref().filter(|c| !c.trim().is_empty()) else {
        bail!(
            "no publish_command configured — set one in {}, e.g. \
             {{\"publish_command\": \"rsync -a \\\"$1\\\"/ host:/srv/replays/$(basename \\\"$1\\\") \
             && echo https://example.com/replays/$(basename \\\"$1\\\")\"}}",
            publish_config_path().display()
        );
    };
    let out = std::process::Command::new("sh")
        .arg("-lc")
        .arg(cmd)
        .arg("seance-publish") // $0
        .arg(bundle_dir) //      $1
        .output()
        .context("running publish_command")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "publish_command failed ({}): {}",
            out.status,
            err.trim().lines().next_back().unwrap_or("no stderr")
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .next_back()
        .map(|l| l.to_string())
        .ok_or_else(|| anyhow!("publish_command printed nothing — expected the URL on stdout"))
}

// ---------------------------------------------------------------------------
// time parsing (pure — tested below)
// ---------------------------------------------------------------------------

/// Accepts `now`, a relative offset (`-30m`, `-2h`, `-90s`, `-1d`), a plain
/// integer of unix milliseconds, or RFC3339.
pub fn parse_time(s: &str, now: u64) -> Result<u64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Some(rest) = s.strip_prefix('-') {
        let split = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let (num, unit) = rest.split_at(split);
        let n: u64 = num
            .parse()
            .map_err(|_| anyhow!("bad relative time {s:?} (want e.g. -30m)"))?;
        let mult = match unit {
            "s" => 1_000,
            "m" | "" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            other => bail!("unknown time unit {other:?} in {s:?} (want s/m/h/d)"),
        };
        return Ok(now.saturating_sub(n.saturating_mul(mult)));
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().map_err(|_| anyhow!("bad unix ms {s:?}"));
    }
    parse_rfc3339_ms(s)
}

/// Minimal RFC3339 → unix ms. No chrono dep for one field on one CLI flag.
pub fn parse_rfc3339_ms(s: &str) -> Result<u64> {
    let bad = || anyhow!("bad timestamp {s:?} (want RFC3339, unix ms, `now`, or `-30m`)");
    let b = s.as_bytes();
    if b.len() < 19 || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return Err(bad());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64> {
        s.get(r).ok_or_else(bad)?.parse::<i64>().map_err(|_| bad())
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return Err(bad());
    }

    let mut rest = &s[19..];
    let mut millis: i64 = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return Err(bad());
        }
        let ms: String = digits.chars().chain(std::iter::repeat('0')).take(3).collect();
        millis = ms.parse::<i64>().map_err(|_| bad())?;
        rest = &rest[1 + digits.len()..];
    }

    // Offset: `Z`, `±HH:MM`, `±HHMM`, or absent (treated as UTC).
    let offset_min: i64 = if rest.is_empty() || rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return Err(bad()),
        };
        let body: String = rest[1..].chars().filter(|c| *c != ':').collect();
        if body.len() != 4 || !body.chars().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        let oh: i64 = body[0..2].parse().map_err(|_| bad())?;
        let om: i64 = body[2..4].parse().map_err(|_| bad())?;
        sign * (oh * 60 + om)
    };

    let secs = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3_600 + mi * 60 + sec
        - offset_min * 60;
    let ms = secs * 1000 + millis;
    if ms < 0 {
        return Err(bad());
    }
    Ok(ms as u64)
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds-resolution UTC — enough to eyeball coverage in `replay list`.
fn format_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const USAGE: &str = "\
seance replay <command>

  export --workspace W [--panes a,b] [--from T] [--to T] [--title S] [-o DIR] [--publish]
      Build a shareable bundle. T is RFC3339, unix ms, `now`, or a relative
      offset like -30m / -2h / -90s / -1d. Defaults: --from -30m --to now.
      --panes defaults to the workspace's recorded panes (needs the daemon).

  list
      Panes with recording coverage.

  edit --workspace W
      Print (and open) the bridge's replay-editor URL.
";

pub fn run_cli(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("export") => cmd_export(&args[1..]),
        Some("list") => cmd_list(),
        Some("edit") => cmd_edit(&args[1..]),
        Some("-h") | Some("--help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow!("unknown `seance replay` command: {other}\n\n{USAGE}")),
    }
}

fn next_val<'a>(args: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str> {
    *i += 1;
    args.get(*i)
        .map(|s| s.as_str())
        .ok_or_else(|| anyhow!("{flag} needs a value"))
}

fn cmd_export(args: &[String]) -> Result<()> {
    let now = now_ms();
    let mut workspace: Option<String> = None;
    let mut panes: Option<Vec<String>> = None;
    let mut from = now.saturating_sub(30 * 60 * 1000);
    let mut to = now;
    let mut title: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut do_publish = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "-w" => workspace = Some(next_val(args, &mut i, "--workspace")?.into()),
            "--panes" => {
                panes = Some(
                    next_val(args, &mut i, "--panes")?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            }
            "--from" => from = parse_time(next_val(args, &mut i, "--from")?, now)?,
            "--to" => to = parse_time(next_val(args, &mut i, "--to")?, now)?,
            "--title" => title = Some(next_val(args, &mut i, "--title")?.into()),
            "-o" | "--out" => out = Some(PathBuf::from(next_val(args, &mut i, "-o")?)),
            "--publish" => do_publish = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown flag for `seance replay export`: {other}"),
        }
        i += 1;
    }

    let workspace = workspace.ok_or_else(|| anyhow!("--workspace is required"))?;
    let root = ring_root();
    let panes = match panes {
        Some(p) => p,
        None => resolve_panes(&root, &workspace, from, to, false)?,
    };
    if panes.is_empty() {
        bail!("no panes with recordings in that range (try `seance replay list`)");
    }

    let cfg = load_publish_config()?;
    let out_dir = out.unwrap_or_else(|| out_root().join(now.to_string()));
    let spec = ExportSpec {
        workspace,
        panes,
        from_ms: from,
        to_ms: to,
        title,
        chapters_override: None,
    };
    let dir = export_bundle(&spec, &out_dir, cfg.assets_url.as_deref())?;
    println!("bundle: {}", dir.display());

    if do_publish {
        println!("{}", publish(&dir, &cfg)?);
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let root = ring_root();
    let cov = coverage(&root)?;
    if cov.is_empty() {
        println!("no recordings under {}", root.display());
        return Ok(());
    }
    for c in cov {
        println!("{:<24} {} → {}", c.slug, format_ms(c.from_ms), format_ms(c.to_ms));
    }
    Ok(())
}

fn cmd_edit(args: &[String]) -> Result<()> {
    let mut workspace: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "-w" => workspace = Some(next_val(args, &mut i, "--workspace")?.into()),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown flag for `seance replay edit`: {other}"),
        }
        i += 1;
    }
    let workspace = workspace.ok_or_else(|| anyhow!("--workspace is required"))?;
    let token = crate::webbridge::read_token()?;
    let url = format!("http://127.0.0.1:9666/#replay-edit?workspace={workspace}&token={token}");
    println!("{url}");
    crate::sysopen::open_detached(&url);
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: u8, t: u64, payload: &[u8]) -> OwnedRec {
        OwnedRec { kind, t_ms: t, payload: payload.to_vec() }
    }

    /// A scratch dir unique to this process + call (no tempfile dep).
    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "seance-replayexport-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_segment(root: &Path, slug: &str, hour: u64, recs: &[(u8, u64, &[u8])], gz: bool) {
        let dir = root.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let mut buf = MAGIC.to_vec();
        for (k, t, p) in recs {
            buf.extend_from_slice(&encode_record(*k, *t, p));
        }
        if gz {
            std::fs::write(dir.join(format!("{hour}.srr.gz")), gzip(&buf).unwrap()).unwrap();
        } else {
            std::fs::write(dir.join(format!("{hour}.srr")), buf).unwrap();
        }
    }

    fn kinds(stream: &[u8]) -> Vec<(u8, u64)> {
        records(stream).unwrap().map(|r| (r.kind, r.t_ms)).collect()
    }

    // -- time parsing --------------------------------------------------------

    #[test]
    fn parses_relative_and_absolute_times() {
        let now = 1_700_000_000_000u64;
        assert_eq!(parse_time("now", now).unwrap(), now);
        assert_eq!(parse_time("-30m", now).unwrap(), now - 30 * 60_000);
        assert_eq!(parse_time("-2h", now).unwrap(), now - 2 * 3_600_000);
        assert_eq!(parse_time("-90s", now).unwrap(), now - 90_000);
        assert_eq!(parse_time("-1d", now).unwrap(), now - 86_400_000);
        assert_eq!(parse_time("1234567", now).unwrap(), 1_234_567);
    }

    #[test]
    fn relative_times_saturate_at_the_epoch() {
        assert_eq!(parse_time("-2h", 1000).unwrap(), 0);
    }

    #[test]
    fn parses_rfc3339_with_offsets_and_fractions() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(parse_rfc3339_ms("2023-11-14T22:13:20Z").unwrap(), 1_700_000_000_000);
        assert_eq!(parse_rfc3339_ms("2023-11-14T22:13:20.250Z").unwrap(), 1_700_000_000_250);
        assert_eq!(parse_rfc3339_ms("2023-11-14T22:13:20.25Z").unwrap(), 1_700_000_000_250);
        // -08:00 is eight hours *later* in UTC.
        assert_eq!(parse_rfc3339_ms("2023-11-14T14:13:20-08:00").unwrap(), 1_700_000_000_000);
    }

    #[test]
    fn rejects_junk_times() {
        let now = 1_000_000u64;
        assert!(parse_time("yesterday", now).is_err());
        assert!(parse_time("-30x", now).is_err());
        assert!(parse_rfc3339_ms("2023-13-01T00:00:00Z").is_err());
        assert!(parse_rfc3339_ms("nope").is_err());
    }

    #[test]
    fn formats_ms_back_to_utc() {
        assert_eq!(format_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_ms(1_700_000_000_000), "2023-11-14T22:13:20Z");
    }

    // -- keyframe selection --------------------------------------------------

    #[test]
    fn slice_start_prefers_the_last_keyframe_at_or_before_from() {
        let recs = vec![
            rec(KIND_FULL, 100, b"f1"),
            rec(KIND_DAMAGE, 150, b"d"),
            rec(KIND_FULL, 200, b"f2"),
            rec(KIND_DAMAGE, 250, b"d"),
            rec(KIND_FULL, 400, b"f3"),
        ];
        assert_eq!(slice_start(&recs, 300), Some(2)); // the FULL at 200
        assert_eq!(slice_start(&recs, 200), Some(2)); // at-or-before includes ==
    }

    #[test]
    fn slice_start_falls_forward_when_no_earlier_keyframe() {
        let recs = vec![
            rec(KIND_DAMAGE, 100, b"d"),
            rec(KIND_EVENT, 120, b"{}"),
            rec(KIND_FULL, 300, b"f"),
        ];
        assert_eq!(slice_start(&recs, 50), Some(2));
    }

    #[test]
    fn slice_start_without_any_keyframe_takes_events() {
        let recs = vec![rec(KIND_EVENT, 100, b"{}"), rec(KIND_EVENT, 200, b"{}")];
        assert_eq!(slice_start(&recs, 150), Some(1));
        assert_eq!(slice_start(&recs, 900), None);
    }

    #[test]
    fn slice_walks_back_into_the_previous_segment_for_a_keyframe() {
        let root = scratch("walkback");
        // hour 1: keyframe at the top of the hour, then damage.
        write_segment(
            &root,
            "w-1",
            1,
            &[(KIND_FULL, SEGMENT_MS, b"full-h1"), (KIND_DAMAGE, SEGMENT_MS + 10, b"d1")],
            true,
        );
        // hour 2: damage only — the keyframe lives one segment back.
        write_segment(
            &root,
            "w-1",
            2,
            &[(KIND_DAMAGE, 2 * SEGMENT_MS + 5, b"d2"), (KIND_EVENT, 2 * SEGMENT_MS + 6, b"{}")],
            false,
        );

        let from = 2 * SEGMENT_MS;
        let got = kinds(&slice_pane(&root, "w-1", from, from + 1000).unwrap());
        assert_eq!(got[0], (KIND_FULL, SEGMENT_MS), "must lead with a keyframe");
        // Intervening damage from the previous hour replays onto it.
        assert_eq!(got[1], (KIND_DAMAGE, SEGMENT_MS + 10));
        assert_eq!(got[2], (KIND_DAMAGE, 2 * SEGMENT_MS + 5));
        assert_eq!(got.len(), 4);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn slice_drops_damage_before_the_first_keyframe_and_clips_to_range() {
        let root = scratch("clip");
        write_segment(
            &root,
            "w-1",
            0,
            &[
                (KIND_DAMAGE, 10, b"orphan"),
                (KIND_FULL, 20, b"f"),
                (KIND_DAMAGE, 30, b"d"),
                (KIND_FULL, 90, b"late"),
            ],
            false,
        );
        let out = slice_pane(&root, "w-1", 5, 50).unwrap();
        assert_eq!(kinds(&out), vec![(KIND_FULL, 20), (KIND_DAMAGE, 30)]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn slice_of_an_unknown_pane_is_an_empty_stream() {
        let root = scratch("missing");
        let out = slice_pane(&root, "nope", 0, 1000).unwrap();
        assert_eq!(out, MAGIC.to_vec());
        assert!(kinds(&out).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gz_and_raw_segments_read_identically() {
        let root = scratch("gzmix");
        write_segment(&root, "a", 0, &[(KIND_FULL, 1, b"x")], true);
        write_segment(&root, "b", 0, &[(KIND_FULL, 1, b"x")], false);
        assert_eq!(
            slice_pane(&root, "a", 0, 100).unwrap(),
            slice_pane(&root, "b", 0, 100).unwrap()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // -- events + coverage ---------------------------------------------------

    #[test]
    fn pane_events_decodes_only_events_in_range() {
        let root = scratch("events");
        let ev = |text: &str| {
            serde_json::to_vec(&ReplayEvent::Send {
                from: "human".into(),
                text: text.into(),
                submit: true,
            })
            .unwrap()
        };
        let (a, b) = (ev("first"), ev("second"));
        write_segment(
            &root,
            "w-1",
            0,
            &[(KIND_FULL, 5, b"grid"), (KIND_EVENT, 10, &a), (KIND_EVENT, 900, &b)],
            false,
        );
        let got = pane_events(&root, "w-1", 0, 100).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 10);
        assert_eq!(extract_chapters("w-1", &got)[0].text, "first");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn coverage_spans_whole_segment_hours() {
        let root = scratch("coverage");
        write_segment(&root, "w-1", 3, &[(KIND_FULL, 1, b"x")], true);
        write_segment(&root, "w-1", 5, &[(KIND_FULL, 1, b"x")], false);
        assert_eq!(
            coverage(&root).unwrap(),
            vec![Coverage { slug: "w-1".into(), from_ms: 3 * SEGMENT_MS, to_ms: 6 * SEGMENT_MS }]
        );
        assert_eq!(
            panes_with_records(&root, 4 * SEGMENT_MS, 4 * SEGMENT_MS + 1).unwrap(),
            vec!["w-1".to_string()]
        );
        assert!(panes_with_records(&root, 0, 1).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    // -- shell generation ----------------------------------------------------

    #[test]
    fn self_contained_shell_points_at_bundle_local_assets() {
        let html = index_html(Some("fixing the parser"), None);
        assert!(html.contains(r#"window.__SEANCE_REPLAY__ = "./recording/manifest.json";"#));
        assert!(html.contains(r#"import("./seance_web.js")"#));
        assert!(html.contains(r#"href="./style.css""#));
        assert!(html.contains(r#"<div id="app"></div>"#));
        assert!(html.contains(r#"<div id="replay-root"></div>"#));
        assert!(html.contains("<title>fixing the parser</title>"));
        // The global must be assigned before the module is fetched — a static
        // `import` would hoist above it.
        assert!(html.find("__SEANCE_REPLAY__").unwrap() < html.find("import(").unwrap());
    }

    #[test]
    fn shared_assets_shell_points_at_the_host_but_keeps_a_local_manifest() {
        let html = index_html(None, Some("https://cdn.example.com/seance/"));
        assert!(html.contains(r#"import("https://cdn.example.com/seance/seance_web.js")"#));
        assert!(html.contains(r#"href="https://cdn.example.com/seance/style.css""#));
        assert!(html.contains(r#"window.__SEANCE_REPLAY__ = "./recording/manifest.json";"#));
        assert!(html.contains("<title>seance replay</title>"));
    }

    #[test]
    fn shell_escapes_the_title() {
        let html = index_html(Some("a <script> & \"quotes\""), None);
        assert!(html.contains("a &lt;script&gt; &amp; &quot;quotes&quot;"));
    }

    // -- publish seam --------------------------------------------------------

    #[test]
    fn publish_without_a_command_names_the_config_path() {
        let dir = scratch("publish");
        let err = publish(&dir, &PublishConfig::default()).unwrap_err().to_string();
        assert!(err.contains("publish.json"), "unhelpful error: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn publish_returns_the_last_non_empty_stdout_line() {
        let dir = scratch("publish-ok");
        let cfg = PublishConfig {
            assets_url: None,
            publish_command: Some(
                "echo uploading; echo; echo https://share/$(basename \"$1\")".into(),
            ),
        };
        let url = publish(&dir, &cfg).unwrap();
        assert_eq!(url, format!("https://share/{}", dir.file_name().unwrap().to_str().unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn publish_surfaces_a_failing_command() {
        let dir = scratch("publish-fail");
        let cfg = PublishConfig {
            assets_url: None,
            publish_command: Some("echo nope >&2; exit 3".into()),
        };
        assert!(publish(&dir, &cfg).unwrap_err().to_string().contains("nope"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn publish_config_parses_partials() {
        let cfg: PublishConfig = serde_json::from_str(r#"{"assets_url":"https://x/y"}"#).unwrap();
        assert_eq!(cfg.assets_url.as_deref(), Some("https://x/y"));
        assert!(cfg.publish_command.is_none());
        assert!(serde_json::from_str::<PublishConfig>("{}").unwrap().assets_url.is_none());
    }
}
