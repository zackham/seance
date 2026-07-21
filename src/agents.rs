//! Agent launch profiles — absolute paths + flags for swarm / pair work.
//!
//! Config: `$SEANCE_STATE_DIR/agents.toml` or `~/.config/seance/agents.toml`.
//! Built-in defaults cover claude / grok / codex when config is missing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Profile name (`claude`, `grok`, `codex`, …).
    pub name: String,
    /// Absolute or PATH-resolved binary.
    pub bin: String,
    /// Extra argv after the binary.
    #[serde(default)]
    pub args: Vec<String>,
    /// Human-readable notes.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub agents: HashMap<String, AgentProfileToml>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentProfileToml {
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn config_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            let expanded = shellexpand::full(&dir)
                .map(|s| s.into_owned())
                .unwrap_or(dir);
            v.push(PathBuf::from(expanded).join("agents.toml"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        v.push(PathBuf::from(home).join(".config/seance/agents.toml"));
    }
    v
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        if p.exists() {
            return Some(p);
        }
    }
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_escape(bin)))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Built-in defaults (paths resolved at lookup time).
fn builtin(name: &str) -> Option<AgentProfile> {
    match name {
        "claude" => Some(AgentProfile {
            name: "claude".into(),
            bin: first_existing(&[
                "/home/zack/.local/bin/claude",
                "/usr/local/bin/claude",
            ])
            .unwrap_or_else(|| "claude".into()),
            args: vec!["--dangerously-skip-permissions".into()],
            note: Some(
                "Claude Code interactive; skip tool confirms for pair/swarm. \
                 Human still owns input via agency."
                    .into(),
            ),
        }),
        "grok" => Some(AgentProfile {
            name: "grok".into(),
            bin: first_existing(&["/home/zack/.grok/bin/grok", "/usr/local/bin/grok"])
                .unwrap_or_else(|| "grok".into()),
            args: vec!["--always-approve".into()],
            note: Some("Grok Build TUI with always-approve.".into()),
        }),
        "codex" => {
            let bin = find_codex().unwrap_or_else(|| "codex".into());
            Some(AgentProfile {
                name: "codex".into(),
                bin,
                args: vec![
                    "-a".into(),
                    "never".into(),
                    "-s".into(),
                    "danger-full-access".into(),
                ],
                note: Some(
                    "Codex interactive with full sandbox access so SEANCE \
                     scratchpad + socket work. Prefer propose/caps for \
                     risky shell work."
                        .into(),
                ),
            })
        }
        "shell" => Some(AgentProfile {
            name: "shell".into(),
            bin: "bash".into(),
            args: vec!["-l".into()],
            note: Some("Login shell (human-friendly; use propose for risky inject).".into()),
        }),
        _ => None,
    }
}

fn first_existing(paths: &[&str]) -> Option<String> {
    for p in paths {
        if Path::new(p).exists() {
            return Some((*p).to_string());
        }
    }
    None
}

