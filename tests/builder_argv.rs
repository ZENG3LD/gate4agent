//! CLI builder argv tests — verifies each `CliCommandBuilder` produces the
//! correct argv for every documented option combination.
//!
//! These tests call `builder.build_command(&opts)` and inspect
//! `cmd.get_program()` and `cmd.get_args()` without spawning a process.
//!
//! Run with:
//!   cargo test --test builder_argv

use gate4agent::pipe::cli_builder;
use gate4agent::{CliTool, SpawnOptions};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn program(cmd: &std::process::Command) -> &str {
    cmd.get_program().to_str().unwrap()
}

fn args(cmd: &std::process::Command) -> Vec<&str> {
    cmd.get_args().map(|a| a.to_str().unwrap()).collect()
}

fn opts(prompt: &str) -> SpawnOptions {
    SpawnOptions {
        prompt: prompt.to_string(),
        ..Default::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Claude Code
// ─────────────────────────────────────────────────────────────────────────────

/// Default argv: `-p --output-format stream-json --verbose --dangerously-skip-permissions`
/// Prompt must NOT appear in argv — it is delivered via stdin.
#[test]
fn claude_default_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&opts("hello"));

    assert_eq!(program(&cmd), "claude");
    assert_eq!(
        args(&cmd),
        &[
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
        ],
        "Claude default: prompt must NOT appear in argv (delivered via stdin)"
    );
}

/// `--model <m>` appears after the default flags.
#[test]
fn claude_with_model() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        model: Some("claude-opus-4".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--model", "claude-opus-4"]),
        "--model claude-opus-4 must appear in argv"
    );
    // -p must be present
    assert!(got.contains(&"-p"));
    // prompt must NOT appear
    assert!(!got.contains(&"hello"), "prompt must not be in Claude argv");
}

/// `--resume <id>` replaces session-start flow; `-p` is still present.
#[test]
fn claude_with_resume() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        resume_session_id: Some("ses_abc123".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(got.contains(&"-p"), "-p must appear even when resuming");
    assert!(
        got.windows(2).any(|w| w == ["--resume", "ses_abc123"]),
        "--resume ses_abc123 must appear in argv"
    );
    assert!(
        !got.contains(&"--continue"),
        "--continue must NOT appear when resume_session_id is set"
    );
}

/// `--continue` added, no `-p` implied resume behavior (still has `-p`).
#[test]
fn claude_with_continue_last() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        continue_last: true,
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(got.contains(&"--continue"), "--continue must appear");
    assert!(
        !got.contains(&"--resume"),
        "--resume must NOT appear when only continue_last=true"
    );
}

/// `--append-system-prompt <text>` appears before resume/model flags.
#[test]
fn claude_with_system_prompt() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        append_system_prompt: Some("Be concise.".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--append-system-prompt", "Be concise."]),
        "--append-system-prompt 'Be concise.' must appear in argv"
    );
}

/// `--allowedTools Edit,Read,Bash` (comma-joined, single arg value).
#[test]
fn claude_with_allowed_tools() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        allowed_tools: vec!["Edit".to_string(), "Read".to_string(), "Bash".to_string()],
        ..Default::default()
    });

    let got = args(&cmd);
    let pos = got.iter().position(|a| *a == "--allowedTools");
    assert!(pos.is_some(), "--allowedTools flag must appear");
    assert_eq!(
        got.get(pos.unwrap() + 1).copied(),
        Some("Edit,Read,Bash"),
        "--allowedTools value must be comma-joined"
    );
}

/// `--permission-mode accept-all` added; `--dangerously-skip-permissions` OMITTED.
#[test]
fn claude_with_permission_mode() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        permission_mode: Some("accept-all".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--permission-mode", "accept-all"]),
        "--permission-mode accept-all must appear"
    );
    assert!(
        !got.contains(&"--dangerously-skip-permissions"),
        "--dangerously-skip-permissions must NOT appear when permission_mode is set"
    );
}

