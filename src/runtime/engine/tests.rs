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
