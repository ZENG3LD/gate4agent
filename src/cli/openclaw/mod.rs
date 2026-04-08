//! OpenClaw CLI bindings — Phase 2 stub.
//!
//! OpenClaw uses a `DaemonHarness` transport: the openclaw daemon must be
//! pre-running, and `acpx openclaw --format json` is used as the client
//! command. Full implementation arrives in Phase 4 after daemon liveness probe
//! design and real output capture.
//!
//! # IMPORTANT — verify before Phase 4
//! The exact `acpx` flags and the daemon health endpoint URL must be confirmed
//! by inspecting the openclaw daemon manually (port, `/health` route, etc.).

use crate::cli::traits::CliCommandBuilder;
use crate::transport::SpawnOptions;

/// Builder for the OpenClaw client spawn command.
///
/// Assumed argv (fresh session):
///   `acpx openclaw --format json "<prompt>"`
///
/// Phase 4 will validate against a live openclaw daemon.
/// This stub will be replaced with a verified impl at that point.
/// Spawn builder for OpenClaw (DaemonHarness transport).
///
/// Phase 4 stub — DaemonHarness transport not yet implemented.
/// Replace with confirmed argv after live daemon capture in Phase 4.
pub struct OpenClawPipeBuilder;

impl CliCommandBuilder for OpenClawPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        // Phase 4 stub — DaemonHarness transport not yet implemented.
        // Replace with confirmed argv after live daemon capture.
        let mut cmd = std::process::Command::new("echo");
        cmd.arg("openclaw: Phase 4 stub — not yet implemented");
        let _ = opts; // opts will be used in Phase 4 impl
        cmd
    }
}
