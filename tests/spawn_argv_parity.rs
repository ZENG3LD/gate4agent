//! Regression tests: per-CLI `build_command` produces the expected argv.
//!
//! These tests assert that the argv produced by each `CliCommandBuilder`
//! implementation matches exactly what the old `build_command_with_options`
//! match block in `pipe/process.rs` (git 8c0e428) would have produced.
//!
//! We test the bare `Command` returned by `build_command` — the Windows
//! `cmd /C` wrapping is applied by `pipe/process.rs` and is tested separately
//! via the shell-quoting helpers in that module.

use gate4agent::pipe::cli_builder;
use gate4agent::{CliTool, SpawnOptions};

fn get_program(cmd: &std::process::Command) -> &str {
    cmd.get_program().to_str().unwrap()
}

fn get_args(cmd: &std::process::Command) -> Vec<&str> {
    cmd.get_args().map(|a| a.to_str().unwrap()).collect()
}

fn make_opts(prompt: &str) -> SpawnOptions {
    SpawnOptions {
        prompt: prompt.to_string(),
        ..Default::default()
    }
}

// ─────────────────────────────────────────────
// Claude Code
// ─────────────────────────────────────────────

#[test]
fn claude_fresh_session_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = make_opts("hello world");
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "claude");
    let got = get_args(&cmd);
    assert_eq!(
        got,
        &[
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "plan",
        ],
        "Claude fresh session: prompt must NOT appear in argv (delivered via stdin)"
    );
}

#[test]
fn claude_with_resume_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        resume_session_id: Some("ses_abc123".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "claude");
    let got = get_args(&cmd);
    assert_eq!(
        got,
        &[
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--resume",
            "ses_abc123",
            "--permission-mode",
            "plan",
        ]
    );
}

#[test]
fn claude_with_model_and_append_system_prompt_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        model: Some("claude-opus-4".to_string()),
        append_system_prompt: Some("Be concise.".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "claude");
    let got = get_args(&cmd);
    // append_system_prompt comes before model in the builder
    assert_eq!(
        got,
        &[
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--append-system-prompt",
            "Be concise.",
            "--model",
            "claude-opus-4",
            "--permission-mode",
            "plan",
        ]
    );
}

#[test]
fn claude_extra_args_appear_in_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        extra_args: vec!["--foo".to_string(), "bar".to_string()],
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(got.contains(&"--foo"), "extra_args must appear in argv");
    assert!(got.contains(&"bar"), "extra_args values must appear in argv");
    // Prompt still must NOT appear
    assert!(
        !got.contains(&"hello"),
        "Claude prompt must not appear in argv"
    );
}

// ─────────────────────────────────────────────
// Claude — new SpawnOptions fields
// ─────────────────────────────────────────────

#[test]
fn claude_continue_last_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        continue_last: true,
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.contains(&"--continue"),
        "continue_last=true must add --continue to Claude argv"
    );
    assert!(
        !got.contains(&"--resume"),
        "--resume must NOT appear when continue_last is used without resume_session_id"
    );
}

#[test]
fn claude_continue_last_ignored_when_resume_session_id_set() {
    // resume_session_id takes priority over continue_last.
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        continue_last: true,
        resume_session_id: Some("ses_explicit".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.contains(&"--resume"),
        "--resume must appear when resume_session_id is set"
    );
    assert!(
        !got.contains(&"--continue"),
        "--continue must NOT appear when resume_session_id is also set"
    );
}

#[test]
fn claude_allowed_tools_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        allowed_tools: vec!["Edit".to_string(), "Read".to_string(), "Bash".to_string()],
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    let tools_pos = got.iter().position(|a| *a == "--allowedTools");
    assert!(tools_pos.is_some(), "--allowedTools flag must appear in argv");
    assert_eq!(
        got.get(tools_pos.unwrap() + 1).copied(),
        Some("Edit,Read,Bash"),
        "--allowedTools value must be comma-joined tool names"
    );
}

#[test]
fn claude_permission_mode_replaces_dangerously_skip() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        permission_mode: Some("default".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.contains(&"--permission-mode"),
        "--permission-mode flag must appear when permission_mode is set"
    );
    assert_eq!(
        got.iter().position(|a| *a == "--permission-mode")
            .and_then(|i| got.get(i + 1).copied()),
        Some("default"),
        "--permission-mode value must be 'default'"
    );
    assert!(
        !got.contains(&"--dangerously-skip-permissions"),
        "--dangerously-skip-permissions must NOT appear when permission_mode is set"
    );
}

