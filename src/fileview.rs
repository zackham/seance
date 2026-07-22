//! A live, read-only file viewer pane with change history.
//!
//! The second seance pane kind (terminals were the first). Use case: the human
//! says "let's work on this report" and an agent pops open a [`FileView`] on the
//! markdown file. The pane shows the file's content, live-updates as anyone
//! edits it on disk, and lets you step back through the history of changes.
//!
//! # Design notes for the integrator
//!
//! - **Watch mechanism: 1s mtime poll on a gpui background task.** Copied wholesale
//!   from `scratchpad.rs` — a self-rescheduling `cx.spawn_in` loop that polls the
//!   file's modified-time once a second and self-terminates when the entity is
//!   dropped (the weak handle stops upgrading). We deliberately reuse the house
//!   pattern rather than reach for `notify`: it needs only gpui primitives and
//!   gives us the `&mut Window` the render path expects.
//!
//! - **Read-only.** Unlike the scratchpad we never write the *watched* file. The
//!   only files we write live under our own history dir (see [`history`] below).
//!
//! - **Markdown rendering.** For `.md` / `.markdown` files we peel YAML
//!   frontmatter into a GH-style box (injected as a custom first block via a
//!   fenced `seance-fm` code fence + block parser), then render with
//!   `gpui_component::text::markdown().scrollable(true)` so the document is
//!   **virtualized** (`gpui::list`) — only on-screen blocks shape/paint.
//!   Fit-content mode re-laid-out the entire file on every scroll/resize and
//!   felt like frame limiting on large docs (changelog, reports). Frontmatter
//!   is block 0 of the same list, so it still scrolls away (not sticky chrome).
//!   Theme from `cx.theme()`. Non-markdown uses an outer scroll + per-line
//!   monospace. See `docs/FILE-PANES.md`.
//!
//! - **History storage.** Plain file copies under
//!   `~/.local/share/seance/filehist/<hash>/NNNN.snap`, capped at the most
//!   recent [`MAX_SNAPSHOTS`]. Snapshot `0000` is taken at open; each observed
//!   external change appends the next-numbered snapshot. Stepping ◀/▶ reads the
//!   snapshot files back off disk. See [`history`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use std::sync::Arc;

use gpui::{
    div, list, prelude::*, px, rems, ListAlignment, ListState, SharedString, StyleRefinement,
    Task, WeakEntity, Window,
};
use gpui_component::{
    highlighter::HighlightTheme,
    text::{markdown, markdown_ast, MarkdownNode, TextViewStyle},
    ActiveTheme as _, StyledExt as _,
};

use crate::theme::SeancePalette;

/// Candlelit markdown style — dark highlight theme, compact headings, surface
/// code blocks. Default TextViewStyle is light-themed and reads as "GitHub
/// paper" on seance grounds.
fn candlelit_markdown_style() -> TextViewStyle {
    let mut code = StyleRefinement::default();
    code.background = Some(SeancePalette::surface().into());
    TextViewStyle {
        paragraph_gap: rems(0.55),
        heading_base_font_size: px(15.),
        heading_font_size: Some(Arc::new(|level, base| match level {
            1 => px(22.),
            2 => px(18.),
            3 => px(16.),
            _ => base,
        })),
        highlight_theme: HighlightTheme::default_dark(),
        code_block: code,
        table: StyleRefinement::default(),
        table_cell: StyleRefinement::default(),
        is_dark: true,
    }
}

// ---------------------------------------------------------------------------
// YAML frontmatter — GH-style box
// ---------------------------------------------------------------------------

/// Split a markdown document into optional frontmatter fields + body.
/// Recognizes a leading `---` … `---` (or `...`) fence. Body keeps leading
/// blank lines stripped once.
fn split_frontmatter(source: &str) -> (Option<Vec<(String, String)>>, &str) {
    let s = source;
    // Must start with --- on its own line (optional BOM / whitespace).
    let rest = s.strip_prefix('\u{feff}').unwrap_or(s);
    let rest = rest.strip_prefix("---\n").or_else(|| rest.strip_prefix("---\r\n"));
    let Some(rest) = rest else {
        return (None, source);
    };
    // Find closing fence.
    let close = rest
        .find("\n---\n")
        .map(|i| (i, 5))
        .or_else(|| rest.find("\n---\r\n").map(|i| (i, 6)))
        .or_else(|| rest.find("\n...\n").map(|i| (i, 5)))
        .or_else(|| {
            // Closing fence at EOF
            if rest.ends_with("\n---") {
                Some((rest.len() - 4, 4))
            } else {
                None
            }
        });
    let Some((end, close_len)) = close else {
        return (None, source);
    };
    let yaml = &rest[..end];
    let body = rest[end + close_len..].trim_start_matches(['\r', '\n']);
    let fields: Vec<(String, String)> = parse_frontmatter_fields(yaml);
    if fields.is_empty() {
        (None, source)
    } else {
        (Some(fields), body)
    }
}

/// Minimal YAML-ish line parser for the common report frontmatter shape.
/// Handles `key: value`, quoted strings, and simple `[a, b]` lists. Nested
/// maps / multi-line blocks fall back to the raw rest-of-line.
fn parse_frontmatter_fields(yaml: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for raw in yaml.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Skip nested list items (we flatten simple arrays on the key line).
        if line.starts_with('-') {
            if let Some((_, last_val)) = out.last_mut() {
                let item = line.trim_start_matches('-').trim().trim_matches('"').trim_matches('\'');
                if !last_val.is_empty() {
                    last_val.push_str(", ");
                }
                last_val.push_str(item);
            }
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim().to_string();
        // Strip surrounding quotes.
        if (val.starts_with('"') && val.ends_with('"'))
            || (val.starts_with('\'') && val.ends_with('\''))
        {
            val = val[1..val.len() - 1].to_string();
        }
        // Flatten [a, b, c]
        if val.starts_with('[') && val.ends_with(']') {
            val = val[1..val.len() - 1]
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\''))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
        }
        out.push((key, val));
    }
    out
}