/// `--mcp-config path.json` added.
#[test]
fn claude_with_mcp_config() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        mcp_config: Some(std::path::PathBuf::from("path.json")),
        ..Default::default()
    });

    let got = args(&cmd);
    let pos = got.iter().position(|a| *a == "--mcp-config");
    assert!(pos.is_some(), "--mcp-config must appear in argv");
    assert!(
        got.get(pos.unwrap() + 1)
            .map(|v| v.contains("path.json"))
            .unwrap_or(false),
        "--mcp-config value must contain path.json"
    );
}

/// `--max-turns 10` added.
#[test]
fn claude_with_max_turns() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        max_turns: Some(10),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--max-turns", "10"]),
        "--max-turns 10 must appear in argv"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Codex
// ─────────────────────────────────────────────────────────────────────────────

/// Fresh session: `exec --json --full-auto <prompt>`
#[test]
fn codex_default_argv() {
    let builder = cli_builder(CliTool::Codex);
    let cmd = builder.build_command(&opts("write rust"));

    assert_eq!(program(&cmd), "codex");
    assert_eq!(
        args(&cmd),
        &["exec", "--json", "--full-auto", "write rust"],
        "Codex fresh: exec subcommand, --json, --full-auto, prompt as last arg"
    );
}

/// `--model <m>` inserted between `--full-auto` and the prompt.
#[test]
fn codex_with_model() {
    let builder = cli_builder(CliTool::Codex);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "write rust".to_string(),
        model: Some("o3".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--model", "o3"]),
        "--model o3 must appear in argv"
    );
    assert_eq!(got.last().copied(), Some("write rust"), "prompt must be last");
}

/// Resume by ID: `exec resume <id> --json --full-auto <prompt>`
#[test]
fn codex_with_resume_id() {
    let builder = cli_builder(CliTool::Codex);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "continue".to_string(),
        resume_session_id: Some("rollout-abc".to_string()),
        ..Default::default()
    });

    assert_eq!(program(&cmd), "codex");
    let got = args(&cmd);
    assert_eq!(&got[..3], &["exec", "resume", "rollout-abc"]);
    assert!(got.contains(&"--json"));
    assert!(got.contains(&"--full-auto"));
    assert_eq!(got.last().copied(), Some("continue"));
}

/// Continue last: `exec resume --last --json --full-auto <prompt>`
#[test]
fn codex_with_continue_last() {
    let builder = cli_builder(CliTool::Codex);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "continue".to_string(),
        continue_last: true,
        ..Default::default()
    });

    assert_eq!(program(&cmd), "codex");
    let got = args(&cmd);
    assert_eq!(&got[..3], &["exec", "resume", "--last"]);
    assert!(got.contains(&"--json"));
    assert!(got.contains(&"--full-auto"));
    assert_eq!(got.last().copied(), Some("continue"), "prompt must be last");
}

// ─────────────────────────────────────────────────────────────────────────────
// Gemini
// ─────────────────────────────────────────────────────────────────────────────

/// Default: `--output-format stream-json -p <prompt>` (no `--verbose`).
#[test]
fn gemini_default_argv() {
    let builder = cli_builder(CliTool::Gemini);
    let cmd = builder.build_command(&opts("explain lifetimes"));

    assert_eq!(program(&cmd), "gemini");
    assert_eq!(
        args(&cmd),
        &["--output-format", "stream-json", "-p", "explain lifetimes"],
        "Gemini default: --output-format stream-json -p <prompt>, no --verbose"
    );
}

