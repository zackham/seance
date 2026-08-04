//! Hermetic `handle_gui` tests for the SUBSCRIPTION model (0.12): Attach
//! seeding, Subscribe/Unsubscribe, auto-subscribe on select/spawn/create/fork,
//! the grid-rate matrix, and the recorder invariant. Driven through a fake
//! `GuiConn` (an in-memory mpsc channel registered via `register_gui`). No real
//! sockets, no PTYs (stub panes only), `SEANCE_STATE_DIR` guarded by
//! `test_env_lock` via `with_test_state_dir`.

use super::helpers::now_ms;
use super::tests::with_test_state_dir;
use super::*;
use crate::runtime::protocol::{GuiEvent, GuiRequest};
use crate::runtime::pty_session::SessionEvent;
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
    focused_pane: Option<String>,
    workspace_order: Vec<String>,
    subscriptions: Vec<String>,
    panes: Vec<String>,
    window_id: Option<String>,
    windows: Vec<(String, usize)>, // (window id, workspace_count)
}

impl StateSnapshot {
    fn from_event(ev: GuiEvent) -> Option<Self> {
        match ev {
            GuiEvent::State {
                selected_workspace,
                focused_pane,
                workspace_order,
                subscriptions,
                panes,
                window_id,
                windows,
                ..
            } => Some(StateSnapshot {
                selected_workspace,
                focused_pane,
                workspace_order,
                subscriptions,
                panes: panes.into_iter().map(|p| p.slug).collect(),
                window_id,
                windows: windows
                    .into_iter()
                    .map(|w| (w.id, w.workspace_count))
                    .collect(),
            }),
            _ => None,
        }
    }

    fn subscribes(&self, ws: &str) -> bool {
        self.subscriptions.iter().any(|w| w == ws)
    }

    fn knows_ws(&self, ws: &str) -> bool {
        self.workspace_order.iter().any(|w| w == ws)
    }
}

/// Pull the `State` out of the `Some(GuiEvent)` returned by an Attach.
fn state_of(ev: Option<GuiEvent>) -> StateSnapshot {
    StateSnapshot::from_event(ev.expect("attach returns Some(State)"))
        .expect("attach returns a State event")
}

/// Attach with the "seed me with everything" default (`subscriptions: None`).
fn attach_all(eng: &mut Engine, id: &str) -> StateSnapshot {
    state_of(eng.handle_gui(
        GuiRequest::Attach {
            selected_workspace: None,
            focused_pane: None,
            subscriptions: None,
        },
        id,
    ))
}

/// Attach with an explicit subscription list.
fn attach_with(eng: &mut Engine, id: &str, subs: &[&str]) -> StateSnapshot {
    state_of(eng.handle_gui(
        GuiRequest::Attach {
            selected_workspace: None,
            focused_pane: None,
            subscriptions: Some(subs.iter().map(|s| s.to_string()).collect()),
        },
        id,
    ))
}

