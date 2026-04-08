//! Transport layer types for spawning CLI agent processes.
//!
//! Phase 2 deliverable: `SpawnOptions` struct used by per-CLI `build_command`
//! functions. The full `TransportSession` unified entry point arrives in Phase 5.
//!
//! Phase 4 additions: `DaemonProbe`, `DaemonSpec`, and `ensure_daemon_running`
//! for DaemonHarness-class transports (OpenClaw).

mod options;
pub mod daemon_spec;
pub mod daemon_runner;

pub use options::SpawnOptions;
pub use daemon_spec::{DaemonProbe, DaemonSpec};
pub use daemon_runner::ensure_daemon_running;
