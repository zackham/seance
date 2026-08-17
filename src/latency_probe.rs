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

/// Accumulate a volume (bytes, frames) and print totals + a per-second rate
/// every ~5s, tagged `[seance vol]`.
///
/// Deliberately *not* [`record`]: that formats every sample as milliseconds,
/// and a byte count printed as "5.7ms" is an instrument that lies. Wire volume
/// is the number that had to be reconstructed from `ss` counters the last time
/// this got debugged, so it is worth its own line.
pub fn count(stat: &'static str, n: u64) {
    static VOL: OnceLock<Mutex<HashMap<&'static str, (u64, u64, Instant)>>> = OnceLock::new();
    let mut g = VOL
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let e = g.entry(stat).or_insert_with(|| (0, 0, Instant::now()));
    e.0 += n;
    e.1 += 1;
    let elapsed = e.2.elapsed();
    if elapsed >= Duration::from_secs(5) {
        let secs = elapsed.as_secs_f64();
        eprintln!(
            "[seance vol] {stat}: n={} total={:.1}KB rate={:.1}KB/s avg={:.0}B",
            e.1,
            e.0 as f64 / 1024.0,
            e.0 as f64 / 1024.0 / secs,
            e.0 as f64 / e.1.max(1) as f64,
        );
        *e = (0, 0, Instant::now());
    }
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
