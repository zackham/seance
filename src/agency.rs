//! Input ownership / co-presence — human and agents share panes with
//! transferrable agency.
//!
//! Rules:
//! - Human keystrokes always steal ownership of that pane.
//! - Agent inject (`ctl send` / `send-raw`) is denied while a human owns the
//!   pane (unless idle-grace has elapsed, or `--force`, or explicit release).
//! - Ownership is daemon state, broadcast to the GUI, and visible on `ctl human`.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long after the last human keystroke before an agent may inject again
/// without an explicit `release`.
pub const HUMAN_IDLE_GRACE: Duration = Duration::from_millis(3000);

/// Who currently holds the keyboard for a pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// Either may claim with the next input/inject.
    #[default]
    None,
    Human,
    /// `agent:<slug>` or bare principal string.
    Agent(String),
}

impl Owner {
    pub fn as_str(&self) -> String {
        match self {
            Self::None => "none".into(),
            Self::Human => "human".into(),
            Self::Agent(a) => {
                if a.starts_with("agent:") || a == "cli" {
                    a.clone()
                } else {
                    format!("agent:{a}")
                }
            }
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "" | "none" | "idle" => Self::None,
            "human" | "you" => Self::Human,
            other => {
                if let Some(rest) = other.strip_prefix("agent:") {
                    Self::Agent(rest.to_string())
                } else if other == "cli" {
                    Self::Agent("cli".into())
                } else {
                    Self::Agent(other.to_string())
                }
            }
        }
    }

    pub fn is_human(&self) -> bool {
        matches!(self, Self::Human)
    }

    #[allow(dead_code)] // owner-posture predicate exercised by agency tests; symmetric with is_human
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Pair-led / agent-led / human-locked posture for a pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DriveMode {
    /// Default: human-wins; agents inject when owner allows.
    #[default]
    Pair,
    /// Human has locked the pane; agents cannot inject even after idle grace
    /// (unless `--force`).
    LockedHuman,
    /// Explicit “let agents drive” — still human-wins on keystroke, but idle
    /// grace is shorter conceptually (same grace for now).
    AgentLed,
}

impl DriveMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pair => "pair",
            Self::LockedHuman => "locked_human",
            Self::AgentLed => "agent_led",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pair" | "free" | "default" => Some(Self::Pair),
            "locked_human" | "locked" | "human" => Some(Self::LockedHuman),
            "agent_led" | "agent" | "drive" => Some(Self::AgentLed),
            _ => None,
        }
    }
}

/// Per-pane agency state (lives on EnginePane).
#[derive(Clone, Debug)]
pub struct Agency {
    pub owner: Owner,
    pub drive_mode: DriveMode,
    pub last_human_input: Option<Instant>,
    pub last_agent_input: Option<Instant>,
    /// Optional exit code when process died but pane is kept as tombstone.
    pub exit_code: Option<i32>,
    pub exited: bool,
}

impl Default for Agency {
    fn default() -> Self {
        Self {
            owner: Owner::None,
            drive_mode: DriveMode::Pair,
            last_human_input: None,
            last_agent_input: None,
            exit_code: None,
            exited: false,
        }
    }
}

impl Agency {
    pub fn human_steal(&mut self) -> bool {
        let changed = !self.owner.is_human();
        self.owner = Owner::Human;
        self.last_human_input = Some(Instant::now());
        changed
    }

    pub fn agent_claim(&mut self, principal: &str) {
        let id = principal
            .strip_prefix("agent:")
            .unwrap_or(principal)
            .to_string();
        self.owner = Owner::Agent(id);
        self.last_agent_input = Some(Instant::now());
    }

    pub fn release(&mut self) {
        self.owner = Owner::None;
    }

    #[allow(dead_code)] // exit-transition helper exercised by agency tests; live exits go via engine
    pub fn mark_exited(&mut self, code: Option<i32>) {
        self.exited = true;
        self.exit_code = code;
        self.owner = Owner::None;
    }

    /// Human idle long enough that an agent may inject again?
    pub fn human_idle(&self) -> bool {
        match self.last_human_input {
            None => true,
            Some(t) => t.elapsed() >= HUMAN_IDLE_GRACE,
        }
    }

    /// May `principal` inject into this pane?
    /// `force` bypasses ownership (emergency / human-approved).
    pub fn may_inject(&self, principal: &str, force: bool) -> Result<(), String> {
        if force {
            return Ok(());
        }
        if self.exited {
            return Err("pane has exited (tombstone) — spawn a new pane".into());
        }
        if matches!(self.drive_mode, DriveMode::LockedHuman) {
            return Err(
                "pane locked to human (`seance ctl release PANE` or unlock drive mode)".into(),
            );
        }
        match &self.owner {
            Owner::None => Ok(()),
            Owner::Agent(id) => {
                let p = principal.strip_prefix("agent:").unwrap_or(principal);
                // Same agent, or cli orchestrator, may continue driving.
                if p == id || principal == "cli" || id == "cli" {
                    Ok(())
                } else {
                    Err(format!(
                        "pane owned by agent:{id} — wait, or `seance ctl seize` as human first"
                    ))
                }
            }
            Owner::Human => {
                if self.human_idle() {
                    Ok(())
                } else {
                    Err(format!(
                        "pane owned by human (idle grace {}ms) — wait, or human runs \
                         `seance ctl release PANE`",
                        HUMAN_IDLE_GRACE.as_millis()
                    ))
                }
            }
        }
    }

