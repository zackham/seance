//! SSH tunnel supervisor for thin-client mode.
//!
//! When the GUI connects to a daemon on another machine, the transport is an
//! ssh-forwarded unix socket: `ssh -N -L <local.sock>:<remote.sock> host`.
//! This module owns that ssh child: discovery of the remote socket path,
//! ensuring the remote daemon is up, spawning the forward, health-probing it,
//! and re-spawning with backoff if it dies. The tunnel is tied to the GUI
//! process lifetime — dropping [`Tunnel`] (or process exit) kills the child;
//! there is deliberately no daemonized tunnel.
//!
//! Requires passwordless ssh (BatchMode). When auth or connectivity fails we
//! return an error that includes a copy-pasteable `autossh` stopgap command
//! the human can run in a terminal (typing a password there) and leave up.

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};

/// How long to wait for the forwarded socket to accept after spawning ssh.
const CONNECT_WAIT: Duration = Duration::from_secs(10);
/// Backoff between respawn attempts once established.
const RESPAWN_BACKOFF: Duration = Duration::from_secs(2);

/// A supervised `ssh -N -L` unix-socket forward to a remote seance daemon.
pub struct Tunnel {
    pub host: String,
    pub local_sock: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
}

impl Tunnel {
    /// Establish a tunnel to `host`: discover the remote socket path, ensure
    /// the remote daemon is running (auto-start over ssh), spawn the forward,
    /// verify the local socket accepts, then hand back a supervised tunnel.
    pub fn establish(host: &str) -> Result<Self> {
        // Fail fast + loud on unreachable / password-requiring hosts.
        preflight(host)?;

        let remote_sock = remote_socket_path(host)?;
        ensure_remote_daemon(host)?;

        let local_sock = local_socket_path(host);
        if let Some(dir) = local_sock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(&local_sock);

        let child = spawn_forward(host, &local_sock, &remote_sock)?;
        wait_for_socket(&local_sock, CONNECT_WAIT).map_err(|e| {
            anyhow!(
                "{e}\n\nstopgap — run this in a terminal (enter password once, leave it up):\n  {}",
                stopgap_command(host, &local_sock, &remote_sock)
            )
        })?;

        let child = Arc::new(Mutex::new(Some(child)));
        let stop = Arc::new(AtomicBool::new(false));
        supervise(
            host.to_string(),
            local_sock.clone(),
            remote_sock,
            Arc::clone(&child),
            Arc::clone(&stop),
        );

        Ok(Self {
            host: host.to_string(),
            local_sock,
            child,
            stop,
        })
    }

    /// Stop supervising and kill the ssh child.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        let _ = std::fs::remove_file(&self.local_sock);
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Local path for the forwarded socket. Keep it short: unix socket paths cap
/// around 104 bytes on macOS.
pub fn local_socket_path(host: &str) -> PathBuf {
    let safe: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    std::env::temp_dir().join(format!("seance-tunnel-{safe}.sock"))
}

fn ssh_base(host: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", host]);
    cmd
}

/// Cheap reachability + passwordless-auth probe.
fn preflight(host: &str) -> Result<()> {
    let out = ssh_base(host)
        .args(["--", "true"])
        .stdin(Stdio::null())
        .output()
        .context("spawn ssh for preflight")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "ssh to '{host}' failed (passwordless auth required): {}",
            err.trim()
        );
    }
    Ok(())
}