/// Encode frontmatter as a synthetic fenced block that our markdown extension
/// turns back into the GH-style box — so it participates in the virtualized
/// document list (scrolls away) instead of sitting as sticky chrome above it.
fn frontmatter_fence(fields: &[(String, String)]) -> String {
    let mut s = String::from("```seance-fm\n");
    for (k, v) in fields {
        // One field per line; tabs separate key/value. Flatten newlines so the
        // fence stays well-formed.
        s.push_str(&k.replace(['\n', '\r', '\t'], " "));
        s.push('\t');
        s.push_str(&v.replace(['\n', '\r', '\t'], " "));
        s.push('\n');
    }
    s.push_str("```\n\n");
    s
}

fn parse_frontmatter_fence(value: &str) -> Vec<(String, String)> {
    value
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('\t')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// GitHub-style frontmatter box: rounded border, subtle surface fill, key/value
/// rows. Rendered as custom markdown block 0 so it scrolls with the doc.
fn render_frontmatter_box(fields: &[(String, String)]) -> impl IntoElement {
    div()
        .id("fileview-frontmatter")
        .flex_none()
        .w_full()
        .mb_3()
        .rounded_lg()
        .border_1()
        .border_color(SeancePalette::border())
        .bg(SeancePalette::surface())
        .overflow_hidden()
        .child(
            // Header strip
            div()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(SeancePalette::border())
                .bg(SeancePalette::bg_elevated())
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(SeancePalette::violet())
                        .child("frontmatter"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(SeancePalette::text_faint())
                        .child(format!("{} fields", fields.len())),
                ),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .children(fields.iter().map(|(k, v)| {
                    div()
                        .flex()
                        .items_start()
                        .gap_3()
                        .child(
                            div()
                                .flex_none()
                                .w(px(110.))
                                .text_xs()
                                .font_semibold()
                                .text_color(SeancePalette::text_dim())
                                .child(k.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(SeancePalette::text())
                                .child(if v.is_empty() {
                                    "—".to_string()
                                } else {
                                    v.clone()
                                }),
                        )
                })),
        )
}

/// How often we poll the file's mtime for external edits.
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Keep at most this many history snapshots per file; oldest beyond this are
/// pruned. 200 × a typical report is a few MB — cheap, and enough to step back
/// through a working session.
const MAX_SNAPSHOTS: usize = 200;

/// What the body is currently showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Following the live file (tail). Reloads + records history on change.
    Live,
    /// Pinned to a historical snapshot index (0-based, into `snapshots`).
    History(usize),
}

/// One line of a unified diff between two snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
    /// Unchanged run collapsed out of view (VS Code / git-style hunk gap).
    Skip(usize),
}

/// A gpui view: a live, read-only viewer bound to one file on disk.
pub struct FileView {
    /// The file we are watching.
    path: PathBuf,

    /// Per-file history directory (`.../filehist/<hash>/`).
    hist_dir: PathBuf,

    /// The content currently displayed in the body.
    content: String,

    /// True when the live file could not be read (missing / permissions).
    unreadable: bool,

    /// Live tail vs. pinned to a historical snapshot.
    mode: ViewMode,

    /// Ordered list of on-disk snapshot indices (the `NNNN` numbers), oldest
    /// first. `snapshots.last()` is the newest recorded snapshot.
    snapshots: Vec<u32>,

    /// True once an external change is observed while the viewer is pinned to
    /// history — drives the "file has changed since" hint.
    changed_since_pin: bool,

    /// When pinned to history with a predecessor snapshot, show a line-level
    /// unified diff of this version against the previous one (instead of the
    /// plain snapshot body). Toggled by clicking the history hint bar.
    show_diff: bool,

    /// Last mtime we are "in sync" with. Compared against the live mtime by the
    /// poller to detect external edits.
    last_seen_mtime: Option<SystemTime>,

    /// The self-rescheduling file-watch poll task. Held so it lives as long as
    /// the view; when the view drops, the weak handle stops upgrading and the
    /// loop ends.
    _watch_task: Task<()>,
}

impl FileView {
    /// Build a viewer for `path`, loading its current contents, taking the
    /// initial history snapshot (index 0), and starting the 1s file watcher.
    pub fn new(path: PathBuf, cx: &mut gpui::Context<Self>) -> Self {
        let hist_dir = history::dir_for(&path);
        let _ = std::fs::create_dir_all(&hist_dir);

        // Load current contents. A missing/unreadable file is not an error —
        // we show a friendly placeholder and keep watching for it to appear.
        let (content, unreadable) = match std::fs::read_to_string(&path) {
            Ok(c) => (c, false),
            Err(_) => (String::new(), true),
        };
        let last_seen_mtime = history::mtime(&path);

        // Snapshot 0 at open: the baseline the human/agent starts from. If a
        // history dir already exists from a prior session we continue its
        // numbering (history survives re-open) and skip recording when the
        // current content already matches the newest snapshot — re-opening an
        // unchanged file shouldn't grow history.
        let mut snapshots = history::list(&hist_dir);
        if !unreadable {
            let already = snapshots
                .last()
                .and_then(|&idx| history::read(&hist_dir, idx))
                .map(|prev| prev == content)
                .unwrap_or(false);
            if !already {
                history::record(&hist_dir, &content, &mut snapshots);
            }
        }

        let mut this = Self {
            path,
            hist_dir,
            content,
            unreadable,
            mode: ViewMode::Live,
            snapshots,
            changed_since_pin: false,
            show_diff: false,
            last_seen_mtime,
            _watch_task: Task::ready(()),
        };
        this.start_watch(cx);
        this
    }

