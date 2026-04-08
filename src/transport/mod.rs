//! Transport layer — unified entry point for spawning CLI agent processes.
//!
//! Phase 2: `SpawnOptions` struct used by per-CLI `build_command` functions.
//! Phase 4: `DaemonProbe`, `DaemonSpec`, `ensure_daemon_running` for daemon-backed tools.
//! Phase 5: `TransportSession` unified entry point + `pipe_runner` with SessionEnd synthesis.

mod options;
pub mod daemon_spec;
pub mod daemon_runner;
pub(crate) mod pipe_runner;
pub mod session;

pub use options::SpawnOptions;
pub use daemon_spec::{DaemonProbe, DaemonSpec};
pub use daemon_runner::ensure_daemon_running;
pub use session::TransportSession;
