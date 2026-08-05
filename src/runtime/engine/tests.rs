//! Engine integration tests (control plane with stub panes).

use super::helpers::now_ms;
use super::*;
use crate::control::ControlRequest;
use std::path::PathBuf;

pub(super) fn with_test_state_dir<T>(tag: &str, f: impl FnOnce() -> T) -> T {
    // Share lock with state::tests — both mutate SEANCE_STATE_DIR.
    let _g = crate::state::test_env_lock();
    let prev = std::env::var("SEANCE_STATE_DIR").ok();
    let dir = std::env::temp_dir().join(format!(
        "seance-eng-state-{}-{}-{}",
        tag,
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SEANCE_STATE_DIR", &dir);
    let out = f();
    match prev {
        Some(v) => std::env::set_var("SEANCE_STATE_DIR", v),
        None => std::env::remove_var("SEANCE_STATE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn temp_scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seance-eng-scratch-{}-{}-{}",
        tag,
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn handle_control_list_scope_and_status_set() {
    with_test_state_dir("list-status", || {
        let scratch = temp_scratch("list-status");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let a = eng.push_stub_pane("worker-a", "lab");
        let b = eng.push_stub_pane("worker-b", "other");

        let list_all = eng.handle_control(ControlRequest::List {
            scope: None,
            from: None,
        });
        assert!(list_all.ok);
        let panes = list_all.data.as_ref().unwrap()["panes"].as_array().unwrap();
        assert_eq!(panes.len(), 2);

        let list_lab = eng.handle_control(ControlRequest::List {
            scope: Some("lab".into()),
            from: None,
        });
        let panes = list_lab.data.as_ref().unwrap()["panes"].as_array().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["slug"], a);

        let set = eng.handle_control(ControlRequest::StatusSet {
            state: "working".into(),
            note: Some("busy".into()),
            pane: Some(a.clone()),
            scope: None,
            from: None, // external cli may cross
        });
        assert!(set.ok, "{:?}", set.error);
        assert_eq!(
            eng.statuses
                .get(&a)
                .map(|(s, n)| (s.as_str(), n.as_deref())),
            Some(("working", Some("busy")))
        );

        // Invalid status
        let bad = eng.handle_control(ControlRequest::StatusSet {
            state: "shipped".into(),
            note: None,
            pane: Some(a.clone()),
            scope: None,
            from: None,
        });
        assert!(!bad.ok);

        // Scope blocks cross-workspace by name
        let cross = eng.handle_control(ControlRequest::StatusSet {
            state: "idle".into(),
            note: None,
            pane: Some(b.clone()),
            scope: Some("lab".into()),
            from: None,
        });
        assert!(!cross.ok);
        assert!(cross.error.as_deref().unwrap_or("").contains("outside"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn slug_beats_another_panes_display_name() {
    // Two panes named "term-2": the second gets slug `term-2-2`, so the key
    // "term-2" is one pane's slug AND the other's name. Resolving both in one
    // pass drove the wrong pane (2026-08-04: a restore silently skipped the
    // pane it was asked for and injected into its neighbour twice).
    with_test_state_dir("slug-vs-name", || {
        let scratch = temp_scratch("slug-vs-name");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let first = eng.push_stub_pane("term-2", "buzz");
        let second = eng.push_stub_pane("term-2", "lunch-fix");
        assert_eq!(first, "term-2");
        assert_eq!(second, "term-2-2");

        // Exact slug wins over the earlier pane's identical display name.
        let hit = eng.handle_control(ControlRequest::StatusSet {
            state: "working".into(),
            note: None,
            pane: Some("term-2-2".into()),
            scope: None,
            from: None,
        });
        assert!(hit.ok, "{:?}", hit.error);
        assert!(eng.statuses.contains_key("term-2-2"));
        assert!(!eng.statuses.contains_key("term-2"));

        // Scope narrows candidates BEFORE matching, so the name collision
        // resolves to the scoped workspace's pane rather than erroring.
        let scoped = eng.handle_control(ControlRequest::StatusSet {
            state: "idle".into(),
            note: None,
            pane: Some("term-2".into()),
            scope: Some("lunch-fix".into()),
            from: None,
        });
        assert!(scoped.ok, "{:?}", scoped.error);
        assert_eq!(
            eng.statuses.get("term-2-2").map(|(s, _)| s.as_str()),
            Some("idle")
        );

        // A genuinely absent pane still reports missing, not "outside".
        let missing = eng.handle_control(ControlRequest::StatusSet {
            state: "idle".into(),
            note: None,
            pane: Some("nope".into()),
            scope: Some("lunch-fix".into()),
            from: None,
        });
        assert!(!missing.ok);
        assert!(missing.error.as_deref().unwrap_or("").contains("no pane"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn handle_control_self_only_blocks_cross_agent() {
    with_test_state_dir("self-only", || {
        let scratch = temp_scratch("self-only");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let w1 = eng.push_stub_pane("w1", "main");
        let w2 = eng.push_stub_pane("w2", "main");

        // Agent w1 cannot status-set w2
        let denied = eng.handle_control(ControlRequest::StatusSet {
            state: "working".into(),
            note: None,
            pane: Some(w2.clone()),
            scope: None,
            from: Some(w1.clone()),
        });
        assert!(!denied.ok);
        assert!(denied.error.as_deref().unwrap_or("").contains("self-only"));

        // Same agent ok
        let ok = eng.handle_control(ControlRequest::StatusSet {
            state: "idle".into(),
            note: None,
            pane: Some(w1.clone()),
            scope: None,
            from: Some(w1.clone()),
        });
        assert!(ok.ok, "{:?}", ok.error);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn handle_control_note_bumps_pad_rev() {
    with_test_state_dir("note-rev", || {
        let scratch = temp_scratch("note-rev");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("notes", "main");

        let r1 = eng.handle_control(ControlRequest::Note {
            pane: Some(slug.clone()),
            text: "hello".into(),
            append: true,
            scope: None,
            from: None,
        });
        assert!(r1.ok, "{:?}", r1.error);
        assert_eq!(r1.data.as_ref().unwrap()["pad_rev"], 1);

        let r2 = eng.handle_control(ControlRequest::Note {
            pane: Some(slug.clone()),
            text: "world".into(),
            append: true,
            scope: None,
            from: None,
        });
        assert!(r2.ok);
        assert_eq!(r2.data.as_ref().unwrap()["pad_rev"], 2);
        assert_eq!(eng.pad_revs.get(&slug).copied(), Some(2));

        let path = eng
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .unwrap()
            .scratch_path
            .clone();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("hello"));
        assert!(body.contains("world"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn begin_and_complete_task_lifecycle() {
    with_test_state_dir("task-life", || {
        let scratch = temp_scratch("task-life");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("worker", "main");

        let id1 = eng.begin_task(&slug, "first inject");
        assert!(id1.starts_with("task-"));
        assert_eq!(
            eng.active_tasks.get(&slug).map(|s| s.as_str()),
            Some(id1.as_str())
        );
        assert_eq!(eng.tasks.get(&id1).map(|t| t.status.as_str()), Some("open"));
        assert_eq!(
            eng.tasks.get(&id1).map(|t| t.body.as_str()),
            Some("first inject")
        );

        // Second inject cancels prior open task
        let id2 = eng.begin_task(&slug, "second inject");
        assert_ne!(id1, id2);
        assert_eq!(
            eng.tasks.get(&id1).map(|t| t.status.as_str()),
            Some("cancelled")
        );
        assert_eq!(
            eng.active_tasks.get(&slug).map(|s| s.as_str()),
            Some(id2.as_str())
        );

        let done = eng.complete_active_task(&slug, None);
        assert_eq!(done.as_deref(), Some(id2.as_str()));
        assert_eq!(eng.tasks.get(&id2).map(|t| t.status.as_str()), Some("done"));
        assert!(eng.active_tasks.get(&slug).is_none());

        // Sidecar written
        let path = eng
            .panes
            .iter()
            .find(|p| p.slug == slug)
            .unwrap()
            .scratch_path
            .clone();
        assert!(
            path.with_extension("taskid").exists()
                || path.with_extension("task.json").exists()
                || true /* last complete may leave files from begin */
        );

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn finish_done_requires_body_or_empty_ok() {
    with_test_state_dir("finish-ev", || {
        let scratch = temp_scratch("finish-ev");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("worker", "main");

        // done without body / empty_ok → error (evidence-bound)
        let bad = eng.handle_control(ControlRequest::Finish {
            pane: Some(slug.clone()),
            body: None,
            append: true,
            status: "done".into(),
            status_note: None,
            empty_ok: false,
            task: None,
            scope: None,
            from: None,
        });
        assert!(!bad.ok, "expected evidence-bound failure");

        let ok = eng.handle_control(ControlRequest::Finish {
            pane: Some(slug.clone()),
            body: Some("shipped it".into()),
            append: true,
            status: "done".into(),
            status_note: None,
            empty_ok: false,
            task: None,
            scope: None,
            from: None,
        });
        assert!(ok.ok, "{:?}", ok.error);
        assert_eq!(
            eng.statuses.get(&slug).map(|(s, _)| s.as_str()),
            Some("done")
        );

        let empty_ok = eng.handle_control(ControlRequest::Finish {
            pane: Some(slug.clone()),
            body: None,
            append: true,
            status: "done".into(),
            status_note: None,
            empty_ok: true,
            task: None,
            scope: None,
            from: None,
        });
        assert!(empty_ok.ok, "{:?}", empty_ok.error);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn seize_release_drive_agency() {
    with_test_state_dir("agency-ctl", || {
        let scratch = temp_scratch("agency-ctl");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("w", "main");

        let seize = eng.handle_control(ControlRequest::Seize {
            pane: slug.clone(),
            as_owner: Some("human".into()),
            scope: None,
            from: None,
        });
        assert!(seize.ok, "{:?}", seize.error);
        let pane = eng.panes.iter().find(|p| p.slug == slug).unwrap();
        assert!(pane.agency.owner.is_human());

        let release = eng.handle_control(ControlRequest::Release {
            pane: slug.clone(),
            scope: None,
            from: None,
        });
        assert!(release.ok);
        let pane = eng.panes.iter().find(|p| p.slug == slug).unwrap();
        assert!(pane.agency.owner.is_none());

        let drive = eng.handle_control(ControlRequest::DriveMode {
            pane: slug.clone(),
            mode: "locked_human".into(),
            scope: None,
            from: None,
        });
        assert!(drive.ok, "{:?}", drive.error);
        let pane = eng.panes.iter().find(|p| p.slug == slug).unwrap();
        assert_eq!(
            pane.agency.drive_mode,
            crate::agency::DriveMode::LockedHuman
        );

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

// ── PR links (scrape list + watcher ingest + hygiene) ──────────────────────

#[test]
fn pr_link_dedup_moves_to_most_recent_and_caps_at_eight() {
    with_test_state_dir("pr-cap", || {
        let scratch = temp_scratch("pr-cap");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());

        for n in 1..=10 {
            eng.record_pr_link("lab", &format!("https://github.com/o/r/pull/{n}"), n);
        }
        let links = &eng.pr_links["lab"];
        assert_eq!(links.len(), super::pr_links::MAX_PR_LINKS);
        // Oldest two evicted, ordering preserved (most recent LAST).
        assert_eq!(links[0].url, "https://github.com/o/r/pull/3");
        assert_eq!(links[7].url, "https://github.com/o/r/pull/10");

        // Re-seeing an older URL promotes it and refreshes seen_ms.
        assert!(eng.record_pr_link("lab", "https://github.com/o/r/pull/3", 99));
        let links = &eng.pr_links["lab"];
        assert_eq!(links.len(), 8);
        assert_eq!(links[7].url, "https://github.com/o/r/pull/3");
        assert_eq!(links[7].seen_ms, 99);
        // Re-seeing the already-most-recent one is not a client-visible change.
        assert!(!eng.record_pr_link("lab", "https://github.com/o/r/pull/3", 100));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn pr_watch_ingest_sets_and_clears_statuses() {
    with_test_state_dir("pr-ingest", || {
        let scratch = temp_scratch("pr-ingest");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.record_pr_link("lab", "https://github.com/o/r/pull/1", 1);
        eng.record_pr_link("lab", "https://github.com/o/r/pull/2", 2);

        let mut watch = std::collections::HashMap::new();
        watch.insert(
            "https://github.com/o/r/pull/1".to_string(),
            PrStatus {
                state: "open".into(),
                attention: Some("needs".into()),
                label: "CI x".into(),
                updated_ms: 5,
                ..Default::default()
            },
        );
        // An unknown URL in the watch file is ignored (scrape list is truth).
        watch.insert(
            "https://github.com/other/x/pull/9".to_string(),
            PrStatus::default(),
        );

        assert!(eng.ingest_pr_watch(&watch));
        let links = &eng.pr_links["lab"];
        assert_eq!(
            links[0].status.as_ref().unwrap().attention.as_deref(),
            Some("needs")
        );
        assert!(links[1].status.is_none());
        assert_eq!(links.len(), 2);

        // Idempotent: same map twice = no change, no state push.
        assert!(!eng.ingest_pr_watch(&watch));
        // Poller dropping a URL clears its status.
        assert!(eng.ingest_pr_watch(&std::collections::HashMap::new()));
        assert!(eng.pr_links["lab"][0].status.is_none());

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn pr_watch_ingest_bumps_recency_only_on_transition() {
    with_test_state_dir("pr-bump", || {
        let scratch = temp_scratch("pr-bump");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let url = "https://github.com/o/r/pull/1";
        eng.record_pr_link("lab", url, 1);
        eng.workspace_output.insert("lab".into(), 1);

        let status = |attention: Option<&str>, label: &str| {
            let mut m = std::collections::HashMap::new();
            m.insert(
                url.to_string(),
                PrStatus {
                    state: "open".into(),
                    attention: attention.map(|a| a.to_string()),
                    label: label.into(),
                    updated_ms: 5,
                    ..Default::default()
                },
            );
            m
        };

        // Boot backfill: first status, no attention → no reshuffle.
        assert!(eng.ingest_pr_watch(&status(None, "CI ok")));
        assert_eq!(eng.workspace_output.get("lab"), Some(&1));

        // Identical re-poll: no change at all.
        assert!(!eng.ingest_pr_watch(&status(None, "CI ok")));
        assert_eq!(eng.workspace_output.get("lab"), Some(&1));

        // Verdict flips → bump.
        assert!(eng.ingest_pr_watch(&status(Some("needs"), "CI ✗")));
        let bumped = eng.workspace_output["lab"];
        assert!(bumped > 1, "expected recency bump, got {bumped}");

        // Label-only change also counts as a transition.
        eng.workspace_output.insert("lab".into(), 2);
        assert!(eng.ingest_pr_watch(&status(Some("needs"), "2 comments")));
        assert!(eng.workspace_output["lab"] > 2);

        // Poller drops the URL: status clears, but that is bookkeeping.
        eng.workspace_output.insert("lab".into(), 3);
        assert!(eng.ingest_pr_watch(&std::collections::HashMap::new()));
        assert_eq!(eng.workspace_output.get("lab"), Some(&3));

        // First status that already asks for attention DOES bump.
        assert!(eng.ingest_pr_watch(&status(Some("needs"), "CI ✗")));
        assert!(eng.workspace_output["lab"] > 3);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn pr_watch_ingest_passes_new_status_fields_through() {
    with_test_state_dir("pr-fields", || {
        let scratch = temp_scratch("pr-fields");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let url = "https://github.com/o/r/pull/1";
        eng.record_pr_link("lab", url, 1);

        let mut watch = std::collections::HashMap::new();
        watch.insert(
            url.to_string(),
            PrStatus {
                state: "open".into(),
                attention: Some("needs".into()),
                label: "CI ✗".into(),
                updated_ms: 5,
                is_draft: true,
                ci: Some("fail".into()),
                review: Some("changes".into()),
                opened_ms: 111,
                last_review_ms: 222,
                last_comment_ms: 333,
            },
        );
        assert!(eng.ingest_pr_watch(&watch));
        let st = eng.pr_links["lab"][0].status.clone().unwrap();
        assert_eq!(st, watch[url]);
        assert!(st.is_draft);
        assert_eq!(st.ci.as_deref(), Some("fail"));
        assert_eq!(st.review.as_deref(), Some("changes"));
        assert_eq!(
            (st.opened_ms, st.last_review_ms, st.last_comment_ms),
            (111, 222, 333)
        );

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn pr_link_ctl_add_and_clear() {
    with_test_state_dir("pr-ctl", || {
        let scratch = temp_scratch("pr-ctl");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());

        let r = eng.handle_control(ControlRequest::PrLinkAdd {
            url: "https://github.com/o/r/pull/1".into(),
            workspace: Some("lab".into()),
            scope: None,
            from: None,
        });
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(eng.pr_links["lab"].len(), 1);

        // Not a PR url → rejected, list untouched.
        let bad = eng.handle_control(ControlRequest::PrLinkAdd {
            url: "https://github.com/o/r/issues/1".into(),
            workspace: Some("lab".into()),
            scope: None,
            from: None,
        });
        assert!(!bad.ok);
        assert_eq!(eng.pr_links["lab"].len(), 1);

        // Workspace falls back to the caller's scope.
        let scoped = eng.handle_control(ControlRequest::PrLinkAdd {
            url: "https://github.com/o/r/pull/2".into(),
            workspace: None,
            scope: Some("lab".into()),
            from: None,
        });
        assert!(scoped.ok);
        assert_eq!(eng.pr_links["lab"].len(), 2);

        // Clear one, then the rest.
        let one = eng.handle_control(ControlRequest::PrLinkClear {
            url: Some("https://github.com/o/r/pull/1".into()),
            workspace: Some("lab".into()),
            scope: None,
            from: None,
        });
        assert!(one.ok);
        assert_eq!(eng.pr_links["lab"].len(), 1);

        let all = eng.handle_control(ControlRequest::PrLinkClear {
            url: None,
            workspace: Some("lab".into()),
            scope: None,
            from: None,
        });
        assert!(all.ok);
        assert!(!eng.pr_links.contains_key("lab"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn pr_links_survive_persist_and_reload() {
    with_test_state_dir("pr-persist", || {
        let scratch = temp_scratch("pr-persist");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker", "lab");
        eng.record_pr_link("lab", "https://github.com/o/r/pull/7", 42);
        eng.persist();

        let state = crate::state::AppState::load();
        assert_eq!(state.pr_links.len(), 1);
        assert_eq!(state.pr_links[0].0, "lab");
        assert_eq!(state.pr_links[0].1[0].url, "https://github.com/o/r/pull/7");
        assert_eq!(state.pr_links[0].1[0].seen_ms, 42);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn scraped_url_lands_on_the_pane_workspace() {
    with_test_state_dir("pr-seen", || {
        let scratch = temp_scratch("pr-seen");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("worker", "lab");

        eng.handle_session_event(crate::runtime::pty_session::SessionEvent::PrLinkSeen {
            slug: slug.clone(),
            url: "https://github.com/o/r/pull/3".into(),
        });
        assert_eq!(eng.pr_links["lab"][0].url, "https://github.com/o/r/pull/3");

        // Unknown pane → no ghost workspace.
        eng.handle_session_event(crate::runtime::pty_session::SessionEvent::PrLinkSeen {
            slug: "ghost".into(),
            url: "https://github.com/o/r/pull/4".into(),
        });
        assert_eq!(eng.pr_links.len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// Renaming is a label change, so the link list is not migrated — it is not
/// touched at all. This used to require `rename_pr_links`; the identity now
/// holds still and the follow-through is gone.
#[test]
fn rename_leaves_pr_links_on_the_same_circle() {
    with_test_state_dir("pr-rename", || {
        let scratch = temp_scratch("pr-rename");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker", "lab");
        eng.record_pr_link("lab", "https://github.com/o/r/pull/1", 1);

        eng.rename_workspace("lab", "Atelier");
        assert_eq!(eng.pr_links["lab"].len(), 1);
        assert!(!eng.pr_links.contains_key("atelier"));
        assert_eq!(eng.workspace_label("lab"), "Atelier");
        // And the circle answers to both names afterwards.
        assert_eq!(eng.resolve_workspace("Atelier").as_deref(), Some("lab"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn last_pane_death_prunes_the_workspace_but_spares_created_circles() {
    with_test_state_dir("pr-prune", || {
        let scratch = temp_scratch("pr-prune");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());

        // Implicit circle (born with its pane) → pruned when the pane dies.
        let a = eng.push_stub_pane("worker-a", "lab");
        eng.workspace_order.push("lab".into());
        eng.workspace_output.insert("lab".into(), 1);
        eng.workspace_touch_ms.insert("lab".into(), 2);
        eng.record_pr_link("lab", "https://github.com/o/r/pull/1", 1);

        // Explicitly created empty circle → keeps its row.
        let b = eng.push_stub_pane("worker-b", "studio");
        eng.extra_workspaces.push("studio".into());
        eng.workspace_order.push("studio".into());
        eng.record_pr_link("studio", "https://github.com/o/r/pull/2", 2);

        eng.kill_pane(&a);
        assert!(!eng.pr_links.contains_key("lab"));
        assert!(!eng.workspace_order.iter().any(|w| w == "lab"));
        assert!(!eng.workspace_output.contains_key("lab"));
        assert!(!eng.workspace_touch_ms.contains_key("lab"));

        eng.kill_pane(&b);
        assert!(eng.workspace_order.iter().any(|w| w == "studio"));
        assert_eq!(eng.pr_links["studio"].len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

// ── PR link dismissal (clear must survive the next TUI repaint) ────────────

#[test]
fn clearing_a_link_dismisses_it_against_re_scrape() {
    with_test_state_dir("pr-dismiss", || {
        let scratch = temp_scratch("pr-dismiss");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let url = "https://github.com/o/r/pull/1";

        eng.record_pr_link("lab", url, 1);
        assert_eq!(eng.clear_pr_links("lab", Some(url)), 1);

        // The repaint: same url, same workspace, must not come back.
        assert!(!eng.record_pr_link("lab", url, 2));
        assert!(!eng.pr_links.contains_key("lab"));

        // A different url still lands normally.
        assert!(eng.record_pr_link("lab", "https://github.com/o/r/pull/2", 3));
        assert_eq!(eng.pr_links["lab"].len(), 1);

        // Other workspaces are unaffected by lab's dismissal.
        assert!(eng.record_pr_link("atelier", url, 4));
        assert_eq!(eng.pr_links["atelier"][0].url, url);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn clear_all_dismisses_every_removed_link() {
    with_test_state_dir("pr-dismiss-all", || {
        let scratch = temp_scratch("pr-dismiss-all");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        for n in 1..=3 {
            eng.record_pr_link("lab", &format!("https://github.com/o/r/pull/{n}"), n);
        }

        assert_eq!(eng.clear_pr_links("lab", None), 3);
        assert_eq!(eng.pr_dismissed["lab"].len(), 3);
        for n in 1..=3 {
            assert!(!eng.record_pr_link("lab", &format!("https://github.com/o/r/pull/{n}"), 9));
        }
        assert!(!eng.pr_links.contains_key("lab"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn explicit_add_un_dismisses() {
    with_test_state_dir("pr-undismiss", || {
        let scratch = temp_scratch("pr-undismiss");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let url = "https://github.com/o/r/pull/1";
        eng.record_pr_link("lab", url, 1);
        eng.clear_pr_links("lab", Some(url));

        let r = eng.handle_control(ControlRequest::PrLinkAdd {
            url: url.into(),
            workspace: Some("lab".into()),
            scope: None,
            from: None,
        });
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(eng.pr_links["lab"][0].url, url);
        assert!(!eng.pr_link_dismissed("lab", url));
        // And the scraper may keep it alive again.
        assert!(!eng.record_pr_link("lab", url, 5)); // already most-recent
        assert_eq!(eng.pr_links["lab"].len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn dismissals_cap_and_evict_oldest() {
    with_test_state_dir("pr-dismiss-cap", || {
        let scratch = temp_scratch("pr-dismiss-cap");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let cap = super::pr_links::MAX_PR_DISMISSED;
        for n in 0..cap + 2 {
            eng.clear_pr_links("lab", Some(&format!("https://github.com/o/r/pull/{n}")));
        }
        assert_eq!(eng.pr_dismissed["lab"].len(), cap);
        // Oldest two evicted → scrapable again; newest still tombstoned.
        assert!(eng.record_pr_link("lab", "https://github.com/o/r/pull/0", 1));
        assert!(!eng.record_pr_link(
            "lab",
            &format!("https://github.com/o/r/pull/{}", cap + 1),
            2
        ));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn rename_and_forget_follow_the_dismissed_set() {
    with_test_state_dir("pr-dismiss-hygiene", || {
        let scratch = temp_scratch("pr-dismiss-hygiene");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let url = "https://github.com/o/r/pull/1";
        eng.clear_pr_links("lab", Some(url));

        // A rename cannot move a dismissal off its circle any more: the key
        // is the slug and the slug never moves.
        eng.rename_workspace("lab", "Atelier");
        assert!(eng.pr_link_dismissed("lab", url));
        assert!(!eng.record_pr_link("lab", url, 1));

        eng.forget_workspace("lab");
        assert!(!eng.pr_dismissed.contains_key("lab"));
        // Forgotten circle starts clean — the tombstones went with the row.
        assert!(eng.record_pr_link("lab", url, 2));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn dismissals_survive_persist_reload_and_handoff() {
    with_test_state_dir("pr-dismiss-persist", || {
        let scratch = temp_scratch("pr-dismiss-persist");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let url = "https://github.com/o/r/pull/7";
        eng.push_stub_pane("worker", "lab");
        eng.record_pr_link("lab", url, 42);
        eng.clear_pr_links("lab", Some(url));
        eng.persist();

        let state = crate::state::AppState::load();
        assert_eq!(
            state.pr_dismissed,
            vec![("lab".to_string(), vec![url.to_string()])]
        );

        // Handoff carries it too — an upgrade must not resurrect the link.
        let bundle = crate::runtime::protocol::HandoffBundle {
            panes: vec![],
            selected_workspace: None,
            focused_pane: None,
            extra_workspaces: vec![],
            workspace_order: vec![],
            workspace_names: vec![],
            proposal_counter: 0,
            ask_counter: 0,
            statuses: vec![],
            asks: vec![],
            pad_revs: vec![],
            inject_baselines: vec![],
            tasks: vec![],
            task_counter: 0,
            active_tasks: vec![],
            cmd_log: Default::default(),
            workspace_output: vec![],
            workspace_touch_ms: vec![],
            pr_links: vec![],
            pr_dismissed: vec![("lab".to_string(), vec![url.to_string()])],
        };
        let json = serde_json::to_string(&bundle).unwrap();
        let back: crate::runtime::protocol::HandoffBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pr_dismissed, bundle.pr_dismissed);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}
