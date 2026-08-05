//! Optional host-bridge surfaces (vita-adjacent surfaces without linking vita).
//!
//! Config: `~/.config/seance/host.json` (auto-seeded with claude accounts if
//! the default vita adapter script exists). Poll commands emit JSON on stdout;
//! seance only renders + shells `select` templates. Fail closed: hide strip.
//!
//! Two shapes share that config and that JSON schema:
//!
//! - **sidebar widgets** (`sidebar[]`) — polled on a clock, every item painted
//!   as a chip. For small, ambient, always-true state (which claude account is
//!   live). The daemon polls; every window gets the broadcast.
//! - **menus** (`menus[]`) — one chip that asks its question only when clicked.
//!   `list_cmd` runs on demand, its items drop into a picker, and choosing one
//!   runs `select_cmd`. For lists too long, too slow, or too rarely wanted to
//!   sit in the rail (the week's meetings). Nothing about a menu is polled, and
//!   the GUI reads the config itself over the fs bridge — no daemon involvement
//!   beyond running the two commands.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// One item from a host: a chip in a polled sidebar widget, or a row in a menu.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// Menus only: heading this item clusters under ("thu · aug 6"). Items are
    /// rendered in host order, so a group is a run of adjacent items — the host
    /// decides the ordering, seance only draws the seams.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
}

/// `{id}` is pasted into `select_cmd` unquoted (same as the sidebar-widget
/// path), so an id has to survive `sh -c` untouched. Hosts put the human text
/// in `label`; the id is a handle.
pub fn safe_item_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@+-=".contains(c))
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
    /// Last error from poll (seance-side; not from host). Serialized so the
    /// daemon-side poller's errors reach thin-client sidebars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// One on-demand menu: a single chip that runs `list_cmd` when clicked and
/// `select_cmd` when a row is chosen. Never polled.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct HostMenuConfig {
    pub id: String,
    #[serde(default = "default_title")]
    pub title: String,
    /// Shell command (tilde-expanded). Stdout = HostWidgetSnap JSON — the same
    /// schema the polled widgets emit, `items[]` being the rows.
    pub list_cmd: String,
    /// Shell command; `{id}` replaced with the chosen item's id.
    pub select_cmd: String,
    /// What the dropdown says when `items` comes back empty. A host that knows
    /// why its list is empty ("no meetings in the next 7 days") should say so
    /// here rather than leaving seance to guess.
    #[serde(default)]
    pub empty: Option<String>,
}

/// What a menu's `select_cmd` may print back. All optional — exit 0 with no
/// output is still success; this is how a host adds detail to it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HostSelectResult {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
    /// Success line for the notification (defaults to the item's label).
    #[serde(default)]
    pub message: Option<String>,
    /// A circle the host just created and wants the rail to jump to.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Whether that circle is pinned to the top of the rail. Defaults to
    /// TRUE: a circle you just deliberately conjured is, by definition, the
    /// thing you are about to work in, and having it land somewhere you then
    /// have to go find defeats the point of the menu. A host that spawns
    /// background work sets this false.
    #[serde(default)]
    pub pin: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub sidebar: Vec<HostSidebarConfig>,
    #[serde(default)]
    pub menus: Vec<HostMenuConfig>,
}

/// Menus out of a host.json body, dropping any entry missing a command.
/// Pure: the GUI feeds this bytes read over the fs bridge, because host.json
/// lives on the DAEMON machine and a thin client has no business reading its
/// own. A parse error is the caller's to report — it keeps the last good list.
pub fn parse_menus(raw: &str) -> Result<Vec<HostMenuConfig>, serde_json::Error> {
    Ok(serde_json::from_str::<HostConfig>(raw)?
        .menus
        .into_iter()
        .filter(|m| {
            !m.id.trim().is_empty()
                && !m.list_cmd.trim().is_empty()
                && !m.select_cmd.trim().is_empty()
        })
        .collect())
}

/// Items out of a `list_cmd`'s stdout: the polled-widget parse, minus the
/// widget. Items with an id that couldn't survive `sh -c` are dropped here —
/// see [`safe_item_id`].
pub fn parse_menu_items(stdout: &str) -> Result<Vec<HostItem>, String> {
    let snap = parse_snapshot(stdout)?;
    Ok(snap
        .items
        .into_iter()
        .filter(|i| safe_item_id(&i.id))
        .collect())
}