    /// The file this viewer is bound to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start the 1s mtime poll loop. Self-reschedules until the entity drops.
    /// (Lifted from `scratchpad.rs::start_watch` — proven working.)
    fn start_watch(&mut self, cx: &mut gpui::Context<Self>) {
        self._watch_task = cx.spawn(async move |this: WeakEntity<Self>, cx| loop {
            cx.background_executor().timer(WATCH_POLL_INTERVAL).await;
            let Some(this) = this.upgrade() else { break };
            this.update(cx, |this: &mut FileView, cx| this.poll_external(cx));
        });
    }

    /// Poll the file's mtime; on an external change, reload the live content and
    /// append a history snapshot. We record history regardless of view mode, but
    /// only yank the *displayed* content when we are in [`ViewMode::Live`].
    fn poll_external(&mut self, cx: &mut gpui::Context<Self>) {
        let current = history::mtime(&self.path);
        if current == self.last_seen_mtime {
            return; // no change since we last synced
        }
        self.last_seen_mtime = current;

        match std::fs::read_to_string(&self.path) {
            Ok(contents) => {
                // Record a snapshot only when the content actually differs from
                // the newest one we hold (mtime can bump without a content
                // change — e.g. `touch`).
                let differs = self
                    .newest_snapshot_content()
                    .map(|prev| prev != contents)
                    .unwrap_or(true);
                if differs {
                    history::record(&self.hist_dir, &contents, &mut self.snapshots);
                }

                match self.mode {
                    ViewMode::Live => {
                        // Following the tail: adopt the new content.
                        self.content = contents;
                        self.unreadable = false;
                    }
                    ViewMode::History(_) => {
                        // Pinned to history: don't yank the view, just flag that
                        // the file moved on underneath us.
                        if differs {
                            self.changed_since_pin = true;
                        }
                    }
                }
                cx.notify();
            }
            Err(_) => {
                // The file went away or became unreadable. In live mode reflect
                // that; in history mode keep showing the pinned snapshot.
                if matches!(self.mode, ViewMode::Live) {
                    self.unreadable = true;
                    self.content = String::new();
                    cx.notify();
                }
            }
        }
    }

    /// Content of the newest recorded snapshot, if any (read off disk).
    fn newest_snapshot_content(&self) -> Option<String> {
        let idx = *self.snapshots.last()?;
        history::read(&self.hist_dir, idx)
    }

    // ---- history navigation ----

    /// Jump back to following the live file tail.
    fn go_live(&mut self, cx: &mut gpui::Context<Self>) {
        self.mode = ViewMode::Live;
        self.changed_since_pin = false;
        self.show_diff = false;
        match std::fs::read_to_string(&self.path) {
            Ok(c) => {
                self.content = c;
                self.unreadable = false;
            }
            Err(_) => {
                self.unreadable = true;
                self.content = String::new();
            }
        }
        cx.notify();
    }

    /// Step to an older snapshot (◀).
    fn step_older(&mut self, cx: &mut gpui::Context<Self>) {
        if self.snapshots.is_empty() {
            return;
        }
        let cur = match self.mode {
            // From live, stepping back lands on the newest snapshot.
            ViewMode::Live => self.snapshots.len(),
            ViewMode::History(i) => i,
        };
        if cur == 0 {
            return; // already at the oldest
        }
        self.show_history(cur - 1, cx);
    }

    /// Step to a newer snapshot (▶). Stepping past the newest returns to live.
    fn step_newer(&mut self, cx: &mut gpui::Context<Self>) {
        match self.mode {
            ViewMode::Live => {} // already at the tail
            ViewMode::History(i) => {
                if i + 1 >= self.snapshots.len() {
                    self.go_live(cx);
                } else {
                    self.show_history(i + 1, cx);
                }
            }
        }
    }

    /// Pin the body to snapshot at `idx` (index into `snapshots`).
    fn show_history(&mut self, idx: usize, cx: &mut gpui::Context<Self>) {
        let Some(&snap) = self.snapshots.get(idx) else {
            return;
        };
        match history::read(&self.hist_dir, snap) {
            Some(c) => {
                // Entering history from live → default to diff. Within history,
                // keep the user's content/diff preference (except at the oldest
                // snap, which has no predecessor).
                let from_live = matches!(self.mode, ViewMode::Live);
                self.content = c;
                self.unreadable = false;
                self.mode = ViewMode::History(idx);
                // Fresh pin: clear the stale-since flag; the poller re-sets it if
                // the live file moves while we sit here.
                self.changed_since_pin = false;
                if idx == 0 {
                    self.show_diff = false;
                } else if from_live {
                    self.show_diff = true;
                }
                cx.notify();
            }
            None => {
                // Snapshot vanished — drop it and bail.
                self.snapshots.retain(|&s| s != snap);
            }
        }
    }

    /// Toggle the unified-diff body (history only, when a previous snapshot
    /// exists). Click target is the history hint bar.
    fn toggle_diff(&mut self, cx: &mut gpui::Context<Self>) {
        if self.predecessor_content().is_none() {
            self.show_diff = false;
            cx.notify();
            return;
        }
        self.show_diff = !self.show_diff;
        cx.notify();
    }

