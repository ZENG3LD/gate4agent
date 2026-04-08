//! OpenCode CLI bindings — pipe mode spawn builder.
//!
//! OpenCode (`sst/opencode`, later `opencode-ai/opencode`) v1.4.0+ provides a
//! structured pipe mode via `opencode run --format json`. It emits NDJSON with
//! a 5-event schema distinct from Claude/Cursor stream-json.
//!
//! # Source documentation
//! - https://opencode.ai/docs/cli/
//! - https://github.com/opencode-ai/opencode
//! - https://deepwiki.com/sst/opencode/6.1-command-line-interface-(cli)
//!
//! # Argv shape (built from docs, not from live capture)
//!
//! Fresh session:
//! ```text
//! opencode run --format json [<extra>...] "<prompt>"
//! ```
//!
//! Resumed session:
//! ```text
//! opencode run --format json --session <ses_XXXX> [<extra>...] "<prompt>"
//! ```
//!
//! Session IDs use the `ses_XXXX` prefix. The `--session` / `-s` flag resumes
//! by exact session ID. `-c` / `--continue` resumes the most recent session, but
//! since gate4agent tracks explicit IDs, we always use `--session`.
//!
//! If real output differs from the assumed shape, reconcile in a future patch.

use crate::cli::traits::CliCommandBuilder;
use crate::transport::SpawnOptions;

/// Pipe-mode spawn builder for OpenCode.
///
/// Argv produced (fresh session, from docs):
/// ```text
/// opencode run --format json [<extra>...] "<prompt>"
/// ```
///
/// Argv produced (resumed session):
/// ```text
/// opencode run --format json --session <ses_XXXX> [<extra>...] "<prompt>"
/// ```
///
/// # Prompt delivery
///
/// The prompt is appended as the final positional argv token.
/// OpenCode does not use stdin for the one-shot `run` invocation.
///
/// # Field sources
/// All flag names taken from: https://opencode.ai/docs/cli/
/// - `run`: non-interactive run subcommand
/// - `--format json` / `-f json`: NDJSON output mode
/// - `--session <id>` / `-s <id>`: resume specific session by ID
/// - `--continue` / `-c`: resume most recent session (not used here — we need explicit IDs)
pub struct OpenCodePipeBuilder;

impl CliCommandBuilder for OpenCodePipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("opencode");
        cmd.arg("run");
        cmd.arg("--format");
        cmd.arg("json");

        if let Some(ref session_id) = opts.resume_session_id {
            cmd.arg("--session");
            cmd.arg(session_id);
        }
        for arg in &opts.extra_args {
            cmd.arg(arg);
        }
        // Prompt as final positional arg.
        cmd.arg(&opts.prompt);
        cmd
    }
}
