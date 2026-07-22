# Seance design pass — 2026-07-21

Grounded in `docs/THEME.md` (candlelit) + product goals from README:
visibility, shared space, amber = human attention, violet = agent.

**Reviewers:** Gemini 3.1 Pro (`google/gemini-3.1-pro-preview`) and
Claude Fable 5 / Opus proxy (OpenRouter) on `docs/screenshot.png` (live grim
capture was blocked by compositor; screenshot still representative of chrome).

---

## Shipped this pass

| change | why |
|--------|-----|
| Removed **run in pane** launch bar (claude/codex/grok) | Dead chrome; yolo profiles typed by hand |
| Removed **whisper** UI earlier | Steer via TUI / `ctl send` / notes |
| Pane header: short label + tooltip for long TUI title | Cut breadcrumb noise on auto-named panes |
| Pane header: **owner accent rail** only (no ⌨/⚡ text chip) | Rail is enough; ☠ exit label kept |
| Pane header: action cluster demoted (opacity, tighter glyphs, ✎ not "notes") | Icon soup → scannable strip; still one-click |
| Inactive pane opacity 0.88 → **0.94** | Ghosted neighbors read as "broken" / bleed-through |
| Title strip height 26 → **28** | Slight breathing room |
| Workspace badges: **working** + **needs** only | Drop sticky `done` (status vocab) |
| Host switcher: usage + **reset times** visible again | Capacity glance surface (was over-trimmed) |
| File-pane markdown: dark highlight + surface code + compact H1–H3 | Candlelit deep theme |
| Stage strip stays empty-when-quiet | Urgency-only; no always-on working chips |

---

## Leave alone (reviewers agreed)

- Candlelit palette + tile geometry
- Collapsed host account switcher density
- Stage strip as urgency surface (needs-human / blocked only)
- Flame caret / focus as human-attention signal
- Full-bleed selected sidebar row

---

## Decisions (bio-zack 2026-07-21)

| Q | Call | Implementation |
|---|------|----------------|
| 1 status vocab | recommend | Workspace badges: **working** (live) + **needs** only; drop sticky `done`. Pane `status-set` unchanged. Agent TUI verbs untouched. |
| 2 account meters | leave Claude alone | **Reverted 2026-07-21:** host switcher shows **full detail/detail2** again (5h/7d + reset times are load-bearing for capacity). Claude TUI still untouched. |
| 3 owner chip + rail | drop text | Accent rail only; keep `☠ exit` text when exited. |
| 4 secondary actions | no overflow menu | Icons stay one-click; tooltips only. |
| 5 markdown | deep theme | `TextViewStyle` dark highlight + surface code blocks + compact H1–H3 + warm body text. |
| 6 stage strip | make the call | **Empty when quiet** — urgency-only surface; no always-on working chips. |

---

## Not shipping without more evidence

- Full redesign of sidebar row anatomy (mostly consistent already)
- Killing terminal ghosting inside Claude (agent TUI, not seance compositing — verify with a pure-shell pane)
- Input restyle of Claude prompt rows (agent TUI)
- Destructive action separation (banish is sidebar context, not header)

---

## Raw reviewer notes

- Full Gemini/Fable outputs: `/tmp/seance-design/{gemini,fable,fable_proxy}.md`
- Screenshot used: `docs/screenshot.png` (still shows pre-removal launch bar — ship confirms removal)
