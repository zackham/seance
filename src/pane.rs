//! Pane model: a named pane grouped by workspace. A pane's body is either a
//! daemon-backed remote terminal or a live file view (see [`PaneBody`]).

use std::path::PathBuf;

use gpui::Entity;

use crate::{
    fileview::FileView, remote_term::RemoteTerminal, remote_term_view::RemoteTerminalView,
};

/// What actually lives inside a pane.
pub enum PaneBody {
    /// Daemon-backed terminal (PTY lives in `seance daemon`).
    Remote {
        terminal: Entity<RemoteTerminal>,
        view: Entity<RemoteTerminalView>,
    },
    File {
        view: Entity<FileView>,
    },
}

pub struct Pane {
    pub name: String,
    pub slug: String,
    pub workspace: String,
    pub cwd: String,
    pub command: String,
    pub tiled: bool,
    pub body: PaneBody,
    /// When popped out, the handle of the OS window hosting this pane's view.
    /// Not persisted — restarts bring every pane back into the main window.
    pub popped: Option<gpui::WindowHandle<gpui_component::Root>>,
}

impl Pane {
    pub fn remote_terminal(&self) -> Option<&Entity<RemoteTerminal>> {
        match &self.body {
            PaneBody::Remote { terminal, .. } => Some(terminal),
            _ => None,
        }
    }

    /// The content as an AnyView (for hosting in a pop-out window).
    pub fn content_any_view(&self) -> gpui::AnyView {
        match &self.body {
            PaneBody::Remote { view, .. } => view.clone().into(),
            PaneBody::File { view } => view.clone().into(),
        }
    }

    /// The renderable content view, whatever the kind.
    pub fn content_element(&self) -> gpui::AnyElement {
        use gpui::IntoElement as _;
        match &self.body {
            PaneBody::Remote { view, .. } => view.clone().into_any_element(),
            PaneBody::File { view } => view.clone().into_any_element(),
        }
    }

    pub fn is_running(&self, cx: &gpui::App) -> bool {
        match &self.body {
            PaneBody::Remote { terminal, .. } => terminal.read(cx).is_running(),
            PaneBody::File { .. } => true,
        }
    }

    pub fn title(&self, cx: &gpui::App) -> Option<String> {
        match &self.body {
            PaneBody::Remote { terminal, .. } => terminal.read(cx).title(),
            PaneBody::File { view } => Some(
                view.read(cx)
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
        }
    }

    /// Focus handle for the pane content, if any.
    pub fn focus_content(&self, window: &mut gpui::Window, cx: &mut gpui::App) {
        match &self.body {
            PaneBody::Remote { view, .. } => {
                let h = view.read(cx).focus_handle();
                window.focus(&h, cx);
            }
            PaneBody::File { .. } => {}
        }
    }
}

pub struct SpawnRequest {
    pub name: String,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub workspace: Option<String>,
    /// When set, this pane is a FILE pane monitoring the given path.
    pub file: Option<String>,
}

/// Path where the app installs its bash shell-integration rc file.
pub fn shell_rc_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/seance.bash").into_owned())
}
