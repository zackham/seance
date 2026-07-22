//! Optional host-bridge widgets (vita-adjacent surfaces without linking vita).
//!
//! Config: `~/.config/seance/host.json` (auto-seeded with claude accounts if
//! the default vita adapter script exists). Poll commands emit JSON on stdout;
//! seance only renders + shells `select` templates. Fail closed: hide strip.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// One chip in a host sidebar widget.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub state: String,
    /// Primary detail line (e.g. "4% 5h · ↻3:00pm").
    #[serde(default)]
    pub detail: String,
    /// Optional second line (e.g. "87% wk · ↻thu 2pm").
    #[serde(default)]
    pub detail2: String,
    #[serde(default)]
    pub selected: bool,
}

/// Snapshot returned by a poll command (schema 1).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostWidgetSnap {
    #[serde(default = "schema_one")]
    pub schema: u32,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub items: Vec<HostItem>,
    #[serde(default)]
    pub active: Option<String>,
    /// Last error from poll (seance-side; not from host).
    #[serde(skip)]
    pub error: Option<String>,
    #[serde(skip)]
    pub fetched_at: Option<Instant>,
}

fn schema_one() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize)]
pub struct HostSidebarConfig {
    pub id: String,
    #[serde(default = "default_title")]
    pub title: String,
    /// Shell command (tilde-expanded). Stdout = HostWidgetSnap JSON.
    pub poll_cmd: String,
    /// Shell command; `{id}` replaced with item id.
    #[serde(default)]
    pub select_cmd: Option<String>,
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

fn default_title() -> String {
    "host".into()
}
fn default_poll_secs() -> u64 {
    20
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub sidebar: Vec<HostSidebarConfig>,
}

/// Runtime state for all host sidebar widgets.
#[derive(Clone, Debug, Default)]
pub struct HostState {
    pub widgets: Vec<HostWidgetSnap>,
    pub configs: Vec<HostSidebarConfig>,
    /// True after first successful poll of any widget (for empty-vs-missing UI).
    pub ever_ok: bool,
}

impl HostState {
    pub fn load() -> Self {
        let configs = load_host_config();
        Self {
            widgets: Vec::new(),
            configs,
            ever_ok: false,
        }
    }

    pub fn poll_all(&mut self) {
        if self.configs.is_empty() {
            self.widgets.clear();
            return;
        }
        let mut next = Vec::with_capacity(self.configs.len());
        for cfg in &self.configs {
            match poll_widget(cfg) {
                Ok(mut snap) => {
                    if snap.id.is_empty() {
                        snap.id = cfg.id.clone();
                    }
                    if snap.title.is_empty() {
                        snap.title = cfg.title.clone();
                    }
                    snap.fetched_at = Some(Instant::now());
                    self.ever_ok = true;
                    next.push(snap);
                }
                Err(e) => {
                    // Keep last good snapshot for this id if any.
                    if let Some(prev) = self.widgets.iter().find(|w| w.id == cfg.id) {
                        let mut keep = prev.clone();
                        keep.error = Some(e);
                        next.push(keep);
                    }
                    // else: omit — strip hidden until first success
                }
            }
        }
        self.widgets = next;
    }

    pub fn select(&mut self, widget_id: &str, item_id: &str) -> Result<String, String> {
        let cfg = self
            .configs
            .iter()
            .find(|c| c.id == widget_id)
            .ok_or_else(|| format!("unknown host widget '{widget_id}'"))?;
        let tmpl = cfg
            .select_cmd
            .as_deref()
            .ok_or_else(|| "no select_cmd configured".to_string())?;
        let cmd = expand_tilde(&tmpl.replace("{id}", item_id));
        let out = run_shell(&cmd)?;
        // Refresh this widget immediately.
        if let Ok(mut snap) = poll_widget(cfg) {
            if snap.id.is_empty() {
                snap.id = cfg.id.clone();
            }
            if snap.title.is_empty() {
                snap.title = cfg.title.clone();
            }
            snap.fetched_at = Some(Instant::now());
            if let Some(slot) = self.widgets.iter_mut().find(|w| w.id == widget_id) {
                *slot = snap;
            } else {
                self.widgets.push(snap);
            }
            self.ever_ok = true;
        }
        Ok(out)
    }

    pub fn min_poll_secs(&self) -> u64 {
        self.configs
            .iter()
            .map(|c| c.poll_secs.max(5))
            .min()
            .unwrap_or(20)
    }

    pub fn enabled(&self) -> bool {
        !self.configs.is_empty()
    }
}

fn host_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("seance/host.json");
    }
    PathBuf::from(shellexpand::tilde("~/.config/seance/host.json").into_owned())
}

fn default_adapter_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/work/vita/scripts/seance_host_accounts.py").into_owned())
}

fn default_config_json(adapter: &Path) -> String {
    let a = adapter.display();
    format!(
        r#"{{
  "sidebar": [
    {{
      "id": "claude-accounts",
      "title": "claude",
      "poll_secs": 20,
      "poll_cmd": "python3 {a} list",
      "select_cmd": "python3 {a} select {{id}}"
    }}
  ]
}}
"#
    )
}

/// Load host.json; seed default claude adapter if missing and script exists.
pub fn load_host_config() -> Vec<HostSidebarConfig> {
    let path = host_config_path();
    let adapter = default_adapter_path();

    if !path.exists() {
        if adapter.is_file() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, default_config_json(&adapter));
        } else {
            return Vec::new();
        }
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let cfg: HostConfig = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[seance host] bad host.json: {e}");
            return Vec::new();
        }
    };
    cfg.sidebar
        .into_iter()
        .filter(|c| !c.poll_cmd.trim().is_empty())
        .collect()
}

fn expand_tilde(s: &str) -> String {
    shellexpand::tilde(s).into_owned()
}

fn run_shell(cmd: &str) -> Result<String, String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let code = out.status.code().unwrap_or(-1);
        return Err(format!("exit {code}: {}", err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn poll_widget(cfg: &HostSidebarConfig) -> Result<HostWidgetSnap, String> {
    let cmd = expand_tilde(&cfg.poll_cmd);
    let stdout = run_shell(&cmd)?;
    if stdout.is_empty() {
        return Err("empty stdout".into());
    }
    // Tolerate trailing log lines: take last non-empty line that parses as object.
    let mut last_err = "no json object in stdout".to_string();
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        match serde_json::from_str::<HostWidgetSnap>(line) {
            Ok(snap) if snap.schema == 0 || snap.schema == 1 => return Ok(snap),
            Ok(_) => last_err = "unsupported schema".into(),
            Err(e) => last_err = e.to_string(),
        }
    }
    // Whole stdout as one json
    match serde_json::from_str::<HostWidgetSnap>(&stdout) {
        Ok(snap) if snap.schema == 0 || snap.schema == 1 => Ok(snap),
        Ok(_) => Err("unsupported schema".into()),
        Err(_) => Err(last_err),
    }
}
