//! Per-session markdown scratchpads shared between the human and the agent.
//!
//! Each Claude Code session gets a plain markdown file on disk. The agent
//! running *inside* the session is handed `SEANCE_SCRATCHPAD` pointing at that
//! same file, so both halves read and write the same notes. The killer feature
//! is that external writes (by the agent) show up live in the UI.
//!
//! # Design notes for the integrator
//!
//! - **Watch mechanism: 1s mtime poll on a gpui background task.** We poll the
//!   file's modified-time once a second from a self-rescheduling gpui task
//!   (the same pattern `gpui-component`'s `BlinkCursor` uses). This was a
//!   deliberate choice over `notify` v8: `notify` runs its own OS thread and
//!   needs a channel bridged back onto gpui's foreground executor, which is
//!   awkward and easy to get wrong. The poll uses only gpui primitives, gives
//!   us the `&mut Window` we need for `InputState::set_value`, and
//!   self-terminates when the drawer entity is dropped (the weak handle stops
//!   upgrading).
//!
//! - **Last-writer-wins on conflict.** If the file changed on disk *and* the
//!   input has unsaved local edits, we skip the external reload and let the
//!   pending local autosave win. Simple and predictable; noted here so the
//!   integrator knows we are not attempting a merge.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use gpui::AppContext as _;
use gpui::{
    div, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Task, WeakEntity,
};
use gpui_component::{
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _,
};

/// How long we wait after the last keystroke before flushing to disk.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(800);
/// How often we poll the file's mtime for external (agent) writes.
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Header written into a scratchpad the first time it is created.
fn header_template(title: &str) -> String {
    format!(
        "# {title} — scratchpad\n\
         \n\
         <!-- Shared notes. Both you and the agent in this pane read/write \
         this file (agent sees it via $SEANCE_SCRATCHPAD). Agents: run \
         `seance ctl skill` to learn to drive sibling panes. -->\n\
         \n"
    )
}

/// Owns the scratchpad directory and hands out per-session file paths.
///
/// Cheap to clone-by-reference; the only state is the resolved directory.
pub struct ScratchpadStore {
    dir: PathBuf,
}

impl ScratchpadStore {
    /// Create the store, ensuring `~/.local/share/seance/scratch/` exists.
    pub fn new() -> Result<Self> {
        let dir = PathBuf::from(shellexpand::tilde("~/.local/share/seance/scratch").into_owned());
        Self::with_dir(dir)
    }

