//! Session-replay **ring recorder** — the always-on write side of replay.
//!
//! The engine already computes SCG3 full/damage frames for the GUI; this module
//! computes them *again*, recorder-side, against its **own** last-cells state.
//! That duplication is deliberate: the GUI's damage base depends on who is
//! attached, which workspace is selected, and the 60/15fps throttle, so piggy-
//! backing on it would make the recording's damage chain depend on whether a
//! human happened to be watching. The recorder owns an independent chain and is
//! therefore always self-consistent.
//!
//! # Design decisions
//!
//! - **Never block the engine.** The handle is an unbounded [`mpsc::Sender`];
//!   every method is fire-and-forget and silently drops if the thread is gone.
//!   A dead recorder must never take the daemon down.
//! - **Coalesce at 33ms/pane, timer-less.** A newer snapshot inside the window
//!   *replaces* the pending one (a terminal frame is idempotent state, not an
//!   increment — dropping intermediates loses nothing but bytes). A trailing
//!   frame lands via the 100ms `recv_timeout` tick.
//! - **Keyframes are seek points.** FULL on first frame, on resize, every 10s,
//!   and immediately before a human `Input` event — so a player seeking to a
//!   prompt chapter never replays a long damage chain.
//! - **Ring layout is a contract** with the exporter:
//!   `<root>/<pane-slug>/<unix_hour>.srr` for the current hour (raw, starts
//!   with `SRR1`), gzip'd to `<unix_hour>.srr.gz` on rotation. Readers tolerate
//!   a torn tail, so we buffer writes and flush per record batch: losing the
//!   last few bytes on a crash is fine, losing an hour is not.
//! - **Time comes in on the message**, stamped by the handle at call time. That
//!   keeps the thread free of clock reads on the hot path and lets tests drive
//!   rotation/pruning without sleeping.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;
use seance_core::replay::{encode_record, ReplayEvent, KIND_DAMAGE, KIND_EVENT, KIND_FULL, MAGIC};
use seance_core::snapshot::{dirty_rows, encode_grid_bin_ex, CellSnap, GridSnapshot};

/// Max one grid record per pane per this window.
const COALESCE_MS: u64 = 33;
/// Forced keyframe cadence.
const KEYFRAME_MS: u64 = 10_000;
/// Tick so trailing coalesced frames land.
const TICK: Duration = Duration::from_millis(100);
/// Retention by age.
const MAX_AGE_HOURS: u64 = 48;
/// Retention by total bytes under `root`.
const MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Background prune cadence.
const PRUNE_EVERY_MS: u64 = 10 * 60 * 1000;
/// Error log rate limit.
const ERR_EVERY_MS: u64 = 5_000;

const MS_PER_HOUR: u64 = 3_600_000;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// handle
// ---------------------------------------------------------------------------

/// What the engine sends the recorder. `t_ms` is stamped by the handle so the
/// thread never has to guess when something actually happened (and so tests can
/// inject time).
pub(crate) enum Msg {
    Grid { t_ms: u64, snap: Box<GridSnapshot> },
    Event { t_ms: u64, pane: String, ev: Box<ReplayEvent> },
    PaneClosed { t_ms: u64, pane: String },
    Stop,
}

/// Cheap, clonable, non-blocking producer side. Safe to hold anywhere in the
/// engine; a send after the thread dies is a silent no-op by design.
#[derive(Clone)]
pub struct RecorderHandle {
    tx: Sender<Msg>,
}

impl RecorderHandle {
    /// Fire-and-forget: full grid snapshot for a pane (recorder diffs + encodes).
    pub fn grid(&self, snap: GridSnapshot) {
        self.send(Msg::Grid {
            t_ms: now_ms(),
            snap: Box::new(snap),
        });
    }

    /// Fire-and-forget: attributed event for a pane.
    pub fn event(&self, pane: &str, ev: ReplayEvent) {
        self.send(Msg::Event {
            t_ms: now_ms(),
            pane: pane.to_string(),
            ev: Box::new(ev),
        });
    }

