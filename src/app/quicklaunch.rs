//! Configurable quicklaunch strip: one-click "terminal in DIR running CMD"
//! buttons in the sidebar, above the host-bridge (claude accounts) strip.
//!
//! Config: `~/.config/seance/quicklaunch.json` — a JSON array:
//! ```json
//! [
//!   {"name": "vita", "cwd": "~/work/vita", "command": "claude"},
//!   {"name": "scratch", "cwd": "~"}
//! ]
//! ```
//! `command` omitted/empty = plain shell in `cwd`. Every launch opens a
//! FRESH workspace named after the entry (uniquified: vita, vita-2, …) with
//! a single pane — no rename prompt. A legacy `"workspace"` key in the JSON
//! still parses but is ignored. The file is mtime-watched with a 2s throttle
//! — edits show up without restarting the GUI; a parse error keeps the
//! previous entries.
//!
//! Management (right-click a chip → edit/remove, drag to reorder, the `+`
//! button to add) round-trips the config through serde. NOTE: a UI edit
//! re-serializes the whole file, so any *unknown* JSON fields a hand-editor
//! added do not survive an edit (serde drops what `QuickLaunchEntry` doesn't
//! model). Known fields, ordering, and untouched entries are preserved.

use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, Context, Entity, Focusable as _, SharedString, Window};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::ContextMenuExt as _;
use gpui_component::StyledExt as _;
use serde::{Deserialize, Serialize};

use crate::pane::SpawnRequest;
use crate::theme::SeancePalette;

use super::actions::{ActQuickLaunchEdit, ActQuickLaunchRemove};
use super::util::{tip_s, DragPill, DraggedQuickLaunch};
use super::SeanceApp;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct QuickLaunchEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Live state for the quicklaunch create/edit modal. `original` is `None` for a
/// fresh add, or the name of the entry being edited (so a rename can find and
/// replace it in place, and collision-checks can exclude it).
pub(super) struct QuickLaunchEditor {
    pub original: Option<String>,
    pub name: Entity<InputState>,
    pub cwd: Entity<InputState>,
    pub command: Entity<InputState>,
}

fn quicklaunch_path() -> std::path::PathBuf {
    std::path::PathBuf::from(shellexpand::tilde("~/.config/seance/quicklaunch.json").into_owned())
}

fn parse_quicklaunch(s: &str) -> Result<Vec<QuickLaunchEntry>, serde_json::Error> {
    serde_json::from_str(s)
}

// ---- pure list ops (unit-tested; no I/O) ----

/// True if `name` (already trimmed) collides with an existing entry other than
/// `original` (the entry being edited; `None` for a fresh add). Used to block a
/// save that would create a duplicate name.
pub(super) fn name_collides(
    entries: &[QuickLaunchEntry],
    name: &str,
    original: Option<&str>,
) -> bool {
    entries
        .iter()
        .any(|e| e.name == name && Some(e.name.as_str()) != original)
}

/// Insert or update an entry. When `original` names an existing entry, that
/// entry is replaced *in place* (position preserved, name may change). A fresh
/// add (`original == None`, or a name not currently present) appends at the end.
/// Callers gate on [`name_collides`]/empty-name before this; upsert itself does
/// not validate.
pub(super) fn upsert_entry(
    entries: &mut Vec<QuickLaunchEntry>,
    original: Option<&str>,
    entry: QuickLaunchEntry,
) {
    if let Some(orig) = original {
        if let Some(slot) = entries.iter_mut().find(|e| e.name == orig) {
            *slot = entry;
            return;
        }
    }
    entries.push(entry);
}

/// Remove the entry named `name`. No-op if absent.
pub(super) fn remove_entry(entries: &mut Vec<QuickLaunchEntry>, name: &str) {
    entries.retain(|e| e.name != name);
}

/// Move entry `moved` to sit immediately before entry `before` (insert-before
/// semantics, matching the sidebar reorder). No-op when they're the same or
/// either is missing.
pub(super) fn reorder_entry(entries: &mut Vec<QuickLaunchEntry>, moved: &str, before: &str) {
    if moved == before {
        return;
    }
    let Some(from) = entries.iter().position(|e| e.name == moved) else {
        return;
    };
    let entry = entries.remove(from);
    // Recompute the target index after removal (indices shift left when the
    // moved item was earlier in the list).
    let idx = entries
        .iter()
        .position(|e| e.name == before)
        .unwrap_or(entries.len());
    entries.insert(idx, entry);
}