    pub fn to_wire(&self) -> AgencyWire {
        AgencyWire {
            owner: self.owner.as_str(),
            drive_mode: self.drive_mode.as_str().to_string(),
            human_idle: self.human_idle(),
            exited: self.exited,
            exit_code: self.exit_code,
        }
    }
}

/// JSON-friendly agency snapshot for ctl / GUI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgencyWire {
    pub owner: String,
    pub drive_mode: String,
    pub human_idle: bool,
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Durable agency snapshot for handoff / disk (no Instant timers).
/// After restore, idle-grace is treated as elapsed (human_idle = true).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AgencySnap {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub drive_mode: String,
    #[serde(default)]
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl Agency {
    pub fn to_snap(&self) -> AgencySnap {
        AgencySnap {
            owner: self.owner.as_str(),
            drive_mode: self.drive_mode.as_str().to_string(),
            exited: self.exited,
            exit_code: self.exit_code,
        }
    }

    pub fn from_snap(s: &AgencySnap) -> Self {
        let mut a = Self::default();
        a.owner = Owner::parse(if s.owner.is_empty() { "none" } else { &s.owner });
        a.drive_mode = DriveMode::parse(&s.drive_mode).unwrap_or(DriveMode::Pair);
        a.exited = s.exited;
        a.exit_code = s.exit_code;
        // Timers intentionally reset — post-restore, agents may inject after grace.
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_blocks_until_idle() {
        let mut a = Agency::default();
        a.human_steal();
        assert!(a.may_inject("agent:w", false).is_err());
        a.last_human_input = Some(Instant::now() - HUMAN_IDLE_GRACE - Duration::from_millis(10));
        assert!(a.may_inject("agent:w", false).is_ok());
    }

    #[test]
    fn force_bypasses() {
        let mut a = Agency::default();
        a.human_steal();
        assert!(a.may_inject("cli", true).is_ok());
    }

    #[test]
    fn locked_blocks_even_when_idle() {
        let mut a = Agency::default();
        a.drive_mode = DriveMode::LockedHuman;
        a.owner = Owner::None;
        assert!(a.may_inject("cli", false).is_err());
    }

    #[test]
    fn same_agent_may_continue() {
        let mut a = Agency::default();
        a.agent_claim("agent:w1");
        assert!(a.may_inject("agent:w1", false).is_ok());
        assert!(a.may_inject("agent:w2", false).is_err());
        assert!(a.may_inject("cli", false).is_ok()); // orchestrator
    }

    #[test]
    fn owner_parse_and_as_str_roundtrip() {
        assert_eq!(Owner::parse("none"), Owner::None);
        assert_eq!(Owner::parse("human"), Owner::Human);
        assert_eq!(Owner::parse("you"), Owner::Human);
        assert_eq!(Owner::parse("cli"), Owner::Agent("cli".into()));
        assert_eq!(Owner::parse("agent:w1"), Owner::Agent("w1".into()));
        assert_eq!(Owner::parse("w1"), Owner::Agent("w1".into()));
        assert_eq!(Owner::None.as_str(), "none");
        assert_eq!(Owner::Human.as_str(), "human");
        assert_eq!(Owner::Agent("cli".into()).as_str(), "cli");
        assert_eq!(Owner::Agent("w1".into()).as_str(), "agent:w1");
    }

    #[test]
    fn drive_mode_parse_aliases() {
        assert_eq!(DriveMode::parse("pair"), Some(DriveMode::Pair));
        assert_eq!(DriveMode::parse("free"), Some(DriveMode::Pair));
        assert_eq!(DriveMode::parse("locked"), Some(DriveMode::LockedHuman));
        assert_eq!(DriveMode::parse("agent"), Some(DriveMode::AgentLed));
        assert_eq!(DriveMode::parse("nope"), None);
        assert_eq!(DriveMode::Pair.as_str(), "pair");
        assert_eq!(DriveMode::LockedHuman.as_str(), "locked_human");
    }

    #[test]
    fn exited_pane_blocks_inject() {
        let mut a = Agency::default();
        a.mark_exited(Some(1));
        assert!(a.exited);
        assert!(a.owner.is_none());
        assert!(a.may_inject("cli", false).is_err());
        assert!(a.may_inject("cli", true).is_ok()); // force still ok
    }

    #[test]
    fn release_clears_owner() {
        let mut a = Agency::default();
        a.human_steal();
        assert!(a.owner.is_human());
        a.release();
        assert!(a.owner.is_none());
        assert!(a.may_inject("agent:w", false).is_ok());
    }

    #[test]
    fn snap_roundtrip_resets_timers() {
        let mut a = Agency::default();
        a.human_steal();
        a.drive_mode = DriveMode::AgentLed;
        a.mark_exited(Some(42));
        let snap = a.to_snap();
        let restored = Agency::from_snap(&snap);
        assert!(restored.exited);
        assert_eq!(restored.exit_code, Some(42));
        assert_eq!(restored.drive_mode, DriveMode::AgentLed);
        // Timers reset → human_idle true even if owner was human pre-exit
        // (mark_exited cleared owner to None).
        assert!(restored.human_idle());
        assert!(restored.owner.is_none());
    }
}