    /// Content of the snapshot immediately before the one being viewed.
    /// `None` in live mode, at the oldest snapshot, or if the prior snap is gone.
    fn predecessor_content(&self) -> Option<String> {
        let viewed_idx = match self.mode {
            ViewMode::Live => return None,
            ViewMode::History(i) => i,
        };
        if viewed_idx == 0 {
            return None;
        }
        history::read(&self.hist_dir, *self.snapshots.get(viewed_idx - 1)?)
    }

    /// Is this file worth rendering as markdown?
    fn is_markdown(&self) -> bool {
        matches!(
            self.path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("md") | Some("markdown") | Some("mdown") | Some("mkd")
        )
    }

    /// "+N/-N" line-count delta vs. the snapshot immediately before the one
    /// being viewed. Cheap line-multiset delta for the hint bar; the real
    /// ordered diff is in [`unified_line_diff`]. `None` when there's no
    /// meaningful predecessor (live tail, or the oldest snapshot).
    fn line_delta(&self) -> Option<(usize, usize)> {
        let prev = self.predecessor_content()?;
        Some(line_count_delta(&prev, &self.content))
    }

    // ---- rendering helpers ----

    /// The header strip: filename + dimmed path, live/history indicator, and the
    /// ◀ N/M ▶ ⦿live controls. Buttons are plain styled divs (house style).
    fn render_header(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let basename = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(file)")
            .to_string();
        let full = self.path.to_string_lossy().to_string();

        let total = self.snapshots.len();
        // 1-based counter for humans. Live tail reads as "M/M" (newest).
        let position = match self.mode {
            ViewMode::Live => total,
            ViewMode::History(i) => i + 1,
        };
        let is_live = matches!(self.mode, ViewMode::Live);

        // A control "button": plain div, amber on hover, dim when inert.
        let btn = |id: &str, glyph: &str, enabled: bool| {
            div()
                .id(SharedString::from(format!("fileview-{id}")))
                .flex_none()
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .text_sm()
                .text_color(if enabled {
                    SeancePalette::text_dim()
                } else {
                    SeancePalette::text_faint()
                })
                .when(enabled, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.text_color(SeancePalette::flame()).bg(SeancePalette::surface()))
                })
                .child(glyph.to_string())
        };

        let can_older = !self.snapshots.is_empty()
            && match self.mode {
                ViewMode::Live => true,
                ViewMode::History(i) => i > 0,
            };
        let can_newer = !is_live;

        div()
            .flex_none()
            .h(px(30.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(SeancePalette::bg_elevated())
            .border_b_1()
            .border_color(SeancePalette::border())
            // filename + dimmed full-path suffix
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .text_color(SeancePalette::text())
                            .child(basename),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(SeancePalette::text_faint())
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(full),
                    ),
            )
            // live / history indicator dot + word
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(if is_live {
                        SeancePalette::success()
                    } else {
                        SeancePalette::violet()
                    })
                    .child(if is_live { "● live" } else { "◐ history" }.to_string()),
            )
            // ◀ N/M ▶
            .child(
                btn("older", "◀", can_older).on_click(
                    cx.listener(|this, _, _, cx| this.step_older(cx)),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(SeancePalette::text_dim())
                    .child(format!("{position}/{total}")),
            )
            .child(
                btn("newer", "▶", can_newer).on_click(
                    cx.listener(|this, _, _, cx| this.step_newer(cx)),
                ),
            )
            // ⦿ live (jump back to tail)
            .child(
                btn("golive", "⦿ live", !is_live).on_click(
                    cx.listener(|this, _, _, cx| this.go_live(cx)),
                ),
            )
    }

    /// The scrollable body: markdown or monospace text, optional unified diff,
    /// or a placeholder.
    fn render_body(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        // Empty / unreadable → friendly placeholder (skip when showing a diff
        // of a non-empty predecessor → empty snap, which is still interesting).
        if !self.show_diff && (self.unreadable || self.content.trim().is_empty()) {
            let msg = if self.unreadable {
                "can't read this file — it may have been moved or deleted"
            } else {
                "this file is empty"
            };
            return div()
                .id("fileview-body")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_y_scroll()
                .overflow_x_hidden()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(SeancePalette::text_faint())
                        .child(msg),
                )
                .into_any_element();
        }

        // Optional "viewing history N/M" hint. When a previous snapshot exists
        // the whole bar is clickable to toggle the unified-diff body.
        // Index check only (no disk read) — snap read happens on toggle/render.
        let can_diff = matches!(self.mode, ViewMode::History(i) if i > 0);
        let hint = if let ViewMode::History(i) = self.mode {
            let mut label = format!("viewing history {}/{}", i + 1, self.snapshots.len());
            if let Some((added, removed)) = self.line_delta() {
                label.push_str(&format!("  ·  +{added}/-{removed} lines vs. previous"));
            }
            if self.show_diff {
                label.push_str("  ·  DIFF");
            }
            if self.changed_since_pin {
                label.push_str("  ·  file has changed since");
            }
            if can_diff {
                if self.show_diff {
                    label.push_str("  ·  click to show content");
                } else {
                    label.push_str("  ·  click to show diff");
                }
            }

            let bar = div()
                .id("fileview-history-hint")
                .flex_none()
                .mx_3()
                .mt_2()
                .px_2()
                .py_1()
                .rounded_md()
                .text_xs()
                .text_color(if self.show_diff {
                    SeancePalette::flame()
                } else {
                    SeancePalette::violet()
                })
                .bg(SeancePalette::surface())
                .when(can_diff, |d| {
                    d.cursor_pointer()
                        .hover(|s| {
                            s.bg(SeancePalette::bg_elevated())
                                .text_color(SeancePalette::flame())
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_diff(cx)))
                })
                .child(label);
            Some(bar)
        } else {
            None
        };

        // Diff body (history only): monospace unified lines vs. previous snap.
        let content: gpui::AnyElement = if self.show_diff {
            if let Some(prev) = self.predecessor_content() {
                self.render_diff_body(&prev)
            } else {
                // Race: predecessor vanished — fall through to normal body.
                self.render_content_body()
            }
        } else {
            self.render_content_body()
        };

        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            .children(hint)
            .child(content)
            .into_any_element()
    }

    /// Normal (non-diff) content: markdown with optional frontmatter box, or
    /// preserved-newline monospace for everything else.
    fn render_content_body(&self) -> gpui::AnyElement {
        if self.is_markdown() {
            let (frontmatter, body) = split_frontmatter(&self.content);
            // Virtualized TextView (scrollable true): only visible blocks paint.
            // Frontmatter is injected as a custom first block so it scrolls away
            // with the doc (same UX as the old outer-scroll layout).
            let source = match frontmatter {
                Some(ref fields) => {
                    let mut s = frontmatter_fence(fields);
                    s.push_str(body);
                    s
                }
                None => body.to_string(),
            };

            let md = markdown(source)
                .style(candlelit_markdown_style())
                .selectable(true)
                .scrollable(true)
                .markdown_block_parser(|node, _cx| {
                    let markdown_ast::Node::Code(code) = node else {
                        return None;
                    };
                    if code.lang.as_deref() != Some("seance-fm") {
                        return None;
                    }
                    let fields = parse_frontmatter_fence(&code.value);
                    Some(
                        MarkdownNode::new("seance-fm", fields)
                            .text("frontmatter")
                            .markdown("```seance-fm\n```"),
                    )
                })
                .markdown_block_renderer("seance-fm", |node, _window, _cx| {
                    if let Some(fields) = node.data::<Vec<(String, String)>>() {
                        render_frontmatter_box(fields).into_any_element()
                    } else {
                        div().into_any_element()
                    }
                })
                .w_full()
                .h_full()
                .min_w_0()
                .text_color(SeancePalette::text());

            // Fixed-height parent required by scrollable TextView (list + scrollbar).
            div()
                .id("fileview-md")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .p_3()
                .bg(SeancePalette::bg())
                .text_color(SeancePalette::text())
                .child(md)
                .into_any_element()
        } else {
            // Virtualized line list — one child per *visible* line. The old
            // path mounted every line as a div; large plain files (logs) choked
            // scroll/resize the same way fit-content markdown did.
            let lines: Vec<String> = self.content.lines().map(|l| l.to_string()).collect();
            let n = lines.len().max(1);
            let list_state = ListState::new(n, ListAlignment::Top, px(64.));
            let text_color = SeancePalette::text();
            div()
                .id("fileview-body")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .p_3()
                .text_sm()
                .text_color(text_color)
                .child(
                    list(list_state, move |ix, _window, _cx| {
                        let line = lines.get(ix).map(|s| s.as_str()).unwrap_or("");
                        if line.is_empty() {
                            div().h(px(14.)).into_any_element()
                        } else {
                            div()
                                .w_full()
                                .child(line.to_string())
                                .into_any_element()
                        }
                    })
                    .size_full(),
                )
                .into_any_element()
        }
    }

    /// Unified-diff body: previous snapshot → current snapshot, hunk-collapsed
    /// (a few context lines around each change — not the whole file).
    /// Monospace regardless of file type so `+/-` columns stay aligned.
    fn render_diff_body(&self, prev: &str) -> gpui::AnyElement {
        let lines = unified_line_diff(prev, &self.content);
        let (n_add, n_del, n_skip) =
            lines
                .iter()
                .fold((0usize, 0usize, 0usize), |(a, d, s), line| match line {
                    DiffLine::Added(_) => (a + 1, d, s),
                    DiffLine::Removed(_) => (a, d + 1, s),
                    DiffLine::Context(_) => (a, d, s),
                    DiffLine::Skip(n) => (a, d, s + n),
                });

        let summary = if n_add == 0 && n_del == 0 {
            "diff vs. previous snapshot  ·  no line changes".to_string()
        } else if n_skip > 0 {
            format!("diff vs. previous snapshot  ·  +{n_add}/-{n_del}  ·  {n_skip} unchanged hidden")
        } else {
            format!("diff vs. previous snapshot  ·  +{n_add}/-{n_del}")
        };

        // Virtualize the diff lines — history diffs on large files were another
        // scroll/resize cliff when every line was a real element.
        let n = lines.len().max(1);
        let list_state = ListState::new(n, ListAlignment::Top, px(64.));
        let lines = std::sync::Arc::new(lines);

        div()
            .id("fileview-diff")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_0()
            .child(
                div()
                    .flex_none()
                    .mb_2()
                    .text_xs()
                    .text_color(SeancePalette::text_dim())
                    .child(summary),
            )
            .child(
                list(list_state, {
                    let lines = lines.clone();
                    move |ix, _window, _cx| {
                        let Some(line) = lines.get(ix) else {
                            return div().into_any_element();
                        };
                        match line {
                            DiffLine::Skip(n) => div()
                                .id(SharedString::from(format!("diff-skip-{ix}")))
                                .flex_none()
                                .w_full()
                                .my_1()
                                .px_2()
                                .py_0p5()
                                .rounded_sm()
                                .bg(SeancePalette::bg_elevated())
                                .border_1()
                                .border_color(SeancePalette::border())
                                .text_xs()
                                .text_color(SeancePalette::text_faint())
                                .child(format!("⋯ {n} unchanged lines"))
                                .into_any_element(),
                            other => {
                                let (prefix, text, color, bg) = match other {
                                    DiffLine::Context(t) => (
                                        "  ",
                                        t.as_str(),
                                        SeancePalette::text_dim(),
                                        None,
                                    ),
                                    DiffLine::Added(t) => (
                                        "+ ",
                                        t.as_str(),
                                        SeancePalette::success(),
                                        Some(hsl_with_alpha(SeancePalette::success(), 0.10)),
                                    ),
                                    DiffLine::Removed(t) => (
                                        "- ",
                                        t.as_str(),
                                        SeancePalette::danger(),
                                        Some(hsl_with_alpha(SeancePalette::danger(), 0.12)),
                                    ),
                                    DiffLine::Skip(_) => unreachable!(),
                                };
                                let display = if text.is_empty() {
                                    format!("{prefix}")
                                } else {
                                    format!("{prefix}{text}")
                                };
                                div()
                                    .id(SharedString::from(format!("diff-line-{ix}")))
                                    .flex_none()
                                    .w_full()
                                    .px_1()
                                    .rounded_sm()
                                    .text_sm()
                                    .text_color(color)
                                    .when_some(bg, |d, c| d.bg(c))
                                    .child(if display.trim().is_empty() {
                                        "  ".to_string()
                                    } else {
                                        display
                                    })
                                    .into_any_element()
                            }
                        }
                    }
                })
                .flex_1()
                .min_h_0()
                .size_full(),
            )
            .into_any_element()
    }
}