impl SeanceApp {
    /// Cheap hot-reload: stat at most every 2s, re-parse only on mtime change.
    /// Called from render() — a bad edit keeps the last good entries.
    pub(super) fn reload_quicklaunch_if_stale(&mut self) {
        let now = Instant::now();
        if self
            .quicklaunch_checked
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
        {
            return;
        }
        self.quicklaunch_checked = Some(now);
        let path = quicklaunch_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime == self.quicklaunch_mtime {
            return;
        }
        self.quicklaunch_mtime = mtime;
        if mtime.is_none() {
            self.quicklaunch.clear();
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(s) => match parse_quicklaunch(&s) {
                Ok(v) => self.quicklaunch = v,
                Err(e) => {
                    eprintln!("[seance gui] quicklaunch.json parse error: {e} (keeping previous)")
                }
            },
            Err(e) => eprintln!("[seance gui] quicklaunch.json read error: {e}"),
        }
    }

    /// Persist `self.quicklaunch` to disk (pretty JSON, atomic temp+rename in
    /// the same dir — mirrors `state.rs::save`) and re-arm the mtime so the next
    /// `reload_quicklaunch_if_stale` doesn't re-read our own write. Best-effort:
    /// a write failure logs and leaves in-memory state intact.
    pub(super) fn save_quicklaunch(&mut self) {
        let path = quicklaunch_path();
        let Some(dir) = path.parent() else {
            eprintln!("[seance gui] quicklaunch.json path has no parent dir");
            return;
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[seance gui] quicklaunch.json mkdir error: {e}");
            return;
        }
        let json = match serde_json::to_string_pretty(&self.quicklaunch) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[seance gui] quicklaunch.json serialize error: {e}");
                return;
            }
        };
        // Temp file in the SAME directory so the rename is atomic (same fs).
        let tmp = dir.join(format!(".quicklaunch.json.tmp.{}", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, &json) {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("[seance gui] quicklaunch.json write error: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("[seance gui] quicklaunch.json rename error: {e}");
            return;
        }
        // Re-arm mtime so the hot-reload sees no change (harmless if it did).
        self.quicklaunch_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    }

    /// Context-menu "remove": drop the entry and persist.
    pub(super) fn quicklaunch_remove(&mut self, name: &str, cx: &mut Context<Self>) {
        remove_entry(&mut self.quicklaunch, name);
        self.save_quicklaunch();
        cx.notify();
    }

    /// Drop of one chip before another (insert-before) + persist.
    pub(super) fn quicklaunch_reorder(
        &mut self,
        moved: &str,
        before: &str,
        cx: &mut Context<Self>,
    ) {
        if moved == before {
            return;
        }
        reorder_entry(&mut self.quicklaunch, moved, before);
        self.save_quicklaunch();
        cx.notify();
    }

    // ---- modal editor (create + edit) ----

    /// Open the editor. `edit` = `Some(name)` pre-fills from that entry (and
    /// keeps its position on save); `None` opens blank (new entry, appended).
    /// Focuses the name field. Enter in any field saves; Esc cancels.
    pub(super) fn open_quicklaunch_editor(
        &mut self,
        edit: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = edit.and_then(|n| self.quicklaunch.iter().find(|e| e.name == n).cloned());
        let opt = |s: Option<String>| s.unwrap_or_default();
        let (name0, cwd0, cmd0) = match &existing {
            Some(e) => (e.name.clone(), opt(e.cwd.clone()), opt(e.command.clone())),
            None => (String::new(), String::new(), String::new()),
        };
        let field = |window: &mut Window,
                     cx: &mut Context<Self>,
                     placeholder: &'static str,
                     initial: String| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(initial)
            })
        };
        let name = field(window, cx, "vita", name0);
        let cwd = field(window, cx, "~/work/vita", cwd0);
        let command = field(window, cx, "claude (empty = plain shell)", cmd0);
        // Enter in any field commits; Blur is ignored (cancel is Esc / button).
        for input in [&name, &cwd, &command] {
            cx.subscribe_in(
                input,
                window,
                |this: &mut SeanceApp, _input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.commit_quicklaunch_editor(window, cx);
                    }
                },
            )
            .detach();
        }
        let focus = name.read(cx).focus_handle(cx);
        self.quicklaunch_editor = Some(QuickLaunchEditor {
            original: edit.map(|s| s.to_string()),
            name,
            cwd,
            command,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(super) fn cancel_quicklaunch_editor(&mut self, cx: &mut Context<Self>) {
        self.quicklaunch_editor = None;
        cx.notify();
    }

    /// Validate + persist the editor. Blocked (no-op, stays open) on an empty
    /// trimmed name or a name that collides with a *different* existing entry —
    /// the render shows the hint line for either case.
    pub(super) fn commit_quicklaunch_editor(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ed) = self.quicklaunch_editor.as_ref() else {
            return;
        };
        let name = ed.name.read(cx).value().trim().to_string();
        if name.is_empty() || name_collides(&self.quicklaunch, &name, ed.original.as_deref()) {
            cx.notify();
            return;
        }
        let norm = |input: &Entity<InputState>| {
            let v = input.read(cx).value().trim().to_string();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        };
        let entry = QuickLaunchEntry {
            name,
            cwd: norm(&ed.cwd),
            command: norm(&ed.command),
        };
        let original = ed.original.clone();
        upsert_entry(&mut self.quicklaunch, original.as_deref(), entry);
        self.save_quicklaunch();
        self.quicklaunch_editor = None;
        cx.notify();
    }

    /// Full-window dimmed overlay + centered card. Mirrors the overview/palette
    /// overlay pattern; mounted from `impl Render` after those so it stacks on
    /// top. `None` when the editor is closed.
    pub(super) fn render_quicklaunch_editor(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let ed = self.quicklaunch_editor.as_ref()?;
        let name_val = ed.name.read(cx).value().trim().to_string();
        // Hint: empty name, or collides with a *different* existing entry.
        let hint: Option<&str> = if name_val.is_empty() {
            Some("name required")
        } else if name_collides(&self.quicklaunch, &name_val, ed.original.as_deref()) {
            Some("name in use")
        } else {
            None
        };
        let title = if ed.original.is_some() {
            "edit quicklaunch"
        } else {
            "new quicklaunch"
        };
        let field_row = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .child(label),
                )
                .child(Input::new(input))
        };
        Some(
            div()
                .id("ql-editor-overlay")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.5))
                // Block mouse events from reaching the panes underneath —
                // without this, mouse-down passes through and the terminal
                // steals focus from the modal inputs.
                .occlude()
                // Click the dim backdrop to cancel.
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_quicklaunch_editor(cx);
                }))
                .child(
                    div()
                        .id("ql-editor-card")
                        .w(px(420.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(SeancePalette::flame_dim())
                        .bg(SeancePalette::bg_elevated())
                        // Swallow clicks inside the card so the backdrop
                        // cancel doesn't fire.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .text_color(SeancePalette::text())
                                .child(title),
                        )
                        .child(field_row("name", &ed.name))
                        .children(
                            hint.map(|h| {
                                div().text_xs().text_color(SeancePalette::danger()).child(h)
                            }),
                        )
                        .child(field_row("cwd", &ed.cwd))
                        .child(field_row("command", &ed.command))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .id("ql-editor-cancel")
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .text_sm()
                                        .cursor_pointer()
                                        .text_color(SeancePalette::text_faint())
                                        .hover(|s| s.bg(SeancePalette::surface()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.cancel_quicklaunch_editor(cx);
                                        }))
                                        .child("cancel"),
                                )
                                .child(
                                    div()
                                        .id("ql-editor-save")
                                        .px_3()
                                        .py_1()
                                        .rounded_md()
                                        .text_sm()
                                        .cursor_pointer()
                                        .bg(SeancePalette::surface())
                                        .text_color(SeancePalette::flame())
                                        .hover(|s| s.bg(SeancePalette::border()))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.commit_quicklaunch_editor(window, cx);
                                        }))
                                        .child("save"),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Chip strip above the host-bridge widgets. The title row (with its `+`
    /// add button) is always shown so an empty config can still be populated
    /// from the UI; the chip row is omitted when there are no entries.
    pub(super) fn render_quicklaunch(&self, cx: &Context<Self>) -> impl IntoElement {
        let has_entries = !self.quicklaunch.is_empty();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .py_1p5()
            .gap_1()
            .border_t_1()
            .border_color(SeancePalette::border())
            .child(
                // Title row: label on the left, `+` add button on the right.
                div()
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .child("── quicklaunch ──"),
                    )
                    .child(
                        div()
                            .id("ql-add")
                            .px_1p5()
                            .rounded_md()
                            .text_xs()
                            .cursor_pointer()
                            .text_color(SeancePalette::text_faint())
                            .hover(|s| {
                                s.bg(SeancePalette::surface())
                                    .text_color(SeancePalette::flame())
                            })
                            .tooltip(tip_s("add quicklaunch entry"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_quicklaunch_editor(None, window, cx);
                            }))
                            .child("+"),
                    ),
            )
            .when(has_entries, |strip| {
                strip.child(
                    div().px_2().flex().flex_row().flex_wrap().gap_1().children(
                        self.quicklaunch
                            .iter()
                            .map(|e| self.render_quicklaunch_chip(e, cx)),
                    ),
                )
            })
            .into_any_element()
    }

    /// One draggable, right-clickable chip.
    fn render_quicklaunch_chip(
        &self,
        e: &QuickLaunchEntry,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let entry = e.clone();
        let name = e.name.clone();
        let cmd_desc = entry
            .command
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| "shell".into());
        let cwd_desc = entry.cwd.clone().unwrap_or_else(|| "~".into());
        let drag_name = name.clone();
        let drop_name = name.clone();
        let menu_name = name.clone();
        div()
            .id(SharedString::from(format!("ql-{name}")))
            .px_2()
            .py_0p5()
            .rounded_md()
            .text_xs()
            .cursor_pointer()
            .bg(SeancePalette::surface())
            .text_color(SeancePalette::flame())
            .hover(|s| s.bg(SeancePalette::border()))
            .tooltip(tip_s(format!("{cwd_desc} $ {cmd_desc}")))
            .on_drag(DraggedQuickLaunch { name: drag_name }, |drag, _, _, cx| {
                let label = format!("▸ {}", drag.name);
                cx.new(|_| DragPill { label })
            })
            .drag_over::<DraggedQuickLaunch>(|style, _, _, _| style.bg(SeancePalette::flame_dim()))
            .on_drop(cx.listener(move |this, drag: &DraggedQuickLaunch, _, cx| {
                this.quicklaunch_reorder(&drag.name, &drop_name, cx);
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                let cwd = entry
                    .cwd
                    .as_ref()
                    .map(|c| shellexpand::tilde(c).into_owned());
                let command = entry.command.clone().filter(|c| !c.trim().is_empty());
                // Always a FRESH workspace named after the entry (uniquified
                // against every window's workspaces), single pane, no rename
                // prompt — the quicklaunch name IS the name.
                let mut taken: Vec<String> = this.known_workspace_names().into_iter().collect();
                taken.extend(this.foreign_workspaces.iter().map(|f| f.workspace.clone()));
                let taken_refs: Vec<&str> = taken.iter().map(|s| s.as_str()).collect();
                let ws = crate::state::unique_slug(&entry.name, &taken_refs);
                this.spawn_internal(
                    SpawnRequest {
                        name: entry.name.clone(),
                        cwd,
                        command,
                        workspace: Some(ws),
                        file: None,
                    },
                    cx,
                );
            }))
            .context_menu(move |menu, _, _| {
                menu.menu("edit…", Box::new(ActQuickLaunchEdit(menu_name.clone())))
                    .menu("remove", Box::new(ActQuickLaunchRemove(menu_name.clone())))
            })
            .child(name)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_entry() {
        // Legacy "workspace" key must still parse (ignored since 0.9.20).
        let v = parse_quicklaunch(
            r#"[{"name":"vita","cwd":"~/work/vita","command":"claude","workspace":"vita"}]"#,
        )
        .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "vita");
        assert_eq!(v[0].cwd.as_deref(), Some("~/work/vita"));
        assert_eq!(v[0].command.as_deref(), Some("claude"));
    }

    #[test]
    fn parse_name_only_defaults_rest() {
        let v = parse_quicklaunch(r#"[{"name":"scratch"}]"#).unwrap();
        assert_eq!(v[0].name, "scratch");
        assert!(v[0].cwd.is_none() && v[0].command.is_none());
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_quicklaunch(r#"{"name":"not-an-array"}"#).is_err());
        assert!(parse_quicklaunch("[{]").is_err());
    }

    #[test]
    fn parse_empty_array_ok() {
        assert!(parse_quicklaunch("[]").unwrap().is_empty());
    }

    // ---- pure list-op tests ----

    fn entry(name: &str) -> QuickLaunchEntry {
        QuickLaunchEntry {
            name: name.into(),
            cwd: None,
            command: None,
        }
    }

    fn names(entries: &[QuickLaunchEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn upsert_new_appends_at_end() {
        let mut v = vec![entry("a"), entry("b")];
        upsert_entry(&mut v, None, entry("c"));
        assert_eq!(names(&v), ["a", "b", "c"]);
    }

    #[test]
    fn upsert_edit_replaces_in_place_and_can_rename() {
        let mut v = vec![entry("a"), entry("b"), entry("c")];
        // Edit "b" -> rename to "b2" with a new cwd; position (index 1) held.
        let updated = QuickLaunchEntry {
            name: "b2".into(),
            cwd: Some("~/x".into()),
            command: None,
        };
        upsert_entry(&mut v, Some("b"), updated);
        assert_eq!(names(&v), ["a", "b2", "c"]);
        assert_eq!(v[1].cwd.as_deref(), Some("~/x"));
    }

    #[test]
    fn upsert_edit_missing_original_appends() {
        // `original` names an entry that isn't present -> treated as an add.
        let mut v = vec![entry("a")];
        upsert_entry(&mut v, Some("ghost"), entry("z"));
        assert_eq!(names(&v), ["a", "z"]);
    }

    #[test]
    fn remove_entry_drops_by_name() {
        let mut v = vec![entry("a"), entry("b"), entry("c")];
        remove_entry(&mut v, "b");
        assert_eq!(names(&v), ["a", "c"]);
        // Removing an absent name is a no-op.
        remove_entry(&mut v, "nope");
        assert_eq!(names(&v), ["a", "c"]);
    }

    #[test]
    fn reorder_to_front() {
        let mut v = vec![entry("a"), entry("b"), entry("c")];
        reorder_entry(&mut v, "c", "a"); // move c before a
        assert_eq!(names(&v), ["c", "a", "b"]);
    }

    #[test]
    fn reorder_to_middle() {
        let mut v = vec![entry("a"), entry("b"), entry("c"), entry("d")];
        reorder_entry(&mut v, "d", "b"); // move d before b
        assert_eq!(names(&v), ["a", "d", "b", "c"]);
    }

    #[test]
    fn reorder_to_end_when_target_is_last_uses_insert_before() {
        // Insert-before semantics: dropping "a" on the last entry lands it
        // *before* that last entry, not strictly last (mirrors sidebar).
        let mut v = vec![entry("a"), entry("b"), entry("c")];
        reorder_entry(&mut v, "a", "c");
        assert_eq!(names(&v), ["b", "a", "c"]);
    }

    #[test]
    fn reorder_noops_on_same_or_missing() {
        let mut v = vec![entry("a"), entry("b")];
        reorder_entry(&mut v, "a", "a"); // same -> no-op
        assert_eq!(names(&v), ["a", "b"]);
        reorder_entry(&mut v, "ghost", "b"); // missing moved -> no-op
        assert_eq!(names(&v), ["a", "b"]);
        reorder_entry(&mut v, "a", "ghost"); // missing target -> append
        assert_eq!(names(&v), ["b", "a"]);
    }

    #[test]
    fn name_collides_excludes_the_edited_entry() {
        let v = vec![entry("a"), entry("b")];
        // Fresh add colliding with an existing name.
        assert!(name_collides(&v, "a", None));
        assert!(!name_collides(&v, "c", None));
        // Editing "a": keeping its own name is NOT a collision...
        assert!(!name_collides(&v, "a", Some("a")));
        // ...but renaming "a" onto "b" IS.
        assert!(name_collides(&v, "b", Some("a")));
    }

    // ---- serialization round-trip ----

    #[test]
    fn serialize_roundtrip_preserves_known_fields() {
        let entries = vec![
            QuickLaunchEntry {
                name: "vita".into(),
                cwd: Some("~/work/vita".into()),
                command: Some("claude".into()),
            },
            entry("scratch"),
        ];
        let json = serde_json::to_string_pretty(&entries).unwrap();
        let back = parse_quicklaunch(&json).unwrap();
        assert_eq!(entries, back);
    }

    #[test]
    fn serialize_omits_none_fields() {
        // `skip_serializing_if` keeps a name-only entry compact and, crucially,
        // makes the written form re-parse identically (no `null`s to trip on).
        let json = serde_json::to_string(&vec![entry("scratch")]).unwrap();
        assert_eq!(json, r#"[{"name":"scratch"}]"#);
    }
}
