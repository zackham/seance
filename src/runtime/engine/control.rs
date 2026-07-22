//! Control-plane request handling (`handle_control` + pad/task bookkeeping).

use std::collections::HashMap;

use serde_json::json;

use super::helpers::{
    assert_self_or_cross, atomic_append_pad, atomic_write_pad, base64_decode, chrono_lite_stamp,
    now_ms, task_json, validate_status, write_task_sidecar,
};
use super::{Engine, PendingAsk, SpawnSpec};
use crate::control::{ControlRequest, ControlResponse};
use crate::events;
use crate::runtime::protocol::*;
use crate::runtime::snapshot::GhostSnap;

impl Engine {
    pub fn handle_control(&mut self, request: ControlRequest) -> ControlResponse {
        use ControlRequest::*;
        let ok = |data: serde_json::Value| ControlResponse::ok(data);
        let err = |m: String| ControlResponse::err(m);
        let find = |eng: &Engine, key: &str, scope: &Option<String>| -> Result<usize, String> {
            let idx = eng
                .panes
                .iter()
                .position(|p| p.slug == key || p.name == key)
                .ok_or_else(|| format!("no pane '{key}'"))?;
            if let Some(ws) = scope {
                if eng.panes[idx].workspace != *ws {
                    return Err(format!(
                        "pane '{key}' is outside your workspace '{ws}' (use --all to cross)"
                    ));
                }
            }
            Ok(idx)
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
                        let (ws, scratch, pname) = {
                            let pane = self.panes.iter().find(|p| p.slug == slug).unwrap();
                            (
                                pane.workspace.clone(),
                                pane.scratch_path.to_string_lossy().to_string(),
                                pane.name.clone(),
                            )
                        };
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
                            "workspace": ws,
                            "scratchpad": scratch,
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
                    if self.panes[idx].session.is_none() {
                        return err(if exited {
                            "pane has exited (tombstone)".into()
                        } else {
                            "not a terminal pane".into()
                        });
                    }
                    self.panes[idx].agency.agent_claim(&act);
                    events::log_ex(
                        &act,
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
                    let task_id = self.begin_task(&slug, &text);
                    if let Some(session) = self.panes[idx].session.as_ref() {
                        session.set_input_origin(&act);
                        session.scroll_to_bottom();
                        session.inject(text, submit);
                    }
                    let (pad_rev, pad_bytes) =
                        self.inject_baselines.get(&slug).copied().unwrap_or((0, 0));
                    self.statuses.insert(
                        slug.clone(),
                        ("working".into(), Some(format!("inject from {act}"))),
                    );
                    self.broadcast(GuiEvent::Status {
                        slug: slug.clone(),
                        state: "working".into(),
                        note: Some(format!("inject from {act}")),
                    });
                    events::log(
                        &act,
                        Some(&ws),
                        Some(&slug),
                        "status_set",
                        format!("working: inject from {act} task={task_id}"),
                    );
                    events::log(&act, Some(&ws), Some(&slug), "task_open", task_id.clone());
                    self.broadcast_agency(&slug);
                    self.broadcast(GuiEvent::InputOrigin {
                        pane: slug.clone(),
                        origin: act.clone(),
                    });
                    self.broadcast(GuiEvent::Touch {
                        slug: slug.clone(),
                        verb: "⚡ driven".into(),
                        actor: act,
                    });
                    self.persist();
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
            Status { pane, scope, .. } => match find(self, &pane, &scope) {
                Ok(idx) => {
                    let p = &self.panes[idx];
                    ok(json!({
                        "kind": p.kind,
                        "name": p.name,
                        "slug": p.slug,
                        "workspace": p.workspace,
                        "command": p.command,
                        "running": p.session.as_ref().map(|s| s.is_running()).unwrap_or(true),
                        "title": p.session.as_ref().and_then(|s| s.title()),
                        "tiled": p.tiled,
                    }))
                }
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
            WorkspaceFork {
                workspace,
                name,
                scope,
                from,
            } => {
                let src = workspace
                    .or_else(|| scope.clone())
                    .or_else(|| self.selected_workspace.clone());
                let Some(src) = src else {
                    return err("fork: no source workspace".into());
                };
                match self.fork_workspace(&src, name) {
                    Ok(n) => {
                        events::log(&actor(&from), Some(&n), None, "workspace_forked", n.clone());
                        self.persist();
                        ok(json!({"workspace": n}))
                    }
                    Err(e) => err(e.to_string()),
                }
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
                ok(json!({
                    "principal": principal,
                    "session": session,
                    "workspace": scope,
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
        // Supersede prior open task on this pane.
        if let Some(old) = self.active_tasks.remove(slug) {
            if let Some(t) = self.tasks.get_mut(&old) {
                if t.status == "open" {
                    t.status = "cancelled".into();
                    t.finished_ms = Some(now_ms());
                }
            }
        }
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
        if let Some(t) = self.tasks.get_mut(&tid) {
            if t.pane != slug && want.is_some() {
                // Explicit task id for wrong pane — ignore quietly.
                return None;
            }
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
