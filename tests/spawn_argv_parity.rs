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
            "--full-auto",
            "write a hello world in rust",
        ],
        "Codex fresh: exec subcommand, --json, --full-auto, then prompt as final arg"
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
    // Resumed shape: codex exec resume <id> --json --full-auto <prompt>
    assert_eq!(
        got,
        &[
            "exec",
            "resume",
            "rollout-20260409-abc",
            "--json",
            "--full-auto",
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
fn gemini_resume_passes_flag() {
    // Gemini supports --resume <id> in -p mode.
    // Source: packages/cli/src/config/config.ts — --resume / -r flag.
    let builder = cli_builder(CliTool::Gemini);
    let opts = SpawnOptions {
        prompt: "test".to_string(),
        resume_session_id: Some("some-id".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert!(
        got.contains(&"--resume"),
        "Gemini must pass --resume when resume_session_id is set"
    );
    assert!(
        got.contains(&"some-id"),
        "Gemini resume ID must appear in argv after --resume"
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
// Cursor Agent
// ─────────────────────────────────────────────
//
// Source: https://cursor.com/docs/cli/headless
// Argv shape: cursor-agent -p --output-format stream-json [--model <m>] [--resume <id>] [<extra>...] "<prompt>"
// The prompt is a positional arg (unlike Claude which uses stdin).

#[test]
fn cursor_fresh_session_argv() {
    let builder = cli_builder(CliTool::Cursor);
    let opts = make_opts("explain rust lifetimes");
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "cursor-agent");
    let got = get_args(&cmd);
    // Must contain -p and --output-format stream-json
    assert!(got.contains(&"-p"), "Cursor argv must include -p");
    assert!(
        got.windows(2).any(|w| w == ["--output-format", "stream-json"]),
        "Cursor argv must include --output-format stream-json"
    );
    // Prompt must be the last arg (positional, not stdin)
    assert_eq!(
        got.last().copied(),
        Some("explain rust lifetimes"),
        "Cursor: prompt must be the last positional argv token"
    );
}

#[test]
fn cursor_with_resume_argv() {
    let builder = cli_builder(CliTool::Cursor);
    let opts = SpawnOptions {
        prompt: "continue the task".to_string(),
        resume_session_id: Some("cursor_ses_abc123".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "cursor-agent");
    let got = get_args(&cmd);
    // --resume <id> must appear
    assert!(
        got.windows(2).any(|w| w == ["--resume", "cursor_ses_abc123"]),
        "--resume <id> must appear in Cursor resume argv"
    );
    // Prompt must still be last
    assert_eq!(
        got.last().copied(),
        Some("continue the task"),
        "Cursor resume: prompt must be the last positional argv token"
    );
}

#[test]
fn cursor_prompt_is_last_arg() {
    let builder = cli_builder(CliTool::Cursor);
    let opts = make_opts("my cursor prompt");
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert_eq!(
        got.last().copied(),
        Some("my cursor prompt"),
        "Cursor: prompt must be the last argv token"
    );
}

// ─────────────────────────────────────────────
// OpenCode
// ─────────────────────────────────────────────
//
// Source: https://opencode.ai/docs/cli/ — packages/opencode/src/cli/cmd/run.ts
// Argv shape: opencode --format json [--session <ses_XXXX>] [<extra>...] "<prompt>"
//
// There is NO "run" subcommand — the default command takes the message directly.

#[test]
fn opencode_fresh_session_argv() {
    let builder = cli_builder(CliTool::OpenCode);
    let opts = make_opts("write a function");
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "opencode");
    let got = get_args(&cmd);
    // Must start with "run" — the `run` subcommand is required for headless pipe mode.
    assert_eq!(got.first().copied(), Some("run"), "OpenCode argv must start with 'run' subcommand");
    // Must contain --format json
    assert!(
        got.windows(2).any(|w| w == ["--format", "json"]),
        "OpenCode argv must include --format json"
    );
    // Prompt must be last
    assert_eq!(
        got.last().copied(),
        Some("write a function"),
        "OpenCode: prompt must be the last positional argv token"
    );
    // Must NOT contain --session when no resume
    assert!(
        !got.contains(&"--session"),
        "OpenCode fresh session must not contain --session"
    );
}

#[test]
fn opencode_with_session_argv() {
    let builder = cli_builder(CliTool::OpenCode);
    let opts = SpawnOptions {
        prompt: "continue".to_string(),
        resume_session_id: Some("ses_abc".to_string()),
        ..Default::default()
    };
    let cmd = builder.build_command(&opts);

    assert_eq!(get_program(&cmd), "opencode");
    let got = get_args(&cmd);
    // --session ses_abc must appear before the prompt
    let session_pos = got.iter().position(|a| *a == "--session");
    let prompt_pos = got.iter().position(|a| *a == "continue");
    assert!(session_pos.is_some(), "--session flag must appear in argv");
    // The value after --session must be the session ID
    assert_eq!(
        got.get(session_pos.unwrap() + 1).copied(),
        Some("ses_abc"),
        "--session value must be the session ID"
    );
    // --session must appear before the prompt
    assert!(
        session_pos.unwrap() < prompt_pos.unwrap(),
        "--session must appear before the prompt in argv"
    );
}

#[test]
fn opencode_prompt_is_last_arg() {
    let builder = cli_builder(CliTool::OpenCode);
    let opts = make_opts("my opencode prompt");
    let cmd = builder.build_command(&opts);

    let got = get_args(&cmd);
    assert_eq!(
        got.last().copied(),
        Some("my opencode prompt"),
        "OpenCode: prompt must be the last argv token"
    );
}

