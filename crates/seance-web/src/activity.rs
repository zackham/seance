//! Activity drawer — stub; the real drawer replaces this file.

use crate::state::ClientState;

pub fn toggle(_state: &ClientState) {}

pub fn is_open() -> bool {
    false
}

/// Re-render if open (called when badges/activity change).
pub fn refresh(_state: &ClientState) {}

/// Close if open; true when something was closed.
pub fn close() -> bool {
    false
}
