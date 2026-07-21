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
    /// rather than a fresh command.
    #[serde(default)]
    pub resume_on_restore: bool,
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
fn state_dir() -> anyhow::Result<PathBuf> {
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
/// Lowercases, keeps ASCII alphanumerics, maps every other run of characters to
/// a single `-`, trims leading/trailing `-`, and falls back to `"session"` when
/// nothing usable remains.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            // Collapse any run of non-alnum (incl. existing dashes) into one dash.
            out.push('-');
            prev_dash = true;
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Slugify `name`, then disambiguate against already-taken slugs.
///
/// On collision, appends `-2`, `-3`, ... until the result is unused. `taken` is
/// the set of slugs already in play (compared case-sensitively against the
/// lowercase slug output).
pub fn unique_slug(name: &str, taken: &[&str]) -> String {
    let base = slugify(name);
    if !taken.contains(&base.as_str()) {
        return base;
    }

    let mut n = 2u64;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // `set_var`/`remove_var` are process-global; serialize env-mutating tests so
    // they don't race each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that points `SEANCE_STATE_DIR` at a unique temp dir for the
    /// duration of a test, restores the previous value, and cleans up.
    struct StateDirGuard {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
        dir: PathBuf,
    }

    impl StateDirGuard {
        fn new(tag: &str) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        };

        state.save().expect("save should succeed");
        let loaded = AppState::load();

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
