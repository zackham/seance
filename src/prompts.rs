//! Precanned prompt library — fuzzy-palette snippets for daily inject.
//!
//! Config: `~/.config/seance/prompts.json` (or `$SEANCE_STATE_DIR/prompts.json`).
//! Built-ins always available; user file merges by id (user wins).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptEntry {
    pub id: String,
    pub title: String,
    /// Body injected (may contain `{selection}` / `{cwd}` / `{pane}` placeholders).
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// If set, only offer when active pane command contains this substring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_command: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PromptFile {
    #[serde(default)]
    prompts: Vec<PromptEntry>,
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            let expanded = shellexpand::full(&dir)
                .map(|s| s.into_owned())
                .unwrap_or(dir);
            return PathBuf::from(expanded).join("prompts.json");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.config/seance/prompts.json").into_owned())
}

fn builtins() -> Vec<PromptEntry> {
    vec![
        PromptEntry {
            id: "arm".into(),
            title: "⚡ arm agent (ctl skill orientation)".into(),
            body: "You are in seance. Run `seance ctl skill` now, then `seance ctl whoami` and `seance ctl roster`. Prefer finish/note/status-set; use propose for risky shell; durable text goes to the scratchpad.".into(),
            tags: vec!["agent".into(), "arm".into()],
            when_command: None,
        },
        PromptEntry {
            id: "finish-remind".into(),
            title: "remind: finish --stdin when done".into(),
            body: "When finished, complete with:\nseance ctl finish --stdin --status done <<'EOF'\n…your answer…\nEOF".into(),
            tags: vec!["agent".into(), "finish".into()],
            when_command: None,
        },
        PromptEntry {
            id: "status-working".into(),
            title: "status-set working".into(),
            body: "seance ctl status-set working \"on it\"".into(),
            tags: vec!["status".into()],
            when_command: None,
        },
        PromptEntry {
            id: "status-needs-human".into(),
            title: "status-set needs-human".into(),
            body: "seance ctl status-set needs-human \"blocked on you\"".into(),
            tags: vec!["status".into()],
            when_command: None,
        },
        PromptEntry {
            id: "debrief".into(),
            title: "ergonomics debrief (≤40 lines)".into(),
            body: "Short ergonomics debrief only (≤40 lines):\n1. What felt A+?\n2. What was painful?\n3. One change you'd want most.\nComplete with seance ctl finish --stdin --status done --note debrief.".into(),
            tags: vec!["agent".into(), "debrief".into()],
            when_command: None,
        },
        PromptEntry {
            id: "review-diff".into(),
            title: "review uncommitted diff".into(),
            body: "Review `git status` and `git diff` in this repo. Summarize risk, missing tests, and a ship/no-ship call. Write the answer via seance ctl finish --stdin --status done.".into(),
            tags: vec!["git".into(), "review".into()],
            when_command: None,
        },
        PromptEntry {
            id: "explain-error".into(),
            title: "explain the last error on screen".into(),
            body: "Look at the last error/output on screen. Explain root cause and the smallest fix. Prefer finish with the answer.".into(),
            tags: vec!["debug".into()],
            when_command: None,
        },
        PromptEntry {
            id: "shell-summary".into(),
            title: "summarize last command result".into(),
            body: "Summarize what the last command did and whether it succeeded. Note follow-ups.".into(),
            tags: vec!["shell".into()],
            when_command: Some("bash".into()),
        },
    ]
}

/// Merge builtins with an optional user-file JSON blob (user ids win).
/// Pure — the GUI feeds this daemon-fetched bytes from the remote cache;
/// `load_all` feeds it the local file (ctl path).
pub fn merge_with_user(user_json: Option<&str>) -> Vec<PromptEntry> {
    let mut by_id: std::collections::BTreeMap<String, PromptEntry> =
        builtins().into_iter().map(|p| (p.id.clone(), p)).collect();
    if let Some(bytes) = user_json {
        if let Ok(file) = serde_json::from_str::<PromptFile>(bytes) {
            for p in file.prompts {
                by_id.insert(p.id.clone(), p);
            }
        }
    }
    by_id.into_values().collect()
}

/// Load merged library (builtins + LOCAL user file). CLI/ctl path — the GUI
/// reads through `RemoteCache` + [`merge_with_user`] instead (daemon file).
pub fn load_all() -> Vec<PromptEntry> {
    merge_with_user(std::fs::read_to_string(config_path()).ok().as_deref())
}

/// Config path as a bridge string for the DAEMON machine (tilde expands
/// daemon-side). Honors `SEANCE_STATE_DIR` when set (shared-env launches).
pub fn remote_config_path() -> String {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            return format!("{}/prompts.json", dir.trim_end_matches('/'));
        }
    }
    "~/.config/seance/prompts.json".into()
}

/// Pretty JSON body for a fresh user file (example entry). Shared by the
/// local `ensure_user_file` and the GUI's bridge write-if-missing.
pub fn default_user_file_json() -> String {
    let example = PromptFile {
        prompts: vec![PromptEntry {
            id: "my-standup".into(),
            title: "my standup dump".into(),
            body: "Dump a terse standup for the last day of work in this pane's cwd.".into(),
            tags: vec!["personal".into()],
            when_command: None,
        }],
    };
    serde_json::to_string_pretty(&example).unwrap_or_else(|_| "{\"prompts\":[]}".into())
}

/// Fuzzy filter: all query tokens must appear in title/body/tags/id (case-insensitive).
pub fn filter(entries: &[PromptEntry], query: &str) -> Vec<PromptEntry> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return entries.to_vec();
    }
    let tokens: Vec<&str> = q.split_whitespace().collect();
    entries
        .iter()
        .filter(|e| {
            let hay = format!("{} {} {} {}", e.id, e.title, e.body, e.tags.join(" "))
                .to_ascii_lowercase();
            tokens.iter().all(|t| hay.contains(t))
        })
        .cloned()
        .collect()
}

/// Expand placeholders in a prompt body.
pub fn expand(body: &str, pane: &str, cwd: &str, selection: &str) -> String {
    body.replace("{pane}", pane)
        .replace("{cwd}", cwd)
        .replace("{selection}", selection)
}

/// Ensure a default user file exists with comments-as-example (JSON only).
pub fn ensure_user_file() -> PathBuf {
    let path = config_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, default_user_file_json());
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_tokens() {
        let all = builtins();
        let hit = filter(&all, "finish stdin");
        assert!(hit.iter().any(|p| p.id == "finish-remind"));
        let miss = filter(&all, "zzzz-nope");
        assert!(miss.is_empty());
    }

    #[test]
    fn merge_user_overrides_builtin_and_adds() {
        let user = r#"{"prompts":[
            {"id":"arm","title":"custom arm","body":"x"},
            {"id":"mine","title":"mine","body":"y"}
        ]}"#;
        let merged = merge_with_user(Some(user));
        let arm = merged.iter().find(|p| p.id == "arm").unwrap();
        assert_eq!(arm.title, "custom arm");
        assert!(merged.iter().any(|p| p.id == "mine"));
        // No user file → builtins only, and bad JSON degrades the same way.
        assert_eq!(merge_with_user(None).len(), builtins().len());
        assert_eq!(merge_with_user(Some("[not json")).len(), builtins().len());
    }

    #[test]
    fn expand_placeholders() {
        let s = expand("pane={pane} cwd={cwd}", "w1", "/tmp", "");
        assert_eq!(s, "pane=w1 cwd=/tmp");
    }
}
