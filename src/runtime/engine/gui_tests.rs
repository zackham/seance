//! Hermetic `handle_gui` tests: multi-window Attach / Transfer / Collect /
//! Overview dispatch driven through a fake `GuiConn` (an in-memory mpsc channel
//! registered via `register_gui`). No real sockets, no PTYs (stub panes only),
//! `SEANCE_STATE_DIR` guarded by `test_env_lock` via `with_test_state_dir`.

use super::helpers::now_ms;
use super::tests::with_test_state_dir;
use super::*;
use crate::runtime::protocol::{GuiEvent, GuiRequest};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

fn temp_scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seance-gui-scratch-{}-{}-{}",
        tag,
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fake GUI window: a registered mpsc receiver we can drain and inspect.
/// Keeping the `Receiver` alive is what makes `prune_dead_guis` treat the
/// window as live (it liveness-probes via `tx.send(Pong)`).
struct FakeGui {
    id: String,
    rx: Receiver<GuiEvent>,
}

impl FakeGui {
    fn attach_to(eng: &mut Engine) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let id = eng.register_gui(tx);
        FakeGui { id, rx }
    }

    /// Drain everything queued so far, dropping the `Pong` liveness probes that
    /// `push_state_to_all` injects on every broadcast.
    fn drain(&self) -> Vec<GuiEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            if !matches!(ev, GuiEvent::Pong) {
                out.push(ev);
            }
        }
        out
    }

    /// The most recent `State` event pushed to this window (after draining).
    fn last_state(&self) -> Option<StateSnapshot> {
        self.drain()
            .into_iter()
            .rev()
            .find_map(StateSnapshot::from_event)
    }
}

/// Flattened copy of a `GuiEvent::State` payload for ergonomic assertions.
struct StateSnapshot {
    selected_workspace: Option<String>,
    workspace_order: Vec<String>,
    panes: Vec<String>,
    foreign: Vec<(String, String)>, // (workspace, owning window)
    window_id: Option<String>,
    windows: Vec<(String, usize)>, // (window id, workspace_count)
}

impl StateSnapshot {
    fn from_event(ev: GuiEvent) -> Option<Self> {
        match ev {
            GuiEvent::State {
                selected_workspace,
                workspace_order,
                panes,
                foreign_workspaces,
                window_id,
                windows,
                ..
            } => Some(StateSnapshot {
                selected_workspace,
                workspace_order,
                panes: panes.into_iter().map(|p| p.slug).collect(),
                foreign: foreign_workspaces
                    .into_iter()
                    .map(|f| (f.workspace, f.window_id))
                    .collect(),
                window_id,
                windows: windows
                    .into_iter()
                    .map(|w| (w.id, w.workspace_count))
                    .collect(),
            }),
            _ => None,
        }
    }

    fn owns_ws(&self, ws: &str) -> bool {
        self.workspace_order.iter().any(|w| w == ws)
    }
}

/// Pull the `State` out of the `Some(GuiEvent)` returned by an Attach.
fn state_of(ev: Option<GuiEvent>) -> StateSnapshot {
    StateSnapshot::from_event(ev.expect("attach returns Some(State)"))
        .expect("attach returns a State event")
}

