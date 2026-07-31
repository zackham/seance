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
include/exclude + inline title fixes, a review pass that steps every prompt,
and the standing warning — everything on those screens ships. Publish POSTs
to the bridge, which exports a bundle and runs your configured publisher.

## the player

Chapters = your prompts (reconstructed from keystrokes; injected tasks come
through verbatim). Default mode fast-forwards agent output ~20× and pauses at
each prompt with the text on an overlay card; real-time and chapter-step modes
too. Timeline has a flame tick per prompt; seeks are keyframe-cheap. Not a
video — a session you can *read*.

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
