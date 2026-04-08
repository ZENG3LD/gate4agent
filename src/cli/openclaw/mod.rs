//! OpenClaw CLI bindings — Phase 4 implementation.
//!
//! OpenClaw uses a DaemonHarness transport: the openclaw daemon must be
//! pre-running, and the `acpx` client binary is used to communicate with it.
//!
//! # Client invocation
//! ```text
//! acpx openclaw --format json "<prompt>"
//! acpx openclaw --format json --session <id> "<prompt>"   # resume
//! ```
//!
//! # Daemon default endpoint
//! Assumed: `127.0.0.1:8787` — the acpx default agent gateway port per docs.
//! This should be verified against `acpx --help` or openclaw source when a
//! live install is available. See `default_daemon_probe()`.
//!
//! # Resume flow
//! The OpenClaw resume flow is not finalized in available documentation.
//! When `resume_session_id` is present, `--session <id>` is passed as a
//! best-effort guess based on the ACP protocol convention. This should be
//! confirmed against a live install; the flag name may differ.
//!
//! # Output format
//! OpenClaw output is NDJSON, assumed ACP stream-json-compatible
//! (same shape as Claude Code `--output-format stream-json`). See
//! `OpenClawNdjsonParser` in `crate::ndjson::parsers`.
//!
//! # References
//! - docs/research/cli-agents-headless-modes-2026.md — OpenClaw / acpx section

use crate::cli::traits::CliCommandBuilder;
use crate::transport::{DaemonProbe, DaemonSpec, SpawnOptions};

/// Builder for the OpenClaw client spawn command.
///
/// Produces: `acpx openclaw --format json [--session <id>] "<prompt>"`
///
/// The daemon must be pre-running before this command is spawned.
/// Use `ensure_daemon_running(&default_daemon_probe())` as the pre-spawn gate.
pub struct OpenClawPipeBuilder;

impl CliCommandBuilder for OpenClawPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("acpx");

        // Subcommand: acpx openclaw
        cmd.arg("openclaw");

        // JSON output format — assumed flag name per ACP client convention.
        // Source: docs/research/cli-agents-headless-modes-2026.md, OpenClaw section.
        // Assumed per research doc, not confirmed against live capture.
        cmd.arg("--format");
        cmd.arg("json");

        // Resume session if requested.
        // The OpenClaw resume flow is not finalized in documentation.
        // "--session <id>" is a best-effort guess based on ACP protocol convention.
        // Assumed per research doc, not confirmed against live capture.
        if let Some(ref id) = opts.resume_session_id {
            cmd.arg("--session");
            cmd.arg(id);
        }

        // Extra user-supplied arguments (inserted before the prompt).
        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        // Prompt is a positional argument (last), as per ACP client convention.
        // Assumed per research doc, not confirmed against live capture.
        cmd.arg(&opts.prompt);

        // Inject any environment overrides.
        for (k, v) in &opts.env_vars {
            cmd.env(k, v);
        }

        if opts.working_dir != std::path::Path::new("") {
            cmd.current_dir(&opts.working_dir);
        }

        cmd
    }
}

/// Return the default `DaemonProbe` for OpenClaw.
///
/// Assumed: `127.0.0.1:8787` — the acpx default agent gateway port per
/// documentation. This should be verified via `acpx --help` or openclaw
/// source when a live install is available.
///
/// Timeout: 2000 ms (generous default for local daemon on startup).
pub fn default_daemon_probe() -> DaemonProbe {
    DaemonProbe::new("127.0.0.1", 8787, 2000)
}

/// Return the full `DaemonSpec` for OpenClaw.
///
/// Wraps `default_daemon_probe()` with daemon name and install hint.
pub fn default_daemon_spec() -> DaemonSpec {
    DaemonSpec {
        name: "openclaw",
        probe: default_daemon_probe(),
        install_hint: "npm install -g acpx",
    }
}
