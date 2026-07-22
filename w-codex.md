# codex — seance roadmap answer

## (1) Fundamental pieces we are missing or should improve

The most important missing layer is a **causal activity model**, not more commands. Every meaningful action should produce one durable record containing `event_id`, `parent_id`, actor, pane, intent, capability decision, affected artifacts, and outcome. A command, its file writes, a failed test, and the agent’s follow-up should read as one chain. Today’s event bus can carry this, but the journal should become the source for timeline, recovery, review, and “why did this change?” views. Store it locally as append-only JSONL or SQLite; make retention and export explicit.

Second, make **attention state** first-class. Each pane needs a tiny, stable state machine: quiet, active, waiting on process, waiting on human, failed, done, plus an unread boundary. Agents should update intent and status, while observed facts—running process, exit code, recent output—remain system-derived. Never let an agent paint itself “done” over a failing command. Workspace chrome should answer, at a glance: what changed, what needs me, and what is merely busy.

Third, upgrade proposals from text prompts into **typed transactions**. A proposal should declare the operation, targets, preview, reversible/irreversible flag, expiry, and requested grant. Approval can then be “once,” “for this pane,” or “for this operation for 20 minutes.” Capability decisions must appear beside the attempted action, not in a remote settings page.

Finally, treat daemon restart and GUI detachment as normal. Persist stable pane identities, ownership transitions, unread cursors, proposals, and tombstones. On reattach, restore the visible story—not just processes and geometry.

## (2) Additional pane types to prioritize

1. **Timeline pane.** This should be next. It is a filterable, live projection of the causal journal: group by intent, expand raw events, jump to the originating pane, and scrub back to the exact terminal/file state. It is the workspace’s memory, not an audit-log dump.

2. **Review pane.** Collect pending file diffs, commands, and proposed mutations into one approval queue. Support per-hunk accept/reject, “open at source,” and a final receipt showing what actually happened. This is more valuable than building a full editor.

3. **Run pane.** Give long-running work a structured surface: goal, stages, subprocess tree, checks, artifacts, elapsed time, and current blocker. Terminal output remains available, but users should not infer progress from scrolling ANSI text.

4. **Agency pane.** A compact roster of humans/agents/processes showing current intent, ownership, granted capabilities, last meaningful action, and waiting state. It should also offer revoke, release, steer, and focus actions.

Do not prioritize browser, debugger, database, or chat panes yet. They broaden surface area without strengthening visible collaboration.

## (3) Big ideas we are missing entirely

The big idea is **workspace time travel**. A user should be able to select any event and see a read-only reconstruction of pane layout, terminal snapshot, file versions, ownership, and active proposals at that moment. “Replay the last five minutes” turns concurrency from confusing spectacle into something inspectable. Branching from a historical point can come later; faithful replay is enough initially.

Next is **proof-carrying work**. Completion should be an inspectable receipt: intent, files changed, commands run, checks passed/failed, capabilities exercised, and unresolved assumptions. `status-set done` becomes a claim linked to evidence, not a decorative badge. Receipts can be generated entirely from local events and exported as Markdown.

Also add **attention budgets**. Let the human declare “interrupt me only for destructive actions or failed checks.” Seance can batch lesser questions into a visible inbox and surface them at command boundaries. This preserves co-presence without turning the product into a notification machine.

## (4) Small QoL polish items

- Add “follow activity” and “pin focus”; automatic focus movement must always be opt-in and visibly indicated.
- Put unread ticks on pane borders and provide one shortcut to visit meaningful changes in chronological order.
- Freeze terminal scrollback when the user scrolls up, with a clear “live +123 lines” badge and one-key return.
- Make ownership transitions animate briefly and leave a timestamped breadcrumb in pane chrome.
- Add copy-as-Markdown for terminal selections, diffs, event groups, and completion receipts.
- Preserve per-pane zoom, scroll position, history cursor, and shelved state across restarts.
- Show command duration and exit status in a quiet gutter; collapse successful noise, never failed output.
- Provide named workspace layouts (“observe,” “review,” “single focus”) as local view presets, not project configuration.
- Make every empty state teach one useful action and its shortcut; remove modal setup whenever a safe default exists.
