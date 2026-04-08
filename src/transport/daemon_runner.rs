//! Pre-spawn daemon liveness gate for DaemonHarness transports.
//!
//! Phase 4 deliverable. The actual client spawn is handled by the existing
//! `PipeProcess::new_with_options` path — this function is the pre-spawn gate.
//!
//! Phase 5 `TransportSession` will call `ensure_daemon_running` before
//! dispatching to the pipe runner for daemon-class tools.

use crate::daemon::probe_daemon;
use crate::error::AgentError;
use crate::transport::DaemonProbe;

/// Probe daemon liveness, returning early if the daemon is unreachable.
///
/// Call this before spawning a daemon-class client (e.g. `acpx openclaw`).
/// Returns `Ok(())` if the daemon is accepting TCP connections, or an
/// appropriate `AgentError` variant if not.
///
/// See `probe_daemon` in `crate::daemon::probe` for the full error semantics.
pub fn ensure_daemon_running(probe: &DaemonProbe) -> Result<(), AgentError> {
    probe_daemon(probe)
}