/// Tint an Hsla with a low alpha for subtle diff row backgrounds. Theme colors
/// are fully opaque; we rebuild with the given alpha so sage/danger washes
/// sit under the line text without drowning the candlelit ground.
fn hsl_with_alpha(color: gpui::Hsla, alpha: f32) -> gpui::Hsla {
    gpui::Hsla {
        h: color.h,
        s: color.s,
        l: color.l,
        a: alpha,
    }
}

#[cfg(test)]
mod frontmatter_tests {
    use super::{parse_frontmatter_fields, split_frontmatter};

    #[test]
    fn splits_standard_fence() {
        let src = "---\ntitle: Hello\ndate: 2026-07-20\n---\n\n# Body\n";
        let (fm, body) = split_frontmatter(src);
        let fields = fm.expect("frontmatter");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0], ("title".into(), "Hello".into()));
        assert!(body.starts_with("# Body"));
    }

    #[test]
    fn flattens_inline_array() {
        let fields = parse_frontmatter_fields("tags: [seance, gpui, rust]\n");
        assert_eq!(fields[0].1, "seance, gpui, rust");
    }

    #[test]
    fn no_fence_returns_whole_doc() {
        let src = "# Just a heading\n";
        let (fm, body) = split_frontmatter(src);
        assert!(fm.is_none());
        assert_eq!(body, src);
    }
}