#[test]
fn attach_normal_assigns_workspaces_to_window() {
    with_test_state_dir("gui-attach", || {
        let scratch = temp_scratch("gui-attach");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g = FakeGui::attach_to(&mut eng);
        let st = state_of(eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g.id,
        ));

        // Sole window vacuums every known circle.
        assert_eq!(st.window_id.as_deref(), Some(g.id.as_str()));
        assert!(st.owns_ws("lab"), "order={:?}", st.workspace_order);
        assert!(st.owns_ws("cadence"));
        // Selection defaults to first owned workspace; both panes visible.
        assert!(st.selected_workspace.is_some());
        assert_eq!(st.panes.len(), 2);
        // No foreign workspaces — this is the only window.
        assert!(st.foreign.is_empty());
        assert_eq!(st.windows.len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn attach_empty_second_window_starts_blank_with_foreign() {
    with_test_state_dir("gui-attach-empty", || {
        let scratch = temp_scratch("gui-attach-empty");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        // First window claims everything.
        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );

        // Second window attaches empty → owns nothing, sees g1's workspaces as foreign.
        let g2 = FakeGui::attach_to(&mut eng);
        let st = state_of(eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: true,
            },
            &g2.id,
        ));

        assert_eq!(st.window_id.as_deref(), Some(g2.id.as_str()));
        assert!(
            st.workspace_order.is_empty(),
            "empty window owns nothing, got {:?}",
            st.workspace_order
        );
        assert!(st.selected_workspace.is_none());
        assert!(st.panes.is_empty());
        // Every foreign workspace is owned by g1 (the only other window). The
        // default "main" circle from bare_for_test is claimed by g1 too, so we
        // assert membership rather than a brittle exact count.
        assert!(st.foreign.iter().all(|(_, owner)| owner == &g1.id));
        let foreign_ws: Vec<&str> = st.foreign.iter().map(|(w, _)| w.as_str()).collect();
        assert!(foreign_ws.contains(&"lab"));
        assert!(foreign_ws.contains(&"cadence"));
        // Two live windows now.
        assert_eq!(st.windows.len(), 2);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn transfer_workspace_moves_ownership_between_windows() {
    with_test_state_dir("gui-transfer", || {
        let scratch = temp_scratch("gui-transfer");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: true,
            },
            &g2.id,
        );

        // g1 pushes "cadence" to g2.
        let ack = eng
            .handle_gui(
                GuiRequest::TransferWorkspace {
                    workspace: "cadence".into(),
                    to_window: g2.id.clone(),
                },
                &g1.id,
            )
            .expect("transfer acks");
        match ack {
            GuiEvent::Ack { ok, .. } => assert!(ok),
            other => panic!("expected Ack, got {other:?}"),
        }

        // Ownership moved.
        assert_eq!(
            eng.workspace_window.get("cadence").map(|s| s.as_str()),
            Some(g2.id.as_str())
        );
        assert_eq!(
            eng.workspace_window.get("lab").map(|s| s.as_str()),
            Some(g1.id.as_str())
        );

        // Both windows' State reflects the move (State was pushed to all).
        let s1 = g1.last_state().expect("g1 got State");
        let s2 = g2.last_state().expect("g2 got State");
        assert!(s1.owns_ws("lab"));
        assert!(!s1.owns_ws("cadence"));
        // g1 now sees cadence as foreign owned by g2.
        assert!(s1
            .foreign
            .iter()
            .any(|(w, o)| w == "cadence" && o == &g2.id));
        assert!(s2.owns_ws("cadence"));
        // g2's selection follows the transferred workspace.
        assert_eq!(s2.selected_workspace.as_deref(), Some("cadence"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn transfer_to_unknown_window_is_rejected() {
    with_test_state_dir("gui-transfer-bad", || {
        let scratch = temp_scratch("gui-transfer-bad");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );

        let ack = eng
            .handle_gui(
                GuiRequest::TransferWorkspace {
                    workspace: "lab".into(),
                    to_window: "w-nonexistent".into(),
                },
                &g1.id,
            )
            .expect("transfer returns Ack");
        match ack {
            GuiEvent::Ack { ok, error, .. } => {
                assert!(!ok);
                assert!(error.unwrap_or_default().contains("unknown window"));
            }
            other => panic!("expected Ack, got {other:?}"),
        }
        // Ownership unchanged.
        assert_eq!(
            eng.workspace_window.get("lab").map(|s| s.as_str()),
            Some(g1.id.as_str())
        );

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn collect_all_pulls_every_workspace_to_requesting_window() {
    with_test_state_dir("gui-collect", || {
        let scratch = temp_scratch("gui-collect");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");
        eng.push_stub_pane("worker-c", "notes");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: true,
            },
            &g2.id,
        );
        // Give g2 something first so "collect" has to pull it back.
        let _ = eng.handle_gui(
            GuiRequest::TransferWorkspace {
                workspace: "notes".into(),
                to_window: g2.id.clone(),
            },
            &g1.id,
        );
        assert_eq!(
            eng.workspace_window.get("notes").map(|s| s.as_str()),
            Some(g2.id.as_str())
        );

        // g1 collects all.
        let ack = eng
            .handle_gui(GuiRequest::CollectAll, &g1.id)
            .expect("collect acks");
        match ack {
            GuiEvent::Ack { ok, .. } => assert!(ok),
            other => panic!("expected Ack, got {other:?}"),
        }

        // Every workspace now owned by g1.
        for ws in ["lab", "cadence", "notes"] {
            assert_eq!(
                eng.workspace_window.get(ws).map(|s| s.as_str()),
                Some(g1.id.as_str()),
                "ws {ws} should belong to g1"
            );
        }
        // g1 State owns all three; g2 State is empty with them foreign.
        let s1 = g1.last_state().expect("g1 State");
        assert!(s1.owns_ws("lab") && s1.owns_ws("cadence") && s1.owns_ws("notes"));
        assert_eq!(s1.panes.len(), 3);
        let s2 = g2.last_state().expect("g2 State");
        assert!(s2.workspace_order.is_empty());
        assert!(s2.selected_workspace.is_none());
        // The three pane workspaces (plus the default "main") are all foreign to g2.
        for ws in ["lab", "cadence", "notes"] {
            assert!(
                s2.foreign.iter().any(|(w, o)| w == ws && o == &g1.id),
                "{ws} should be foreign, owned by g1"
            );
        }

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn set_overview_flips_flag_without_error() {
    with_test_state_dir("gui-overview", || {
        let scratch = temp_scratch("gui-overview");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );

        // Enabling overview returns no event (fire-and-forget) and doesn't panic
        // even with a session-less stub pane (the FULL-flush loop skips it).
        let r = eng.handle_gui(GuiRequest::SetOverview { enabled: true }, &g1.id);
        assert!(r.is_none());
        // Disabling is likewise a clean no-op-return.
        let r = eng.handle_gui(GuiRequest::SetOverview { enabled: false }, &g1.id);
        assert!(r.is_none());

        // Overview against an unknown window id must not panic.
        let r = eng.handle_gui(GuiRequest::SetOverview { enabled: true }, "w-nope");
        assert!(r.is_none());

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn bye_releases_workspaces_to_surviving_window() {
    with_test_state_dir("gui-bye", || {
        let scratch = temp_scratch("gui-bye");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: true,
            },
            &g2.id,
        );
        // Hand "cadence" to g2 so it owns something to release on Bye.
        let _ = eng.handle_gui(
            GuiRequest::TransferWorkspace {
                workspace: "cadence".into(),
                to_window: g2.id.clone(),
            },
            &g1.id,
        );
        assert_eq!(
            eng.workspace_window.get("cadence").map(|s| s.as_str()),
            Some(g2.id.as_str())
        );

        // g2 closes — its workspace must reassign to the surviving g1, never orphan.
        let r = eng.handle_gui(GuiRequest::Bye, &g2.id);
        assert!(r.is_none());
        assert_eq!(
            eng.workspace_window.get("cadence").map(|s| s.as_str()),
            Some(g1.id.as_str()),
            "cadence should fall back to the survivor"
        );
        // g2's connection is gone.
        assert!(!eng.has_gui_window(&g2.id));
        // g1 now owns both.
        let s1 = g1.last_state().expect("g1 State after Bye");
        assert!(s1.owns_ws("lab") && s1.owns_ws("cadence"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn last_window_close_orphans_then_reattach_collects() {
    with_test_state_dir("gui-lastclose", || {
        let scratch = temp_scratch("gui-lastclose");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );
        assert_eq!(
            eng.workspace_window.get("lab").map(|s| s.as_str()),
            Some(g1.id.as_str())
        );

        // Sole window closes → workspace map cleared (truly orphaned, no owner).
        let _ = eng.handle_gui(GuiRequest::Bye, &g1.id);
        assert!(eng.gui_conns.is_empty());
        assert!(
            eng.workspace_window.get("lab").is_none(),
            "last close should orphan the map entry"
        );

        // A fresh window re-attaches and vacuums everything back.
        let g2 = FakeGui::attach_to(&mut eng);
        let st = state_of(eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g2.id,
        ));
        assert!(st.owns_ws("lab"));
        assert_eq!(st.panes.len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn grid_interval_selection_is_pure_and_clockless() {
    with_test_state_dir("gui-interval", || {
        let scratch = temp_scratch("gui-interval");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let sel = eng.push_stub_pane("worker-a", "lab");
        let other = eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        // Attach + focus "lab" so it becomes the selected workspace.
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: Some("lab".into()),
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );

        // Selected workspace → ~60fps (16ms).
        assert_eq!(eng.grid_interval_ms_for(&sel), Some(16));
        // Non-selected, overview off → not streamed (None).
        assert_eq!(eng.grid_interval_ms_for(&other), None);

        // Enable overview → non-selected circles push at the ~15fps thumb rate (66ms).
        let _ = eng.handle_gui(GuiRequest::SetOverview { enabled: true }, &g1.id);
        assert_eq!(eng.grid_interval_ms_for(&sel), Some(16));
        assert_eq!(eng.grid_interval_ms_for(&other), Some(66));

        // Disable overview → back to None for the non-selected circle.
        let _ = eng.handle_gui(GuiRequest::SetOverview { enabled: false }, &g1.id);
        assert_eq!(eng.grid_interval_ms_for(&other), None);

        // Unknown pane slug → None (no panic).
        assert_eq!(eng.grid_interval_ms_for("ghost-slug"), None);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn prune_dead_guis_reassigns_dropped_window() {
    with_test_state_dir("gui-prune", || {
        let scratch = temp_scratch("gui-prune");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: false,
            },
            &g1.id,
        );
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                empty: true,
            },
            &g2.id,
        );
        let _ = eng.handle_gui(
            GuiRequest::TransferWorkspace {
                workspace: "cadence".into(),
                to_window: g2.id.clone(),
            },
            &g1.id,
        );
        let g2_id = g2.id.clone();

        // Kill g2's receiver — its send channel is now dead.
        drop(g2);
        eng.prune_dead_guis();

        // Dead window pruned; its workspace reassigned to the survivor, no panic.
        assert!(!eng.has_gui_window(&g2_id));
        assert_eq!(
            eng.workspace_window.get("cadence").map(|s| s.as_str()),
            Some(g1.id.as_str())
        );
        assert!(eng.has_gui_window(&g1.id));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}
