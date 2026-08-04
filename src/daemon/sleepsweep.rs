//! Automatic sleep: circles nobody has touched in 12h stop costing RAM.
//!
//! Deliberately dumb, and deliberately conservative. It only ever sleeps a
//! circle that could be woken back exactly (`Engine::workspace_restorable` —
//! claude panes with a live conversation id, and file panes), and it reads the
//! daemon's own activity clocks, so "12h idle" means the same thing the
//! sidebar row means. A circle with a shell in it never qualifies; neither
//! does one whose clock was never stamped, because no observation is not
//! evidence of idleness.
//!
//! Manual sleep (`seance ctl sleep`, right-click → sleep) is the primary path;
//! this is the backstop for the ones you forget.

use std::time::Duration;

use crate::runtime::engine::AUTO_SLEEP_IDLE_MS;

use super::SharedEngine;

/// How often to look. Idleness is measured in hours — checking every few
/// minutes is already far finer-grained than the threshold.
const SWEEP: Duration = Duration::from_secs(300);

/// Start the sweep thread. `idle_ms` is the threshold ([`AUTO_SLEEP_IDLE_MS`]).
pub fn start_sleep_sweeper(engine: SharedEngine) {
    start_sleep_sweeper_with(engine, AUTO_SLEEP_IDLE_MS, SWEEP)
}

pub fn start_sleep_sweeper_with(engine: SharedEngine, idle_ms: u64, every: Duration) {
    std::thread::Builder::new()
        .name("seance-sleep-sweep".into())
        .spawn(move || loop {
            std::thread::sleep(every);
            let Ok(mut eng) = engine.lock() else { continue };
            let slept = eng.auto_sleep_sweep(idle_ms);
            if !slept.is_empty() {
                eprintln!(
                    "[seance daemon] auto-slept idle circles: {}",
                    slept.join(", ")
                );
                eng.persist();
                eng.push_state_to_all();
            }
        })
        .ok();
}
