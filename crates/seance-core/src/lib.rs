//! # seance-core — the sans-io heart of seance
//!
//! Everything a seance client needs that is *pure state and bytes*: the
//! JSON-lines wire protocol (control plane + GUI plane), the SCG3 binary grid
//! codec, and keyboard/mouse → PTY-byte encoding. No sockets, no threads, no
//! platform APIs — transport is injected by the consumer:
//!
//! - the native GPUI app speaks it over a unix socket,
//! - the web client speaks it over a websocket (compiled to wasm32),
//! - a future iOS client speaks it over whatever Network.framework offers.
//!
//! **Discipline: this crate must always compile for `wasm32-unknown-unknown`
//! and native with zero features.** If a change needs `libc`, files, or a
//! clock, it belongs in the consuming crate, not here.

pub mod auth;
pub mod control;
pub mod input;
pub mod protocol;
pub mod snapshot;
