//! Pre-spawn daemon liveness gate and daemon-backed tool runner.
//!
//! Phase 4 delivered `ensure_daemon_running` — the liveness probe gate.
//! Phase 5 adds `run_daemon` — the full spawn path for daemon-class tools
//! (currently OpenClaw via the `acpx` client).
//!
//! # Design
//!
//! `run_daemon` first calls `ensure_daemon_running` to verify the daemon is
//! reachable, then delegates to [`pipe_runner::run_pipe`] for the actual
//! process spawn. The `acpx` client communicates over NDJSON like any other
//! pipe-mode CLI; the only difference is the pre-spawn daemon check.

use crate::daemon::probe_daemon;
use crate::error::AgentError;
use crate::transport::pipe_runner::{run_pipe, PipeRunnerHandle};
use crate::transport::{DaemonProbe, SpawnOptions};
use crate::types::CliTool;

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

/// Spawn a daemon-backed CLI process and return a [`PipeRunnerHandle`].
///
/// 1. Calls [`ensure_daemon_running`] against `probe`.
///    Returns `Err(DaemonNotRunning)` or `Err(DaemonProbeTimeout)` on failure.
/// 2. Delegates to [`run_pipe`] to spawn the client command (e.g. `acpx openclaw`).
///
/// The `tool` determines which `CliCommandBuilder` and NDJSON parser are selected,
/// just as with `run_pipe`. For OpenClaw, this selects `OpenClawPipeBuilder` and
/// `OpenClawNdjsonParser`.
pub fn run_daemon(
    tool: CliTool,
    opts: SpawnOptions,
    probe: &DaemonProbe,
) -> Result<PipeRunnerHandle, AgentError> {
    ensure_daemon_running(probe)?;
    run_pipe(tool, opts)
}
