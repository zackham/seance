//! Persistent application state for seance.
//!
//! Pure serde module — no gpui imports. Responsible for serializing the set of
//! Claude Code sessions (and a little window/layout chrome) to disk and reading
//! it back, so the app can restore its shape on the next launch.
//!
//! # State location
//!
//! State lives at `~/.local/share/seance/state.json` by default (XDG data dir).
//! The directory is created as needed on save.
//!
//! # `SEANCE_STATE_DIR` override
//!
//! If the `SEANCE_STATE_DIR` environment variable is set, it overrides the
//! default location: state is read from / written to `$SEANCE_STATE_DIR/state.json`.
//! Tilde (`~`) and env vars in the value are expanded. This is primarily used by
//! the test suite so tests never touch the real state file, but the app itself
//! may honor it too (e.g. to run isolated profiles).

use std::path::PathBuf;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// A single pane as persisted to disk. Terminals are the first pane kind;
/// future kinds (markdown viewer, graph, ...) slot in via `kind`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistedPane {
    /// Pane kind discriminator. `"terminal"` today.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Human-facing display name shown in the sidebar.
    pub name: String,
    /// Filesystem-safe, unique identifier (see [`slugify`] / [`unique_slug`]).
    pub slug: String,
    /// Working directory the session's PTY runs in.
    pub cwd: String,
    /// Command to launch (default shell; agents via explicit command).
    pub command: String,
    /// Whether the session lives in the autotiling region (`true`) or is
    /// shelved in the sidebar (`false`).
    pub tiled: bool,
    /// If true, restore relaunches the session with `claude --continue` in `cwd`
    /// rather than a fresh command. Legacy: superseded by [`Self::claude_session`]
    /// for panes that own a minted session id.
    #[serde(default)]
    pub resume_on_restore: bool,
    /// Claude conversation this pane owns (minted at spawn via `--session-id`).
    /// Restore relaunches with `--resume <id>`, so a daemon crash costs nothing.
    /// `None` for shells, file panes, and panes persisted before 0.14.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<String>,
    /// Named workspace this session belongs to (sidebar grouping).
    #[serde(default = "default_workspace")]
    pub workspace: String,
    /// Last known status badge (0.9.5+ — survive cold restart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_note: Option<String>,
    /// Scratchpad revision counter.
    #[serde(default)]
    pub pad_rev: u64,
    /// Agency owner string (`none`/`human`/`agent:…`/`cli`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_mode: Option<String>,
    #[serde(default)]
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Inject baseline (0.9.6 — cold-restart evidence for wait).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_pad_rev: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_pad_bytes: Option<u64>,
}

fn default_kind() -> String {
    "terminal".to_string()
}

fn default_workspace() -> String {
    "main".to_string()
}

/// Top-level persisted application state.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AppState {
    /// All known panes, tiled and shelved. Serialized as `sessions` for
    /// back-compat with v0.1 state files.
    #[serde(rename = "sessions")]
    pub panes: Vec<PersistedPane>,
    /// Width of the left sidebar in pixels, if the user has resized it.
    pub sidebar_width: Option<f32>,
    /// Width of the drawer in pixels, if applicable.
    pub drawer_width: Option<f32>,
    /// Whether the drawer is currently open.
    pub drawer_open: bool,
    /// Slug of the currently-focused session, if any.
    pub active_slug: Option<String>,
    /// Currently selected workspace (the tiling region shows only its panes).
    #[serde(default)]
    pub selected_workspace: Option<String>,
    /// Workspaces that exist independently of panes (created empty).
    #[serde(default)]
    pub extra_workspaces: Vec<String>,
    /// Sidebar display order of workspaces (drag-to-reorder).
    #[serde(default)]
    pub workspace_order: Vec<String>,
    /// Last known window size `(width, height)` in pixels.
    pub window_size: Option<(f32, f32)>,
    /// Dispatch tasks (inject inbox + completion envelope).
    #[serde(default)]
    pub tasks: Vec<crate::runtime::protocol::TaskRecord>,
    #[serde(default)]
    pub task_counter: u64,
    /// pane slug → active task id
    #[serde(default)]
    pub active_tasks: Vec<(String, String)>,
    /// 2-pane horizontal split ratio (0.2–0.8). Default 0.5.
    #[serde(default = "default_split_ratio")]
    pub split_ratio: f32,
    /// Per-pane flex weights for multi-pane tile resize (slug → weight ≥ 0.15).
    #[serde(default)]
    pub pane_weights: Vec<(String, f32)>,
    /// Shell command log (cold restore).
    #[serde(default)]
    pub cmd_log: crate::cmdlog::CommandLog,
    /// workspace → last real pane output (unix ms). Daemon-owned activity
    /// clock; persisted so a cold daemon restart keeps the sidebar's
    /// "time since update" instead of blanking every circle.
    #[serde(default)]
    pub workspace_output: Vec<(String, u64)>,
    /// workspace → last human input (unix ms) — sidebar recency sort key.
    #[serde(default)]
    pub workspace_touch_ms: Vec<(String, u64)>,
    /// workspace → scraped PR links (0.13 — survive upgrade).
    #[serde(default)]
    pub pr_links: Vec<(String, Vec<crate::runtime::protocol::PrLink>)>,
    /// workspace → PR urls the human cleared (0.14.1). Persisted because the
    /// scraper would otherwise re-add a cleared url from the next repaint.
    #[serde(default)]
    pub pr_dismissed: Vec<(String, Vec<String>)>,
}

