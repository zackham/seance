//! Launch preference: run against the local daemon or a remote host.
//!
//! Persisted at `~/.config/seance/launch.json` (or `$XDG_CONFIG_HOME/seance/`)
//! so the picker prefills — choose "remote: desk" once and every subsequent
//! launch goes straight there. CLI overrides: `seance --local` /
//! `seance --remote <host>` bypass and rewrite the preference.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Local,
    Remote,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchPref {
    pub mode: LaunchMode,
    /// Remote host (ssh destination) — kept even in local mode so switching
    /// back to remote prefills the last host.
    #[serde(default)]
    pub host: Option<String>,
}

pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("seance/launch.json");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.config/seance/launch.json").as_ref())
}

pub fn load() -> Option<LaunchPref> {
    let bytes = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&bytes).ok()
}

pub fn save(pref: &LaunchPref) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(json) = serde_json::to_string_pretty(pref) else {
        return;
    };
    // Atomic-ish: write sibling then rename.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pref_roundtrips() {
        let pref = LaunchPref {
            mode: LaunchMode::Remote,
            host: Some("desk".into()),
        };
        let json = serde_json::to_string(&pref).unwrap();
        assert!(json.contains("\"remote\""));
        let back: LaunchPref = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mode, LaunchMode::Remote);
        assert_eq!(back.host.as_deref(), Some("desk"));
    }

    #[test]
    fn local_pref_without_host_parses() {
        let back: LaunchPref = serde_json::from_str(r#"{"mode":"local"}"#).unwrap();
        assert_eq!(back.mode, LaunchMode::Local);
        assert!(back.host.is_none());
    }
}