    /// Pane is gone — flush + close its segment.
    pub fn pane_closed(&self, pane: &str) {
        self.send(Msg::PaneClosed {
            t_ms: now_ms(),
            pane: pane.to_string(),
        });
    }

    /// Flush everything and stop the thread. Used by `spawn_with_join` callers
    /// (tests, daemon shutdown); after this the handle is inert.
    pub fn shutdown(&self) {
        self.send(Msg::Stop);
    }

    #[allow(dead_code)] // time injection: tests + any future replay-from-log tooling
    pub(crate) fn send_at(&self, msg: Msg) {
        self.send(msg);
    }

    fn send(&self, msg: Msg) {
        // Recorder death must be invisible to the engine.
        let _ = self.tx.send(msg);
    }
}

/// Spawn the recorder thread. `root` = `<state_dir>/replay` (created here).
pub fn spawn(root: PathBuf) -> RecorderHandle {
    spawn_with_join(root).0
}

/// Same as [`spawn`], but hands back the join handle so a caller can
/// `shutdown()` then `join()` deterministically (tests, clean daemon exit).
pub fn spawn_with_join(root: PathBuf) -> (RecorderHandle, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name("seance-recorder".into())
        .spawn(move || {
            let mut rec = Recorder::new(root);
            rec.run(rx);
        })
        .expect("spawn seance-recorder");
    (RecorderHandle { tx }, join)
}

// ---------------------------------------------------------------------------
// recorder thread
// ---------------------------------------------------------------------------

/// Recorder-side mirror of the engine's damage base.
struct LastFrame {
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    title: Option<String>,
    cells: Vec<CellSnap>,
}

struct PaneState {
    dir: PathBuf,
    last: Option<LastFrame>,
    /// Open segment for `hour`; `None` until the first record lands.
    writer: Option<BufWriter<File>>,
    hour: u64,
    last_write_ms: u64,
    last_keyframe_ms: u64,
    /// Coalesced snapshot awaiting its window (keep-latest).
    pending: Option<(u64, GridSnapshot)>,
    /// Next grid record must be a keyframe (post human Input).
    force_full: bool,
}

impl PaneState {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            last: None,
            writer: None,
            hour: 0,
            last_write_ms: 0,
            last_keyframe_ms: 0,
            pending: None,
            force_full: false,
        }
    }
}

struct Recorder {
    root: PathBuf,
    panes: HashMap<String, PaneState>,
    last_prune_ms: u64,
    last_err_ms: u64,
    suppressed_errs: u64,
}

