# session replay — record, trim, share

Seance records every pane, always — a 48h rolling DVR (`~/.local/share/seance/
replay/<pane>/<hour>.srr[.gz]`, ~4 GiB cap) written by a daemon-side recorder
thread. The recording IS the wire protocol persisted: SCG3 grid frames (FULL
keyframes ~10s + before every human prompt, damage between) plus an attributed
event track (who typed/injected what, status, titles). Idle panes cost zero;
a busy hour is ~5–15 MB before gzip. Format: `seance-core::replay` (SRR1).

## share a session

- **native GUI**: right-click a workspace → *share replay…* (opens the web editor)
- **web GUI**: same item in the workspace context menu
- **CLI**: `seance replay edit --workspace W` · `seance replay list` ·
  `seance replay export --workspace W --from -2h --to now [-o DIR] [--publish]`

The **editor** (web, one implementation for every surface) previews the exact
bytes that would ship: trim handles + set-start/end-to-playhead, per-chapter
include/exclude + inline title fixes, a review pass (seek + pause on the first
included chapter), and the standing warning — everything on those screens ships. Publish POSTs
to the bridge, which exports a bundle and runs your configured publisher.

## the player

Panes render at their RECORDED resolution, scaled to fit (never reflowed —
the terminal you share is the terminal you had). Chapters = the prompts a pane
received — yours, reconstructed from keystrokes; anything injected (`ctl send`,
an accepted propose) verbatim. Chapters carry no attribution, so an agent's
`ctl send` reads like a human prompt. Controls: prev/next
prompt fly-to (animated ~0.6s scrub, lands paused with the prompt typed),
play/pause, speeds 1×/1.5×/2×/5× shown only while playing. The timeline is
ACTIVITY time — idle gaps >3s collapse to a 1.5s beat (dim hash marks; hover
for "skipped 14m idle") while typing plays at its real cadence. Paused
positions land in the URL (`#t=…`) so a link shares the exact moment; ↺
resets to a clean link. Not a video — a session you can *read*.

## publisher seam (arms-length by design)

`~/.config/seance/publish.json`:
```json
{
  "assets_url": "https://your.host/rp-player/v1",      // optional: thin bundles
  "publish_command": "your-script \"$1\""              // $1 = bundle dir; print URL
}
```
No config → `seance replay export` still writes fully self-contained static
bundles (open index.html anywhere). Vita's plugin lives in vita
(`scripts/seance_share/publish_replay.py` → vita-reports.ham.xyz/rp/<token>);
seance never learns what it is.