/// Model flag: `-m <model>` or `--model` — check whichever the builder uses.
/// Gemini builder does NOT have a model flag in SpawnOptions (ignored), so
/// test that model in SpawnOptions doesn't corrupt the argv.
#[test]
fn gemini_with_model_ignored() {
    // Gemini builder currently ignores SpawnOptions.model (no -m flag supported).
    // This test ensures that setting model does NOT corrupt the base argv.
    let builder = cli_builder(CliTool::Gemini);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "test".to_string(),
        model: Some("gemini-2.5-pro".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    // Base structure must still be intact
    assert!(
        got.windows(2).any(|w| w == ["--output-format", "stream-json"]),
        "--output-format stream-json must be present"
    );
    assert!(got.contains(&"-p"), "-p must be present");
    assert_eq!(got.last().copied(), Some("test"), "prompt must be last");
}

/// `--resume <id>` added before `-p <prompt>`.
#[test]
fn gemini_with_resume() {
    let builder = cli_builder(CliTool::Gemini);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "test".to_string(),
        resume_session_id: Some("session-42".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--resume", "session-42"]),
        "--resume session-42 must appear"
    );
    assert!(got.contains(&"-p"), "-p must still be present");
    assert_eq!(got.last().copied(), Some("test"), "prompt must be last");
}

/// `--sandbox` added when `sandbox=true`.
#[test]
fn gemini_with_sandbox() {
    let builder = cli_builder(CliTool::Gemini);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "test".to_string(),
        sandbox: true,
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(got.contains(&"--sandbox"), "--sandbox must appear when sandbox=true");
}

/// `--sandbox` absent when `sandbox=false`.
#[test]
fn gemini_no_sandbox_by_default() {
    let builder = cli_builder(CliTool::Gemini);
    let cmd = builder.build_command(&opts("test"));
    assert!(!args(&cmd).contains(&"--sandbox"), "--sandbox must not appear by default");
}

// ─────────────────────────────────────────────────────────────────────────────
// OpenCode
// ─────────────────────────────────────────────────────────────────────────────

/// Default: `run --format json <prompt>`
#[test]
fn opencode_default_argv() {
    let builder = cli_builder(CliTool::OpenCode);
    let cmd = builder.build_command(&opts("write a function"));

    assert_eq!(program(&cmd), "opencode");
    let got = args(&cmd);
    assert_eq!(got.first().copied(), Some("run"), "must start with 'run' subcommand");
    assert!(
        got.windows(2).any(|w| w == ["--format", "json"]),
        "--format json must appear"
    );
    assert_eq!(
        got.last().copied(),
        Some("write a function"),
        "prompt must be last"
    );
    assert!(!got.contains(&"--session"), "--session must not appear in fresh session");
    assert!(!got.contains(&"--continue"), "--continue must not appear in fresh session");
}

/// `-m <model>` added.
#[test]
fn opencode_with_model() {
    let builder = cli_builder(CliTool::OpenCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "hello".to_string(),
        model: Some("anthropic/claude-opus-4".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["-m", "anthropic/claude-opus-4"]),
        "-m <model> must appear in OpenCode argv"
    );
    assert_eq!(got.last().copied(), Some("hello"), "prompt must be last");
}

/// `--session <ses_XXX>` added when `resume_session_id` is set.
#[test]
fn opencode_with_session_id() {
    let builder = cli_builder(CliTool::OpenCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "continue".to_string(),
        resume_session_id: Some("ses_ABCDEF".to_string()),
        ..Default::default()
    });

    let got = args(&cmd);
    let pos = got.iter().position(|a| *a == "--session");
    assert!(pos.is_some(), "--session must appear");
    assert_eq!(
        got.get(pos.unwrap() + 1).copied(),
        Some("ses_ABCDEF"),
        "--session value must be the session ID"
    );
    assert!(!got.contains(&"--continue"), "--continue must not appear when session ID is set");
    assert_eq!(got.last().copied(), Some("continue"), "prompt must be last");
}

/// `--continue` added when `continue_last=true`.
#[test]
fn opencode_with_continue_last() {
    let builder = cli_builder(CliTool::OpenCode);
    let cmd = builder.build_command(&SpawnOptions {
        prompt: "continue".to_string(),
        continue_last: true,
        ..Default::default()
    });

    let got = args(&cmd);
    assert!(got.contains(&"--continue"), "--continue must appear when continue_last=true");
    assert!(!got.contains(&"--session"), "--session must not appear when continue_last only");
    assert_eq!(got.last().copied(), Some("continue"), "prompt must be last");
}
