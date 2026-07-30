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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::AppContext as _;

use crate::gui_client::GuiClient;
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
    fn header_template_mentions_title_and_env_var() {
        let body = header_template("worker-1");
        assert!(body.starts_with("# worker-1 — scratchpad\n"));
        assert!(body.contains("$SEANCE_SCRATCHPAD"));
    }

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

/// A gpui view: an editable panel bound to one session's scratchpad file.
///
/// All IO goes through the daemon fs bridge ([`GuiClient`]) so the GUI can run
/// on a different machine than the daemon. Bridge calls are synchronous (block
/// up to 10s), so every one of them runs on the background executor; results
/// hop back to the UI thread via `cx.update`.
pub struct ScratchpadDrawer {
    title: String,
    /// Daemon-side pad path (from `PaneInfo.scratchpad`). Opaque to the GUI.
    path: String,
    client: Arc<GuiClient>,

    /// The multi-line text editor state (gpui-component).
    input: gpui::Entity<InputState>,

    /// Set when the user edits and cleared once we flush to disk. Used both to
    /// gate the debounced save and to decide whether an external change is safe
    /// to reload (we skip reload while dirty — last-writer-wins).
    dirty: bool,

    /// Bumped on every edit. An async flush captures the generation with the
    /// contents and only clears `dirty` if no newer edit landed meanwhile.
    edit_gen: u64,