impl Recorder {
    fn new(root: PathBuf) -> Self {
        let mut me = Self {
            root,
            panes: HashMap::new(),
            last_prune_ms: 0,
            last_err_ms: 0,
            suppressed_errs: 0,
        };
        let root = me.root.clone();
        me.check(fs::create_dir_all(&root), "create replay root");
        me
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Msg::Stop) => break,
                Ok(msg) => {
                    let clock = msg_time(&msg);
                    self.flush_due(clock);
                    self.handle(msg);
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = now_ms();
                    self.flush_due(now);
                    if now.saturating_sub(self.last_prune_ms) >= PRUNE_EVERY_MS {
                        self.last_prune_ms = now;
                        self.prune(now);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // Terminal flush: trailing frames must not be lost on shutdown.
        let panes: Vec<String> = self.panes.keys().cloned().collect();
        for slug in panes {
            self.flush_pending(&slug);
            self.close_segment(&slug, false);
        }
    }

    /// One message = one unit of work; nothing here may panic the thread, so
    /// every fallible step goes through `check`.
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Grid { t_ms, snap } => {
                let slug = sanitize(&snap.pane);
                self.ensure_pane(&slug);
                let due = {
                    let st = self.pane(&slug);
                    t_ms.saturating_sub(st.last_write_ms) >= COALESCE_MS
                };
                if due {
                    self.write_grid(&slug, t_ms, *snap);
                } else {
                    // Keep-latest: a newer frame supersedes the pending one.
                    self.pane(&slug).pending = Some((t_ms, *snap));
                }
            }
            Msg::Event { t_ms, pane, ev } => {
                let slug = sanitize(&pane);
                self.ensure_pane(&slug);
                let human = matches!(&*ev, ReplayEvent::Input { origin, .. } if origin == "human");
                if human {
                    // Keyframe *before* the prompt so seeks land cheap, and the
                    // next grid is a keyframe too (the post-submit screen).
                    self.pane(&slug).force_full = true;
                    self.flush_pending(&slug);
                    self.pane(&slug).force_full = true;
                }
                match serde_json::to_vec(&*ev) {
                    Ok(json) => self.write_record(&slug, t_ms, KIND_EVENT, &json),
                    Err(e) => self.err(&format!("event encode: {e}")),
                }
            }
            Msg::PaneClosed { t_ms, pane } => {
                let slug = sanitize(&pane);
                if self.panes.contains_key(&slug) {
                    self.flush_pending(&slug);
                    self.close_segment(&slug, true);
                    self.panes.remove(&slug);
                }
                let _ = t_ms;
            }
            Msg::Stop => {}
        }
    }

    fn pane(&mut self, slug: &str) -> &mut PaneState {
        self.panes
            .get_mut(slug)
            .expect("pane state ensured before use")
    }

    fn ensure_pane(&mut self, slug: &str) {
        if !self.panes.contains_key(slug) {
            let dir = self.root.join(slug);
            self.check(fs::create_dir_all(&dir), "create pane dir");
            self.panes.insert(slug.to_string(), PaneState::new(dir));
        }
    }

    /// Flush any pending grid whose coalesce window has elapsed.
    fn flush_due(&mut self, now: u64) {
        let ready: Vec<String> = self
            .panes
            .iter()
            .filter(|(_, st)| {
                st.pending.is_some() && now.saturating_sub(st.last_write_ms) >= COALESCE_MS
            })
            .map(|(k, _)| k.clone())
            .collect();
        for slug in ready {
            self.flush_pending(&slug);
        }
    }

    fn flush_pending(&mut self, slug: &str) {
        let Some(st) = self.panes.get_mut(slug) else {
            return;
        };
        let Some((t_ms, snap)) = st.pending.take() else {
            return;
        };
        self.write_grid(slug, t_ms, snap);
    }

    // -- encoding ----------------------------------------------------------

    fn write_grid(&mut self, slug: &str, t_ms: u64, snap: GridSnapshot) {
        self.ensure_pane(slug);
        let st = self.pane(slug);
        let cols = snap.cols as usize;
        let rows = snap.rows as usize;

        let mut full = st.force_full
            || st.last.is_none()
            || t_ms.saturating_sub(st.last_keyframe_ms) >= KEYFRAME_MS;
        let mut dirty: Option<Vec<u16>> = None;

        if let Some(prev) = &st.last {
            if prev.cols != snap.cols || prev.rows != snap.rows {
                full = true; // resize invalidates the damage base
            } else if !full {
                let d = dirty_rows(&prev.cells, &snap.cells, cols, rows);
                if d.is_empty() {
                    let moved =
                        prev.cursor_col != snap.cursor_col || prev.cursor_row != snap.cursor_row;
                    if !moved && prev.title == snap.title {
                        // Nothing changed at all — record nothing, keep base.
                        return;
                    }
                    if prev.title != snap.title {
                        full = true; // title lives in the header: needs a full frame
                    } else {
                        dirty = Some(vec![snap.cursor_row]);
                    }
                } else {
                    dirty = Some(d);
                }
            }
        }

        // Mirror `encode_grid_bin_ex`'s own full-vs-damage predicate so the
        // record KIND always matches the frame it wraps.
        let use_damage = !full
            && match &dirty {
                Some(d) => {
                    !d.is_empty()
                        && !snap.cells.is_empty()
                        && d.len() * 2 < rows.max(1)
                        && d.iter().all(|&r| (r as usize) < rows)
                }
                None => false,
            };

        let enc = if use_damage {
            encode_grid_bin_ex(&snap, dirty.as_deref())
        } else {
            encode_grid_bin_ex(&snap, None)
        };
        let bytes = match enc {
            Ok(b) => b,
            Err(e) => {
                self.err(&format!("grid encode: {e}"));
                return;
            }
        };
        let kind = if use_damage { KIND_DAMAGE } else { KIND_FULL };
        self.write_record(slug, t_ms, kind, &bytes);

        let st = self.pane(slug);
        if kind == KIND_FULL {
            st.last_keyframe_ms = t_ms;
        }
        st.force_full = false;
        st.last = Some(LastFrame {
            cols: snap.cols,
            rows: snap.rows,
            cursor_col: snap.cursor_col,
            cursor_row: snap.cursor_row,
            title: snap.title.clone(),
            cells: snap.cells,
        });
    }

    // -- segments ----------------------------------------------------------

    fn write_record(&mut self, slug: &str, t_ms: u64, kind: u8, payload: &[u8]) {
        self.ensure_pane(slug);
        let hour = t_ms / MS_PER_HOUR;
        self.ensure_segment(slug, hour, t_ms);
        let buf = encode_record(kind, t_ms, payload);
        let st = self.pane(slug);
        let res = st.writer.as_mut().map(|w| {
            w.write_all(&buf)?;
            w.flush()
        });
        st.last_write_ms = t_ms;
        if let Some(r) = res {
            self.check(r, "write record");
        }
    }

    fn ensure_segment(&mut self, slug: &str, hour: u64, t_ms: u64) {
        let rotate = {
            let st = self.pane(slug);
            st.writer.is_none() || st.hour != hour
        };
        if !rotate {
            return;
        }
        let rotated = self.pane(slug).writer.is_some();
        if rotated {
            self.close_segment(slug, true);
        }
        let path = self.pane(slug).dir.join(format!("{hour}.srr"));
        let existed = path.exists();
        match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => {
                let mut w = BufWriter::new(f);
                if !existed {
                    // Every segment is a standalone SRR1 stream.
                    let r = w.write_all(MAGIC.as_slice()).and_then(|_| w.flush());
                    self.check(r, "write segment magic");
                }
                let st = self.pane(slug);
                st.writer = Some(w);
                st.hour = hour;
            }
            Err(e) => {
                self.err(&format!("open segment {}: {e}", path.display()));
                let st = self.pane(slug);
                st.writer = None;
                st.hour = hour;
            }
        }
        if rotated {
            self.prune(t_ms);
        }
    }

    /// Close the open segment; `compress` gzips the finished file (rotation and
    /// pane-close), leaving the *current* hour raw for live readers.
    fn close_segment(&mut self, slug: &str, compress: bool) {
        let Some(st) = self.panes.get_mut(slug) else {
            return;
        };
        let Some(mut w) = st.writer.take() else {
            return;
        };
        let r = w.flush();
        drop(w);
        self.check(r, "flush segment");
        if !compress {
            return;
        }
        let dir = self.pane(slug).dir.clone();
        let hour = self.pane(slug).hour;
        self.gzip_segment(&dir.join(format!("{hour}.srr")));
    }

    fn gzip_segment(&mut self, raw: &Path) {
        if !raw.exists() {
            return;
        }
        let gz_path = raw.with_extension("srr.gz");
        let res = (|| -> std::io::Result<()> {
            let data = fs::read(raw)?;
            let out = File::create(&gz_path)?;
            let mut enc = GzEncoder::new(BufWriter::new(out), Compression::default());
            enc.write_all(&data)?;
            let mut inner = enc.finish()?;
            inner.flush()?;
            fs::remove_file(raw)?;
            Ok(())
        })();
        if res.is_err() {
            // Leave the raw segment in place — a readable .srr beats nothing.
            let _ = fs::remove_file(&gz_path);
        }
        self.check(res, "gzip segment");
    }

    // -- retention ---------------------------------------------------------

    fn prune(&mut self, now_ms_val: u64) {
        let cur_hour = now_ms_val / MS_PER_HOUR;
        let open: Vec<PathBuf> = self
            .panes
            .values()
            .filter(|st| st.writer.is_some())
            .map(|st| st.dir.join(format!("{}.srr", st.hour)))
            .collect();

        let mut segs: Vec<(u64, u64, PathBuf)> = Vec::new(); // (hour, bytes, path)
        let Ok(panes) = fs::read_dir(&self.root) else {
            return;
        };
        for pane in panes.flatten() {
            let Ok(files) = fs::read_dir(pane.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                let Some(hour) = segment_hour(&path) else {
                    continue;
                };
                let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
                if hour + MAX_AGE_HOURS < cur_hour && !open.contains(&path) {
                    let r = fs::remove_file(&path);
                    self.check(r, "prune aged segment");
                    continue;
                }
                segs.push((hour, bytes, path));
            }
        }

        // Global cap: oldest-first across all panes until under the ceiling.
        let mut total: u64 = segs.iter().map(|(_, b, _)| *b).sum();
        if total <= MAX_BYTES {
            return;
        }
        segs.sort_by_key(|(hour, _, _)| *hour);
        for (_, bytes, path) in segs {
            if total <= MAX_BYTES {
                break;
            }
            if open.contains(&path) {
                continue; // never unlink the segment we're writing
            }
            let r = fs::remove_file(&path);
            if r.is_ok() {
                total = total.saturating_sub(bytes);
            }
            self.check(r, "prune oversize segment");
        }
    }

    // -- errors ------------------------------------------------------------

    fn check<T, E: std::fmt::Display>(&mut self, r: Result<T, E>, what: &str) {
        if let Err(e) = r {
            self.err(&format!("{what}: {e}"));
        }
    }

    /// Rate-limited stderr; a broken disk must not turn into a log firehose.
    fn err(&mut self, msg: &str) {
        let now = now_ms();
        if now.saturating_sub(self.last_err_ms) < ERR_EVERY_MS {
            self.suppressed_errs += 1;
            return;
        }
        let suppressed = std::mem::take(&mut self.suppressed_errs);
        self.last_err_ms = now;
        if suppressed > 0 {
            eprintln!("[seance recorder] {msg} (+{suppressed} suppressed)");
        } else {
            eprintln!("[seance recorder] {msg}");
        }
    }
}

