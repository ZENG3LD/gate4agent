//! OpenCode CLI bindings — Phase 2 stub.
//!
//! OpenCode (`sst/opencode` v1.4.0+) uses a 5-event NDJSON schema distinct
//! from Claude/Gemini stream-json. Full parser implementation arrives in Phase 3
//! after real output capture.
//!
//! # IMPORTANT — verify before Phase 3
//! The exact OpenCode CLI flags must be confirmed by running
//! `opencode run "hello" --format json` on a real OpenCode installation and
//! recording stdout. Field names (`step_id`? `id`?) are not confirmed.

use crate::cli::traits::CliCommandBuilder;
use crate::transport::SpawnOptions;

/// Builder for the OpenCode spawn command.
///
/// Assumed argv (fresh session):
///   `opencode run "<prompt>" --format json`
///
/// Assumed argv (resumed session):
///   `opencode run "<prompt>" --format json --session <ses_XXXX>`
///
/// Phase 3 will validate these flags against real `opencode run` output.
/// This stub will be replaced with a verified impl at that point.
/// Pipe-mode spawn builder for OpenCode.
///
/// Phase 3 stub — flags unverified against real opencode output.
/// Replace with confirmed argv after live capture in Phase 3.
pub struct OpenCodePipeBuilder;

impl CliCommandBuilder for OpenCodePipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        // Phase 3 stub — flags unverified against real opencode output.
        // Replace with confirmed argv after live capture.
        let mut cmd = std::process::Command::new("echo");
        cmd.arg("opencode: Phase 3 stub — not yet implemented");
        let _ = opts; // opts will be used in Phase 3 impl
        cmd
    }
}
