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

/// Load merged library (builtins + user file). User ids override builtins.
pub fn load_all() -> Vec<PromptEntry> {
    let mut by_id: std::collections::BTreeMap<String, PromptEntry> = builtins()
        .into_iter()
        .map(|p| (p.id.clone(), p))
        .collect();
    let path = config_path();
    if let Ok(bytes) = std::fs::read_to_string(&path) {
        if let Ok(file) = serde_json::from_str::<PromptFile>(&bytes) {
            for p in file.prompts {
                by_id.insert(p.id.clone(), p);
            }
        }
    }
    by_id.into_values().collect()
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
            let hay = format!(
                "{} {} {} {}",
                e.id,
                e.title,
                e.body,
                e.tags.join(" ")
            )
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
        let example = PromptFile {
            prompts: vec![PromptEntry {
                id: "my-standup".into(),
                title: "my standup dump".into(),
                body: "Dump a terse standup for the last day of work in this pane's cwd.".into(),
                tags: vec!["personal".into()],
                when_command: None,
            }],
        };
        if let Ok(s) = serde_json::to_string_pretty(&example) {
            let _ = std::fs::write(&path, s);
        }
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
    fn expand_placeholders() {
        let s = expand("pane={pane} cwd={cwd}", "w1", "/tmp", "");
        assert_eq!(s, "pane=w1 cwd=/tmp");
    }
}
