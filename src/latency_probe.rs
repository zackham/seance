//! Typing-latency probes shared by the daemon and the GUI (same binary,
//! different processes). Always on: samples only accrue when keys are in
//! flight, so the idle cost is one hashmap lookup per grid event.
//!
//! Model: `mark(chan, key)` stamps "a keystroke for `key` is in flight" (the
//! FIRST unanswered keystroke wins — later marks don't reset the clock, so a
//! burst reports the worst key, not the last). `complete(chan, key, stat)`
//! resolves it into the named aggregate; `transfer` re-homes the original
//! stamp into another channel (e.g. GUI key→apply hands off to key→paint).
//! Aggregates print p50/p95/max every ~5s to stderr, tagged
//! `[seance lat]` — daemon stderr lands in daemon-upgrade.log, GUI stderr in
//! gui.stderr.log.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static PENDING: OnceLock<Mutex<HashMap<(&'static str, String), Instant>>> = OnceLock::new();
static STATS: OnceLock<Mutex<HashMap<&'static str, Agg>>> = OnceLock::new();

struct Agg {
    samples: Vec<u64>, // micros
    since: Instant,
}

fn pending() -> &'static Mutex<HashMap<(&'static str, String), Instant>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stamp the first in-flight keystroke for `key` on channel `chan`.
pub fn mark(chan: &'static str, key: &str) {
    let mut g = pending().lock().unwrap();
    g.entry((chan, key.to_string()))
        .or_insert_with(Instant::now);
    // Bound: dead panes / never-completed marks must not leak forever.
    if g.len() > 256 {
        let cutoff = Instant::now() - Duration::from_secs(30);
        g.retain(|_, t| *t > cutoff);
    }
}

/// Resolve an in-flight mark into aggregate `stat`. No-op when nothing is
/// pending (the overwhelmingly common case for grid pushes).
pub fn complete(chan: &'static str, key: &str, stat: &'static str) {
    let t = {
        let mut g = pending().lock().unwrap();
        g.remove(&(chan, key.to_string()))
    };
    if let Some(t) = t {
        record(stat, t.elapsed().as_micros() as u64);
    }
}

/// Move an in-flight mark to another channel, preserving the original stamp.
/// Returns true when a mark was transferred.
pub fn transfer(from: &'static str, to: &'static str, key: &str) -> Option<Instant> {
    let mut g = pending().lock().unwrap();
    let t = g.remove(&(from, key.to_string()))?;
    g.insert((to, key.to_string()), t);
    Some(t)
}

/// Record a raw duration sample into aggregate `stat`.
pub fn record(stat: &'static str, micros: u64) {
    let mut g = STATS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let agg = g.entry(stat).or_insert_with(|| Agg {
        samples: Vec::new(),
        since: Instant::now(),
    });
    agg.samples.push(micros);
    if agg.since.elapsed() >= Duration::from_secs(5) {
        agg.samples.sort_unstable();
        let n = agg.samples.len();
        let pick = |q: f64| agg.samples[((n - 1) as f64 * q) as usize] as f64 / 1000.0;
        eprintln!(
            "[seance lat] {stat}: n={n} p50={:.1}ms p95={:.1}ms max={:.1}ms",
            pick(0.5),
            pick(0.95),
            agg.samples[n - 1] as f64 / 1000.0,
        );
        agg.samples.clear();
        agg.since = Instant::now();
    }
}
