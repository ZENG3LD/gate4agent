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

/// Default argv: `-p --output-format stream-json --verbose --permission-mode plan`
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
            "--permission-mode",
            "plan",
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

/// Fresh session: `exec --json --skip-git-repo-check -s read-only <prompt>`
#[test]
fn codex_default_argv() {
    let builder = cli_builder(CliTool::Codex);
    let cmd = builder.build_command(&opts("write rust"));

    assert_eq!(program(&cmd), "codex");
    let got = args(&cmd);
    assert_eq!(got.first().copied(), Some("exec"), "must start with 'exec' subcommand");
    assert!(got.contains(&"--json"), "--json must be present");
    assert!(
        got.windows(2).any(|w| w == ["-s", "read-only"]),
        "read-only sandbox must be present"
    );
    assert_eq!(got.last().copied(), Some("write rust"), "prompt must be last");
}

/// `--model <m>` is inserted before the prompt.
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

/// Resume by ID uses the current `exec resume` configuration override shape.
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
    assert_eq!(&got[..2], &["exec", "resume"]);
    assert!(got.contains(&"--json"));
    assert!(
        got.windows(2)
            .any(|w| w == ["-c", "sandbox_mode=\"read-only\""])
    );
    assert_eq!(got.get(got.len() - 2).copied(), Some("rollout-abc"));
    assert_eq!(got.last().copied(), Some("continue"));
}

/// Continue last places `--last` immediately before the prompt.
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
    assert_eq!(&got[..2], &["exec", "resume"]);
    assert!(got.contains(&"--json"));
    assert!(
        got.windows(2)
            .any(|w| w == ["-c", "sandbox_mode=\"read-only\""])
    );
    assert_eq!(got.get(got.len() - 2).copied(), Some("--last"));
    assert_eq!(got.last().copied(), Some("continue"), "prompt must be last");
}