/// Config path as a bridge string for the DAEMON machine (tilde expands
/// daemon-side), for the GUI's own read of `menus[]`.
pub fn remote_config_path() -> String {
    "~/.config/seance/host.json".into()
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

/// A snapshot, if `raw` is one. `schema` 0 (absent) and 1 are accepted;
/// anything else is refused rather than half-read.
fn as_snapshot(raw: &str) -> Result<HostWidgetSnap, String> {
    match serde_json::from_str::<HostWidgetSnap>(raw) {
        Ok(snap) if snap.schema == 0 || snap.schema == 1 => Ok(snap),
        Ok(_) => Err("unsupported schema".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Schema-v1 snapshot out of a host command's stdout. Shared by the polled
/// widgets and the on-demand menus — one wire schema, two affordances.
///
/// Whole-stdout FIRST, line-scan second. The order matters: every field of
/// `HostWidgetSnap` has a default, so a lone `{"id":"x","label":"X"}` — one
/// pretty-printed *item* line — deserializes cleanly into an empty snapshot.
/// Scanning first therefore turned any indented JSON into "host returned no
/// items", which is the worst possible failure: it looks like an answer. The
/// line scan survives only as the leaked-log fallback it was written to be,
/// and now insists on a line that carries a snapshot's own key.
pub fn parse_snapshot(stdout: &str) -> Result<HostWidgetSnap, String> {
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Err("empty stdout".into());
    }
    let whole_err = match as_snapshot(stdout) {
        Ok(snap) => return Ok(snap),
        Err(e) => e,
    };
    // Logs leaked around the JSON: take the last line that is a snapshot.
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        // `items`/`schema` are the snapshot's alone — an item line has neither,
        // so it can no longer masquerade as the whole payload.
        let is_snapshot = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| {
                v.as_object()
                    .map(|o| o.contains_key("items") || o.contains_key("schema"))
            })
            .unwrap_or(false);
        if !is_snapshot {
            continue;
        }
        if let Ok(snap) = as_snapshot(line) {
            return Ok(snap);
        }
    }
    Err(whole_err)
}

fn poll_widget(cfg: &HostSidebarConfig) -> Result<HostWidgetSnap, String> {
    let cmd = expand_tilde(&cfg.poll_cmd);
    parse_snapshot(&run_shell(&cmd)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_parse_alongside_sidebar_widgets() {
        let raw = r#"{
          "sidebar": [{"id":"acct","poll_cmd":"x"}],
          "menus": [{
            "id":"meetings","title":"meeting",
            "list_cmd":"adapter list","select_cmd":"adapter select {id}",
            "empty":"nothing this week"
          }]
        }"#;
        let menus = parse_menus(raw).unwrap();
        assert_eq!(menus.len(), 1);
        assert_eq!(menus[0].id, "meetings");
        assert_eq!(menus[0].select_cmd, "adapter select {id}");
        assert_eq!(menus[0].empty.as_deref(), Some("nothing this week"));
        // The sidebar half of the same file still loads unchanged.
        let cfg: HostConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.sidebar.len(), 1);
    }

    #[test]
    fn menus_default_to_empty_and_drop_incomplete_entries() {
        // A config with no menus key at all is the common case, not an error.
        assert!(parse_menus(r#"{"sidebar":[]}"#).unwrap().is_empty());
        // Half a menu can't be clicked, so it isn't drawn.
        let half = r#"{"menus":[
          {"id":"a","list_cmd":"x","select_cmd":"  "},
          {"id":" ","list_cmd":"x","select_cmd":"y"}
        ]}"#;
        assert!(parse_menus(half).unwrap().is_empty());
        assert!(parse_menus("[not json").is_err());
    }

    #[test]
    fn menu_items_come_back_in_host_order_with_groups() {
        let out = r#"{"schema":1,"items":[
          {"id":"mtg:2026-08-06/l10","label":"L10","detail":"9:00a","group":"thu"},
          {"id":"mtg:2026-08-06/1-1","label":"1:1","group":"thu"}
        ]}"#;
        let items = parse_menu_items(out).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "mtg:2026-08-06/l10");
        assert_eq!(items[0].group, "thu");
        assert_eq!(items[1].label, "1:1");
    }

    #[test]
    fn menu_items_drop_ids_that_could_not_survive_the_shell() {
        // `{id}` is pasted into `sh -c` unquoted; anything that could change
        // the command's meaning never reaches the dropdown.
        let out = r#"{"schema":1,"items":[
          {"id":"good-one","label":"keep"},
          {"id":"rm -rf /; x","label":"drop"},
          {"id":"$(whoami)","label":"drop"},
          {"id":"","label":"drop"}
        ]}"#;
        let items = parse_menu_items(out).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "good-one");
    }

    #[test]
    fn safe_item_id_accepts_refs_and_rejects_metacharacters() {
        assert!(safe_item_id("mtg:2026-08-06/web-fe-sync"));
        assert!(safe_item_id("account-3"));
        assert!(safe_item_id("a@b.com"));
        for bad in [
            "", "a b", "a;b", "a|b", "a`b`", "a&b", "a\nb", "a'b", "a\"b", "a>b", "a*b",
        ] {
            assert!(!safe_item_id(bad), "{bad:?} must not be shell-safe");
        }
        assert!(!safe_item_id(&"a".repeat(257)));
    }

    #[test]
    fn snapshot_parse_tolerates_leaked_log_lines_and_pretty_json() {
        let leaked =
            "loading…\nwarn: whatever\n{\"schema\":1,\"items\":[{\"id\":\"x\",\"label\":\"X\"}]}";
        assert_eq!(parse_snapshot(leaked).unwrap().items.len(), 1);
        let pretty = "{\n  \"schema\": 1,\n  \"items\": []\n}";
        assert!(parse_snapshot(pretty).unwrap().items.is_empty());
        assert!(parse_snapshot("   ").is_err());
        assert!(parse_snapshot("not json at all").is_err());
        // Schema the code doesn't know is refused rather than half-read.
        assert!(parse_snapshot(r#"{"schema":9,"items":[]}"#).is_err());
    }

    #[test]
    fn select_result_tolerates_an_absent_or_partial_envelope() {
        // Exit 0 with no output is success; the envelope only adds detail.
        let empty = serde_json::from_str::<HostSelectResult>("{}").unwrap();
        assert!(empty.ok.is_none() && empty.workspace.is_none());
        let full = serde_json::from_str::<HostSelectResult>(
            r#"{"ok":true,"workspace":"mtg-l10","message":"armed"}"#,
        )
        .unwrap();
        assert_eq!(full.workspace.as_deref(), Some("mtg-l10"));
        assert_eq!(full.message.as_deref(), Some("armed"));
    }
}
