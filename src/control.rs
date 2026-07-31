//! The seance **control plane** wire types: the JSON-lines protocol that lets
//! one session (a "master" agent — Claude, codex, grok, or any CLI running in a
//! pane) drive the *other* sessions. Spawn them, inject prompts, read their
//! rendered screens — all while the human watches the terminals get driven live.
//!
//! # Shape
//!
//! - **JSON-lines protocol.** One request JSON per `\n`-terminated line in, one
//!   [`ControlResponse`] JSON line out. The connection may stay open and carry
//!   many request/response pairs (a master session keeps a socket and pipelines
//!   `send`/`read` calls without reconnecting).
//! - [`socket_path`] resolves the bound Unix socket
//!   (`$XDG_RUNTIME_DIR/seance.sock`, falling back to `/tmp/seance-$UID.sock`).
//!
//! This module is deliberately **gpui-free** so it compiles independently: it
//! defines only the protocol types ([`ControlRequest`] / [`ControlResponse`])
//! and the socket-path helper. The actual socket server + request dispatch now
//! lives in the **daemon** (`src/daemon` + `Engine::handle_control`); the older
//! in-GUI server was retired in the daemon split.

use std::path::PathBuf;

pub use seance_core::control::{ControlRequest, ControlResponse};

/// Resolve the control socket path.
///
/// `$SEANCE_SOCKET`, when set and non-empty, wins outright — this is how a
/// thin client (or a pane spawned by the daemon, which exports it) targets a
/// specific socket, e.g. an ssh-forwarded unix socket to a remote daemon.
/// Otherwise prefers `$XDG_RUNTIME_DIR/seance.sock` (the user's runtime dir,
/// cleaned on logout), falling back to `/tmp/seance-$UID.sock` when
/// `XDG_RUNTIME_DIR` is unset — the `$UID` suffix keeps it per-user on a
/// shared `/tmp`.
pub fn socket_path() -> PathBuf {
    if let Some(sock) = std::env::var_os("SEANCE_SOCKET") {
        if !sock.is_empty() {
            return PathBuf::from(sock);
        }
    }
    bind_socket_path()
}

/// The socket path a daemon on *this* machine binds (and exports to panes).
///
/// Deliberately ignores `$SEANCE_SOCKET`: that variable redirects *clients*
/// (possibly at a forwarded socket to another machine); a daemon inheriting it
/// must still bind its own local path or a stray env turns into a split brain.
pub fn bind_socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("seance.sock");
        }
    }
    let uid = current_uid();
    PathBuf::from(format!("/tmp/seance-{uid}.sock"))
}

/// Real UID for the `/tmp` fallback socket name — portable (macOS has no
/// procfs), and a per-user token is all we need (the socket file's own
/// permissions are the security boundary).
fn current_uid() -> u32 {
    // SAFETY: getuid is always safe to call.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_has_seance_sock_shape() {
        let p = socket_path().to_string_lossy().into_owned();
        let name = p.rsplit('/').next().unwrap_or_default();
        assert!(
            name.starts_with("seance") && name.ends_with(".sock"),
            "unexpected socket path: {p}"
        );
    }
}
