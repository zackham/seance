# codex — council recommendations

Seance’s moat is not “many terminals.” It is **legible agency**: at a glance, the human should know who is doing what, why, with what authority, what changed, and where attention is required. I would optimize every addition against that test.

## 1. Fundamental pieces missing or worth improving

**Make work—not panes—the primary object.** A pane is an execution surface; a task is the durable contract. Add a small task model: brief, acceptance criteria, owner(s), parent task, state, artifacts, and completion evidence. A pane can perform several tasks, and a task can fan out across panes. `status-set` should attach to a task and expire: distinguish **agent-reported** state from **observed** state such as “last output 8s ago,” “command running 3m,” or “human owns keyboard.” Stale green “working” badges are worse than no badge.

**Build a durable attention ledger.** Asks, proposals, permission failures, agent exits, conflicting edits, and completed reviews should enter one ordered inbox with acknowledge, resolve, and snooze. Toasts can announce; they must not be the system of record. The central question is “what needs me now?” without scanning six TUIs.

**Promote events into causal spans.** The event bus is strong plumbing, but a flat timeline will become noise. Introduce task/turn/command spans: dispatched → accepted → tools/commands/files → blocked or completed. Use shell markers, agent hooks/adapters, and explicit artifact declarations. Preserve raw events underneath, but show the causal story by default. Clicking “done” should reveal exactly what makes it done.

**Make authority inspectable at the point of action.** Put effective capabilities, policy, drive mode, current owner, and the reason for denial in pane chrome and proposal UI. Offer a one-action temporary grant with scope and expiry. Since this is local-first/single-user, avoid enterprise RBAC machinery; concentrate on preventing accidental invisible action and making handoff unmistakable.

**Harden recovery.** Persist enough scrollback, task state, attention items, and span metadata to reconstruct the room after a daemon or machine crash. Add retention controls and a portable “workspace replay bundle” containing events, briefs, artifacts, and environment metadata—never secrets by default.

## 2. Pane types to prioritize

1. **Stage pane (roster + task board + attention queue).** This should be first. Rows show task, agent/pane, reported and observed state, owner, elapsed time, newest artifact, and outstanding ask. Expand a row for the brief and evidence; jump to the live terminal. Do not split roster and tasks into two weak dashboards.
2. **Trace pane.** A multi-lane causal timeline grouped by task and pane, with filters for human-visible changes, commands, files, ownership, and permissions. Clicking an event jumps to the terminal scrollback or file snapshot at that moment.
3. **Review pane.** A change-set/evidence surface: file diffs, test results, proposed commands, generated documents, and approval decisions in one review packet. It should consume artifacts, not become a general editor.
4. **Artifact panes.** PDF, image, structured JSON/table, and logs with follow/pause/search. These make agent outputs visible without pulling Seance toward an IDE or browser shell.

I would defer embedded chat, source-tree explorers, debuggers, and full editors. Existing terminals plus external tools already do those jobs.

## 3. Big ideas missing entirely

**Fork → compare → synthesize** should be a native ritual. One task can spawn isolated worktree-backed rooms, give every worker the same visible contract, collect comparable evidence cards, then let the human select or commission a synthesis. Provenance survives the merge. This is a genuinely agent-native interaction, not another IDE feature.

Add **semantic zoom and replay**. Zoomed out, the room is tasks and attention; mid-level is causal spans; zoomed in is raw terminal/file state. After stepping away, “replay the last 20 minutes” should animate only meaningful transitions. Visibility over time matters as much as visibility across panes.

Finally, let agents **point**, not merely print: anchored callouts on a file range, terminal span, artifact region, or another pane, with “look here because…” and a resolvable state. Shared attention is the real collaboration primitive.

## 4. Small QoL polish

- Manual resize, focus-zoom, saved layouts, and workspace-specific layout restore.
- Fuzzy jump palette for pane/task/artifact; consistent keyboard navigation and numbered quick-jump overlays.
- Unread/change markers with “mark seen,” plus quiet-hours and notification batching.
- Visible staged-input preview showing author, destination, and whether Enter will submit; undo before submission.
- Scratchpad sections, append/patch APIs, backlinks to tasks/events, and collision-safe multi-writer updates.
- OSC 8 links and OSC 133 command boundaries; copy link to any task, span, snapshot, or pane.
- Profile readiness checks in the summon UI, with exact remediation when a binary, model, trust prompt, or capability is wrong.
- Accessibility pass: non-color status cues, configurable motion/tint, larger hit targets, and a high-contrast palette.
