//! Pane model: a named pane grouped by workspace. Terminals are the first
//! pane kind; the `PaneKind` seam is where markdown/graph/etc panes land.

use std::{collections::HashMap, path::PathBuf};

use anyhow::Result;
use gpui::{AppContext as _, Context, Entity};

use crate::{
    control,
    fileview::FileView,
    remote_term::RemoteTerminal,
    remote_term_view::RemoteTerminalView,
    scratchpad::ScratchpadStore,
    state::{slugify, unique_slug, PersistedPane},
    terminal::{SpawnConfig, Terminal},
    terminal_view::TerminalView,
};

/// Default pane command: a plain interactive login shell — type whatever
/// you think. Agents are one `claude` away (or `seance ctl new --command`).
pub const DEFAULT_COMMAND: &str = "bash -l";

/// Pane kinds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PaneKind {
    Terminal,
    File,
}

/// What actually lives inside a pane.
pub enum PaneBody {
    Terminal {
        terminal: Entity<Terminal>,
        view: Entity<TerminalView>,
    },
    /// Daemon-backed terminal (PTY lives in `seance daemon`).
    Remote {
        terminal: Entity<RemoteTerminal>,
        view: Entity<RemoteTerminalView>,
    },
    File {
        view: Entity<FileView>,
    },
}
pub const DEFAULT_WORKSPACE: &str = "main";

pub struct Pane {
    pub kind: PaneKind,
    pub name: String,
    pub slug: String,
    pub workspace: String,
    pub cwd: String,
    pub command: String,
    pub tiled: bool,
    pub resume_on_restore: bool,
    pub scratch_path: PathBuf,
    pub body: PaneBody,
    /// When popped out, the handle of the OS window hosting this pane's view.
    /// Not persisted — restarts bring every pane back into the main window.
    pub popped: Option<gpui::WindowHandle<gpui_component::Root>>,
}

impl Pane {
    pub fn terminal(&self) -> Option<&Entity<Terminal>> {
        match &self.body {
            PaneBody::Terminal { terminal, .. } => Some(terminal),
            _ => None,
        }
    }

    pub fn remote_terminal(&self) -> Option<&Entity<RemoteTerminal>> {
        match &self.body {
            PaneBody::Remote { terminal, .. } => Some(terminal),
            _ => None,
        }
    }

    pub fn term_view(&self) -> Option<&Entity<TerminalView>> {
        match &self.body {
            PaneBody::Terminal { view, .. } => Some(view),
            _ => None,
        }
    }

    pub fn file_view(&self) -> Option<&Entity<FileView>> {
        match &self.body {
            PaneBody::File { view } => Some(view),
            _ => None,
        }
    }

    /// The content as an AnyView (for hosting in a pop-out window).
    pub fn content_any_view(&self) -> gpui::AnyView {
        match &self.body {
            PaneBody::Terminal { view, .. } => view.clone().into(),
            PaneBody::Remote { view, .. } => view.clone().into(),
            PaneBody::File { view } => view.clone().into(),
        }
    }

    /// The renderable content view, whatever the kind.
    pub fn content_element(&self) -> gpui::AnyElement {
        use gpui::IntoElement as _;
        match &self.body {
            PaneBody::Terminal { view, .. } => view.clone().into_any_element(),
            PaneBody::Remote { view, .. } => view.clone().into_any_element(),
            PaneBody::File { view } => view.clone().into_any_element(),
        }
    }

    pub fn is_running(&self, cx: &gpui::App) -> bool {
        match &self.body {
            PaneBody::Terminal { terminal, .. } => terminal.read(cx).is_running(),
            PaneBody::Remote { terminal, .. } => terminal.read(cx).is_running(),
            PaneBody::File { .. } => true,
        }
    }

    pub fn title(&self, cx: &gpui::App) -> Option<String> {
        match &self.body {
            PaneBody::Terminal { terminal, .. } => terminal.read(cx).title.clone(),
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

    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            PaneKind::Terminal => "terminal",
            PaneKind::File => "file",
        }
    }

    /// Focus handle for the pane content, if any.
    pub fn focus_content(&self, window: &mut gpui::Window, cx: &mut gpui::App) {
        match &self.body {
            PaneBody::Terminal { view, .. } => {
                let h = view.read(cx).focus_handle();
                window.focus(&h, cx);
            }
            PaneBody::Remote { view, .. } => {
                let h = view.read(cx).focus_handle();
                window.focus(&h, cx);
            }
            PaneBody::File { .. } => {}
        }
    }
}