#[test]
fn attach_without_a_list_subscribes_to_every_workspace() {
    with_test_state_dir("gui-attach", || {
        let scratch = temp_scratch("gui-attach");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g = FakeGui::attach_to(&mut eng);
        let st = attach_all(&mut eng, &g.id);

        assert_eq!(st.window_id.as_deref(), Some(g.id.as_str()));
        assert!(st.subscribes("lab"), "subs={:?}", st.subscriptions);
        assert!(st.subscribes("cadence"));
        assert!(st.selected_workspace.is_some());
        assert_eq!(st.panes.len(), 2);
        assert_eq!(st.windows.len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn attach_with_a_list_subscribes_to_the_known_intersection() {
    with_test_state_dir("gui-attach-list", || {
        let scratch = temp_scratch("gui-attach-list");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g = FakeGui::attach_to(&mut eng);
        let st = attach_with(&mut eng, &g.id, &["cadence", "ghost-circle"]);

        assert_eq!(st.subscriptions, vec!["cadence".to_string()]);
        assert_eq!(st.selected_workspace.as_deref(), Some("cadence"));
        // State is GLOBAL now: lab is still described, just not subscribed.
        assert!(st.knows_ws("lab"));
        assert_eq!(st.panes.len(), 2, "panes are global: {:?}", st.panes);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn attach_with_empty_list_is_a_blank_window() {
    with_test_state_dir("gui-attach-empty", || {
        let scratch = temp_scratch("gui-attach-empty");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &g1.id);

        let g2 = FakeGui::attach_to(&mut eng);
        let st = attach_with(&mut eng, &g2.id, &[]);

        assert_eq!(st.window_id.as_deref(), Some(g2.id.as_str()));
        assert!(st.subscriptions.is_empty(), "{:?}", st.subscriptions);
        assert!(st.selected_workspace.is_none());
        // Blank means "subscribed to nothing", NOT "told nothing" — the census
        // is what phase 2's parked list renders from.
        assert!(st.knows_ws("lab") && st.knows_ws("cadence"));
        assert_eq!(st.windows.len(), 2);
        // g1 keeps everything — a second window takes nothing away (no custody).
        let s1 = g1.last_state().expect("g1 got a roster refresh");
        assert!(s1.subscribes("lab") && s1.subscribes("cadence"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn two_windows_can_subscribe_to_the_same_workspace() {
    with_test_state_dir("gui-shared-sub", || {
        let scratch = temp_scratch("gui-shared-sub");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &g1.id);
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g2.id, &[]);

        let _ = eng.handle_gui(
            GuiRequest::Subscribe {
                workspace: "lab".into(),
            },
            &g2.id,
        );

        assert!(eng.subscriptions_of(&g1.id).contains(&"lab".to_string()));
        assert!(eng.subscriptions_of(&g2.id).contains(&"lab".to_string()));
        let s2 = g2.last_state().expect("subscribe pushes State");
        assert!(s2.subscribes("lab"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn unsubscribe_drops_the_workspace_and_moves_selection_on() {
    with_test_state_dir("gui-unsub", || {
        let scratch = temp_scratch("gui-unsub");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &g.id);
        let _ = eng.handle_gui(
            GuiRequest::SetFocus {
                pane: None,
                workspace: Some("cadence".into()),
            },
            &g.id,
        );

        let _ = eng.handle_gui(
            GuiRequest::Unsubscribe {
                workspace: "cadence".into(),
            },
            &g.id,
        );
        let st = g.last_state().expect("unsubscribe pushes State");
        assert!(!st.subscribes("cadence"));
        assert_ne!(st.selected_workspace.as_deref(), Some("cadence"));
        assert!(
            st.selected_workspace
                .as_deref()
                .is_some_and(|s| st.subscribes(s)),
            "selection must stay inside the subscription set: {st:?}",
            st = st.subscriptions
        );

        // Unsubscribing the last one leaves no selection at all.
        let remaining = st.selected_workspace.clone().unwrap();
        let _ = eng.handle_gui(
            GuiRequest::Unsubscribe {
                workspace: remaining,
            },
            &g.id,
        );
        let st = g.last_state().unwrap();
        for ws in &st.subscriptions {
            let _ = eng.handle_gui(
                GuiRequest::Unsubscribe {
                    workspace: ws.clone(),
                },
                &g.id,
            );
        }
        let st = g.last_state().unwrap();
        assert!(st.subscriptions.is_empty());
        assert!(st.selected_workspace.is_none());

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn selecting_a_parked_workspace_auto_subscribes() {
    with_test_state_dir("gui-focus-sub", || {
        let scratch = temp_scratch("gui-focus-sub");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g.id, &["lab"]);
        assert!(!eng.subscriptions_of(&g.id).contains(&"cadence".to_string()));

        let _ = eng.handle_gui(
            GuiRequest::SetFocus {
                pane: None,
                workspace: Some("cadence".into()),
            },
            &g.id,
        );
        assert!(eng.subscriptions_of(&g.id).contains(&"cadence".to_string()));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn gui_spawn_create_and_fork_auto_subscribe_the_requester() {
    with_test_state_dir("gui-spawn-sub", || {
        let scratch = temp_scratch("gui-spawn-sub");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");

        let g = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g.id, &["lab"]);

        let _ = eng.handle_gui(
            GuiRequest::CreateWorkspace {
                name: "notes".into(),
            },
            &g.id,
        );
        assert!(eng.subscriptions_of(&g.id).contains(&"notes".to_string()));

        let _ = eng.handle_gui(
            GuiRequest::ForkWorkspace {
                workspace: "lab".into(),
                name: Some("lab-fork".into()),
            },
            &g.id,
        );
        assert!(
            eng.subscriptions_of(&g.id)
                .contains(&"lab-fork".to_string()),
            "fork target must land in the requester's set: {:?}",
            eng.subscriptions_of(&g.id)
        );

        // A ctl-side spawn subscribes NOBODY (parked everywhere; GUIs badge it).
        let _ = eng.spawn(SpawnSpec {
            name: "ctl-worker".into(),
            cwd: None,
            command: None,
            workspace: Some("offstage".into()),
            tiled: true,
            resume: false,
            file: None,
        });
        assert!(!eng
            .subscriptions_of(&g.id)
            .contains(&"offstage".to_string()));

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
        let _ = attach_all(&mut eng, &g1.id);

        // Enabling overview returns no event (fire-and-forget) and doesn't panic
        // even with a session-less stub pane (the FULL-flush loop skips it).
        let r = eng.handle_gui(GuiRequest::SetOverview { enabled: true }, &g1.id);
        assert!(r.is_none());
        let r = eng.handle_gui(GuiRequest::SetOverview { enabled: false }, &g1.id);
        assert!(r.is_none());

        // Overview against an unknown window id must not panic.
        let r = eng.handle_gui(GuiRequest::SetOverview { enabled: true }, "w-nope");
        assert!(r.is_none());

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn bye_drops_the_window_without_reassigning_anything() {
    with_test_state_dir("gui-bye", || {
        let scratch = temp_scratch("gui-bye");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g1.id, &["lab"]);
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g2.id, &["cadence"]);

        let r = eng.handle_gui(GuiRequest::Bye, &g2.id);
        assert!(r.is_none());
        assert!(!eng.has_gui_window(&g2.id));
        // The survivor's subscription set is untouched — no "dump into the
        // fullest window" inheritance any more.
        assert_eq!(eng.subscriptions_of(&g1.id), vec!["lab".to_string()]);
        let s1 = g1.last_state().expect("g1 State after Bye");
        assert_eq!(s1.subscriptions, vec!["lab".to_string()]);
        // cadence still exists globally, just parked everywhere.
        assert!(s1.knows_ws("cadence"));
        assert_eq!(s1.windows.len(), 1);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn reattach_restores_last_selected_workspace() {
    with_test_state_dir("gui-restore-sel", || {
        let scratch = temp_scratch("gui-restore-sel");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");
        eng.push_stub_pane("worker-c", "notes");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &g1.id);
        let _ = eng.handle_gui(
            GuiRequest::SetFocus {
                pane: Some("worker-b".into()),
                workspace: Some("cadence".into()),
            },
            &g1.id,
        );
        assert_eq!(eng.selected_workspace.as_deref(), Some("cadence"));
        assert_eq!(eng.focused_pane.as_deref(), Some("worker-b"));

        let _ = eng.handle_gui(GuiRequest::Bye, &g1.id);
        assert!(eng.gui_conns.is_empty());
        assert_eq!(
            eng.selected_workspace.as_deref(),
            Some("cadence"),
            "Bye must not forget the last selection"
        );

        let g2 = FakeGui::attach_to(&mut eng);
        let st = attach_all(&mut eng, &g2.id);
        assert_eq!(
            st.selected_workspace.as_deref(),
            Some("cadence"),
            "reattach should restore prior workspace, not jump to first"
        );
        assert_eq!(
            st.focused_pane.as_deref(),
            Some("worker-b"),
            "reattach should restore prior focused pane"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn grid_interval_is_the_fastest_rate_any_subscriber_wants() {
    with_test_state_dir("gui-interval", || {
        let scratch = temp_scratch("gui-interval");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let sel = eng.push_stub_pane("worker-a", "lab");
        let other = eng.push_stub_pane("worker-b", "cadence");
        let parked = eng.push_stub_pane("worker-c", "notes");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = state_of(eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: Some("lab".into()),
                focused_pane: None,
                subscriptions: Some(vec!["lab".into(), "cadence".into()]),
            },
            &g1.id,
        ));

        // Selected workspace → ~60fps (16ms).
        assert_eq!(eng.grid_interval_ms_for(&sel), Some(16));
        // Subscribed, non-selected, overview off → not streamed.
        assert_eq!(eng.grid_interval_ms_for(&other), None);
        // Unsubscribed → never streamed, overview or not.
        assert_eq!(eng.grid_interval_ms_for(&parked), None);

        // Overview → subscribed-but-not-selected circles push at the thumb rate.
        let _ = eng.handle_gui(GuiRequest::SetOverview { enabled: true }, &g1.id);
        assert_eq!(eng.grid_interval_ms_for(&sel), Some(16));
        assert_eq!(eng.grid_interval_ms_for(&other), Some(66));
        assert_eq!(
            eng.grid_interval_ms_for(&parked),
            None,
            "overview must not resurrect an unsubscribed circle"
        );

        // A second window selecting "cadence" makes it the fast one: the pane's
        // rate is the MIN across interested connections.
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g2.id, &["cadence"]);
        let _ = eng.handle_gui(
            GuiRequest::SetFocus {
                pane: None,
                workspace: Some("cadence".into()),
            },
            &g2.id,
        );
        assert_eq!(eng.grid_interval_ms_for(&other), Some(16));

        // g1 leaving drops it back to its own overview thumb rate.
        let _ = eng.handle_gui(GuiRequest::SetOverview { enabled: false }, &g1.id);
        assert_eq!(eng.grid_interval_ms_for(&other), Some(16));
        let _ = eng.handle_gui(GuiRequest::Bye, &g2.id);
        assert_eq!(eng.grid_interval_ms_for(&other), None);

        // Unknown pane slug → None (no panic).
        assert_eq!(eng.grid_interval_ms_for("ghost-slug"), None);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// The DVR is not a function of who's watching: `record_grid_tap` must run for
/// a pane in a workspace with ZERO subscribers (no grid rate, no fan-out), and
/// the workspace output clock must still advance.
#[test]
fn recorder_tap_and_activity_clock_fire_with_zero_subscribers() {
    with_test_state_dir("gui-tap-parked", || {
        let scratch = temp_scratch("gui-tap-parked");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let parked = eng.push_stub_pane("worker-a", "offstage");

        let g = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g.id, &[]);
        assert!(eng.subscriptions_of(&g.id).is_empty());
        assert_eq!(
            eng.grid_interval_ms_for(&parked),
            None,
            "nobody is streaming this pane"
        );

        eng.handle_session_event(SessionEvent::Wakeup {
            slug: parked.clone(),
        });
        assert!(
            eng.record_tap_log.contains(&parked),
            "PTY wakeup must reach the recorder tap even with no subscribers"
        );

        eng.record_tap_log.clear();
        eng.handle_session_event(SessionEvent::FlushGrid {
            slug: parked.clone(),
        });
        assert!(eng.record_tap_log.contains(&parked));

        // …and the daemon-owned activity clock still advances + broadcasts.
        eng.handle_session_event(SessionEvent::ActivityNote {
            slug: parked.clone(),
            t_ms: 1_700_000_000_000,
        });
        assert_eq!(
            eng.workspace_output.get("offstage"),
            Some(&1_700_000_000_000)
        );
        assert!(g.drain().iter().any(|ev| matches!(
            ev,
            GuiEvent::Activity { workspace, .. } if workspace == "offstage"
        )));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn rename_and_kill_maintain_every_subscription_set() {
    with_test_state_dir("gui-rename-kill", || {
        let scratch = temp_scratch("gui-rename-kill");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &g1.id);
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g2.id, &["lab"]);

        let _ = eng.handle_gui(
            GuiRequest::RenameWorkspace {
                old: "lab".into(),
                new: "workshop".into(),
            },
            &g1.id,
        );
        for id in [&g1.id, &g2.id] {
            let subs = eng.subscriptions_of(id);
            assert!(subs.contains(&"workshop".to_string()), "{id}: {subs:?}");
            assert!(!subs.contains(&"lab".to_string()), "{id}: {subs:?}");
        }

        let _ = eng.handle_gui(
            GuiRequest::KillWorkspace {
                workspace: "workshop".into(),
            },
            &g1.id,
        );
        for id in [&g1.id, &g2.id] {
            assert!(
                !eng.subscriptions_of(id).contains(&"workshop".to_string()),
                "a killed circle must leave every subscription set ({id})"
            );
        }

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

#[test]
fn prune_dead_guis_drops_the_window_and_leaves_peers_alone() {
    with_test_state_dir("gui-prune", || {
        let scratch = temp_scratch("gui-prune");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker-a", "lab");
        eng.push_stub_pane("worker-b", "cadence");

        let g1 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g1.id, &["lab"]);
        let g2 = FakeGui::attach_to(&mut eng);
        let _ = attach_with(&mut eng, &g2.id, &["cadence"]);
        let g2_id = g2.id.clone();

        // Kill g2's receiver — its send channel is now dead.
        drop(g2);
        eng.prune_dead_guis();

        assert!(!eng.has_gui_window(&g2_id));
        assert!(eng.has_gui_window(&g1.id));
        assert_eq!(eng.subscriptions_of(&g1.id), vec!["lab".to_string()]);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// The activity clocks are daemon state: an ActivityNote stamps the pane's
/// workspace and pushes an incremental `Activity`, human input stamps touch,
/// and both ride every `State` push so a relaunched window can seed itself
/// instead of starting blank.
#[test]
fn activity_and_touch_clocks_are_daemon_owned() {
    with_test_state_dir("gui-clocks", || {
        let scratch = temp_scratch("gui-clocks");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("worker-a", "lab");

        let g = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &g.id);
        g.drain();

        // Recorder note → workspace clock + incremental broadcast.
        eng.handle_session_event(SessionEvent::ActivityNote {
            slug: slug.clone(),
            t_ms: 1_700_000_000_000,
        });
        assert_eq!(eng.workspace_output.get("lab"), Some(&1_700_000_000_000));
        let acts: Vec<(String, u64)> = g
            .drain()
            .into_iter()
            .filter_map(|ev| match ev {
                GuiEvent::Activity {
                    workspace,
                    last_output_ms,
                } => Some((workspace, last_output_ms)),
                _ => None,
            })
            .collect();
        assert_eq!(acts, vec![("lab".to_string(), 1_700_000_000_000)]);

        // Older note never walks the clock back, and emits nothing.
        eng.handle_session_event(SessionEvent::ActivityNote {
            slug: slug.clone(),
            t_ms: 1_600_000_000_000,
        });
        assert_eq!(eng.workspace_output.get("lab"), Some(&1_700_000_000_000));
        assert!(g.drain().is_empty());

        // Human input stamps touch (ctl/agent sends deliberately do not).
        assert!(eng.workspace_touch_ms.get("lab").is_none());
        let _ = eng.handle_gui(
            GuiRequest::Input {
                pane: slug.clone(),
                bytes_b64: "aGk=".into(),
            },
            &g.id,
        );
        assert!(eng.workspace_touch_ms.get("lab").copied().unwrap_or(0) > 0);

        // GUI relaunch: the window dies, a fresh one attaches and must get
        // both clocks in its State push (the whole point of daemon ownership).
        drop(g);
        eng.prune_dead_guis();
        let g2 = FakeGui::attach_to(&mut eng);
        let st = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                subscriptions: None,
            },
            &g2.id,
        );
        let meta = match st {
            Some(GuiEvent::State { workspace_meta, .. }) => workspace_meta,
            _ => panic!("attach returns State"),
        };
        let lab = meta
            .iter()
            .find(|m| m.workspace == "lab")
            .expect("lab meta present");
        assert_eq!(lab.last_output_ms, 1_700_000_000_000);
        assert!(lab.last_touch_ms > 0);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// A parked circle still reports its real clocks — the census a phase-2 parked
/// list will render from must not be blank.
#[test]
fn workspace_meta_covers_unsubscribed_workspaces() {
    with_test_state_dir("gui-meta-parked", || {
        let scratch = temp_scratch("gui-meta-parked");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("worker-a", "offstage");
        eng.handle_session_event(SessionEvent::ActivityNote {
            slug,
            t_ms: 1_700_000_000_000,
        });

        let g = FakeGui::attach_to(&mut eng);
        let st = eng.handle_gui(
            GuiRequest::Attach {
                selected_workspace: None,
                focused_pane: None,
                subscriptions: Some(vec![]),
            },
            &g.id,
        );
        let meta = match st {
            Some(GuiEvent::State { workspace_meta, .. }) => workspace_meta,
            _ => panic!("attach returns State"),
        };
        let m = meta
            .iter()
            .find(|m| m.workspace == "offstage")
            .expect("parked circle still has meta");
        assert_eq!(m.last_output_ms, 1_700_000_000_000);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// PR links ride the State push inside `workspace_meta` — that is the only
/// path a chip/attention badge can reach either GUI.
#[test]
fn state_workspace_meta_carries_pr_links() {
    with_test_state_dir("gui-pr-meta", || {
        let scratch = temp_scratch("gui-pr-meta");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        eng.push_stub_pane("worker", "lab");
        let gui = FakeGui::attach_to(&mut eng);
        attach_all(&mut eng, &gui.id);
        gui.drain();

        eng.handle_session_event(SessionEvent::PrLinkSeen {
            slug: "worker".into(),
            url: "https://github.com/o/r/pull/11".into(),
        });

        let meta = gui
            .drain()
            .into_iter()
            .rev()
            .find_map(|ev| match ev {
                GuiEvent::State { workspace_meta, .. } => Some(workspace_meta),
                _ => None,
            })
            .expect("a State push followed the scrape");
        let lab = meta.iter().find(|m| m.workspace == "lab").unwrap();
        assert_eq!(lab.pr_links.len(), 1);
        assert_eq!(lab.pr_links[0].url, "https://github.com/o/r/pull/11");
        assert!(lab.pr_links[0].status.is_none());

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// Nit #2, client-visible half: killing the last pane of an implicitly-created
/// workspace drops the row from the State push (and from subscriptions).
#[test]
fn empty_workspace_row_disappears_after_last_pane_dies() {
    with_test_state_dir("gui-prune", || {
        let scratch = temp_scratch("gui-prune");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("worker", "lab");
        eng.workspace_order.push("lab".into());
        let gui = FakeGui::attach_to(&mut eng);
        let st = attach_all(&mut eng, &gui.id);
        assert!(st.knows_ws("lab") && st.subscribes("lab"));
        gui.drain();

        eng.kill_pane(&slug);
        eng.push_state_to_all();

        let st = gui.last_state().expect("state after kill");
        assert!(!st.knows_ws("lab"));
        assert!(!st.subscribes("lab"));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// The bug this fixes: a circle that stopped working kept its spinner until
/// you clicked it. Grid frames (which carry the OSC title) are sent only to
/// windows streaming that pane, so a window looking at another circle never
/// learned the title had gone idle. Busy flips are broadcast instead —
/// including to a window that subscribes to neither the pane nor its circle.
#[test]
fn busy_flips_reach_a_window_that_does_not_subscribe_to_the_circle() {
    with_test_state_dir("gui-busy", || {
        let scratch = temp_scratch("gui-busy");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let worker = eng.push_stub_pane("worker", "lab");
        eng.push_stub_pane("other", "cadence");

        // This window watches `cadence` only — `lab` is parked for it.
        let gui = FakeGui::attach_to(&mut eng);
        let st = attach_with(&mut eng, &gui.id, &["cadence"]);
        assert!(!st.subscribes("lab"), "subs={:?}", st.subscriptions);
        gui.drain();

        eng.handle_session_event(SessionEvent::Title {
            slug: worker.clone(),
            title: Some("\u{2809} building".into()),
        });
        assert_eq!(
            busy_events(&gui.drain()),
            vec![(worker.clone(), true)],
            "spinner in an unsubscribed circle must still be announced"
        );

        // Same busy title again: edge-triggered, so nothing new on the wire.
        eng.handle_session_event(SessionEvent::Title {
            slug: worker.clone(),
            title: Some("\u{280B} building".into()),
        });
        assert!(busy_events(&gui.drain()).is_empty(), "no flip, no event");

        // Work finished. THIS is the event the sidebar was never getting.
        eng.handle_session_event(SessionEvent::Title {
            slug: worker.clone(),
            title: Some("\u{2733} idle".into()),
        });
        assert_eq!(busy_events(&gui.drain()), vec![(worker.clone(), false)]);

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// A fresh window's State push carries the daemon's busy verdict, so a GUI
/// that attaches mid-spinner starts in the working band without waiting for
/// the next flip.
#[test]
fn state_push_seeds_busy_from_the_daemon() {
    with_test_state_dir("gui-busy-seed", || {
        let scratch = temp_scratch("gui-busy-seed");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let worker = eng.push_stub_pane("worker", "lab");
        eng.push_stub_pane("idle-one", "lab");
        eng.handle_session_event(SessionEvent::Title {
            slug: worker.clone(),
            title: Some("\u{2809} building".into()),
        });

        let gui = FakeGui::attach_to(&mut eng);
        let _ = attach_all(&mut eng, &gui.id);
        let busy = eng
            .pane_infos()
            .into_iter()
            .filter(|p| p.busy)
            .map(|p| p.slug)
            .collect::<Vec<_>>();
        assert_eq!(busy, vec![worker.clone()]);

        // A killed pane doesn't linger in the busy set.
        eng.kill_pane(&worker);
        assert!(eng.pane_infos().iter().all(|p| !p.busy));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// `(pane, busy)` pairs in wire order.
fn busy_events(evs: &[GuiEvent]) -> Vec<(String, bool)> {
    evs.iter()
        .filter_map(|ev| match ev {
            GuiEvent::PaneBusy { pane, busy } => Some((pane.clone(), *busy)),
            _ => None,
        })
        .collect()
}

/// Restorability is the whole gate on sleep. A stub pane runs `bash -l`, which
/// is exactly the case that must be refused: a shell's cwd drift, history and
/// children can't be rebuilt, so it vetoes its circle.
#[test]
fn a_shell_pane_is_not_restorable_and_vetoes_its_circle() {
    with_test_state_dir("sleep-gate", || {
        let scratch = temp_scratch("sleep-gate");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let shell = eng.push_stub_pane("worker", "lab");

        assert!(!eng.pane_restorable(&shell));
        assert!(!eng.workspace_restorable("lab"));
        assert_eq!(eng.workspace_sleep_blockers("lab"), vec![shell.clone()]);

        let e = eng.sleep_workspace("lab").unwrap_err().to_string();
        assert!(e.contains("not restorable"), "{e}");
        assert!(e.contains(&shell), "{e}");
        // Refused means untouched: the pane is still there, still awake.
        assert!(eng.panes.iter().any(|p| p.slug == shell && !p.asleep));

        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// A claude pane with a live conversation id sleeps, keeps its identity, and
/// its `Exited` (which sleeping causes) must not auto-close it.
#[test]
fn sleeping_keeps_the_pane_and_survives_its_own_exit_event() {
    with_test_state_dir("sleep-keep", || {
        let scratch = temp_scratch("sleep-keep");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let slug = eng.push_stub_pane("agent", "lab");
        let transcript = fake_claude_pane(&mut eng, &slug, "sleep-keep");

        assert!(eng.pane_restorable(&slug));
        assert_eq!(eng.sleep_workspace("lab").unwrap(), 1);
        assert!(eng.workspace_asleep("lab"));
        let p = eng.panes.iter().find(|p| p.slug == slug).unwrap();
        assert!(p.asleep && p.session.is_none());
        assert!(p.claude_session.is_some(), "conversation id is kept");

        // The PTY death that sleeping caused arrives late. It must be ignored:
        // the auto-close path would delete the pane we just slept.
        eng.handle_session_event(SessionEvent::Exited {
            slug: slug.clone(),
            code: Some(0),
        });
        assert!(eng.panes.iter().any(|p| p.slug == slug && p.asleep));

        // Sleeping twice is a no-op, not an error (sweep vs right-click race).
        assert!(!eng.sleep_pane(&slug).unwrap());

        let _ = std::fs::remove_file(&transcript);
        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// The sweep only takes circles that are idle past the threshold AND fully
/// restorable AND actually clocked — no observation is not evidence of idle.
#[test]
fn auto_sleep_takes_only_idle_restorable_clocked_circles() {
    with_test_state_dir("sleep-sweep", || {
        let scratch = temp_scratch("sleep-sweep");
        let (mut eng, _rx) = Engine::bare_for_test(scratch.clone());
        let idle = eng.push_stub_pane("agent", "idle-circle");
        let fresh = eng.push_stub_pane("agent", "fresh-circle");
        let never = eng.push_stub_pane("agent", "unclocked");
        let shell = eng.push_stub_pane("worker", "has-a-shell");
        let t1 = fake_claude_pane(&mut eng, &idle, "sweep-1");
        let t2 = fake_claude_pane(&mut eng, &fresh, "sweep-2");
        let t3 = fake_claude_pane(&mut eng, &never, "sweep-3");

        let now = now_ms();
        let day = 24 * 60 * 60 * 1000;
        eng.workspace_output.insert("idle-circle".into(), now - day);
        eng.workspace_output
            .insert("fresh-circle".into(), now - 1000);
        eng.workspace_output.insert("has-a-shell".into(), now - day);
        // `unclocked` deliberately gets no stamp.

        let idle_ms = 12 * 60 * 60 * 1000;
        assert_eq!(
            eng.auto_sleep_candidates(idle_ms, now),
            vec!["idle-circle".to_string()]
        );

        // Human input inside the window keeps a circle awake even with no output.
        eng.workspace_touch_ms
            .insert("idle-circle".into(), now - 60_000);
        assert!(eng.auto_sleep_candidates(idle_ms, now).is_empty());

        let _ = shell;
        for t in [t1, t2, t3] {
            let _ = std::fs::remove_file(t);
        }
        let _ = std::fs::remove_dir_all(&scratch);
    });
}

/// Give a stub pane the shape of a resumable claude pane: a claude command, a
/// minted session id, and a transcript on disk where `--resume` would find it.
/// Returns the transcript path so the test can clean it up.
fn fake_claude_pane(eng: &mut Engine, slug: &str, tag: &str) -> PathBuf {
    let cwd = std::env::temp_dir().join(format!("seance-sleep-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let session = format!("11111111-2222-4333-a444-{:012}", std::process::id());
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    let dir = PathBuf::from(shellexpand::tilde("~/.claude/projects").into_owned()).join(encoded);
    std::fs::create_dir_all(&dir).unwrap();
    let transcript = dir.join(format!("{session}.jsonl"));
    std::fs::write(&transcript, b"{}\n").unwrap();

    let p = eng.panes.iter_mut().find(|p| p.slug == slug).unwrap();
    p.command = "claude --dangerously-skip-permissions".into();
    p.cwd = cwd.to_string_lossy().to_string();
    p.claude_session = Some(session);
    transcript
}