#[cfg(test)]
mod diff_tests {
    use super::{collapse_unchanged, unified_line_diff, DiffLine, DIFF_CONTEXT_LINES};

    #[test]
    fn identical_is_empty_collapsed() {
        // No changes → nothing to show (header says "no line changes").
        let d = unified_line_diff("a\nb\n", "a\nb\n");
        assert!(d.is_empty());
    }

    #[test]
    fn pure_insert() {
        let d = unified_line_diff("a\n", "a\nb\n");
        assert_eq!(
            d,
            vec![
                DiffLine::Context("a".into()),
                DiffLine::Added("b".into()),
            ]
        );
    }

    #[test]
    fn pure_delete() {
        let d = unified_line_diff("a\nb\n", "a\n");
        assert_eq!(
            d,
            vec![
                DiffLine::Context("a".into()),
                DiffLine::Removed("b".into()),
            ]
        );
    }

    #[test]
    fn replace_middle() {
        let d = unified_line_diff("a\nold\nc\n", "a\nnew\nc\n");
        assert_eq!(
            d,
            vec![
                DiffLine::Context("a".into()),
                DiffLine::Removed("old".into()),
                DiffLine::Added("new".into()),
                DiffLine::Context("c".into()),
            ]
        );
    }

    #[test]
    fn empty_to_content() {
        let d = unified_line_diff("", "hello\n");
        assert_eq!(d, vec![DiffLine::Added("hello".into())]);
    }

