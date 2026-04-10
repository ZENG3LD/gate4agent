//! Live ACP session tests — spawn real CLIs via AcpSession.
//!
//! These tests validate the full ACP layer:
//! - AcpSession spawns the CLI subprocess
//! - initialize + session/new handshake completes
//! - Events are broadcast correctly
//! - Session lifecycle (Started → SessionStart → Text → SessionEnd → Exited)
//!
//! Run: cargo test --test acp_live -- --nocapture
//!
//! Each test checks if the CLI is installed before running.
//! If not installed, the test is skipped (not failed).

use std::time::Duration;

use gate4agent::acp::{AcpSession, AcpSessionOptions};
use gate4agent::{AgentEvent, CliTool};

/// Check if a CLI tool binary exists on PATH.
/// On Windows, npm tools are .cmd files — must spawn via `cmd /C`.
fn cli_available(name: &str) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", name, "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Generic ACP session test for any CLI tool.
async fn run_acp_test(tool: CliTool, binary_name: &str) {
    if !cli_available(binary_name) {
        println!("[{:?}] SKIPPED — {} not found on PATH", tool, binary_name);
        return;
    }

    println!("[{:?}] Spawning ACP session...", tool);

    let session = AcpSession::spawn(
        tool,
        &std::env::current_dir().unwrap(),
        AcpSessionOptions::default(),
    )
    .await
    .unwrap_or_else(|e| panic!("{:?} ACP spawn failed: {}", tool, e));

    assert!(!session.session_id().is_empty(), "session_id must be non-empty");
    println!("[{:?}] Session: {}", tool, session.session_id());

    let mut rx = session.subscribe();

    session
        .prompt("Say exactly: hello from gate4agent ACP. Nothing else.")
        .await
        .unwrap_or_else(|e| panic!("{:?} prompt failed: {}", tool, e));

    let mut got_text = false;
    let timeout = Duration::from_secs(120);

    loop {
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Ok(AgentEvent::Text { ref text, .. })) if !text.is_empty() => {
                println!("[{:?}] Text: {}", tool, &text[..text.len().min(200)]);
                got_text = true;
            }
            Ok(Ok(AgentEvent::TurnComplete { input_tokens, output_tokens })) => {
                println!("[{:?}] TurnComplete: in={} out={}", tool, input_tokens, output_tokens);
                break;
            }
            Ok(Ok(AgentEvent::SessionEnd { ref result, is_error, .. })) => {
                println!("[{:?}] SessionEnd: err={} {}", tool, is_error, result);
                break;
            }
            Ok(Ok(AgentEvent::Exited { code })) => {
                println!("[{:?}] Exited: code={}", tool, code);
                break;
            }
            Ok(Ok(ev)) => {
                println!("[{:?}] {:?}", tool, ev);
            }
            Ok(Err(_)) => break,
            Err(_) => {
                println!("[{:?}] TIMEOUT after 120s", tool);
                break;
            }
        }
    }

    assert!(got_text, "{:?}: must receive Text event with content", tool);

    // Kill the agent process so we don't leave orphans.
    let _ = session.kill().await;
    println!("[{:?}] PASSED", tool);
}

#[tokio::test]
async fn acp_live_gemini() {
    run_acp_test(CliTool::Gemini, "gemini").await;
}

#[tokio::test]
async fn acp_live_opencode() {
    run_acp_test(CliTool::OpenCode, "opencode").await;
}

#[tokio::test]
async fn acp_live_claude() {
    // ClaudeCode ACP adapter is invoked via npx (claude-agent-acp package).
    run_acp_test(CliTool::ClaudeCode, "npx").await;
}

#[tokio::test]
async fn acp_live_codex() {
    // Codex ACP adapter is also invoked via npx.
    run_acp_test(CliTool::Codex, "npx").await;
}

#[tokio::test]
async fn acp_live_cursor() {
    // Cursor has native ACP via `cursor-agent agent acp`.
    run_acp_test(CliTool::Cursor, "cursor-agent").await;
}
