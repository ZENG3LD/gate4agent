//! Cursor Agent CLI bindings — pipe mode spawn builder.
//!
//! Cursor Agent is a standalone CLI (separate from the Cursor IDE) launched in
//! January 2026. It exposes a headless print mode via `-p` that mirrors Claude
//! Code's interface, including the same `--output-format stream-json` NDJSON
//! schema.
//!
//! # Source documentation
//! - https://cursor.com/docs/cli/headless
//! - https://cursor.com/docs/cli/overview
//! - https://cursor.com/blog/cli (January 2026 announcement)
//!
//! # Argv shape (built from docs, not from live capture)
//!
//! Fresh session:
//! ```text
//! cursor-agent -p --output-format stream-json [--model <m>] [<extra>...] "<prompt>"
//! ```
//!
//! Resumed session:
//! ```text
//! cursor-agent -p --output-format stream-json [--model <m>] --resume <id> [<extra>...] "<prompt>"
//! ```
//!
//! Unlike Claude Code (which reads the prompt from stdin after `-p`), Cursor Agent
//! docs indicate the prompt is delivered as a positional argument at the end of the
//! argv list. This is the canonical invocation for `-p` mode per the Cursor CLI docs.
//!
//! If real output differs from the assumed shape, reconcile in a future patch
//! (see Phase 3 risk note in transport-expansion.md).

use crate::cli::traits::CliCommandBuilder;
use crate::transport::SpawnOptions;

/// Pipe-mode spawn builder for Cursor Agent.
///
/// Argv produced (fresh session, from docs):
/// ```text
/// cursor-agent -p --output-format stream-json [--model <m>] [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session):
/// ```text
/// cursor-agent -p --output-format stream-json [--model <m>] --resume <id> [<extra>...] "<prompt>"
/// ```
///
/// # Prompt delivery
///
/// The prompt is appended as the final positional argv token, NOT written to
/// stdin. This differs from `ClaudePipeBuilder` (which uses stdin). The choice
/// is based on the Cursor CLI docs (https://cursor.com/docs/cli/headless) which
/// show `cursor-agent -p "<prompt>"` as the canonical form for `-p` mode.
///
/// # Field sources
/// All flag names taken from: https://cursor.com/docs/cli/headless
/// - `-p` / `--print`: non-interactive print mode
/// - `--output-format stream-json`: NDJSON event stream, Claude-compatible schema
/// - `--resume <session_id>`: resume by session UUID
/// - `--model <name>`: model selection
pub struct CursorPipeBuilder;

impl CliCommandBuilder for CursorPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("cursor-agent");
        cmd.arg("-p");
        cmd.arg("--output-format");
        cmd.arg("stream-json");

        if let Some(ref model) = opts.model {
            cmd.arg("--model");
            cmd.arg(model);
        }
        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        // Prompt as final positional arg (docs-canonical for -p mode).
        // Differs from ClaudePipeBuilder which uses stdin delivery.
        cmd.arg(&opts.prompt);
        cmd
    }
}