    #[test]
    fn collapses_long_unchanged_middle() {
        // 20 unchanged lines with one edit in the middle → skip gap, not all 20.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..20 {
            old.push_str(&format!("line-{i}\n"));
            if i == 10 {
                new.push_str("CHANGED\n");
            } else {
                new.push_str(&format!("line-{i}\n"));
            }
        }
        let d = unified_line_diff(&old, &new);
        let skips: Vec<usize> = d
            .iter()
            .filter_map(|l| match l {
                DiffLine::Skip(n) => Some(*n),
                _ => None,
            })
            .collect();
        assert!(
            !skips.is_empty(),
            "expected at least one Skip, got {d:?}"
        );
        // Full file is 20 context + 1 remove + 1 add raw; collapsed should be
        // far smaller than 20 context lines.
        let context_count = d
            .iter()
            .filter(|l| matches!(l, DiffLine::Context(_)))
            .count();
        assert!(
            context_count <= DIFF_CONTEXT_LINES * 2,
            "too much context kept: {context_count}"
        );
        assert!(d.iter().any(|l| matches!(l, DiffLine::Removed(_))));
        assert!(d.iter().any(|l| matches!(l, DiffLine::Added(_))));
    }

    #[test]
    fn collapse_merges_nearby_hunks() {
        // Two changes within 2*context of each other → one continuous keep,
        // no Skip between them.
        let raw = vec![
            DiffLine::Context("a".into()),
            DiffLine::Added("x".into()),
            DiffLine::Context("b".into()),
            DiffLine::Context("c".into()),
            DiffLine::Removed("y".into()),
            DiffLine::Context("d".into()),
        ];
        let d = collapse_unchanged(raw, 3);
        assert!(
            !d.iter().any(|l| matches!(l, DiffLine::Skip(_))),
            "nearby hunks should merge: {d:?}"
        );
    }

    #[test]
    fn collapse_skips_distant_gap() {
        let mut raw = Vec::new();
        raw.push(DiffLine::Added("top".into()));
        for i in 0..20 {
            raw.push(DiffLine::Context(format!("mid-{i}")));
        }
        raw.push(DiffLine::Removed("bot".into()));
        let d = collapse_unchanged(raw, 3);
        let skip_total: usize = d
            .iter()
            .filter_map(|l| match l {
                DiffLine::Skip(n) => Some(*n),
                _ => None,
            })
            .sum();
        // 20 mid lines minus 3 after top change minus 3 before bot = 14 skipped.
        assert_eq!(skip_total, 14, "got {d:?}");
    }
}

impl Render for FileView {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        // Non-markdown bodies want a monospace family; markdown brings its own.
        let font_family = if self.is_markdown() {
            None
        } else {
            Some(cx.theme().mono_font_family.clone())
        };

        div()
            .id("fileview")
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(SeancePalette::bg())
            // Diff body always wants mono so +/- columns align; non-md content
            // already asked for mono above. When viewing markdown content (not
            // diff) leave the family alone so headings pick up the UI font.
            .when(self.show_diff, |d| {
                d.font_family(cx.theme().mono_font_family.clone())
            })
            .when_some(font_family, |d, f| d.font_family(f))
            .child(self.render_header(cx))
            .child(self.render_body(cx))
    }
}

/// On-disk history: plain file copies under
/// `~/.local/share/seance/filehist/<hash>/NNNN.snap`.
///
/// `<hash>` is a stable FNV-1a hash of the absolute path, so the same file maps
/// to the same directory across sessions (history persists across re-open) and
/// two different files never collide. Snapshots are plain copies — no diffs, no
/// compression — capped at [`MAX_SNAPSHOTS`]; the oldest are pruned past the cap.
mod history {
    use super::*;

    /// Root of the history store: `~/.local/share/seance/filehist/`.
    fn root() -> PathBuf {
        PathBuf::from(shellexpand::tilde("~/.local/share/seance/filehist").into_owned())
    }

    /// Per-file history directory: `<root>/<fnv-hash-of-abs-path>/`.
    pub fn dir_for(path: &Path) -> PathBuf {
        // Canonicalize when possible so `./foo.md` and `/abs/foo.md` share a
        // dir; fall back to the raw path (canonicalize fails on missing files).
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        root().join(format!("{:016x}", fnv1a(key.to_string_lossy().as_bytes())))
    }

    /// Read the file's modified-time, or `None` if it can't be stat'd.
    pub fn mtime(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    /// List existing snapshot indices in `hist_dir`, oldest first.
    pub fn list(hist_dir: &Path) -> Vec<u32> {
        let mut idxs: Vec<u32> = std::fs::read_dir(hist_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let stem = name.strip_suffix(".snap")?;
                stem.parse::<u32>().ok()
            })
            .collect();
        idxs.sort_unstable();
        idxs
    }

    /// Path to snapshot `idx` inside `hist_dir`.
    fn snap_path(hist_dir: &Path, idx: u32) -> PathBuf {
        hist_dir.join(format!("{idx:04}.snap"))
    }

    /// Read snapshot `idx` back off disk.
    pub fn read(hist_dir: &Path, idx: u32) -> Option<String> {
        std::fs::read_to_string(snap_path(hist_dir, idx)).ok()
    }

    /// Append `content` as the next-numbered snapshot, prune past the cap, and
    /// update `snapshots` in place. Returns the new index, or `None` on write
    /// failure. Idempotent-ish: callers gate on content differing from the
    /// newest snapshot, so we don't record no-op `touch`es.
    pub fn record(hist_dir: &Path, content: &str, snapshots: &mut Vec<u32>) -> Option<u32> {
        let _ = std::fs::create_dir_all(hist_dir);
        let next = snapshots.last().map(|n| n + 1).unwrap_or(0);
        if std::fs::write(snap_path(hist_dir, next), content).is_err() {
            return None;
        }
        snapshots.push(next);

        // Prune oldest beyond the cap.
        while snapshots.len() > MAX_SNAPSHOTS {
            let old = snapshots.remove(0);
            let _ = std::fs::remove_file(snap_path(hist_dir, old));
        }
        Some(next)
    }

