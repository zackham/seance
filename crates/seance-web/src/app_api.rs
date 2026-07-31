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
    fn spawn_pane(&self, name: &str, cwd: Option<String>, command: Option<String>, workspace: Option<String>);
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
}