    /// Last daemon-reported mtime (ms) we are "in sync" with. Updated on load,
    /// on save (fs_write returns the new mtime), and on an accepted external
    /// reload. Compared against the live mtime by the poller.
    last_seen_mtime: Option<u64>,

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
    /// Build a drawer for the daemon-side pad at `path` (from
    /// `PaneInfo.scratchpad`). The editor starts empty; the initial contents
    /// load asynchronously through the fs bridge (seeding the header template
    /// if the pad doesn't exist yet), then the 1s mtime watch loop takes over.
    pub fn new(
        client: Arc<GuiClient>,
        path: String,
        title: String,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("notes for this session — the agent can write here too")
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
            client,
            input,
            dirty: false,
            edit_gen: 0,
            last_seen_mtime: None,
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
        self.edit_gen = self.edit_gen.wrapping_add(1);

        // Replacing the task drops the previous one, cancelling its timer —
        // that is the debounce. When the timer finally elapses we flush the
        // editor contents through the fs bridge on the background executor.
        self._save_task = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            cx.background_executor().timer(AUTOSAVE_DEBOUNCE).await;

            // Snapshot contents + generation on the UI thread.
            let Ok(Some((client, path, contents, gen))) = cx.update(|_window, cx| {
                this.upgrade().map(|entity| {
                    entity.update(cx, |this, cx| {
                        (
                            this.client.clone(),
                            this.path.clone(),
                            this.input.read(cx).value().to_string(),
                            this.edit_gen,
                        )
                    })
                })
            }) else {
                return;
            };

            // Blocking bridge call, off the UI thread. Atomic daemon-side.
            let written = cx
                .background_executor()
                .spawn(async move { client.fs_write(&path, contents.as_bytes()) })
                .await;

            let _ = cx.update(|_window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, _cx| this.finish_flush(gen, written));
                }
            });
        });
    }

    /// Apply the result of an async flush back onto the drawer state.
    fn finish_flush(&mut self, gen: u64, written: Result<Option<u64>>) {
        match written {
            Ok(new_mtime) => {
                // Only go clean if no newer edit landed while we were writing;
                // a newer edit has its own debounced flush pending.
                if self.edit_gen == gen {
                    self.dirty = false;
                }
                // Adopt our own write's mtime so the watcher doesn't treat this
                // as an external change and pointlessly reload.
                self.last_seen_mtime = new_mtime;
            }
            Err(err) => {
                // Non-fatal: keep dirty so a later edit's debounce retries.
                // Surface it for the integrator's logging rather than
                // panicking the UI.
                eprintln!("scratchpad: failed to save {}: {err:#}", self.path);
            }
        }
    }

    /// Start the watch loop: first the initial load (seeding the header if the
    /// pad is missing), then the 1s mtime poll. All bridge IO runs on the
    /// background executor; the loop ends when the entity drops (the weak
    /// handle stops upgrading).
    fn start_watch(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let client = self.client.clone();
        let path = self.path.clone();
        let title = self.title.clone();

        self._watch_task = cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            // ---- Initial load (with ensure-on-open header seeding). ----
            let loaded = {
                let (client, path) = (client.clone(), path.clone());
                cx.background_executor()
                    .spawn(async move { load_or_seed(&client, &path, &title) })
                    .await
            };
            let alive = cx
                .update(|window, cx| {
                    let Some(this) = this.upgrade() else {
                        return false;
                    };
                    this.update(cx, |this, cx| {
                        if let Some((contents, mtime_ms)) = loaded {
                            // Don't clobber keystrokes typed during the load.
                            if !this.dirty {
                                this.input.update(cx, |state, cx| {
                                    state.set_value(contents, window, cx);
                                });
                            }
                            this.last_seen_mtime = mtime_ms;
                            cx.notify();
                        }
                    });
                    true
                })
                .unwrap_or(false);
            if !alive {
                return;
            }

            // ---- 1s mtime poll for external (agent) writes. ----
            loop {
                cx.background_executor().timer(WATCH_POLL_INTERVAL).await;

                // Stat through the bridge, off the UI thread.
                let stat = {
                    let (client, path) = (client.clone(), path.clone());
                    cx.background_executor()
                        .spawn(async move { client.fs_stat(&path) })
                        .await
                };
                let current = match stat {
                    Ok(exists) => exists.flatten(),
                    Err(err) => {
                        // Transport hiccup — don't advance state, just retry.
                        eprintln!("scratchpad: failed to stat {path}: {err:#}");
                        continue;
                    }
                };

                // Decide on the UI thread whether this is a change we take.
                let Ok(Some(reload)) = cx.update(|_window, cx| {
                    this.upgrade().map(|entity| {
                        let this = entity.read(cx);
                        // Unsaved local edits win (last-writer-wins). Don't
                        // clobber in-progress typing; the pending debounce will
                        // overwrite the external change. We do NOT advance
                        // last_seen_mtime, so once clean a later external write
                        // is still picked up.
                        current != this.last_seen_mtime && !this.dirty
                    })
                }) else {
                    return; // entity gone — end the loop
                };
                if !reload {
                    continue;
                }

                let read = {
                    let (client, path) = (client.clone(), path.clone());
                    cx.background_executor()
                        .spawn(async move { client.fs_read_string(&path) })
                        .await
                };

                let alive = cx
                    .update(|window, cx| {
                        let Some(this) = this.upgrade() else {
                            return false;
                        };
                        this.update(cx, |this, cx| match read {
                            Ok((contents, mtime_ms)) => {
                                if this.dirty {
                                    return; // went dirty during the read — local wins
                                }
                                let current_value = this.input.read(cx).value();
                                if current_value.as_ref() != contents {
                                    this.input.update(cx, |state, cx| {
                                        state.set_value(contents, window, cx);
                                    });
                                }
                                this.last_seen_mtime = mtime_ms;
                                cx.notify();
                            }
                            Err(err) => {
                                eprintln!("scratchpad: failed to reload {}: {err:#}", this.path);
                                // Advance anyway so we don't spin re-reporting
                                // the same unreadable state every second.
                                this.last_seen_mtime = current;
                            }
                        });
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
            }
        });
    }
}

/// Read the pad through the bridge; if it doesn't exist yet, seed it with the
/// header template (ensure-on-open, mirroring the old `path_for` behavior).
///
/// Returns `(contents, mtime_ms)` to adopt, or `None` when the pad could not
/// be read or created (the watch loop will keep retrying via its stat poll).
///
/// Blocking — call on the background executor only.
fn load_or_seed(client: &GuiClient, path: &str, title: &str) -> Option<(String, Option<u64>)> {
    match client.fs_read_string(path) {
        Ok((contents, mtime_ms)) => Some((contents, mtime_ms)),
        Err(read_err) => {
            // Only seed when the pad is genuinely missing; a transport error
            // must not overwrite an existing pad with the template.
            match client.fs_stat(path) {
                Ok(None) => {
                    let body = header_template(title);
                    match client.fs_write(path, body.as_bytes()) {
                        Ok(mtime_ms) => Some((body, mtime_ms)),
                        Err(err) => {
                            eprintln!("scratchpad: failed to seed {path}: {err:#}");
                            None
                        }
                    }
                }
                _ => {
                    eprintln!("scratchpad: failed to load {path}: {read_err:#}");
                    None
                }
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
        let path_hint: SharedString = self.path.clone().into();

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