#[test]
fn claude_permission_mode_accept_all() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        permission_mode: Some("accept-all".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--permission-mode", "accept-all"]),
        "--permission-mode accept-all must appear in argv"
    );
    assert!(
        !got.contains(&"--dangerously-skip-permissions"),
        "--dangerously-skip-permissions must NOT appear when permission_mode is explicitly set"
    );
}

#[test]
fn claude_mcp_config_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        mcp_config: Some(std::path::PathBuf::from("/tmp/mcp.json")),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    let mcp_pos = got.iter().position(|a| *a == "--mcp-config");
    assert!(mcp_pos.is_some(), "--mcp-config flag must appear in argv");
    assert!(
        got.get(mcp_pos.unwrap() + 1)
            .map(|v| v.contains("mcp.json"))
            .unwrap_or(false),
        "--mcp-config value must contain the path"
    );
}

#[test]
fn claude_max_turns_argv() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = SpawnOptions {
        prompt: "hello".to_string(),
        max_turns: Some(10),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--max-turns", "10"]),
        "--max-turns 10 must appear in Claude argv"
    );
}

#[test]
fn claude_no_permission_mode_defaults_to_plan() {
    let builder = cli_builder(CliTool::ClaudeCode);
    let opts = make_opts("hello");
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.windows(2).any(|w| w == ["--permission-mode", "plan"]),
        "Claude must default to plan permission mode"
    );
}

// ─────────────────────────────────────────────
// Codex
// ─────────────────────────────────────────────

#[test]
fn codex_fresh_session_argv() {
    let builder = cli_builder(CliTool::Codex);
    let opts = make_opts("write a hello world in rust");
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "codex");
    let got = get_args(&cmd);
    assert_eq!(
        got,
        &[
            "exec",
            "--json",
            "--skip-git-repo-check",
            "-s",
            "read-only",
            "write a hello world in rust",
        ],
        "Codex fresh: current JSON and read-only sandbox contract"
    );
}

#[test]
fn codex_with_resume_argv() {
    let builder = cli_builder(CliTool::Codex);
    let opts = SpawnOptions {
        prompt: "continue".to_string(),
        resume_session_id: Some("rollout-20260409-abc".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "codex");
    let got = get_args(&cmd);
    // Resumed shape: current flags, read-only config, ID, then prompt.
    assert_eq!(
        got,
        &[
            "exec",
            "resume",
            "--json",
            "--skip-git-repo-check",
            "-c",
            "sandbox_mode=\"read-only\"",
            "rollout-20260409-abc",
            "continue",
        ],
        "Codex resume: current config override shape"
    );
}

#[test]
fn codex_prompt_is_last_arg() {
    let builder = cli_builder(CliTool::Codex);
    let opts = make_opts("my prompt");
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert_eq!(
        got.last().copied(),
        Some("my prompt"),
        "Codex: prompt must be the last argv token"
    );
}

#[test]
fn codex_continue_last_argv() {
    let builder = cli_builder(CliTool::Codex);
    let opts = SpawnOptions {
        prompt: "continue".to_string(),
        continue_last: true,
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "codex");
    let got = get_args(&cmd);
    // Shape: current flags and sandbox config, then --last and prompt.
    assert_eq!(
        &got[..2],
        &["exec", "resume"],
        "Codex continue_last must use exec resume"
    );
    assert!(got.contains(&"--json"), "--json must appear in Codex continue_last argv");
    assert!(
        got.windows(2)
            .any(|w| w == ["-c", "sandbox_mode=\"read-only\""]),
        "read-only sandbox config must appear in Codex continue_last argv"
    );
    assert_eq!(got.get(got.len() - 2).copied(), Some("--last"));
    assert_eq!(
        got.last().copied(),
        Some("continue"),
        "Codex: prompt must still be the last argv token"
    );
}

#[test]
fn codex_continue_last_ignored_when_resume_session_id_set() {
    let builder = cli_builder(CliTool::Codex);
    let opts = SpawnOptions {
        prompt: "go".to_string(),
        continue_last: true,
        resume_session_id: Some("rollout-explicit".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.contains(&"rollout-explicit"),
        "explicit session ID must appear in argv"
    );
    assert!(
        !got.contains(&"--last"),
        "--last must NOT appear when resume_session_id is also set"
    );
}