fn default_split_ratio() -> f32 {
    0.5
}

impl AppState {
    /// Load state from disk.
    ///
    /// Reads `~/.local/share/seance/state.json` (or `$SEANCE_STATE_DIR/state.json`).
    /// If the file is missing or corrupt, returns [`AppState::default`] and prints
    /// a warning to stderr. Never panics.
    pub fn load() -> Self {
        let path = match state_file_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("seance: could not resolve state path: {e:#}; using defaults");
                return Self::default();
            }
        };

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First run (or fresh profile) — no warning needed, just defaults.
                return Self::default();
            }
            Err(e) => {
                eprintln!(
                    "seance: could not read state file {}: {e}; using defaults",
                    path.display()
                );
                return Self::default();
            }
        };

        match serde_json::from_slice::<AppState>(&bytes) {
            Ok(state) => state,
            Err(e) => {
                eprintln!(
                    "seance: state file {} is corrupt: {e}; using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Persist state to disk atomically.
    ///
    /// Creates the parent directory as needed, writes to a temp file in the same
    /// directory, then renames it over the target so a reader never observes a
    /// partially-written file. The temp file is cleaned up on write failure.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = state_file_path()?;
        let dir = path
            .parent()
            .context("state file path has no parent directory")?;

        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating state dir {}", dir.display()))?;

        let json = serde_json::to_vec_pretty(self).context("serializing app state")?;

        // Temp file in the SAME directory so the rename is atomic (same filesystem).
        // PID + a nanosecond timestamp keeps concurrent saves from colliding.
        let tmp = dir.join(format!(
            ".state.json.tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        if let Err(e) = std::fs::write(&tmp, &json) {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("writing temp state file {}", tmp.display()));
        }

        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()));
        }

        Ok(())
    }
}

/// Resolve the directory that holds `state.json`.
///
/// Honors `SEANCE_STATE_DIR` (with `~`/env expansion); otherwise falls back to
/// `~/.local/share/seance`.
pub(crate) fn state_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if !dir.is_empty() {
            let expanded = shellexpand::full(&dir)
                .with_context(|| format!("expanding SEANCE_STATE_DIR={dir}"))?;
            return Ok(PathBuf::from(expanded.into_owned()));
        }
    }

    let expanded = shellexpand::tilde("~/.local/share/seance");
    Ok(PathBuf::from(expanded.into_owned()))
}

/// Full path to the state file.
fn state_file_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("state.json"))
}

/// Turn an arbitrary name into a filesystem-safe slug.
///
pub use seance_core::util::{slugify, unique_slug};

