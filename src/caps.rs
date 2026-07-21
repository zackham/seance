//! Capability / consent layer (v0).
//!
//! Daemon-enforced policy for **ctl-mediated** actions. Default is `open`
//! (current behavior) so existing agent loops keep working. Switch a
//! workspace (or the global default) to `propose_required` or `locked` to
//! force ghost proposals / read-only.
//!
//! Grants are per-principal (`agent:<slug>` or `cli`) and optional TTL.
//! Persistence: `~/.local/share/seance/caps.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Workspace / global policy mode.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// All ctl ops allowed (legacy / current default).
    #[default]
    Open,
    /// `send` / `send_raw` require a grant or must use `propose` instead.
    ProposeRequired,
    /// Only observation + HITL: read, list, status, timeline, watch, human,
    /// ask, propose, status_set, scratchpad, commands. Everything else denied.
    Locked,
}

impl PolicyMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "open" => Some(Self::Open),
            "propose_required" | "propose-required" | "cautious" => Some(Self::ProposeRequired),
            "locked" | "lock" | "readonly" | "read-only" => Some(Self::Locked),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::ProposeRequired => "propose_required",
            Self::Locked => "locked",
        }
    }
}

/// Ops that are always free under any policy (observe + HITL + self-report).
const ALWAYS_ALLOW: &[&str] = &[
    "list",
    "read",
    "status",
    "timeline",
    "watch",
    "human",
    "ask",
    "ask_result",
    "propose",
    "propose_result",
    "status_set",
    "scratchpad",
    "commands",
    "last_command",
    "cmd_begin",
    "cmd_end",
    "whoami",
    "caps",
    "policy_get",
    "seize",
    "release",
    "drive_mode",
    "doctor",
    "brief",
    "roster",
    "note",
    "finish",
];

/// Ops that `propose_required` gates (need grant or use propose).
const SEND_OPS: &[&str] = &["send", "send_raw"];

