//! Regression tests: per-CLI `build_command` produces the expected argv.
//!
//! These tests assert that the argv produced by each `CliCommandBuilder`
//! implementation matches exactly what the old `build_command_with_options`
//! match block in `pipe/process.rs` (git 8c0e428) would have produced.
//!
//! We test the bare `Command` returned by `build_command` — the Windows
//! `cmd /C` wrapping is applied by `pipe/process.rs` and is tested separately
//! via the shell-quoting helpers in that module.

use gate4agent::cli::cli_builder;
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
            "--dangerously-skip-permissions",
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
            "--dangerously-skip-permissions",
            "--resume",
            "ses_abc123",
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
            "--dangerously-skip-permissions",
            "--append-system-prompt",
            "Be concise.",
            "--model",
            "claude-opus-4",
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
            "--ask-for-approval",
            "never",
            "--skip-git-repo-check",
            "write a hello world in rust",
        ],
        "Codex fresh: exec subcommand, --json, approval flags, then prompt as final arg"
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
    // Resumed shape: codex exec resume <id> --json --ask-for-approval never --skip-git-repo-check <prompt>
    assert_eq!(
        got,
        &[
            "exec",
            "resume",
            "rollout-20260409-abc",
            "--json",
            "--ask-for-approval",
            "never",
            "--skip-git-repo-check",
            "continue",
        ],
        "Codex resume: exec resume <id> sub-sub-command, then flags, then prompt"
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

// ─────────────────────────────────────────────
// Gemini
// ─────────────────────────────────────────────

#[test]
fn gemini_fresh_session_argv() {
    let builder = cli_builder(CliTool::Gemini);
    let opts = make_opts("explain rust lifetimes");
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "gemini");
    let got = get_args(&cmd);
    assert_eq!(
        got,
        &["--output-format", "stream-json", "-p", "explain rust lifetimes"],
        "Gemini: --output-format stream-json -p <prompt>, NO --verbose"
    );
}

#[test]
fn gemini_no_verbose_flag() {
    let builder = cli_builder(CliTool::Gemini);
    let opts = make_opts("test");
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        !got.contains(&"--verbose"),
        "--verbose must NOT appear in Gemini argv (removed in Phase 1)"
    );
}

#[test]
fn gemini_resume_is_ignored() {
    // Gemini does not support resume in -p mode.
    // The builder silently ignores resume_session_id.
    let builder = cli_builder(CliTool::Gemini);
    let opts = SpawnOptions {
        prompt: "test".to_string(),
        resume_session_id: Some("some-id".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        !got.contains(&"--resume"),
        "Gemini does not support --resume; field must be silently ignored"
    );
    assert!(
        !got.contains(&"some-id"),
        "Gemini resume ID must not appear in argv"
    );
}

#[test]
fn gemini_prompt_is_last_arg() {
    let builder = cli_builder(CliTool::Gemini);
    let opts = make_opts("my gemini prompt");
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert_eq!(
        got.last().copied(),
        Some("my gemini prompt"),
        "Gemini: prompt must be the last argv token (follows -p)"
    );
}

// ─────────────────────────────────────────────
// Stubs — just verify they don't panic
// ─────────────────────────────────────────────

#[test]
fn cursor_stub_does_not_panic() {
    let builder = cli_builder(CliTool::Cursor);
    let opts = make_opts("test");
    let _cmd = builder.build_command(&opts); // must not panic
}

#[test]
fn opencode_stub_does_not_panic() {
    let builder = cli_builder(CliTool::OpenCode);
    let opts = make_opts("test");
    let _cmd = builder.build_command(&opts);
}

#[test]
fn openclaw_stub_does_not_panic() {
    let builder = cli_builder(CliTool::OpenClaw);
    let opts = make_opts("test");
    let _cmd = builder.build_command(&opts);
}