/// Ask the remote side where its daemon socket lives. Mirrors
/// `control::bind_socket_path` without requiring seance on the remote PATH.
fn remote_socket_path(host: &str) -> Result<String> {
    let script = r#"if [ -n "$XDG_RUNTIME_DIR" ]; then echo "$XDG_RUNTIME_DIR/seance.sock"; else echo "/tmp/seance-$(id -u).sock"; fi"#;
    let out = ssh_base(host)
        .args(["--", "sh", "-c", &format!("'{script}'")])
        .stdin(Stdio::null())
        .output()
        .context("spawn ssh for remote socket discovery")?;
    if !out.status.success() {
        bail!(
            "could not resolve remote socket path on '{host}': {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !path.starts_with('/') {
        bail!("unexpected remote socket path from '{host}': {path:?}");
    }
    Ok(path)
}

/// Start the remote daemon if it isn't running. Uses a login shell so `seance`
/// resolves from the remote user's normal PATH.
fn ensure_remote_daemon(host: &str) -> Result<()> {
    let script = "command -v seance >/dev/null 2>&1 && seance _ensure-daemon";
    let out = ssh_base(host)
        .args(["--", "sh", "-lc", &format!("'{script}'")])
        .stdin(Stdio::null())
        .output()
        .context("spawn ssh for remote daemon ensure")?;
    if !out.status.success() {
        bail!(
            "could not ensure seance daemon on '{host}' (is seance installed there?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn spawn_forward(host: &str, local: &std::path::Path, remote: &str) -> Result<Child> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "StreamLocalBindUnlink=yes",
        "-L",
        &format!("{}:{}", local.display(), remote),
        host,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    // Linux: tie the ssh child to our lifetime even on a hard crash (SIGKILL'd
    // GUI can't run Drop). macOS has no PDEATHSIG; there the on_app_quit hook
    // covers normal exits and a hard crash may briefly orphan the forward.
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("spawn ssh forward")?;
    // Drain ssh stderr to our log so forward failures are visible.
    if let Some(err) = child.stderr.take() {
        std::thread::Builder::new()
            .name("seance-tunnel-err".into())
            .spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    eprintln!("[seance tunnel] ssh: {line}");
                }
            })
            .ok();
    }
    Ok(child)
}

fn wait_for_socket(path: &std::path::Path, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "forwarded socket {} did not accept within {:?}",
                path.display(),
                budget
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The copy-pasteable fallback when BatchMode ssh can't establish the forward.
fn stopgap_command(host: &str, local: &std::path::Path, remote: &str) -> String {
    format!(
        "autossh -M 0 -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 \
         -o StreamLocalBindUnlink=yes -L {}:{} {}",
        local.display(),
        remote,
        host
    )
}

/// Background monitor: respawn the forward with backoff whenever ssh exits,
/// until `stop` is set. The GUI's own socket supervisor (gui_client) handles
/// reconnecting through the refreshed forward.
fn supervise(
    host: String,
    local: PathBuf,
    remote: String,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("seance-tunnel".into())
        .spawn(move || loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            // Reap: has the current child exited?
            let died = {
                let mut guard = match child.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                match guard.as_mut() {
                    Some(c) => match c.try_wait() {
                        Ok(Some(status)) => {
                            eprintln!("[seance tunnel] ssh to {host} exited: {status}");
                            *guard = None;
                            true
                        }
                        Ok(None) => false,
                        Err(_) => true,
                    },
                    None => true,
                }
            };
            if died && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(RESPAWN_BACKOFF);
                eprintln!("[seance tunnel] re-establishing forward to {host}…");
                match spawn_forward(&host, &local, &remote) {
                    Ok(c) => {
                        if let Ok(mut guard) = child.lock() {
                            *guard = Some(c);
                        }
                    }
                    Err(e) => eprintln!("[seance tunnel] respawn failed: {e}"),
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_socket_path_is_short_and_sanitized() {
        let p = local_socket_path("zack@desk.tail-net.ts");
        let s = p.to_string_lossy();
        assert!(s.ends_with("seance-tunnel-zack-desk-tail-net-ts.sock"));
        // macOS sun_path cap is ~104 bytes; leave generous headroom.
        assert!(s.len() < 100, "socket path too long: {s}");
    }

    #[test]
    fn stopgap_mentions_autossh_and_paths() {
        let cmd = stopgap_command("desk", std::path::Path::new("/tmp/l.sock"), "/run/r.sock");
        assert!(cmd.contains("autossh"));
        assert!(cmd.contains("/tmp/l.sock:/run/r.sock"));
        assert!(cmd.ends_with("desk"));
    }
}