    /// Store backed by an explicit directory (tests, isolated profiles).
    pub fn with_dir(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating scratchpad dir {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// Path to the scratchpad for `slug`, i.e. `<dir>/<slug>.md`.
    ///
    /// On first access the file is created with a small header template. If the
    /// file already exists it is left untouched. Errors (e.g. a bad slug or a
    /// permissions problem) are swallowed here — we still return the intended
    /// path so callers can surface a friendlier error when they read/write it.
    pub fn path_for(&self, slug: &str) -> PathBuf {
        let path = self.dir.join(format!("{}.md", sanitize_slug(slug)));
        if !path.exists() {
            // Best-effort creation with a header; if this fails, the drawer's
            // own load/save path will report the error to the user.
            let _ = std::fs::write(&path, header_template(slug));
        }
        path
    }
}

/// Replace path-hostile characters in a slug so it stays a single flat file.
fn sanitize_slug(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect();
    if cleaned.is_empty() {
        "scratch".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slug_replaces_hostile_chars() {
        assert_eq!(sanitize_slug("a/b c"), "a-b-c");
        assert_eq!(sanitize_slug("ok-slug_1.md"), "ok-slug_1.md");
        assert_eq!(sanitize_slug(""), "scratch");
        assert_eq!(sanitize_slug("!!!"), "---");
    }

    #[test]
    fn with_dir_path_for_creates_header() {
        let dir = std::env::temp_dir().join(format!(
            "seance-scratch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let store = ScratchpadStore::with_dir(dir.clone()).unwrap();
        let path = store.path_for("worker-1");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("worker-1"));
        assert!(body.contains("scratchpad"));
        // second call leaves existing content
        std::fs::write(&path, "custom\n").unwrap();
        let path2 = store.path_for("worker-1");
        assert_eq!(std::fs::read_to_string(&path2).unwrap(), "custom\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Read the file's modified-time, or `None` if it can't be stat'd.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Atomic-ish write: write to a sibling temp file, then rename over the target.
///
/// Rename-on-the-same-filesystem is atomic on Linux, so a reader (the agent)
/// never sees a half-written scratchpad.
fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, contents)
        .with_context(|| format!("writing temp scratchpad {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// A gpui view: an editable panel bound to one session's scratchpad file.
pub struct ScratchpadDrawer {
    title: String,
    path: PathBuf,

    /// The multi-line text editor state (gpui-component).
    input: gpui::Entity<InputState>,

    /// Set when the user edits and cleared once we flush to disk. Used both to
    /// gate the debounced save and to decide whether an external change is safe
    /// to reload (we skip reload while dirty — last-writer-wins).
    dirty: bool,

    /// Last mtime we are "in sync" with. Updated on load, on save, and on an
    /// accepted external reload. Compared against the live mtime by the poller.
    last_seen_mtime: Option<SystemTime>,

    /// The most recent pending debounced-save task. Dropping it cancels the
    /// prior timer, which is how the debounce collapses rapid keystrokes.
    _save_task: Task<()>,

    /// The self-rescheduling file-watch poll task. Held so it lives as long as
    /// the drawer; when the drawer drops, the weak handle stops upgrading and
    /// the loop ends.
    _watch_task: Task<()>,

    /// Kept alive so the input-change subscription isn't dropped.
    _subscriptions: Vec<Subscription>,
}

impl ScratchpadDrawer {
    /// Build a drawer for `slug`, loading the file's current contents into the
    /// editor and starting both the file watcher and (lazily) the autosave.
    pub fn new(
        store: &ScratchpadStore,
        slug: String,
        title: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let path = store.path_for(&slug);

        // Load current contents (path_for guarantees the file exists, but be
        // defensive in case it was removed between calls).
        let initial = std::fs::read_to_string(&path).unwrap_or_default();
        let last_seen_mtime = mtime(&path);

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("notes for this session — the agent can write here too")
                .default_value(initial)
        });

        // Any real edit marks us dirty and (re)arms the debounced save.
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    this.on_edit(window, cx);
                }
            },
        );

        let mut this = Self {
            title,
            path,
            input,
            dirty: false,
            last_seen_mtime,
            _save_task: Task::ready(()),
            _watch_task: Task::ready(()),
            _subscriptions: vec![subscription],
        };

        this.start_watch(window, cx);
        this
    }

    /// Focus handle of the notes editor (for flipping focus into notes).
    pub fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// Called on every editor change: mark dirty and (re)arm the debounce.
    fn on_edit(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        self.dirty = true;

        // Replacing the task drops the previous one, cancelling its timer —
        // that is the debounce. When the timer finally elapses we flush.
        self._save_task = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            cx.background_executor().timer(AUTOSAVE_DEBOUNCE).await;
            let _ = cx.update(|_window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| this.flush(cx));
                }
            });
        });
    }

    /// Write the current editor contents to disk (atomic) and clear dirty.
    fn flush(&mut self, cx: &mut gpui::Context<Self>) {
        let contents = self.input.read(cx).value().to_string();
        match atomic_write(&self.path, &contents) {
            Ok(()) => {
                self.dirty = false;
                // Adopt our own write's mtime so the watcher doesn't treat this
                // as an external change and pointlessly reload.
                self.last_seen_mtime = mtime(&self.path);
            }
            Err(err) => {
                // Non-fatal: keep dirty so the next debounce retries. Surface it
                // for the integrator's logging rather than panicking the UI.
                eprintln!(
                    "scratchpad: failed to save {}: {err:#}",
                    self.path.display()
                );
            }
        }
    }

    /// Start the 1s mtime poll loop. Self-reschedules until the entity drops.
    fn start_watch(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        self._watch_task = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| loop {
            cx.background_executor().timer(WATCH_POLL_INTERVAL).await;

            // update() lands us back on the foreground thread with a Window, so
            // we can call set_value. If the entity is gone, bail and end loop.
            let keep_going = cx
                .update(|window, cx| {
                    let Some(this) = this.upgrade() else {
                        return false;
                    };
                    this.update(cx, |this, cx| this.poll_external(window, cx));
                    true
                })
                .unwrap_or(false);

            if !keep_going {
                break;
            }
        });
    }

    /// Poll the file's mtime; if it changed externally and we have no unsaved
    /// local edits, reload the file into the editor.
    fn poll_external(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let current = mtime(&self.path);
        if current == self.last_seen_mtime {
            return; // no change since we last synced
        }

        // The file changed underneath us.
        if self.dirty {
            // Unsaved local edits win (last-writer-wins). Don't clobber the
            // user's in-progress typing; our pending debounce will overwrite
            // the external change on its next flush. We do NOT advance
            // last_seen_mtime here, so once the user goes clean a later
            // external write can still be picked up.
            return;
        }

        match std::fs::read_to_string(&self.path) {
            Ok(contents) => {
                let current_value = self.input.read(cx).value();
                if current_value.as_ref() != contents {
                    self.input.update(cx, |state, cx| {
                        state.set_value(contents, window, cx);
                    });
                }
                self.last_seen_mtime = current;
                cx.notify();
            }
            Err(err) => {
                eprintln!(
                    "scratchpad: failed to reload {}: {err:#}",
                    self.path.display()
                );
                // Advance mtime anyway so we don't spin re-reporting the same
                // unreadable state every second.
                self.last_seen_mtime = current;
            }
        }
    }
}

impl Render for ScratchpadDrawer {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let title: SharedString = self.title.clone().into();
        let path_hint: SharedString = self.path.display().to_string().into();

        v_flex()
            .id("scratchpad-face")
            .size_full()
            .gap_2()
            .p_3()
            .bg(theme.background)
            .child(
                // Header: title + shared-with-agent hint. Flip chrome lives in
                // the parent pane strip (app.rs) so this stays pure notes body.
                h_flex()
                    .w_full()
                    .items_baseline()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .font_family(theme.font_family.clone())
                            .child(format!("✎ {title}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("shared via $SEANCE_SCRATCHPAD"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(path_hint),
            )
            .child(
                // The editor fills the remaining space.
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .font_family(theme.mono_font_family.clone())
                    .child(Input::new(&self.input).h_full().appearance(true)),
            )
    }
}