impl Pane {
    pub fn persisted(&self) -> PersistedPane {
        PersistedPane {
            kind: self.kind_str().to_string(),
            name: self.name.clone(),
            slug: self.slug.clone(),
            cwd: self.cwd.clone(),
            command: self.command.clone(),
            tiled: self.tiled,
            resume_on_restore: self.resume_on_restore,
            workspace: self.workspace.clone(),
            status: None,
            status_note: None,
            pad_rev: 0,
            owner: None,
            drive_mode: None,
            exited: false,
            exit_code: None,
        }
    }
}

pub struct SpawnRequest {
    pub name: String,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub workspace: Option<String>,
    pub tiled: bool,
    pub resume: bool,
    /// When set, this pane is a FILE pane monitoring the given path.
    pub file: Option<String>,
}

/// Path where the app installs its bash shell-integration rc file.
pub fn shell_rc_path() -> PathBuf {
    PathBuf::from(shellexpand::tilde("~/.local/share/seance/seance.bash").into_owned())
}

/// Spawn a pane (terminal or file view). Shared by UI and control plane.
pub fn spawn_pane<T: 'static>(
    req: SpawnRequest,
    taken_slugs: &[&str],
    store: &ScratchpadStore,
    cx: &mut Context<T>,
) -> Result<Pane> {
    let name = if req.name.trim().is_empty() {
        "session".to_string()
    } else {
        req.name.trim().to_string()
    };
    let slug = unique_slug(&name, taken_slugs);
    let workspace = req
        .workspace
        .filter(|w| !w.trim().is_empty())
        .map(|w| slugify(&w))
        .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string());
    let cwd_raw = req.cwd.unwrap_or_else(|| "~".to_string());
    let cwd_expanded = shellexpand::tilde(&cwd_raw).to_string();
    let scratch_path = store.path_for(&slug);

    // FILE pane: a live view over a document, no PTY at all.
    if let Some(file) = req.file {
        let path = PathBuf::from(shellexpand::tilde(&file).into_owned());
        let view = cx.new(|cx| FileView::new(path.clone(), cx));
        return Ok(Pane {
            kind: PaneKind::File,
            popped: None,
            name,
            slug,
            workspace,
            cwd: cwd_raw,
            command: path.to_string_lossy().to_string(),
            tiled: req.tiled,
            resume_on_restore: false,
            scratch_path,
            body: PaneBody::File { view },
        });
    }

    let explicit_command = req.command.filter(|c| !c.trim().is_empty());
    // Default shell panes get seance's shell integration (command tracking);
    // explicit --command panes are left untouched.
    let mut command = match &explicit_command {
        Some(c) => c.clone(),
        None => {
            let rc = shell_rc_path();
            if rc.is_file() {
                format!("bash --init-file {}", rc.to_string_lossy())
            } else {
                DEFAULT_COMMAND.to_string()
            }
        }
    };
    if req.resume && command.starts_with("claude") && !command.contains("--continue") {
        command = format!("{command} --continue");
    }

    let mut env = HashMap::new();
    env.insert("SEANCE_SESSION".to_string(), slug.clone());
    env.insert("SEANCE_WORKSPACE".to_string(), workspace.clone());
    env.insert(
        "SEANCE_SCRATCHPAD".to_string(),
        scratch_path.to_string_lossy().to_string(),
    );
    env.insert(
        "SEANCE_SOCKET".to_string(),
        control::socket_path().to_string_lossy().to_string(),
    );

    let config = SpawnConfig {
        command: command.clone(),
        cwd: PathBuf::from(&cwd_expanded),
        env,
    };

    let opened = crate::terminal::open_terminal(config)?;
    let terminal = cx.new(|cx| Terminal::new(opened, cx));
    let view = cx.new(|cx| TerminalView::new(terminal.clone(), cx));

    Ok(Pane {
        kind: PaneKind::Terminal,
        popped: None,
        name,
        slug,
        workspace,
        cwd: cwd_raw,
        // Persist the user's intent (or the plain default), not the rc-file
        // plumbing — restores re-resolve the integration path.
        command: explicit_command.unwrap_or_else(|| DEFAULT_COMMAND.to_string()),
        tiled: req.tiled,
        resume_on_restore: req.resume,
        scratch_path,
        body: PaneBody::Terminal { terminal, view },
    })
}
