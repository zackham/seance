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

**Where the refusal shows up.** The GUI asks before it opens a window
(`gui_client::preflight`, a `ctl` hello + one `whoami`): a refusal goes to the
launch picker with the daemon's own text. If the daemon is upgraded under a
*live* thin client, the refusal arrives mid-session instead and the window
carries a sticky ⚠ bar until it reattaches. Both say the same thing the daemon
said, which names the fix. (Before 0.25.5 neither did — the client read the
refusal into `gui.stderr.log` and reconnected forever behind an empty rail.
A mac stuck on 0.24.0 that way looked exactly like a daemon with no circles.)

**Redeploying the client.** The client is built on the client machine, so
"upgrade the mac" is a pull and a bundle:

```bash
ssh <mac> 'cd ~/work/seance && git pull --ff-only && ./scripts/bundle-macos.sh'
```

Then quit and relaunch the app — an already-running GUI keeps the binary it
started with. Deploy the client *after* the daemon host, never before: a client
ahead of its daemon is the same refusal in the other direction.

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

A thin-client window follows the normal multi-window model: nothing is owned,
each window subscribes to the circles it wants (0.12). **The arrangement
itself — active/parked, pins, folds — is daemon-owned as of 0.23**, so a mac
window opens on the same rail as the desk, in the same order, with the same
circles pinned. Park something here and it parks there; the daemon pushes the
change to every attached window (`~/.local/share/seance/subscriptions.json`
on the daemon host, beside `layout.json`).

The same circle can be live here and at the desk simultaneously — sessions
never move or restart, only which windows render them.

`~/.config/seance/subscriptions.json` still exists on each client, demoted to
a seed cache: it's what `Attach` opens on before the daemon answers, so a
window doesn't attach to all 47 circles and unsubscribe 35 of them a beat
later. The daemon's copy wins any disagreement.
