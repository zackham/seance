# Session export & share

## Product definition (v1)

A scrubable, shareable **decision-timeline** of a seance workspace: who did
what, what was asked, what was answered, what ran in shells. Offline-first;
optionally published like a vita HTML report.

**Not** 60fps TUI / SCG3 grid replay (privacy, size, fidelity, browser cost).
That is an explicit non-goal through 0.9.11.

## CLI

```bash
seance ctl export-session [opts]
  --workspace WS     limit scope (default: $SEANCE_WORKSPACE)
  --out PATH         HTML path (default: ~/.local/share/seance/exports/)
  --title T
  --redact           scrub /home/$USER paths for teaching shares
  --share            publish via ~/work/vita reports.publish
  --pin N            PIN-gate (with --share)
  --open             xdg-open after write
```

## Bundle contents

Embedded JSON (`<script type="application/json">`):

| section | source |
|---------|--------|
| roster | cold `state.json` panes |
| events | `events.jsonl` (capped + high-signal sample) |
| tasks | `state.tasks` inject bodies |
| pads | `scratch/<slug>.md` (truncated UTF-8 safe) |
| commands | `state.cmd_log` (persists on cmd begin/end + handoff) |

HTML player: virtualized timeline (≤400 DOM rows), scrub by index + filters
(agents only, high-signal kinds, detail search).

## Performance budget

| metric | target |
|--------|--------|
| typical 30–60m session HTML | ≤ 1.5–2.5 MB soft |
| hard warn | > 2.5 MB |
| generation | ≤ 2 s for 5k events |
| scrub UI | filter/re-render without full-page rebuild |

## Share path

1. Write HTML under seance exports  
2. `--share` copies to `~/work/vita/data/reports/YYYY-MM-DD-seance-*.html`  
3. Shells `uv run python -m reports.publish <slug> --format html`  
4. Verified domain: `https://vita-reports.ham.xyz/s/{token}`  

Prefer `--redact` when publishing. Do not invent a second share registry.

## Honesty badges

- Pads are **end-state** at export time (not reconstructed per scrub position).  
- Export is **not** full terminal replay.  
- Phone (`ctl phone`) opens a telegram topic + bind file; auto-inject of
  replies is a separate bridge (not part of export).

## Tests / smoke

- Unit: `export_html::tests::{redact_collapses_home, sample_prefers_finish}`  
- E2E: `./scripts/e2e-thorough.sh` asserts v1 markers + size budget  