/// Process-global lock for `SEANCE_STATE_DIR` mutations (tests only).
/// Shared by `state` and `engine` tests so parallel env writes cannot race.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    /// RAII guard that points `SEANCE_STATE_DIR` at a unique temp dir for the
    /// duration of a test, restores the previous value, and cleans up.
    struct StateDirGuard {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl StateDirGuard {
        fn new(tag: &str) -> Self {
            let lock = test_env_lock();
            let prev = std::env::var("SEANCE_STATE_DIR").ok();

            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "seance-test-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::env::set_var("SEANCE_STATE_DIR", &dir);

            Self {
                _lock: lock,
                prev,
                dir,
            }
        }
    }

    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("SEANCE_STATE_DIR", v),
                None => std::env::remove_var("SEANCE_STATE_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("My Project"), "my-project");
    }

    #[test]
    fn slugify_lowercases_and_strips_punctuation() {
        assert_eq!(slugify("Foo_Bar.Baz"), "foo-bar-baz");
        assert_eq!(slugify("rwgps/prod"), "rwgps-prod");
        assert_eq!(slugify("~/work/vita"), "work-vita");
    }

    #[test]
    fn slugify_collapses_repeats() {
        assert_eq!(slugify("a   b"), "a-b");
        assert_eq!(slugify("a---b"), "a-b");
        assert_eq!(slugify("a - - b"), "a-b");
        assert_eq!(slugify("!!!weird!!!name!!!"), "weird-name");
    }

    #[test]
    fn slugify_trims_edges() {
        assert_eq!(slugify("-leading"), "leading");
        assert_eq!(slugify("trailing-"), "trailing");
        assert_eq!(slugify("  spaced  "), "spaced");
    }

    #[test]
    fn slugify_empty_fallback() {
        assert_eq!(slugify(""), "session");
        assert_eq!(slugify("   "), "session");
        assert_eq!(slugify("!@#$%^&*()"), "session");
        assert_eq!(slugify("---"), "session");
    }

    #[test]
    fn slugify_keeps_digits() {
        assert_eq!(slugify("Session 2"), "session-2");
        assert_eq!(slugify("v1.2.3"), "v1-2-3");
    }

    #[test]
    fn unique_slug_no_collision() {
        assert_eq!(unique_slug("Hello World", &[]), "hello-world");
        assert_eq!(unique_slug("Hello World", &["other"]), "hello-world");
    }

    #[test]
    fn unique_slug_suffixes_on_collision() {
        let taken = ["hello-world"];
        assert_eq!(unique_slug("Hello World", &taken), "hello-world-2");

        let taken = ["hello-world", "hello-world-2"];
        assert_eq!(unique_slug("Hello World", &taken), "hello-world-3");

        let taken = ["hello-world", "hello-world-2", "hello-world-3"];
        assert_eq!(unique_slug("Hello World", &taken), "hello-world-4");
    }

    #[test]
    fn unique_slug_applies_fallback_then_suffixes() {
        assert_eq!(unique_slug("!!!", &[]), "session");
        assert_eq!(unique_slug("!!!", &["session"]), "session-2");
    }

    #[test]
    fn round_trip_serde_via_disk() {
        let _guard = StateDirGuard::new("roundtrip");

        let state = AppState {
            panes: vec![
                PersistedPane {
                    kind: "terminal".to_string(),
                    workspace: "main".to_string(),
                    name: "Vita".to_string(),
                    slug: "vita".to_string(),
                    cwd: "/home/agent/project".to_string(),
                    command: "claude".to_string(),
                    tiled: true,
                    resume_on_restore: true,
                    claude_session: None,
                    status: Some("working".into()),
                    status_note: None,
                    pad_rev: 2,
                    owner: Some("agent:vita".into()),
                    drive_mode: Some("pair".into()),
                    exited: false,
                    exit_code: None,
                    inject_pad_rev: None,
                    inject_pad_bytes: None,
                },
                PersistedPane {
                    kind: "terminal".to_string(),
                    workspace: "main".to_string(),
                    name: "Scratch".to_string(),
                    slug: "scratch".to_string(),
                    cwd: "/tmp".to_string(),
                    command: "claude --dangerously-skip-permissions".to_string(),
                    tiled: false,
                    resume_on_restore: false,
                    claude_session: None,
                    status: None,
                    status_note: None,
                    pad_rev: 0,
                    owner: None,
                    drive_mode: None,
                    exited: false,
                    exit_code: None,
                    inject_pad_rev: None,
                    inject_pad_bytes: None,
                },
            ],
            sidebar_width: Some(280.0),
            drawer_width: None,
            drawer_open: true,
            active_slug: Some("vita".to_string()),
            selected_workspace: Some("main".to_string()),
            extra_workspaces: vec![],
            workspace_order: vec![],
            window_size: Some((1280.0, 800.0)),
            tasks: vec![],
            task_counter: 0,
            active_tasks: vec![],
            split_ratio: 0.5,
            pane_weights: vec![],
            cmd_log: crate::cmdlog::CommandLog::new(),
            workspace_output: vec![("main".to_string(), 1_700_000_000_000)],
            workspace_touch_ms: vec![("main".to_string(), 1_700_000_000_500)],
            pr_links: vec![(
                "main".to_string(),
                vec![crate::runtime::protocol::PrLink {
                    url: "https://github.com/o/r/pull/5".into(),
                    status: None,
                    seen_ms: 1_700_000_000_900,
                }],
            )],
            pr_dismissed: vec![(
                "main".to_string(),
                vec!["https://github.com/o/r/pull/4".into()],
            )],
        };

        state.save().expect("save should succeed");
        let loaded = AppState::load();
        assert_eq!(
            loaded.workspace_output,
            vec![("main".to_string(), 1_700_000_000_000)]
        );
        assert_eq!(
            loaded.workspace_touch_ms,
            vec![("main".to_string(), 1_700_000_000_500)]
        );
        assert_eq!(loaded.pr_links.len(), 1);
        assert_eq!(loaded.pr_links[0].0, "main");
        assert_eq!(loaded.pr_links[0].1[0].url, "https://github.com/o/r/pull/5");
        assert_eq!(
            loaded.pr_dismissed,
            vec![(
                "main".to_string(),
                vec!["https://github.com/o/r/pull/4".to_string()]
            )]
        );

        assert_eq!(loaded.panes.len(), 2);
        assert_eq!(loaded.panes[0].name, "Vita");
        assert_eq!(loaded.panes[0].slug, "vita");
        assert_eq!(loaded.panes[0].cwd, "/home/agent/project");
        assert!(loaded.panes[0].tiled);
        assert!(loaded.panes[0].resume_on_restore);
        assert_eq!(loaded.panes[1].name, "Scratch");
        assert!(!loaded.panes[1].tiled);
        assert!(!loaded.panes[1].resume_on_restore);
        assert_eq!(loaded.sidebar_width, Some(280.0));
        assert_eq!(loaded.drawer_width, None);
        assert!(loaded.drawer_open);
        assert_eq!(loaded.active_slug.as_deref(), Some("vita"));
        assert_eq!(loaded.window_size, Some((1280.0, 800.0)));
    }

    #[test]
    fn load_missing_file_returns_default() {
        let _guard = StateDirGuard::new("missing");
        // Nothing written; load must not panic and must return defaults.
        let loaded = AppState::load();
        assert!(loaded.panes.is_empty());
        assert!(!loaded.drawer_open);
        assert_eq!(loaded.active_slug, None);
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let guard = StateDirGuard::new("corrupt");
        std::fs::create_dir_all(&guard.dir).unwrap();
        std::fs::write(guard.dir.join("state.json"), b"{ not valid json ]").unwrap();

        let loaded = AppState::load();
        assert!(loaded.panes.is_empty());
        assert_eq!(loaded.active_slug, None);
    }

    #[test]
    fn resume_on_restore_defaults_when_absent() {
        // Older state files won't have the field; serde(default) must fill false.
        let guard = StateDirGuard::new("default-field");
        std::fs::create_dir_all(&guard.dir).unwrap();
        let json = r#"{
            "sessions": [
                {"name": "Old", "slug": "old", "cwd": "/tmp", "command": "claude", "tiled": true}
            ],
            "drawer_open": false
        }"#;
        std::fs::write(guard.dir.join("state.json"), json).unwrap();

        let loaded = AppState::load();
        assert_eq!(loaded.panes.len(), 1);
        assert!(!loaded.panes[0].resume_on_restore);
    }
}