/// Ops that `locked` additionally denies (even with grants? grants can lift).
const MUTATING_OPS: &[&str] = &[
    "send",
    "send_raw",
    "kill",
    "new",
    "workspace_fork",
    "policy_set",
    "caps_grant",
    "caps_revoke",
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Grant {
    /// Principal: `agent:slug` or `cli` or `*`.
    pub principal: String,
    /// Capability name matching an op (e.g. `send`, `kill`, `new`) or `*`.
    pub cap: String,
    /// Optional workspace scope; None = all workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Expiry as unix millis; None = permanent until revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CapStore {
    /// Global default policy.
    #[serde(default)]
    pub default_policy: PolicyMode,
    /// Per-workspace overrides.
    #[serde(default)]
    pub workspace_policy: HashMap<String, PolicyMode>,
    #[serde(default)]
    pub grants: Vec<Grant>,
}

impl CapStore {
    pub fn load() -> Self {
        let path = store_path();
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = store_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn policy_for(&self, workspace: Option<&str>) -> PolicyMode {
        if let Some(ws) = workspace {
            if let Some(p) = self.workspace_policy.get(ws) {
                return p.clone();
            }
        }
        self.default_policy.clone()
    }

    pub fn set_policy(&mut self, workspace: Option<String>, mode: PolicyMode) {
        match workspace {
            Some(ws) => {
                self.workspace_policy.insert(ws, mode);
            }
            None => self.default_policy = mode,
        }
    }

    pub fn grant(&mut self, g: Grant) {
        // Replace existing identical principal+cap+workspace.
        self.grants.retain(|x| {
            !(x.principal == g.principal && x.cap == g.cap && x.workspace == g.workspace)
        });
        self.grants.push(g);
    }

    pub fn revoke(&mut self, principal: &str, cap: &str, workspace: Option<&str>) -> usize {
        let before = self.grants.len();
        self.grants.retain(|g| {
            !(g.principal == principal
                && (cap == "*" || g.cap == cap)
                && (workspace.is_none() || g.workspace.as_deref() == workspace))
        });
        before - self.grants.len()
    }

    fn purge_expired(&mut self) {
        let now = now_ms();
        self.grants
            .retain(|g| g.expires_ms.is_none_or(|e| e > now));
    }

    fn has_grant(&self, principal: &str, op: &str, workspace: Option<&str>) -> bool {
        let now = now_ms();
        self.grants.iter().any(|g| {
            if g.expires_ms.is_some_and(|e| e <= now) {
                return false;
            }
            let principal_ok = g.principal == "*" || g.principal == principal;
            let cap_ok = g.cap == "*" || g.cap == op;
            let ws_ok = g.workspace.is_none()
                || workspace.is_some_and(|w| g.workspace.as_deref() == Some(w));
            principal_ok && cap_ok && ws_ok
        })
    }

    /// Check whether `principal` may perform `op` in `workspace`.
    /// Returns `Ok(())` or `Err(message)` suitable for ControlResponse.
    pub fn check(
        &mut self,
        principal: &str,
        op: &str,
        workspace: Option<&str>,
    ) -> Result<(), String> {
        self.purge_expired();

        // Human UI and daemon are unrestricted.
        if principal == "human" || principal == "daemon" || principal == "system" {
            return Ok(());
        }

        if ALWAYS_ALLOW.contains(&op) {
            return Ok(());
        }

        // Explicit grant always wins.
        if self.has_grant(principal, op, workspace) {
            return Ok(());
        }

        let policy = self.policy_for(workspace);
        match policy {
            PolicyMode::Open => Ok(()),
            PolicyMode::ProposeRequired => {
                if SEND_OPS.contains(&op) {
                    Err(format!(
                        "policy propose_required: '{op}' needs a grant \
                         (`seance ctl grant {principal} {op}`) or use `propose` instead"
                    ))
                } else {
                    Ok(())
                }
            }
            PolicyMode::Locked => {
                if MUTATING_OPS.contains(&op) || SEND_OPS.contains(&op) {
                    Err(format!(
                        "policy locked: '{op}' denied for {principal} \
                         (grant with `seance ctl grant` or set policy open)"
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }
}

fn store_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            let expanded = shellexpand::full(&dir)
                .map(|s| s.into_owned())
                .unwrap_or(dir);
            return PathBuf::from(expanded).join("caps.json");
        }
    }
    PathBuf::from(shellexpand::tilde("~/.local/share/seance").into_owned()).join("caps.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Map a ControlRequest to an op name for capability checks.
pub fn op_name(req: &crate::control::ControlRequest) -> &'static str {
    use crate::control::ControlRequest::*;
    match req {
        List { .. } => "list",
        New { .. } => "new",
        Send { .. } => "send",
        SendRaw { .. } => "send_raw",
        Read { .. } => "read",
        Status { .. } => "status",
        Kill { .. } => "kill",
        Scratchpad { .. } => "scratchpad",
        Timeline { .. } => "timeline",
        StatusSet { .. } => "status_set",
        Ask { .. } => "ask",
        AskResult { .. } => "ask_result",
        Propose { .. } => "propose",
        ProposeResult { .. } => "propose_result",
        Human { .. } => "human",
        WorkspaceFork { .. } => "workspace_fork",
        CmdBegin { .. } => "cmd_begin",
        CmdEnd { .. } => "cmd_end",
        Commands { .. } => "commands",
        LastCommand { .. } => "last_command",
        Watch { .. } => "watch",
        Whoami { .. } => "whoami",
        Caps { .. } => "caps",
        CapsGrant { .. } => "caps_grant",
        CapsRevoke { .. } => "caps_revoke",
        PolicyGet { .. } => "policy_get",
        PolicySet { .. } => "policy_set",
        Seize { .. } => "seize",
        Release { .. } => "release",
        DriveMode { .. } => "drive_mode",
        Doctor { .. } => "doctor",
        Brief { .. } => "brief",
        Note { .. } => "note",
        Finish { .. } => "finish",
        Roster { .. } => "roster",
    }
}

/// Principal string from a request's `from` field.
pub fn principal_of(from: &Option<String>) -> String {
    match from {
        Some(f) => format!("agent:{f}"),
        None => "cli".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_allows_send() {
        let mut s = CapStore::default();
        assert!(s.check("agent:w", "send", Some("lab")).is_ok());
    }

    #[test]
    fn propose_required_blocks_send_without_grant() {
        let mut s = CapStore {
            default_policy: PolicyMode::ProposeRequired,
            ..Default::default()
        };
        assert!(s.check("agent:w", "send", Some("lab")).is_err());
        assert!(s.check("agent:w", "propose", Some("lab")).is_ok());
        assert!(s.check("agent:w", "read", Some("lab")).is_ok());
        s.grant(Grant {
            principal: "agent:w".into(),
            cap: "send".into(),
            workspace: Some("lab".into()),
            expires_ms: None,
        });
        assert!(s.check("agent:w", "send", Some("lab")).is_ok());
    }

    #[test]
    fn locked_blocks_kill() {
        let mut s = CapStore {
            default_policy: PolicyMode::Locked,
            ..Default::default()
        };
        assert!(s.check("cli", "kill", None).is_err());
        assert!(s.check("cli", "list", None).is_ok());
    }
}