fn msg_time(msg: &Msg) -> u64 {
    match msg {
        Msg::Grid { t_ms, .. } | Msg::Event { t_ms, .. } | Msg::PaneClosed { t_ms, .. } => *t_ms,
        Msg::Stop => now_ms(),
    }
}

/// Pane slugs are already filesystem-safe; this is a defensive backstop so a
/// malformed slug can never escape the replay root.
fn sanitize(slug: &str) -> String {
    let s: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() || s.trim_matches('.').is_empty() {
        "pane".into()
    } else {
        s
    }
}

/// `<hour>.srr` / `<hour>.srr.gz` → hour.
fn segment_hour(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".srr.gz")
        .or_else(|| name.strip_suffix(".srr"))?;
    stem.parse().ok()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seance_core::replay::records;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_root(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "seance-rec-{tag}-{}-{}-{n}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn grid(pane: &str, rev: u64, mark: char) -> GridSnapshot {
        let mut s = GridSnapshot::empty(pane);
        s.cols = 8;
        s.rows = 3;
        s.rev = rev;
        s.cells = vec![CellSnap::blank(); 24];
        s.cells[8].c = mark; // row 1 only → damage-eligible
        s
    }

    fn send_grid(h: &RecorderHandle, t_ms: u64, snap: GridSnapshot) {
        h.send_at(Msg::Grid {
            t_ms,
            snap: Box::new(snap),
        });
    }

    fn send_event(h: &RecorderHandle, t_ms: u64, pane: &str, ev: ReplayEvent) {
        h.send_at(Msg::Event {
            t_ms,
            pane: pane.into(),
            ev: Box::new(ev),
        });
    }

    fn read_segment(root: &Path, pane: &str, hour: u64) -> Vec<u8> {
        fs::read(root.join(pane).join(format!("{hour}.srr"))).expect("segment exists")
    }

    /// Base timestamp aligned to an hour boundary so tests control rotation.
    const T0: u64 = 1_000 * MS_PER_HOUR;

    #[test]
    fn records_land_and_parse() {
        let root = tmp_root("land");
        let (h, join) = spawn_with_join(root.clone());

        send_grid(&h, T0, grid("p1", 1, 'a'));
        send_grid(&h, T0 + 100, grid("p1", 2, 'b'));
        send_event(
            &h,
            T0 + 200,
            "p1",
            ReplayEvent::Status {
                state: "busy".into(),
                note: None,
            },
        );
        h.shutdown();
        join.join().unwrap();

        let data = read_segment(&root, "p1", 1_000);
        let recs: Vec<_> = records(&data).expect("SRR1 magic").collect();
        assert_eq!(recs.len(), 3, "got {:?}", recs.len());
        assert_eq!(recs[0].kind, KIND_FULL, "first frame must be a keyframe");
        assert_eq!(recs[0].t_ms, T0);
        assert_eq!(recs[1].kind, KIND_DAMAGE, "second frame diffs to damage");
        assert_eq!(recs[2].kind, KIND_EVENT);
        let ev: ReplayEvent = serde_json::from_slice(recs[2].payload).unwrap();
        assert!(matches!(ev, ReplayEvent::Status { .. }));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unchanged_grid_is_skipped() {
        let root = tmp_root("skip");
        let (h, join) = spawn_with_join(root.clone());
        send_grid(&h, T0, grid("p1", 1, 'a'));
        send_grid(&h, T0 + 100, grid("p1", 1, 'a')); // identical
        h.shutdown();
        join.join().unwrap();

        let data = read_segment(&root, "p1", 1_000);
        assert_eq!(records(&data).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coalesce_keeps_latest_within_window() {
        let root = tmp_root("coalesce");
        let (h, join) = spawn_with_join(root.clone());
        send_grid(&h, T0, grid("p1", 1, 'a'));
        // Both inside the 33ms window after the first write: only the last one
        // survives, and it lands on shutdown flush.
        send_grid(&h, T0 + 5, grid("p1", 2, 'b'));
        send_grid(&h, T0 + 10, grid("p1", 3, 'c'));
        h.shutdown();
        join.join().unwrap();

        let data = read_segment(&root, "p1", 1_000);
        let recs: Vec<_> = records(&data).unwrap().collect();
        assert_eq!(recs.len(), 2, "intermediate frame must be superseded");
        assert_eq!(recs[1].t_ms, T0 + 10);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn human_input_forces_keyframe_before_event() {
        let root = tmp_root("keyframe");
        let (h, join) = spawn_with_join(root.clone());
        send_grid(&h, T0, grid("p1", 1, 'a'));
        // Pending (inside the window) — must be flushed as FULL, then the event.
        send_grid(&h, T0 + 5, grid("p1", 2, 'b'));
        send_event(
            &h,
            T0 + 6,
            "p1",
            ReplayEvent::Input {
                origin: "human".into(),
                bytes_b64: "aGk=".into(),
            },
        );
        // Next grid is a keyframe too (post-submit screen).
        send_grid(&h, T0 + 500, grid("p1", 3, 'c'));
        h.shutdown();
        join.join().unwrap();

        let data = read_segment(&root, "p1", 1_000);
        let recs: Vec<_> = records(&data).unwrap().collect();
        assert_eq!(recs.len(), 4);
        assert_eq!(recs[0].kind, KIND_FULL);
        assert_eq!(recs[1].kind, KIND_FULL, "keyframe precedes the prompt");
        assert_eq!(recs[2].kind, KIND_EVENT);
        assert_eq!(recs[3].kind, KIND_FULL, "grid after human input is full");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resize_forces_full() {
        let root = tmp_root("resize");
        let (h, join) = spawn_with_join(root.clone());
        send_grid(&h, T0, grid("p1", 1, 'a'));
        let mut wide = grid("p1", 2, 'a');
        wide.cols = 12;
        wide.cells = vec![CellSnap::blank(); 36];
        send_grid(&h, T0 + 100, wide);
        h.shutdown();
        join.join().unwrap();

        let data = read_segment(&root, "p1", 1_000);
        let recs: Vec<_> = records(&data).unwrap().collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1].kind, KIND_FULL);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hour_rotation_gzips_finished_segment() {
        let root = tmp_root("rotate");
        let (h, join) = spawn_with_join(root.clone());
        send_grid(&h, T0, grid("p1", 1, 'a'));
        send_grid(&h, T0 + MS_PER_HOUR, grid("p1", 2, 'b'));
        h.shutdown();
        join.join().unwrap();

        let pane_dir = root.join("p1");
        assert!(
            pane_dir.join("1000.srr.gz").exists(),
            "finished hour must be gzipped"
        );
        assert!(!pane_dir.join("1000.srr").exists(), "raw must be removed");
        assert!(
            pane_dir.join("1001.srr").exists(),
            "current hour stays raw for live readers"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pane_closed_closes_and_compresses() {
        let root = tmp_root("closed");
        let (h, join) = spawn_with_join(root.clone());
        send_grid(&h, T0, grid("p1", 1, 'a'));
        h.send_at(Msg::PaneClosed {
            t_ms: T0 + 10,
            pane: "p1".into(),
        });
        h.shutdown();
        join.join().unwrap();

        assert!(root.join("p1").join("1000.srr.gz").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_removes_aged_segments() {
        let root = tmp_root("prune");
        let old_dir = root.join("p1");
        fs::create_dir_all(&old_dir).unwrap();
        // 1000 - 48 - 1 = 951: comfortably outside the 48h window.
        let stale = old_dir.join("951.srr.gz");
        fs::write(&stale, b"junk").unwrap();

        let (h, join) = spawn_with_join(root.clone());
        // A rotation triggers a prune pass.
        send_grid(&h, T0, grid("p1", 1, 'a'));
        send_grid(&h, T0 + MS_PER_HOUR, grid("p1", 2, 'b'));
        h.shutdown();
        join.join().unwrap();

        assert!(!stale.exists(), "aged segment must be pruned");
        assert!(root.join("p1").join("1001.srr").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn slug_sanitization_stays_inside_root() {
        assert_eq!(sanitize("w-1.term_2"), "w-1.term_2");
        // A '/' can never survive — "../evil" collapses to one safe component.
        assert_eq!(sanitize("../evil"), "..-evil");
        assert_eq!(sanitize(""), "pane");
        assert_eq!(sanitize(".."), "pane");
    }

    #[test]
    fn send_after_thread_exit_is_silent() {
        let root = tmp_root("dead");
        let (h, join) = spawn_with_join(root.clone());
        h.shutdown();
        join.join().unwrap();
        // Must not panic: recorder death is invisible to the engine.
        h.grid(grid("p1", 1, 'a'));
        h.event("p1", ReplayEvent::Exited { code: Some(0) });
        h.pane_closed("p1");
        let _ = fs::remove_dir_all(&root);
    }
}
