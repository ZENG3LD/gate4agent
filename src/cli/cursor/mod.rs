//! Cursor Agent CLI bindings — Phase 2 stub.
//!
//! Full parser implementation arrives in Phase 3 after real output capture.
//! The `CursorCommandBuilder` here provides the spawn argv based on the
//! Cursor Agent documentation (assumed to be nearly identical to Claude Code).
//!
//! # IMPORTANT — verify before Phase 3
//! The exact Cursor Agent CLI flags must be confirmed by running
//! `cursor-agent -p --output-format stream-json "hello"` on a real Cursor
//! installation and recording stdout. The assumed argv here may need adjustment.

use crate::cli::traits::CliCommandBuilder;
use crate::transport::SpawnOptions;

/// Builder for the Cursor Agent spawn command.
///
/// Assumed argv (fresh session):
///   `cursor-agent -p --output-format stream-json [--model <m>] [--resume <id>]`
///
/// Prompt is delivered via stdin (same mechanism as Claude Code).
///
/// Phase 3 will validate these flags against real `cursor-agent` output.
/// This stub will be replaced with a verified impl at that point.
/// Pipe-mode spawn builder for Cursor Agent.
///
/// Phase 3 stub — flags unverified against real cursor-agent output.
/// Replace with confirmed argv after live capture in Phase 3.
pub struct CursorPipeBuilder;

impl CliCommandBuilder for CursorPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        // Phase 3 stub — flags unverified against real cursor-agent output.
        // Replace with confirmed argv after live capture.
        let mut cmd = std::process::Command::new("echo");
        cmd.arg("cursor-agent: Phase 3 stub — not yet implemented");
        let _ = opts; // opts will be used in Phase 3 impl
        cmd
    }
}
