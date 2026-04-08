//! Daemon liveness utilities.
//!
//! Provides `probe_daemon` — a synchronous TCP-connect check used by
//! DaemonHarness-class transports before spawning a client process.

pub mod probe;
pub use probe::probe_daemon;