    /// FNV-1a, 64-bit — a tiny, dependency-free stable hash for path → dirname.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

/// Count added / removed lines between two texts as a multiset delta. This is a
/// cheap affordance, NOT a real diff: `added` = lines in `new` not accounted for
/// in `old` (by count), `removed` = the converse. Order-insensitive.
fn line_count_delta(old: &str, new: &str) -> (usize, usize) {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for line in old.lines() {
        *counts.entry(line).or_default() += 1;
    }
    for line in new.lines() {
        *counts.entry(line).or_default() -= 1;
    }
    // Positive residual = present in old but not new (removed); negative = added.
    let mut added = 0usize;
    let mut removed = 0usize;
    for v in counts.values() {
        if *v > 0 {
            removed += *v as usize;
        } else if *v < 0 {
            added += (-*v) as usize;
        }
    }
    (added, removed)
}

/// Cap for full LCS line-diff. Above this, fall back to a cheaper greedy match
/// so a pathological multi-megabyte paste doesn't O(n·m) the UI thread.
const LCS_LINE_CAP: usize = 2_500;

/// Context lines kept on each side of a change, matching VS Code / git's
/// default unified-diff radius. Longer unchanged runs collapse to a Skip.
const DIFF_CONTEXT_LINES: usize = 3;

/// Ordered unified line-diff of `old` → `new`, collapsed to hunks. Uses LCS
/// when both sides are under [`LCS_LINE_CAP`]; otherwise a linear greedy pass.
/// Unchanged stretches longer than `2 * DIFF_CONTEXT_LINES` are replaced with
/// a [`DiffLine::Skip`] so the body stays scannable like VS Code's diff editor.
fn unified_line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.is_empty() && b.is_empty() {
        return Vec::new();
    }
    let raw = if a.len() > LCS_LINE_CAP || b.len() > LCS_LINE_CAP {
        greedy_line_diff(&a, &b)
    } else {
        lcs_line_diff(&a, &b)
    };
    collapse_unchanged(raw, DIFF_CONTEXT_LINES)
}

/// Keep only change lines plus `context` lines of padding around each; merge
/// overlapping windows; replace omitted context runs with [`DiffLine::Skip`].
fn collapse_unchanged(lines: Vec<DiffLine>, context: usize) -> Vec<DiffLine> {
    if lines.is_empty() {
        return lines;
    }
    let is_change = |l: &DiffLine| matches!(l, DiffLine::Added(_) | DiffLine::Removed(_));
    if !lines.iter().any(is_change) {
        // Identical content (or only-context): nothing useful to show.
        return Vec::new();
    }
    let n = lines.len();
    let mut keep = vec![false; n];
    for (i, line) in lines.iter().enumerate() {
        if is_change(line) {
            let lo = i.saturating_sub(context);
            let hi = (i + context).min(n - 1);
            for k in lo..=hi {
                keep[k] = true;
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        if keep[i] {
            out.push(lines[i].clone());
            i += 1;
        } else {
            let start = i;
            while i < n && !keep[i] {
                i += 1;
            }
            out.push(DiffLine::Skip(i - start));
        }
    }
    out
}

/// Classic LCS dynamic-programming line diff. O(n·m) time and space.
fn lcs_line_diff(a: &[&str], b: &[&str]) -> Vec<DiffLine> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = LCS length of a[i..] and b[j..]. Stored as (n+1)×(m+1).
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| -> usize { i * (m + 1) + j };
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[idx(i, j)] = if a[i] == b[j] {
                dp[idx(i + 1, j + 1)] + 1
            } else {
                dp[idx(i + 1, j)].max(dp[idx(i, j + 1)])
            };
        }
    }
    let mut out = Vec::with_capacity(n + m);
    let mut i = 0usize;
    let mut j = 0usize;
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine::Context(a[i].to_string()));
            i += 1;
            j += 1;
        } else if dp[idx(i + 1, j)] >= dp[idx(i, j + 1)] {
            out.push(DiffLine::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Added(b[j].to_string()));
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine::Removed(a[i].to_string()));
        i += 1;
    }
    while j < m {
        out.push(DiffLine::Added(b[j].to_string()));
        j += 1;
    }
    out
}

/// Linear greedy fallback: walk both sides with a forward-looking equal-line
/// match window. Correct for pure inserts/deletes and common edit patterns;
/// coarser than LCS when lines are heavily reordered.
fn greedy_line_diff(a: &[&str], b: &[&str]) -> Vec<DiffLine> {
    const LOOKAHEAD: usize = 64;
    let mut out = Vec::with_capacity(a.len() + b.len());
    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push(DiffLine::Context(a[i].to_string()));
            i += 1;
            j += 1;
            continue;
        }
        // Look ahead: is a[i] somewhere soon in b? Prefer treating intervening
        // b-lines as adds. Symmetrically for b[j] in a.
        let a_in_b = if i < a.len() {
            b[j..].iter().take(LOOKAHEAD).position(|&x| x == a[i])
        } else {
            None
        };
        let b_in_a = if j < b.len() {
            a[i..].iter().take(LOOKAHEAD).position(|&x| x == b[j])
        } else {
            None
        };
        match (a_in_b, b_in_a) {
            (Some(bj), Some(ai)) if bj <= ai => {
                // a[i] found sooner in b → emit b adds up to the match.
                for k in 0..bj {
                    out.push(DiffLine::Added(b[j + k].to_string()));
                }
                j += bj;
            }
            (Some(_), Some(ai)) => {
                for k in 0..ai {
                    out.push(DiffLine::Removed(a[i + k].to_string()));
                }
                i += ai;
            }
            (Some(bj), None) => {
                for k in 0..bj {
                    out.push(DiffLine::Added(b[j + k].to_string()));
                }
                j += bj;
            }
            (None, Some(ai)) => {
                for k in 0..ai {
                    out.push(DiffLine::Removed(a[i + k].to_string()));
                }
                i += ai;
            }
            (None, None) => {
                if i < a.len() {
                    out.push(DiffLine::Removed(a[i].to_string()));
                    i += 1;
                }
                if j < b.len() {
                    out.push(DiffLine::Added(b[j].to_string()));
                    j += 1;
                }
            }
        }
    }
    out
}
