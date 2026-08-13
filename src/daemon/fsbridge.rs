//! Daemon-side fs/config bridge: executes [`FsOp`]s for attached GUIs.
//!
//! Thin-client contract: every GUI surface that used to read the local disk —
//! file panes, scratchpads, layout, host widgets — goes through here instead,
//! so a GUI on another machine sees the daemon's world, not its own. Ops run
//! on a per-request thread (spawned in `serve_gui`), so a slow disk or a slow
//! host `select_cmd` never stalls the input pipeline.
//!
//! Also owns the daemon-side host widget poller: one thread polling the
//! configs from this machine's `~/.config/seance/host.json`, broadcasting
//! [`GuiEvent::HostWidgets`] to every attached window.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::host::HostState;
use crate::runtime::protocol::{FsOp, GuiEvent};

use super::SharedEngine;

/// Execute one op. Returns `(ok, data, error)` for the FsResult.
pub fn run(op: FsOp, engine: &SharedEngine) -> (bool, Option<Value>, Option<String>) {
    match run_inner(op, engine) {
        Ok(data) => (true, Some(data), None),
        Err(e) => (false, None, Some(e)),
    }
}

fn run_inner(op: FsOp, engine: &SharedEngine) -> Result<Value, String> {
    match op {
        FsOp::Read { path } => {
            let path = expand(&path);
            let bytes = std::fs::read(&path).map_err(|e| format!("read {path:?}: {e}"))?;
            let mtime = mtime_ms(&path);
            Ok(json!({
                "contents_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                "mtime_ms": mtime,
            }))
        }
        FsOp::Write { path, contents_b64 } => {
            let path = expand(&path);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(contents_b64.as_bytes())
                .map_err(|e| format!("bad base64: {e}"))?;
            atomic_write(&path, &bytes).map_err(|e| format!("write {path:?}: {e}"))?;
            Ok(json!({ "mtime_ms": mtime_ms(&path) }))
        }
        FsOp::Stat { path } => {
            let path = expand(&path);
            match std::fs::metadata(&path) {
                Ok(meta) => Ok(json!({
                    "exists": true,
                    "mtime_ms": meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64),
                    "size": meta.len(),
                })),
                Err(_) => Ok(json!({ "exists": false })),
            }
        }
        FsOp::List { path } => {
            let path = expand(&path);
            let mut entries = Vec::new();
            let rd = std::fs::read_dir(&path).map_err(|e| format!("list {path:?}: {e}"))?;
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().to_string();
                let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                entries.push(json!({ "name": name, "is_dir": is_dir }));
            }
            Ok(json!({ "entries": entries }))
        }
        FsOp::Remove { path } => {
            let path = expand(&path);
            std::fs::remove_file(&path).map_err(|e| format!("remove {path:?}: {e}"))?;
            Ok(json!({}))
        }
        FsOp::LayoutLoad => {
            let path = layout_path();
            match std::fs::read_to_string(&path) {
                Ok(s) => Ok(json!({ "json": s })),
                Err(_) => Ok(json!({ "json": null })),
            }
        }
        FsOp::LayoutSave { json: body } => {
            let path = layout_path();
            atomic_write(&path, body.as_bytes()).map_err(|e| format!("layout save: {e}"))?;
            Ok(json!({}))
        }
        FsOp::SubsLoad => {
            let path = subs_path();
            match std::fs::read_to_string(&path) {
                Ok(s) => Ok(json!({ "json": s })),
                Err(_) => Ok(json!({ "json": null })),
            }
        }
        FsOp::SubsSave { json: body } => {
            let path = subs_path();
            atomic_write(&path, body.as_bytes()).map_err(|e| format!("subs save: {e}"))?;
            // Persisting is only half of it: every other window is now
            // rendering a rail that disagrees with the one on disk. Push
            // rather than wait for their next attach — a pin made at the desk
            // should land on the laptop while both are open, which is the
            // whole point of the daemon owning this.
            if let Ok(mut eng) = engine.lock() {
                eng.broadcast(GuiEvent::RailPrefs { json: body });
            }
            Ok(json!({}))
        }
        FsOp::Shell { cmd } => {
            let out = std::process::Command::new("sh")
                .args(["-lc", &cmd])
                .output()
                .map_err(|e| format!("shell: {e}"))?;
            let cap = 64 * 1024;
            let clip = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(cap)]).into_owned();
            Ok(json!({
                "status": out.status.code(),
                "stdout": clip(&out.stdout),
                "stderr": clip(&out.stderr),
            }))
        }
        FsOp::HostSelect { widget, item } => {
            // Fresh load: select is rare and config may have changed on disk.
            let mut host = HostState::load();
            let out = host.select(&widget, &item)?;
            // Push the post-select state to every window right away rather
            // than waiting for the next poll tick.
            host.poll_all();
            let payload = host_payload(&host);
            *LATEST_HOST.lock().unwrap() = Some(payload.clone());
            if let Ok(mut eng) = engine.lock() {
                eng.broadcast(GuiEvent::HostWidgets { widgets: payload });
            }
            Ok(json!({ "output": out }))
        }
    }
}

fn expand(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).as_ref())
}

fn mtime_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

/// Atomic write: temp sibling + rename, dirs created as needed.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("seance-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Shared GUI layout lives beside the daemon's state.json.
fn layout_path() -> PathBuf {
    state_dir_file("layout.json")
}

/// The shared rail arrangement, beside the layout. Note this is the daemon's
/// *state* dir, not `~/.config/seance/` where the pre-0.23 per-GUI file lived
/// — a thin client writing the config path would have written its own disk.
fn subs_path() -> PathBuf {
    state_dir_file("subscriptions.json")
}

fn state_dir_file(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            if let Ok(expanded) = shellexpand::full(&dir) {
                return PathBuf::from(expanded.as_ref()).join(name);
            }
        }
    }
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/").as_ref()).join(name)
}

// ── daemon-side host widget poller ─────────────────────────────────────────

static LATEST_HOST: Mutex<Option<Value>> = Mutex::new(None);

fn host_payload(host: &HostState) -> Value {
    serde_json::to_value(&host.widgets).unwrap_or(Value::Null)
}

/// Latest host widget snapshot, for pushing to a freshly attached window.
pub fn latest_host_widgets() -> Option<Value> {
    LATEST_HOST.lock().ok().and_then(|g| g.clone())
}

/// Start the poll thread. Broadcasts HostWidgets to all GUI conns each tick.
pub fn start_host_poller(engine: SharedEngine) {
    std::thread::Builder::new()
        .name("seance-host-poll".into())
        .spawn(move || {
            let mut host = HostState::load();
            if !host.enabled() {
                return;
            }
            let interval = std::time::Duration::from_secs(host.min_poll_secs());
            loop {
                host.poll_all();
                let payload = host_payload(&host);
                *LATEST_HOST.lock().unwrap() = Some(payload.clone());
                if let Ok(mut eng) = engine.lock() {
                    eng.broadcast(GuiEvent::HostWidgets { widgets: payload });
                }
                std::thread::sleep(interval);
            }
        })
        .ok();
}
