//! The seam between chrome/input modules and the app core: everything UI code
//! may do travels through [`Actions`] (implemented by the app on top of the
//! websocket conn), so ui/renderer/probe modules stay decoupled from each
//! other and from transport.

use seance_core::protocol::GuiRequest;

/// App-level actions available to chrome + input handling. Fire-and-forget:
/// results come back as daemon events folded into `ClientState`.
pub trait Actions {
    /// Escape hatch: send any raw request.
    fn send(&self, req: GuiRequest);

    fn focus_pane(&self, slug: &str);
    fn select_workspace(&self, ws: &str);
    fn spawn_pane(
        &self,
        name: &str,
        cwd: Option<String>,
        command: Option<String>,
        workspace: Option<String>,
    );
    fn kill_pane(&self, slug: &str);
    fn rename_pane(&self, slug: &str, name: &str);
    fn create_workspace(&self, name: &str);
    fn rename_workspace(&self, old: &str, new: &str);
    fn kill_workspace(&self, ws: &str);
    fn answer_ask(&self, id: &str, answer: &str);
    fn inject(&self, pane: &str, text: &str, submit: bool);
    /// Raw PTY bytes (already encoded) into a pane.
    fn input_bytes(&self, pane: &str, bytes: &[u8]);
    fn scroll(&self, pane: &str, delta: i32);
    fn scroll_bottom(&self, pane: &str);
    fn resize(&self, pane: &str, cols: u16, rows: u16);
    fn refresh_grid(&self, pane: &str);
    fn ghost_accept(&self, pane: &str);
    fn ghost_reject(&self, pane: &str);
    /// Toggle the latency probe overlay.
    fn toggle_probe(&self);

    // ── native-parity chrome ────────────────────────────────────────────────

    /// "+ summon": new shell pane in the selected workspace; the pane's tile
    /// header opens an inline rename when it arrives (native
    /// `new_default_session` semantics).
    fn summon(&self);
    /// Quicklaunch chip click: FRESH workspace named after the entry
    /// (uniquified against all known workspaces), single pane,
    /// no rename prompt.
    fn quicklaunch(&self, name: &str, cwd: Option<String>, command: Option<String>);
    /// Sidebar row menu "park": drop a circle out of the active list (and out
    /// of the daemon subscription) into the parked group.
    fn park_workspace(&self, ws: &str);
    /// Parked row menu "add to active": subscribe + promote, without selecting.
    fn activate_workspace(&self, ws: &str);
    /// Sidebar row menu "pin": move a circle into the pinned section at the top
    /// of the rail. Pinning a parked circle activates it first.
    fn pin_workspace(&self, ws: &str);
    /// Pinned row menu "unpin": back to the normal active band.
    fn unpin_workspace(&self, ws: &str);
    /// Repaint chrome on the next frame (client-only view state changed —
    /// e.g. the parked accordion opened).
    fn request_rebuild(&self);
    /// Fold / unfold a rail band or prefix cluster, and persist it.
    fn toggle_collapsed(&self, key: &str);
    /// Ctrl+PageUp/Down — ACTIVE circles only.
    fn cycle_workspace(&self, delta: i32);
    fn cycle_pane(&self, delta: i32);
    /// ctrl+shift+w semantics: kill active pane; last pane in the circle (or
    /// an empty selected circle) banishes the workspace instead.
    fn kill_active(&self);
    fn toggle_zoom(&self, slug: &str);
    fn toggle_help(&self);
    fn toggle_activity(&self);
    /// Toggle the PR board sweep overlay (`PRs (N)` in the sidebar footer).
    fn toggle_pr_board(&self);
    /// Drop ONE PR link from a circle: `pr-link clear` for that url (engine
    /// removes AND sticky-dismisses it) plus the optimistic local mirror, so
    /// chip / popover / board reflect it before the daemon state lands.
    fn remove_pr_link(&self, ws: &str, url: &str);
    /// Daemon fs-bridge call (quicklaunch config, host select, …). Callback
    /// may never fire if the socket drops mid-flight — tolerate silence.
    fn fs_call(
        &self,
        op: seance_core::protocol::FsOp,
        cb: Box<dyn FnOnce(Result<serde_json::Value, String>)>,
    );
    /// Host widget select (account switch) — runs daemon-side, seconds-slow;
    /// success/failure surfaces as a toast.
    fn host_select(&self, widget: &str, item: &str);
}