fn find_codex() -> Option<String> {
    if let Some(p) = which("codex") {
        return Some(p.to_string_lossy().into_owned());
    }
    // mise install layout
    let home = std::env::var("HOME").ok()?;
    let base = PathBuf::from(home).join(".local/share/mise/installs/npm-openai-codex");
    let rd = std::fs::read_dir(base).ok()?;
    let mut versions: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    versions.sort();
    for v in versions.into_iter().rev() {
        let cand = v.join("bin/codex");
        if cand.exists() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

fn load_config() -> AgentConfig {
    for path in config_paths() {
        if let Ok(bytes) = std::fs::read_to_string(&path) {
            // Minimal TOML subset via json-ish: we accept full toml if serde works.
            // Without a toml crate, parse a simple line format OR use json file too.
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(c) = serde_json::from_str(&bytes) {
                    return c;
                }
            }
            // Try JSON even for .toml misnamed; real toml needs a dep — use JSON:
            // also support agents.json
        }
    }
    // agents.json sibling
    for path in config_paths() {
        let json_path = path.with_extension("json");
        if let Ok(bytes) = std::fs::read_to_string(&json_path) {
            if let Ok(c) = serde_json::from_str(&bytes) {
                return c;
            }
        }
    }
    AgentConfig::default()
}

/// Resolve a profile by name. Config overrides builtins.
pub fn resolve(name: &str) -> Result<AgentProfile, String> {
    let key = name.to_ascii_lowercase();
    let cfg = load_config();
    if let Some(t) = cfg.agents.get(&key) {
        let bin = which(&t.bin)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| t.bin.clone());
        return Ok(AgentProfile {
            name: key,
            bin,
            args: t.args.clone(),
            note: t.note.clone(),
        });
    }
    let mut p = builtin(&key).ok_or_else(|| {
        format!("unknown agent profile '{name}' (claude|grok|codex|shell or config)")
    })?;
    // Resolve bin via PATH if relative.
    if !p.bin.contains('/') {
        if let Some(w) = which(&p.bin) {
            p.bin = w.to_string_lossy().into_owned();
        }
    }
    if p.bin.contains('/') && !Path::new(&p.bin).exists() {
        return Err(format!(
            "agent '{name}' binary not found: {} (edit ~/.config/seance/agents.json)",
            p.bin
        ));
    }
    Ok(p)
}

/// Build the full command string for `exec` (space-joined; bins without spaces).
pub fn command_line(profile: &AgentProfile) -> String {
    let mut parts = vec![profile.bin.clone()];
    parts.extend(profile.args.iter().cloned());
    parts.join(" ")
}

/// Boot-dialog clear sequences after `--wait-ready` (raw PTY bytes).
///
/// Agents often present a first-run trust/update modal. Masters should not have
/// to remember `send-raw $'\r'`. Each entry is raw bytes to inject with a short
/// settle delay between them.
pub fn boot_clear_sequence(profile_name: &str) -> Vec<Vec<u8>> {
    match profile_name.to_ascii_lowercase().as_str() {
        // Claude Code: "trust this folder?" → Enter accepts.
        "claude" => vec![b"\r".to_vec()],
        // Codex: update menu — option 2 is typically Skip.
        "codex" => vec![b"2\r".to_vec()],
        // Grok: usually no modal; trailing Enter can help multi-line inject later.
        "grok" => vec![],
        _ => vec![],
    }
}

/// Guess profile name from a command line (for post-spawn boot clear).
pub fn guess_profile_from_command(command: &str) -> Option<&'static str> {
    let c = command.to_ascii_lowercase();
    if c.contains("claude") {
        Some("claude")
    } else if c.contains("codex") {
        Some("codex")
    } else if c.contains("grok") {
        Some("grok")
    } else {
        None
    }
}

/// Doctor report for known agents.
pub fn doctor() -> Vec<DoctorRow> {
    let names = ["claude", "grok", "codex", "shell"];
    names
        .iter()
        .map(|n| match resolve(n) {
            Ok(p) => {
                let exists = Path::new(&p.bin).exists() || which(&p.bin).is_some();
                let version = if exists {
                    version_of(&p.bin)
                } else {
                    None
                };
                DoctorRow {
                    name: (*n).into(),
                    ok: exists,
                    bin: p.bin,
                    args: p.args,
                    version,
                    detail: if exists {
                        p.note.unwrap_or_else(|| "ok".into())
                    } else {
                        "binary missing".into()
                    },
                }
            }
            Err(e) => DoctorRow {
                name: (*n).into(),
                ok: false,
                bin: String::new(),
                args: vec![],
                version: None,
                detail: e,
            },
        })
        .collect()
}

fn version_of(bin: &str) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?.trim();
    if line.is_empty() {
        let e = String::from_utf8_lossy(&out.stderr);
        Some(e.lines().next()?.trim().to_string())
    } else {
        Some(line.to_string())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorRow {
    pub name: String,
    pub ok: bool,
    pub bin: String,
    pub args: Vec<String>,
    pub version: Option<String>,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_profile_resolves() {
        let p = resolve("shell").unwrap();
        assert_eq!(p.name, "shell");
        assert!(command_line(&p).contains("bash"));
    }
}
