//! gpui-free session runtime — owns PTYs, terminal grids, and the control plane.
//!
//! The daemon process is the only owner of this module at runtime. The GUI and
//! `seance ctl` are clients. See `docs/DAEMON.md`.

pub mod engine;
pub mod protocol;
pub mod pty_session;
pub mod snapshot;

pub use engine::Engine;
pub use pty_session::SessionEvent;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Shared engine handle used by the daemon accept loop.
pub type SharedEngine = Arc<Mutex<Engine>>;

/// Path to the daemon pid file.
pub fn daemon_pid_path() -> PathBuf {
    state_data_dir().join("daemon.pid")
}

/// Data directory (state, pid, scratch).
pub fn state_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SEANCE_STATE_DIR") {
        if let Ok(expanded) = shellexpand::full(&dir) {
            return PathBuf::from(expanded.as_ref());
        }
    }
    PathBuf::from(shellexpand::tilde("~/.local/share/seance").into_owned())
}

/// Global "daemon is shutting down for upgrade" flag — I/O threads check this
/// so they stop without SIGHUP'ing children.
pub static UPGRADE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub fn set_upgrade_in_progress(v: bool) {
    UPGRADE_IN_PROGRESS.store(v, Ordering::SeqCst);
}

pub fn upgrade_in_progress() -> bool {
    UPGRADE_IN_PROGRESS.load(Ordering::SeqCst)
}
