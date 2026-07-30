# Remote / thin-client mode

Seance's GUI is a thin client: every PTY, file pane, scratchpad, host widget,
and the shared layout live in the **daemon**, and the GUI renders pushed
state. That makes "run the GUI here, daemon over there" a transport question,
not an architecture question — the transport is an ssh-forwarded unix socket.

## Using it

Launch `seance` with no local daemon running → the **picker** asks where your
daemon lives:

- **connect to remote host** — type an ssh destination (e.g. `desk`), enter.
  Seance establishes `ssh -N -L <local.sock>:<remote.sock> host`, auto-starts
  the remote daemon if needed (`seance _ensure-daemon` over ssh), and connects
  through the forward.
- **run locally** — classic single-machine mode (spawns a local daemon).

The choice persists at `~/.config/seance/launch.json` and prefills next time;
with a saved remote preference, later launches go straight to the tunnel with
no picker. CLI overrides (also rewrite the preference):

```bash
seance --remote desk    # thin client to desk
seance --local          # back to the local daemon
```

## Transport

- Forward: `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes
  -o ServerAliveInterval=15 -o StreamLocalBindUnlink=yes -L …` — spawned and
  supervised by the GUI (`src/tunnel.rs`). It dies with the GUI (Drop +
  `on_app_quit`; PDEATHSIG on Linux) and respawns with backoff if ssh drops.
  The GUI's own socket supervisor then re-attaches through the fresh forward.
- Passwordless ssh (keys/agent) is required — BatchMode never prompts. When
  auth fails, the error includes a copy-pasteable `autossh` stopgap you can
  run in a terminal (type the password once, leave it up) and pick
  remote again.
- `seance ctl` on any machine targets a specific daemon via
  `SEANCE_SOCKET=/path/to/forwarded.sock` (env override in
  `control::socket_path`). The daemon always binds its own local path
  (`bind_socket_path`) — a stray env var can't split-brain a bind.

## Version gate

The daemon strictly refuses `ctl`/`gui` hellos whose `build` doesn't match
its own `CARGO_PKG_VERSION` — cross-machine version skew fails loudly instead
of corrupting the protocol. Upgrade the older side (`cargo build --release &&
seance upgrade` on the daemon host; redeploy the client binary elsewhere).
`handoff`/`upgrade` roles are exempt (upgrades cross versions by design).

## Everything comes from the daemon

The fs bridge (`GuiRequest::Fs` / `daemon::fsbridge`) serves the GUI:

| Surface | Mechanism |
|---|---|
| file panes | `FsOp::Read/Write/Stat/List/Remove` (daemon paths) |
| scratchpads | pad path from `PaneInfo.scratchpad` + fs ops |
| layout (split/weights) | `FsOp::LayoutLoad/LayoutSave` → daemon state dir — all windows share tiling |
| host widgets (claude chips) | polled daemon-side, pushed as `HostWidgets`; select runs daemon-side (`FsOp::HostSelect`) |

Bridge ops execute on their own daemon thread (never the input path) and are
correlated by id; replies route to the waiting caller inside `gui_client`,
not the app event stream.

## macOS

The GUI builds and runs on macOS (gpui is zed's framework; the pinned revs
compile on darwin). Platform notes: `open` instead of `xdg-open`, `ps`
instead of `/proc`, `libc::getuid()` for the socket suffix. Build the same
way as Linux (`scripts/bootstrap-deps.sh` symlinks `deps/zed`, then
`cargo build --release`).
- known limitation: `seance ctl new -a <agent>` resolves the agent binary on the machine running ctl, not the daemon; use quicklaunch/GUI spawns (daemon-side) for cross-machine agent panes.

## Workspaces on a second machine

A thin-client window follows the normal multi-window model: workspaces are
exclusively owned by one window. A fresh mac window starts empty — pull the
circles you want (sidebar pull menu, or collect-all to grab everything);
back at the desk, collect-all there to bring them home. Sessions never move
or restart — only which window renders them.
