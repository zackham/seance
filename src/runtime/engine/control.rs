//! Control-plane request handling (`handle_control` + pad/task bookkeeping).

use std::collections::HashMap;

use serde_json::json;

use super::helpers::{
    assert_self_or_cross, atomic_append_pad, atomic_write_pad, base64_decode, chrono_lite_stamp,
    now_ms, task_json, validate_status, write_task_sidecar,
};
use super::{Engine, EnginePane, PendingAsk, SpawnSpec};
use crate::control::{ControlRequest, ControlResponse};
use crate::events;
use crate::runtime::protocol::*;
use crate::runtime::snapshot::GhostSnap;

impl Engine {
    pub fn handle_control(&mut self, request: ControlRequest) -> ControlResponse {
        use ControlRequest::*;
        // Circles are addressable by slug or by label; every handler below
        // wants the slug. One rewrite at the door beats twenty at the tills.
        let request = self.normalize_workspace_keys(request);
        // A label two circles share resolves to neither. Say so, rather than
        // letting it fall through as a name that matches nothing.
        if let Some(key) = request.workspace_hint() {
            if self.workspace_key_is_ambiguous(key) {
                return ControlResponse::err(format!(
                    "circle '{key}' is ambiguous — more than one circle uses that name; \
                     address it by slug (`seance ctl list --all` shows them)"
                ));
            }
        }
        let ok = |data: serde_json::Value| ControlResponse::ok(data);
        let err = |m: String| ControlResponse::err(m);
        let find = |eng: &Engine, key: &str, scope: &Option<String>| -> Result<usize, String> {
            // `@circle` → the circle's lead pane; `pane:slug` forces a pane.
            let in_scope_ws = |ws: &str| scope.as_ref().is_none_or(|s| s == ws);
            if let Some(name) = key.strip_prefix('@') {
                let (circle, _) = eng
                    .resolve_circle(name)
                    .ok_or_else(|| format!("no circle '{name}'"))?;
                let lead = eng
                    .effective_lead(&circle)
                    .ok_or_else(|| format!("circle '{name}' has no panes"))?;
                if !in_scope_ws(&circle) {
                    return Err(format!(
                        "circle '{name}' is outside your workspace (use --all to cross)"
                    ));
                }
                return eng
                    .panes
                    .iter()
                    .position(|p| p.slug == lead)
                    .ok_or_else(|| format!("no pane '{lead}'"));
            }
            let (key, forced_pane) = match key.strip_prefix("pane:") {
                Some(k) => (k, true),
                None => (key, false),
            };
            // Unscoped, a bare name that is a pane here AND another circle's
            // slug/label is refused: that collision (circle `claude-27` vs
            // cadence-slack's pane `claude-27`) misrouted a reply 2026-09-29.
            if scope.is_none() && !forced_pane {
                if let (Some(p), Some((circle, _))) = (
                    eng.panes.iter().find(|p| p.slug == key),
                    eng.resolve_circle(key),
                ) {
                    let lead = eng.effective_lead(&circle);
                    if p.workspace != circle && lead.as_deref() != Some(key) {
                        return Err(format!(
                            "'{key}' is ambiguous: pane {key} (circle '{}') and circle '{}' \
                             (lead {}). Say pane:{key} or @{key}",
                            eng.workspace_label(&p.workspace),
                            eng.workspace_label(&circle),
                            lead.as_deref().unwrap_or("-")
                        ));
                    }
                }
            }
            // Slug is the unique id; a display name is not. Resolving both in a
            // single pass let an EARLIER pane's name shadow a LATER pane's slug —
            // two panes named "term-2" (one of them slug `term-2-2`) meant
            // `ctl send term-2` drove the wrong pane and silently skipped the
            // one actually asked for. Slug wins, and scope narrows the
            // candidates *before* matching so a scoped call can disambiguate.
            let matches = |p: &EnginePane, by_slug: bool| {
                if by_slug {
                    p.slug == key
                } else {
                    p.name == key
                }
            };
            let in_scope = |p: &EnginePane| scope.as_ref().is_none_or(|ws| p.workspace == *ws);
            // `pane:slug` matches slugs only; otherwise slug first, then name.
            let passes: &[bool] = if forced_pane { &[true] } else { &[true, false] };
            for &by_slug in passes {
                if let Some(i) = eng
                    .panes
                    .iter()
                    .position(|p| matches(p, by_slug) && in_scope(p))
                {
                    return Ok(i);
                }
            }
            // Nothing in scope — distinguish "wrong workspace" from "no such pane".
            if eng
                .panes
                .iter()
                .any(|p| matches(p, true) || matches(p, false))
            {
                if let Some(ws) = scope {
                    return Err(format!(
                        "pane '{key}' is outside your workspace '{ws}' (use --all to cross)"
                    ));
                }
            }
            Err(format!("no pane '{key}'"))
        };
        let actor = |from: &Option<String>| {
            from.as_ref()
                .map(|f| format!("agent:{f}"))
                .unwrap_or_else(|| "cli".into())
        };

        // Capability check (Watch is handled specially by the daemon).
        if !matches!(request, Watch { .. }) {
            let principal = crate::caps::principal_of(request.from_field());
            let op = crate::caps::op_name(&request);
            let ws = request.workspace_hint();
            if let Err(msg) = self.caps.check(&principal, op, ws) {
                events::log(&principal, ws, None, "cap_denied", format!("{op}: {msg}"));
                return err(msg);
            }
        }

        match request {
            List { scope, .. } => {
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| self.pane_summary_json(p))
                    .collect();
                ok(json!({
                    "panes": panes,
                    "scope": scope,
                    "event_seq": events::current_seq(),
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    // Daemon-owned per-workspace clocks (ms since epoch) —
                    // what the GUI rail sorts idle circles by. Exposed so
                    // external walls (ledge) can mirror the rail exactly.
                    "workspace_output": self.workspace_output,
                    "workspace_touch": self.workspace_touch_ms,
                }))
            }
            New {
                name,
                cwd,
                command,
                workspace,
                file,
                scope,
                from,
            } => {
                let workspace = workspace.or_else(|| scope.clone());
                if let (Some(ws), Some(sc)) = (workspace.as_deref(), scope.as_deref()) {
                    if ws != sc {
                        return err(format!(
                            "scoped to workspace '{sc}' — cannot spawn into '{ws}' (use --all)"
                        ));
                    }
                }
                match self.spawn(SpawnSpec {
                    name,
                    cwd,
                    command,
                    workspace,
                    tiled: true,
                    resume: false,
                    file,
                }) {
                    Ok(slug) => {
                        let (ws, scratch, pname, pcmd, pcwd) = {
                            let pane = self.panes.iter().find(|p| p.slug == slug).unwrap();
                            (
                                pane.workspace.clone(),
                                pane.scratch_path.to_string_lossy().to_string(),
                                pane.name.clone(),
                                pane.command.clone(),
                                pane.cwd.clone(),
                            )
                        };
                        // A pane spawned from inside a pane is its child:
                        // roster groups helpers under their orchestrator.
                        let parent = from
                            .clone()
                            .filter(|f| *f != slug && self.panes.iter().any(|p| p.slug == *f));
                        if let Some(parent) = &parent {
                            self.pane_parents.insert(slug.clone(), parent.clone());
                        }
                        events::log(
                            &actor(&from),
                            Some(&ws),
                            Some(&slug),
                            "ctl_new",
                            format!("spawned '{pname}'"),
                        );
                        self.persist();
                        let info = self
                            .pane_infos()
                            .into_iter()
                            .find(|p| p.slug == slug)
                            .unwrap();
                        // PaneSpawned for snappy add + focus steal; full State
                        // so a GUI that missed the push (or just reconnected)
                        // still reconciles the complete pane list. External
                        // `seance ctl new` used to create panes the daemon
                        // owned but a disconnected GUI never painted.
                        self.broadcast(GuiEvent::PaneSpawned { pane: info.clone() });
                        if let Some(snap) = self.snapshot_pane(&slug) {
                            self.broadcast_grid(snap);
                        }
                        self.push_state_to_all();
                        ok(json!({
                            "slug": slug,
                            "name": pname,
                            "workspace": ws,
                            "scratchpad": scratch,
                            "command": pcmd,
                            "cwd": pcwd,
                            "parent": parent,
                        }))
                    }
                    Err(e) => err(e.to_string()),
                }
            }
            Send {
                pane,
                text,
                submit,
                force,
                queue,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let ws = self.panes[idx].workspace.clone();
                    let act = actor(&from);
                    let exited = self.panes[idx].agency.exited;
                    if let Err(e) = self.panes[idx].agency.may_inject(&act, force) {
                        events::log(&act, Some(&ws), Some(&slug), "agency.denied", e.clone());
                        return err(e);
                    }
                    if exited || self.panes[idx].kind == "file" {
                        return err(if exited {
                            "pane has exited (tombstone)".into()
                        } else {
                            "not a terminal pane".into()
                        });
                    }
                    // `--queue`: hold it; the pump injects when the pane is
                    // idle (see queue.rs). Nothing touches the PTY now and the
                    // running task stays open.
                    if queue && !self.panes[idx].asleep {
                        let (tid, position) = self.queue_task(&slug, &text, &act);
                        events::log(&act, Some(&ws), Some(&slug), "task_queued", tid.clone());
                        self.persist();
                        return ok(json!({
                            "slug": slug,
                            "task_id": tid,
                            "status": "queued",
                            "position": position,
                        }));
                    }
                    // Addressing a sleeping pane wakes it. Anything else makes
                    // every orchestrator break silently the first time it
                    // talks to a circle that dozed off.
                    if self.panes[idx].asleep {
                        if let Err(e) = self.wake_pane(&slug) {
                            return err(format!("pane '{slug}' is asleep and would not wake: {e}"));
                        }
                        self.persist();
                        self.push_state_to_all();
                    }
                    if self.panes[idx].session.is_none() {
                        return err("not a terminal pane".into());
                    }
                    self.record_contact(from.as_deref(), &ws);
                    let task_id = self.inject_task(idx, text, submit, &act, None);
                    let (pad_rev, pad_bytes) =
                        self.inject_baselines.get(&slug).copied().unwrap_or((0, 0));
                    ok(json!({
                        "slug": slug,
                        "task_id": task_id,
                        "inject_pad_rev": pad_rev,
                        "inject_pad_bytes": pad_bytes,
                        "status": "working",
                    }))
                }
                Err(e) => err(e),
            },
            MsgSend {
                to,
                text,
                kind,
                from,
                ..
            } => {
                if !matches!(kind.as_str(), "ask" | "tell") {
                    return err(format!("message kind '{kind}': expected ask or tell"));
                }
                if text.trim().is_empty() {
                    return err("message text is empty".into());
                }
                let t = match self.resolve_target(&to) {
                    Ok(t) => t,
                    Err(e) => return err(e),
                };
                if from.as_deref() == Some(t.pane.as_str()) {
                    return err(format!("'{to}' resolves to your own pane ({})", t.pane));
                }
                let target_circle = t.circle.clone().or_else(|| self.circle_of(&t.pane));
                if let Some(c) = &target_circle {
                    self.record_contact(from.as_deref(), c);
                }
                let id = self.post_message(MessageRecord {
                    kind: kind.clone(),
                    from_pane: from.clone(),
                    from_circle: from.as_deref().and_then(|p| self.circle_of(p)),
                    to_pane: t.pane.clone(),
                    to_circle: t.circle.clone(),
                    text,
                    ..Default::default()
                });
                self.persist();
                ok(json!({
                    "id": id,
                    "kind": kind,
                    "to_pane": t.pane,
                    "to_circle": target_circle,
                    "to_circle_label": target_circle.as_deref().map(|c| self.workspace_label(c)),
                    "note": t.note,
                    "status": "pending",
                }))
            }
            MsgReply { id, text, from, .. } => {
                if text.trim().is_empty() {
                    return err("reply text is empty".into());
                }
                match self.reply_to_message(&id, &text, from.as_deref()) {
                    Ok(v) => {
                        self.persist();
                        ok(v)
                    }
                    Err(e) => err(e),
                }
            }
            MsgGet { id, waiting, .. } => {
                if waiting {
                    if let Some(m) = self.message_mut(&id) {
                        m.waiter_seen_ms = super::helpers::now_ms();
                    }
                }
                match self.message(&id) {
                    Some(m) => ok(self.message_json(m)),
                    None => err(format!("no message '{id}'")),
                }
            }
            MsgList {
                pane,
                circle,
                limit,
                from,
                ..
            } => {
                let circle = match circle {
                    Some(c) => match self.resolve_circle(&c) {
                        Some((slug, _)) => Some(slug),
                        None => return err(format!("no circle '{c}'")),
                    },
                    None => None,
                };
                let pane = pane.or(if circle.is_none() { from } else { None });
                let hit = |m: &MessageRecord| match (&pane, &circle) {
                    (Some(p), _) => &m.to_pane == p || m.from_pane.as_ref() == Some(p),
                    (None, Some(c)) => {
                        m.to_circle.as_ref() == Some(c)
                            || m.from_circle.as_ref() == Some(c)
                            || self.circle_of(&m.to_pane).as_ref() == Some(c)
                    }
                    (None, None) => true,
                };
                let mut rows: Vec<serde_json::Value> = self
                    .comms
                    .messages
                    .iter()
                    .rev()
                    .filter(|m| hit(m))
                    .take(limit.unwrap_or(20))
                    .map(|m| self.message_json(m))
                    .collect();
                rows.reverse();
                ok(json!({"messages": rows}))
            }
            Lead {
                workspace,
                pane,
                scope,
                from,
            } => {
                let circle = workspace
                    .or(scope)
                    .or_else(|| from.as_deref().and_then(|p| self.circle_of(p)));
                let Some(circle) = circle else {
                    return err("lead: which circle? (name it, or run inside a pane)".into());
                };
                let Some((circle, _)) = self.resolve_circle(&circle) else {
                    return err(format!("no circle '{circle}'"));
                };
                if let Some(p) = pane {
                    let p = p.strip_prefix("pane:").unwrap_or(&p).to_string();
                    if !self
                        .panes
                        .iter()
                        .any(|x| x.slug == p && x.workspace == circle)
                    {
                        return err(format!(
                            "pane '{p}' is not in circle '{}'",
                            self.workspace_label(&circle)
                        ));
                    }
                    self.comms.leads.insert(circle.clone(), p.clone());
                    events::log(
                        &actor(&from),
                        Some(&circle),
                        Some(&p),
                        "lead_set",
                        p.clone(),
                    );
                    self.persist();
                }
                let members: Vec<String> = self
                    .panes
                    .iter()
                    .filter(|p| p.workspace == circle)
                    .map(|p| p.slug.clone())
                    .collect();
                ok(json!({
                    "workspace": circle,
                    "workspace_name": self.workspace_label(&circle),
                    "lead": self.effective_lead(&circle),
                    "explicit": self.comms.leads.contains_key(&circle),
                    "former_labels": self.comms.former_labels.get(&circle),
                    "panes": members,
                }))
            }
            Contacts { pane, from, .. } => {
                let Some(p) = pane.or(from) else {
                    return err("contacts: which pane? (name it, or run inside a pane)".into());
                };
                let rows: Vec<serde_json::Value> = self
                    .comms
                    .contacts
                    .iter()
                    .filter(|(_, set)| set.contains(&p))
                    .map(|(c, _)| {
                        json!({
                            "workspace": c,
                            "workspace_name": self.workspace_label(c),
                            "lead": self.effective_lead(c),
                        })
                    })
                    .collect();
                ok(json!({"pane": p, "contacts": rows}))
            }
            TaskFail {
                id,
                reason,
                scope,
                from,
            } => {
                let Some(t) = self.tasks.get(&id) else {
                    return err(format!("no task '{id}'"));
                };
                let slug = t.pane.clone();
                if let Err(e) = find(self, &slug, &scope) {
                    return err(e);
                }
                match self.fail_task(&id, reason.as_deref().unwrap_or("failed")) {
                    Ok(restored) => {
                        events::log(
                            &actor(&from),
                            None,
                            Some(&slug),
                            "task_failed",
                            format!("{id} restored={}", restored.as_deref().unwrap_or("-")),
                        );
                        self.persist();
                        ok(json!({"task_id": id, "status": "failed", "restored": restored}))
                    }
                    Err(e) => err(e),
                }
            }
            SendRaw {
                pane,
                bytes_b64,
                force,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => match base64_decode(&bytes_b64) {
                    Ok(bytes) => {
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let act = actor(&from);
                        if let Err(e) = self.panes[idx].agency.may_inject(&act, force) {
                            events::log(&act, Some(&ws), Some(&slug), "agency.denied", e.clone());
                            return err(e);
                        }
                        if self.panes[idx].session.is_none() {
                            return err("not a terminal pane".into());
                        }
                        self.panes[idx].agency.agent_claim(&act);
                        self.record_contact(from.as_deref(), &ws);
                        events::log_ex(
                            &act,
                            Some(&ws),
                            Some(&slug),
                            "ctl_send_raw",
                            format!("{} bytes", bytes.len()),
                            events::LogOpts {
                                origin: Some("ctl_send_raw".into()),
                                ..Default::default()
                            },
                        );
                        self.record_event(
                            &slug,
                            seance_core::replay::ReplayEvent::Input {
                                origin: act.clone(),
                                bytes_b64: {
                                    use base64::Engine as _;
                                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                                },
                            },
                        );
                        if let Some(session) = self.panes[idx].session.as_ref() {
                            session.set_input_origin(&act);
                            session.write_bytes(bytes);
                        }
                        self.broadcast_agency(&slug);
                        self.broadcast(GuiEvent::InputOrigin {
                            pane: slug.clone(),
                            origin: act,
                        });
                        ok(serde_json::Value::Null)
                    }
                    Err(e) => err(format!("bad base64: {e}")),
                },
                Err(e) => err(e),
            },
            Read {
                pane,
                lines,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    if from.as_deref() != Some(slug.as_str()) {
                        self.broadcast(GuiEvent::Touch {
                            slug: slug.clone(),
                            verb: "👁 observed".into(),
                            actor: actor(&from),
                        });
                    }
                    let text = if let Some(s) = self.panes[idx].session.as_ref() {
                        s.screen_text(lines)
                    } else if let Some(path) = &self.panes[idx].file {
                        std::fs::read_to_string(path).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    ok(json!({"screen": text}))
                }
                Err(e) => err(e),
            },
            // The roster row for one pane: everything `brief` knows (cwd,
            // activity, quota, task) so a respawn never needs a second call.
            Status { pane, scope, .. } => match find(self, &pane, &scope) {
                Ok(idx) => ok(self.pane_summary_json(&self.panes[idx])),
                Err(e) => err(e),
            },
            Kill { pane, scope, from } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "ctl_kill",
                        "killed".into(),
                    );
                    self.kill_pane(&slug);
                    self.broadcast(GuiEvent::PaneKilled { slug });
                    self.persist();
                    ok(serde_json::Value::Null)
                }
                Err(e) => err(e),
            },
            Select { pane, scope, from } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let ws = self.panes[idx].workspace.clone();
                    events::log(
                        &actor(&from),
                        Some(&ws),
                        Some(&slug),
                        "ctl_select",
                        "selected".into(),
                    );
                    self.select_pane_everywhere(&slug, &ws);
                    ok(serde_json::Value::Null)
                }
                Err(e) => err(e),
            },
            Scratchpad { pane, scope, .. } => match find(self, &pane, &scope) {
                Ok(idx) => ok(json!({
                    "path": self.panes[idx].scratch_path.to_string_lossy(),
                })),
                Err(e) => err(e),
            },
            Timeline {
                since_secs,
                pane,
                actor: act,
                limit,
                scope,
                ..
            } => {
                let since_ms = since_secs
                    .map(|s| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0)
                            .saturating_sub(s * 1000)
                    })
                    .unwrap_or(0);
                let entries = events::read(
                    since_ms,
                    scope.as_deref(),
                    pane.as_deref(),
                    act.as_deref(),
                    limit.unwrap_or(100),
                );
                let rows: Vec<_> = entries
                    .iter()
                    .map(|e| {
                        json!({
                            "time": events::fmt_time(e.ts),
                            "actor": e.actor,
                            "pane": e.pane,
                            "workspace": e.workspace,
                            "kind": e.kind,
                            "detail": e.detail,
                        })
                    })
                    .collect();
                ok(json!({"events": rows}))
            }
            StatusSet {
                state,
                note,
                pane,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("status-set: no pane".into());
                };
                if let Err(e) = validate_status(&state) {
                    return err(e);
                }
                match find(self, &target, &scope) {
                    Ok(idx) => {
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let act = actor(&from);
                        if let Err(e) = assert_self_or_cross(&slug, &from, &act) {
                            return err(e);
                        }
                        self.record_event(
                            &slug,
                            seance_core::replay::ReplayEvent::Status {
                                state: state.clone(),
                                note: note.clone(),
                            },
                        );
                        // Evidence gate: bare status-set done cannot lie without pad growth.
                        if state == "done" {
                            if let Err(e) = self.require_since_inject_evidence(&slug) {
                                return err(format!(
                                    "{e} — use `finish` with a body, or grow the pad first"
                                ));
                            }
                        }
                        self.statuses
                            .insert(slug.clone(), (state.clone(), note.clone()));
                        if state == "done" {
                            self.complete_active_task(&slug, None);
                        }
                        let detail = match &note {
                            Some(n) => format!("{state}: {n}"),
                            None => state.clone(),
                        };
                        events::log(&act, Some(&ws), Some(&slug), "status_set", detail);
                        self.broadcast(GuiEvent::Status {
                            slug: slug.clone(),
                            state: state.clone(),
                            note: note.clone(),
                        });
                        self.persist();
                        ok(json!({
                            "slug": slug,
                            "status": state,
                            "pad_rev": self.pad_revs.get(&slug).copied().unwrap_or(0),
                            "task_id": self.active_tasks.get(&slug),
                        }))
                    }
                    Err(e) => err(e),
                }
            }
            Ask {
                question,
                choices,
                scope,
                from,
            } => {
                self.ask_counter += 1;
                let id = format!("ask-{}", self.ask_counter);
                let from_label = from.clone().unwrap_or_else(|| "cli".into());
                let ask = PendingAsk {
                    id: id.clone(),
                    from: from_label.clone(),
                    workspace: scope.clone(),
                    question: question.clone(),
                    choices: choices.clone().unwrap_or_default(),
                    answer: None,
                };
                self.asks.push(ask);
                self.broadcast(GuiEvent::Ask {
                    ask: AskInfo {
                        id: id.clone(),
                        from: from_label,
                        workspace: scope,
                        question,
                        choices: choices.unwrap_or_default(),
                        answer: None,
                    },
                });
                ok(json!({"id": id}))
            }
            Propose {
                pane,
                text,
                reason,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    if self.panes[idx].session.is_none() {
                        return err("not a terminal pane".into());
                    }
                    self.proposal_counter += 1;
                    let id = format!("prop-{}", self.proposal_counter);
                    let slug = self.panes[idx].slug.clone();
                    let from_label = from.clone().unwrap_or_else(|| "cli".into());
                    let ghost = GhostSnap {
                        id: id.clone(),
                        text: text.clone(),
                        from: from_label,
                        reason,
                    };
                    *self.panes[idx]
                        .session
                        .as_ref()
                        .unwrap()
                        .ghost
                        .lock()
                        .unwrap() = Some(ghost.clone());
                    self.proposals.insert(id.clone(), (slug.clone(), None));
                    self.broadcast(GuiEvent::Ghost {
                        pane: slug,
                        ghost: Some(ghost),
                    });
                    ok(json!({"id": id}))
                }
                Err(e) => err(e),
            },
            ProposeResult { id, .. } => match self.proposals.get(&id) {
                Some((_, Some(outcome))) => {
                    let outcome = outcome.clone();
                    self.proposals.remove(&id);
                    ok(json!({"resolved": true, "outcome": outcome}))
                }
                Some((_, None)) => ok(json!({"resolved": false})),
                None => err(format!("no proposal '{id}'")),
            },
            Human { scope, .. } => {
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| {
                        let w = p.agency.to_wire();
                        json!({
                            "slug": p.slug,
                            "workspace": p.workspace,
                            "owner": w.owner,
                            "drive_mode": w.drive_mode,
                            "human_idle": w.human_idle,
                            "exited": w.exited,
                            "exit_code": w.exit_code,
                            "running": p.session.as_ref().map(|s| s.is_running()).unwrap_or(false)
                                && !p.agency.exited,
                            "title": p.session.as_ref().and_then(|s| s.title()),
                        })
                    })
                    .collect();
                ok(json!({
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
                    "panes": panes,
                }))
            }
            CmdBegin {
                command, cwd, from, ..
            } => {
                let Some(pane) = from else {
                    return err("cmd-begin: must run inside a pane".into());
                };
                let cwd = cwd.unwrap_or_default();
                let seq = self.cmd_log.begin(&pane, command.clone(), cwd.clone());
                self.persist();
                let span = format!("cmd:{pane}:{seq}");
                events::log_ex(
                    &format!("agent:{pane}"),
                    None,
                    Some(&pane),
                    "cmd_start",
                    format!("$ {command}"),
                    events::LogOpts {
                        span: Some(span),
                        origin: Some("shell_hook".into()),
                        ..Default::default()
                    },
                );
                ok(serde_json::Value::Null)
            }
            CmdEnd { exit, from, .. } => {
                let Some(pane) = from else {
                    return err("cmd-end: must run inside a pane".into());
                };
                let closed = self.cmd_log.end(&pane, exit);
                // Shell turn-end → idle only when:
                //   (1) a real open cmd record closed (not a stray cmd-end),
                //   (2) no inject task is still open (don't clobber agent working),
                //   (3) status was working/planning.
                // Agent CLIs don't source seance.bash; forged ctl cmd-end from an
                // agent pane with an open task is ignored for status.
                if closed
                    && !self.active_tasks.contains_key(&pane)
                    && matches!(
                        self.statuses.get(&pane).map(|(s, _)| s.as_str()),
                        Some("working" | "planning")
                    )
                {
                    self.statuses.insert(
                        pane.clone(),
                        ("idle".into(), Some(format!("cmd exit {exit}"))),
                    );
                    self.broadcast(GuiEvent::Status {
                        slug: pane.clone(),
                        state: "idle".into(),
                        note: Some(format!("cmd exit {exit}")),
                    });
                }
                // Throttle-persist cmdlog so export-from-disk sees recent cmds.
                self.persist();
                // Clients apply their own notify-on-command-finish rule.
                let alert = closed
                    .then(|| self.cmd_log.last(&pane, false))
                    .flatten()
                    .map(|rec| GuiEvent::PaneAlert {
                        pane: pane.clone(),
                        kind: "command".into(),
                        title: String::new(),
                        body: rec.command.clone(),
                        duration_ms: rec.duration_ms().unwrap_or(0),
                        exit_code: Some(exit),
                    });
                if let Some(alert) = alert {
                    self.broadcast(alert);
                }
                let (detail, span) = match self.cmd_log.last(&pane, false) {
                    Some(rec) => (
                        format!(
                            "exit {exit} · {}ms · $ {}",
                            rec.duration_ms().unwrap_or(0),
                            rec.command
                        ),
                        Some(format!("cmd:{pane}:{}", rec.seq)),
                    ),
                    None => (format!("exit {exit}"), None),
                };
                events::log_ex(
                    &format!("agent:{pane}"),
                    None,
                    Some(&pane),
                    "cmd_end",
                    detail,
                    events::LogOpts {
                        span,
                        origin: Some("shell_hook".into()),
                        ..Default::default()
                    },
                );
                ok(serde_json::Value::Null)
            }
            Commands {
                pane, limit, scope, ..
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let records = self.cmd_log.list(&slug, limit.unwrap_or(50));
                    ok(serde_json::to_value(records).unwrap_or(serde_json::Value::Null))
                }
                Err(e) => err(e),
            },
            LastCommand {
                pane,
                failed_only,
                scope,
                ..
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    match self.cmd_log.last(&slug, failed_only) {
                        Some(rec) => {
                            ok(serde_json::to_value(rec).unwrap_or(serde_json::Value::Null))
                        }
                        None => err("no matching command".into()),
                    }
                }
                Err(e) => err(e),
            },
            AskResult { id, .. } => match self.asks.iter().position(|a| a.id == id) {
                Some(idx) => {
                    if let Some(answer) = self.asks[idx].answer.clone() {
                        self.asks.remove(idx);
                        ok(json!({"answered": true, "answer": answer}))
                    } else {
                        ok(json!({"answered": false}))
                    }
                }
                None => err(format!("no ask '{id}'")),
            },
            // Watch is handled by the daemon connection loop (streaming).
            Watch { .. } => ok(json!({
                "watching": true,
                "cursor": events::current_seq(),
                "note": "daemon streams events after this ack",
            })),
            Whoami { scope, from } => {
                let principal = actor(&from);
                let policy = self.caps.policy_for(scope.as_deref());
                let grants: Vec<_> = self
                    .caps
                    .grants
                    .iter()
                    .filter(|g| g.principal == principal || g.principal == "*")
                    .cloned()
                    .collect();
                let session = from.clone();
                let (task_id, task_status, task_chars) = session
                    .as_ref()
                    .and_then(|slug| {
                        let tid = self.active_tasks.get(slug).cloned().or_else(|| {
                            self.tasks
                                .values()
                                .filter(|t| t.pane == *slug)
                                .max_by_key(|t| t.created_ms)
                                .map(|t| t.id.clone())
                        })?;
                        let t = self.tasks.get(&tid)?;
                        Some((Some(tid), Some(t.status.clone()), Some(t.body.len())))
                    })
                    .unwrap_or((None, None, None));
                // Which circle am I in? A property of the PANE, not of what
                // the caller's environment last heard — that is the whole
                // point of the identity split, and this is the canonical
                // answer an agent should trust.
                let ws_slug = session
                    .as_ref()
                    .and_then(|slug| self.panes.iter().find(|p| p.slug == *slug))
                    .map(|p| p.workspace.clone())
                    .or_else(|| scope.clone());
                let ws_label = ws_slug.as_deref().map(|s| self.workspace_label(s));
                ok(json!({
                    "principal": principal,
                    "session": session,
                    "workspace": ws_slug,
                    "workspace_name": ws_label,
                    "policy": policy.as_str(),
                    "grants": grants,
                    "event_seq": events::current_seq(),
                    "task_id": task_id,
                    "task_status": task_status,
                    "task_body_chars": task_chars,
                    "hint": "seance ctl task   # re-read durable inject body for this pane",
                }))
            }
            Caps { .. } => ok(json!({
                "default_policy": self.caps.default_policy.as_str(),
                "workspace_policy": self.caps.workspace_policy.iter().map(|(k,v)| (k, v.as_str())).collect::<HashMap<_,_>>(),
                "grants": self.caps.grants,
            })),
            CapsGrant {
                principal,
                cap,
                workspace,
                ttl_secs,
                from,
                ..
            } => {
                let expires_ms = ttl_secs.map(|s| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64 + s * 1000)
                        .unwrap_or(0)
                });
                let g = crate::caps::Grant {
                    principal: principal.clone(),
                    cap: cap.clone(),
                    workspace: workspace.clone(),
                    expires_ms,
                };
                self.caps.grant(g);
                let _ = self.caps.save();
                events::log(
                    &actor(&from),
                    workspace.as_deref(),
                    None,
                    "cap_grant",
                    format!("granted {cap} to {principal}"),
                );
                ok(json!({"granted": true, "principal": principal, "cap": cap}))
            }
            CapsRevoke {
                principal,
                cap,
                workspace,
                from,
                ..
            } => {
                let n = self.caps.revoke(&principal, &cap, workspace.as_deref());
                let _ = self.caps.save();
                events::log(
                    &actor(&from),
                    workspace.as_deref(),
                    None,
                    "cap_revoke",
                    format!("revoked {n} grant(s) of {cap} from {principal}"),
                );
                ok(json!({"revoked": n}))
            }
            PrLinkAdd {
                url,
                workspace,
                scope,
                from,
            } => {
                let Some(ws) = workspace.or(scope) else {
                    return err("pr-link add: expected WORKSPACE".into());
                };
                if !url.contains("/pull/") {
                    return err(format!("pr-link add: '{url}' is not a PR url"));
                }
                // Explicit add beats a past clear: lift the tombstone first,
                // otherwise `record_pr_link` would silently no-op.
                self.undismiss_pr_link(&ws, &url);
                self.record_pr_link(&ws, &url, now_ms());
                let n = self.pr_links.get(&ws).map(|l| l.len()).unwrap_or(0);
                self.persist();
                self.push_state_to_all();
                events::log(
                    &actor(&from),
                    Some(&ws),
                    None,
                    "pr_link_add",
                    format!("seeded {url}"),
                );
                ok(json!({"workspace": ws, "url": url, "links": n}))
            }
            PrLinkClear {
                url,
                workspace,
                scope,
                from,
            } => {
                let Some(ws) = workspace.or(scope) else {
                    return err("pr-link clear: expected WORKSPACE".into());
                };
                let removed = self.clear_pr_links(&ws, url.as_deref());
                self.persist();
                self.push_state_to_all();
                events::log(
                    &actor(&from),
                    Some(&ws),
                    None,
                    "pr_link_clear",
                    format!("cleared {removed} link(s)"),
                );
                ok(json!({"workspace": ws, "removed": removed}))
            }
            RenamePane {
                name,
                pane,
                scope,
                from,
            } => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return err("rename-pane: the new name is empty".into());
                }
                let Some(key) = pane.or_else(|| from.clone()) else {
                    return err("rename-pane: outside a pane, give PANE NEW-NAME".into());
                };
                match find(self, &key, &scope) {
                    Ok(idx) => {
                        let slug = self.panes[idx].slug.clone();
                        self.panes[idx].name = name.clone();
                        self.persist();
                        self.push_state_to_all();
                        events::log(
                            &actor(&from),
                            Some(&self.panes[idx].workspace.clone()),
                            Some(&slug),
                            "pane_renamed",
                            format!("name -> '{name}'"),
                        );
                        ok(json!({"pane": slug, "name": name}))
                    }
                    Err(e) => err(e),
                }
            }
            RenameCircle {
                name,
                workspace,
                scope,
                from,
            } => {
                let Some(key) = workspace.or(scope) else {
                    return err("rename: expected a circle".into());
                };
                match self.rename_workspace(&key, &name) {
                    Some(slug) => {
                        let label = self.workspace_label(&slug);
                        self.persist();
                        self.push_state_to_all();
                        events::log(
                            &actor(&from),
                            Some(&slug),
                            None,
                            "workspace_renamed",
                            format!("label -> '{label}'"),
                        );
                        ok(json!({"workspace": slug, "workspace_name": label}))
                    }
                    None => err(format!("no circle '{key}'")),
                }
            }
            Sleep {
                workspace,
                scope,
                from,
            } => {
                let Some(ws) = workspace.or(scope) else {
                    return err("sleep: expected WORKSPACE".into());
                };
                match self.sleep_workspace(&ws) {
                    Ok(n) => {
                        self.persist();
                        self.push_state_to_all();
                        events::log(
                            &actor(&from),
                            Some(&ws),
                            None,
                            "workspace_slept",
                            format!("{n} pane(s) slept"),
                        );
                        ok(json!({"workspace": ws, "slept": n}))
                    }
                    Err(e) => err(e.to_string()),
                }
            }
            Wake {
                workspace,
                scope,
                from,
            } => {
                let Some(ws) = workspace.or(scope) else {
                    return err("wake: expected WORKSPACE".into());
                };
                match self.wake_workspace(&ws) {
                    Ok(n) => {
                        self.persist();
                        self.push_state_to_all();
                        events::log(
                            &actor(&from),
                            Some(&ws),
                            None,
                            "workspace_woke",
                            format!("{n} pane(s) woke"),
                        );
                        ok(json!({"workspace": ws, "woke": n}))
                    }
                    Err(e) => err(e.to_string()),
                }
            }
            PolicyGet {
                workspace, scope, ..
            } => {
                let ws = workspace.or(scope);
                let policy = self.caps.policy_for(ws.as_deref());
                ok(json!({
                    "workspace": ws,
                    "policy": policy.as_str(),
                    "default_policy": self.caps.default_policy.as_str(),
                }))
            }
            PolicySet {
                mode,
                workspace,
                scope,
                from,
            } => {
                let Some(mode) = crate::caps::PolicyMode::parse(&mode) else {
                    return err(format!(
                        "unknown policy '{mode}' (open|propose_required|locked)"
                    ));
                };
                let ws = workspace.or(scope);
                self.caps.set_policy(ws.clone(), mode.clone());
                let _ = self.caps.save();
                events::log(
                    &actor(&from),
                    ws.as_deref(),
                    None,
                    "policy_set",
                    format!("policy -> {}", mode.as_str()),
                );
                ok(json!({"policy": mode.as_str(), "workspace": ws}))
            }
            Seize {
                pane,
                as_owner,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    let who = as_owner.unwrap_or_else(|| "human".into());
                    if who == "human" || actor(&from) == "human" {
                        self.panes[idx].agency.human_steal();
                    } else {
                        self.panes[idx].agency.agent_claim(&who);
                    }
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "agency.seized",
                        format!("owner={}", self.panes[idx].agency.owner.as_str()),
                    );
                    self.broadcast_agency(&slug);
                    self.push_state_to_all();
                    ok(json!(self.panes[idx].agency.to_wire()))
                }
                Err(e) => err(e),
            },
            Release {
                pane, scope, from, ..
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let slug = self.panes[idx].slug.clone();
                    self.panes[idx].agency.release();
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "agency.released",
                        "owner=none".into(),
                    );
                    self.broadcast_agency(&slug);
                    self.push_state_to_all();
                    ok(json!(self.panes[idx].agency.to_wire()))
                }
                Err(e) => err(e),
            },
            DriveMode {
                pane,
                mode,
                scope,
                from,
            } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let Some(dm) = crate::agency::DriveMode::parse(&mode) else {
                        return err(format!(
                            "unknown drive mode '{mode}' (pair|locked_human|agent_led)"
                        ));
                    };
                    let slug = self.panes[idx].slug.clone();
                    self.panes[idx].agency.drive_mode = dm;
                    events::log(
                        &actor(&from),
                        Some(&self.panes[idx].workspace),
                        Some(&slug),
                        "agency.drive_mode",
                        mode.clone(),
                    );
                    self.broadcast_agency(&slug);
                    ok(json!(self.panes[idx].agency.to_wire()))
                }
                Err(e) => err(e),
            },
            Doctor { .. } => {
                let rows = crate::agents::doctor();
                ok(serde_json::to_value(rows).unwrap_or(json!([])))
            }
            Brief { scope, .. } => {
                let panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| self.pane_summary_json(p))
                    .collect();
                ok(json!({
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
                    "event_seq": events::current_seq(),
                    "scope": scope,
                    "panes": panes,
                }))
            }
            Note {
                pane,
                text,
                append,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("note: need pane or $SEANCE_SESSION".into());
                };
                match find(self, &target, &scope) {
                    Ok(idx) => {
                        let path = self.panes[idx].scratch_path.clone();
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let author = actor(&from);
                        if let Err(e) = assert_self_or_cross(&slug, &from, &author) {
                            return err(e);
                        }
                        let stamp =
                            format!("\n\n---\n<!-- {} · {} -->\n\n", author, chrono_lite_stamp());
                        let chunk = format!("{stamp}{text}\n");
                        let result = if append {
                            atomic_append_pad(&path, &chunk)
                        } else {
                            atomic_write_pad(&path, &format!("{text}\n"))
                        };
                        match result {
                            Ok(()) => {
                                let rev = self.bump_pad_rev(&slug);
                                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                                events::log(
                                    &author,
                                    Some(&ws),
                                    Some(&slug),
                                    "note",
                                    format!("{} chars rev={rev}", text.len()),
                                );
                                self.persist();
                                ok(json!({
                                    "path": path.to_string_lossy(),
                                    "append": append,
                                    "pad_rev": rev,
                                    "scratchpad_bytes": bytes,
                                }))
                            }
                            Err(e) => err(e),
                        }
                    }
                    Err(e) => err(e),
                }
            }
            Finish {
                pane,
                body,
                append,
                status,
                status_note,
                empty_ok,
                task,
                scope,
                from,
            } => {
                let target = pane.or_else(|| from.clone());
                let Some(target) = target else {
                    return err("finish: need pane or $SEANCE_SESSION".into());
                };
                if let Err(e) = validate_status(&status) {
                    return err(e);
                }
                let body_empty = body.as_ref().map(|b| b.trim().is_empty()).unwrap_or(true);
                if status == "done" && body_empty && !empty_ok {
                    return err("finish: status=done requires a body (or --empty-ok). \
                         Evidence-bound completion: write the answer, then finish."
                        .into());
                }
                match find(self, &target, &scope) {
                    Ok(idx) => {
                        let path = self.panes[idx].scratch_path.clone();
                        let slug = self.panes[idx].slug.clone();
                        let ws = self.panes[idx].workspace.clone();
                        let author = actor(&from);
                        if let Err(e) = assert_self_or_cross(&slug, &from, &author) {
                            return err(e);
                        }
                        if let Some(tid) = task.as_deref() {
                            if let Err(e) = self.validate_finish_task(&slug, tid) {
                                return err(e);
                            }
                        }
                        let mut rev = self.pad_revs.get(&slug).copied().unwrap_or(0);
                        if let Some(body) = body.filter(|b| !b.trim().is_empty()) {
                            let stamp = format!(
                                "\n\n---\n<!-- {} · {} · finish -->\n\n",
                                author,
                                chrono_lite_stamp()
                            );
                            let chunk = format!("{stamp}{body}\n");
                            let write_res = if append {
                                atomic_append_pad(&path, &chunk)
                            } else {
                                atomic_write_pad(&path, &format!("{body}\n"))
                            };
                            if let Err(e) = write_res {
                                return err(format!("finish: scratchpad write failed: {e}"));
                            }
                            rev = self.bump_pad_rev(&slug);
                        }
                        let note = status_note.clone();
                        self.statuses
                            .insert(slug.clone(), (status.clone(), note.clone()));
                        let finished_task = if status == "done" {
                            self.complete_active_task(&slug, task.as_deref())
                        } else {
                            None
                        };
                        self.broadcast(GuiEvent::Status {
                            slug: slug.clone(),
                            state: status.clone(),
                            note: note.clone(),
                        });
                        events::log(
                            &author,
                            Some(&ws),
                            Some(&slug),
                            "status_set",
                            match &note {
                                Some(n) => format!("{status}: {n}"),
                                None => status.clone(),
                            },
                        );
                        events::log(
                            &author,
                            Some(&ws),
                            Some(&slug),
                            "finish",
                            format!(
                                "status={status} rev={rev} task={}",
                                finished_task.as_deref().unwrap_or("-")
                            ),
                        );
                        self.persist();
                        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        ok(json!({
                            "slug": slug,
                            "status": status,
                            "scratchpad": path.to_string_lossy(),
                            "scratchpad_bytes": bytes,
                            "pad_rev": rev,
                            "task_id": finished_task,
                        }))
                    }
                    Err(e) => err(e),
                }
            }
            Task {
                pane,
                id,
                scope,
                from,
            } => {
                // Lookup by id, else active task for pane / $SEANCE_SESSION.
                if let Some(tid) = id {
                    match self.tasks.get(&tid) {
                        Some(t) => ok(task_json(t)),
                        None => err(format!("no task '{tid}'")),
                    }
                } else {
                    let target = pane.or_else(|| from.clone());
                    let Some(target) = target else {
                        return err(
                            "task: need pane, --id, or $SEANCE_SESSION (your inject inbox)".into(),
                        );
                    };
                    match find(self, &target, &scope) {
                        Ok(idx) => {
                            let slug = self.panes[idx].slug.clone();
                            if let Some(tid) = self.active_tasks.get(&slug) {
                                match self.tasks.get(tid) {
                                    Some(t) => ok(task_json(t)),
                                    None => err(format!("active task '{tid}' missing")),
                                }
                            } else {
                                // Fall back to most recent task for this pane.
                                let mut candidates: Vec<_> =
                                    self.tasks.values().filter(|t| t.pane == slug).collect();
                                candidates.sort_by_key(|t| std::cmp::Reverse(t.created_ms));
                                match candidates.first() {
                                    Some(t) => ok(task_json(t)),
                                    None => err(format!("no task for pane '{slug}'")),
                                }
                            }
                        }
                        Err(e) => err(e),
                    }
                }
            }
            Roster { scope, .. } => {
                let mut panes: Vec<_> = self
                    .panes
                    .iter()
                    .filter(|p| scope.as_deref().is_none_or(|ws| p.workspace == ws))
                    .map(|p| self.pane_summary_json(p))
                    .collect();
                // Terminals first, then by status priority (blocked/needs-human first).
                panes.sort_by(|a, b| {
                    let rank = |p: &serde_json::Value| -> u8 {
                        match p.get("status").and_then(|s| s.as_str()) {
                            Some("needs-human") => 0,
                            Some("blocked") => 1,
                            Some("working") => 2,
                            Some("planning") => 3,
                            Some("done") => 5,
                            Some("idle") => 6,
                            _ => 4,
                        }
                    };
                    rank(a).cmp(&rank(b)).then_with(|| {
                        let sa = a.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                        let sb = b.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                        sa.cmp(sb)
                    })
                });
                ok(json!({
                    "focused_pane": self.focused_pane,
                    "selected_workspace": self.selected_workspace,
                    "pending_asks": self.asks.iter().filter(|a| a.answer.is_none()).count(),
                    "event_seq": events::current_seq(),
                    "scope": scope,
                    "panes": panes,
                }))
            }
        }
    }

    /// Paste `text` into pane `idx` as a task: opens a new task, or activates
    /// the queued `queued` one. Owns the envelope bookkeeping (baseline,
    /// working badge, events, broadcasts) for `send` and the queue pump.
    pub(super) fn inject_task(
        &mut self,
        idx: usize,
        text: String,
        submit: bool,
        act: &str,
        queued: Option<String>,
    ) -> String {
        let slug = self.panes[idx].slug.clone();
        let ws = self.panes[idx].workspace.clone();
        self.panes[idx].agency.agent_claim(act);
        events::log_ex(
            act,
            Some(&ws),
            Some(&slug),
            "ctl_send",
            format!("sent {} chars", text.len()),
            events::LogOpts {
                origin: Some("ctl_send".into()),
                ..Default::default()
            },
        );
        // Dispatch envelope + working badge + pad baseline (before inject consumes text).
        let task_id = match queued {
            Some(tid) => self.activate_queued(&slug, &tid),
            None => self.begin_task(&slug, &text),
        };
        self.record_event(
            &slug,
            seance_core::replay::ReplayEvent::Send {
                from: act.to_string(),
                text: text.clone(),
                submit,
            },
        );
        if let Some(session) = self.panes[idx].session.as_ref() {
            session.set_input_origin(act);
            session.scroll_to_bottom();
            session.inject(text, submit);
        }
        let note = format!("inject from {act}");
        self.statuses
            .insert(slug.clone(), ("working".into(), Some(note.clone())));
        self.broadcast(GuiEvent::Status {
            slug: slug.clone(),
            state: "working".into(),
            note: Some(note),
        });
        events::log(
            act,
            Some(&ws),
            Some(&slug),
            "status_set",
            format!("working: inject from {act} task={task_id}"),
        );
        events::log(act, Some(&ws), Some(&slug), "task_open", task_id.clone());
        self.broadcast_agency(&slug);
        self.broadcast(GuiEvent::InputOrigin {
            pane: slug.clone(),
            origin: act.to_string(),
        });
        self.broadcast(GuiEvent::Touch {
            slug: slug.clone(),
            verb: "⚡ driven".into(),
            actor: act.to_string(),
        });
        self.persist();
        task_id
    }

    fn bump_pad_rev(&mut self, slug: &str) -> u64 {
        let e = self.pad_revs.entry(slug.to_string()).or_insert(0);
        *e = e.saturating_add(1);
        *e
    }

    /// Open a dispatch task on inject: baseline + durable inbox body.
    pub(crate) fn begin_task(&mut self, slug: &str, body: &str) -> String {
        let pad_bytes = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| std::fs::metadata(&p.scratch_path).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        let pad_rev = self.pad_revs.get(slug).copied().unwrap_or(0);
        self.inject_baselines
            .insert(slug.to_string(), (pad_rev, pad_bytes));
        let superseded = self.supersede_active(slug);
        self.task_counter = self.task_counter.saturating_add(1);
        let id = format!("task-{}", self.task_counter);
        // Cap stored body so state.json stays sane (full inject usually fits).
        let body = if body.len() > 64_000 {
            format!(
                "{}…\n<!-- truncated {} chars -->\n",
                &body[..64_000],
                body.len()
            )
        } else {
            body.to_string()
        };
        let rec = TaskRecord {
            id: id.clone(),
            pane: slug.to_string(),
            inject_pad_rev: pad_rev,
            inject_pad_bytes: pad_bytes,
            body,
            status: "open".into(),
            created_ms: now_ms(),
            finished_ms: None,
            supersedes: superseded,
            ..Default::default()
        };
        // Sidecar next to scratchpad so workers can discover task_id without
        // env (agents don't re-exec on inject). Paths:
        //   <scratch>.taskid  → bare id
        //   <scratch>.task.json → id + status + body
        if let Some(p) = self.panes.iter().find(|p| p.slug == slug) {
            write_task_sidecar(&p.scratch_path, &rec);
        }
        self.tasks.insert(id.clone(), rec);
        self.active_tasks.insert(slug.to_string(), id.clone());
        id
    }

    fn validate_finish_task(&self, slug: &str, tid: &str) -> Result<(), String> {
        let task = self
            .tasks
            .get(tid)
            .ok_or_else(|| format!("finish: no task '{tid}'"))?;
        if task.pane != slug {
            return Err(format!(
                "finish: task '{tid}' belongs to pane '{}', not '{slug}'",
                task.pane
            ));
        }
        if task.status != "open" || self.active_tasks.get(slug).map(String::as_str) != Some(tid) {
            return Err(format!("finish: task '{tid}' is {} and is not an open current task; read `ctl task` before finishing", task.status));
        }
        Ok(())
    }

    /// Mark active (or named) task done; returns task_id if any.
    pub(crate) fn complete_active_task(
        &mut self,
        slug: &str,
        want: Option<&str>,
    ) -> Option<String> {
        let tid = want
            .map(|s| s.to_string())
            .or_else(|| self.active_tasks.get(slug).cloned());
        let Some(tid) = tid else {
            return None;
        };
        if self.validate_finish_task(slug, &tid).is_err() {
            return None;
        }
        if let Some(t) = self.tasks.get_mut(&tid) {
            t.status = "done".into();
            t.finished_ms = Some(now_ms());
            if let Some(p) = self.panes.iter().find(|p| p.slug == slug) {
                write_task_sidecar(&p.scratch_path, t);
            }
        }
        self.active_tasks.remove(slug);
        Some(tid)
    }

    /// Pad grew since last inject (rev or bytes).
    fn require_since_inject_evidence(&self, slug: &str) -> Result<(), String> {
        let Some((inj_rev, inj_bytes)) = self.inject_baselines.get(slug).copied() else {
            // No inject baseline → allow (manual status or shell pane).
            return Ok(());
        };
        let pad_rev = self.pad_revs.get(slug).copied().unwrap_or(0);
        let pad_bytes = self
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .and_then(|p| std::fs::metadata(&p.scratch_path).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        if pad_rev > inj_rev || pad_bytes > inj_bytes {
            Ok(())
        } else {
            Err(format!(
                "no pad evidence since inject (rev {pad_rev}≤{inj_rev}, bytes {pad_bytes}≤{inj_bytes})"
            ))
        }
    }
}
