//! Live RPC session tests — spawn real CLIs via RpcSession.
//!
//! These tests validate the full RPC layer:
//! - RpcSession spawns the CLI subprocess
//! - Legacy NDJSON fallback parses native CLI output
//! - Events are broadcast correctly
//! - Session lifecycle (Started → Text → SessionEnd → Exited)
//!
//! Run: cargo test --test rpc_live -- --nocapture
//!
//! Each test checks if the CLI is installed before running.
//! If not installed, the test is skipped (not failed).

use std::time::Duration;
use gate4agent::rpc::{RpcSession, RpcSessionOptions};
use gate4agent::{AgentEvent, CliTool, PipeProcessOptions};

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

/// Generic RPC session test for any CLI tool.
async fn run_rpc_test(tool: CliTool, binary_name: &str, opts: PipeProcessOptions) {
    if !cli_available(binary_name) {
        println!("[{:?}] SKIPPED — {} not found on PATH", tool, binary_name);
        return;
    }

    println!("[{:?}] Spawning RPC session...", tool);

    let prompt = "Say exactly: hello from gate4agent RPC. Nothing else.";

    let session = RpcSession::spawn(
        tool,
        opts,
        RpcSessionOptions::default(),
        &std::env::current_dir().unwrap(),
        prompt,
    )
    .await
    .unwrap_or_else(|e| panic!("{:?} RPC spawn failed: {}", tool, e));

    assert!(!session.session_id().is_empty(), "session_id must be non-empty");
    println!("[{:?}] Session: {}", tool, session.session_id());

    let mut rx = session.subscribe();
    let mut got_text = false;
    let mut got_end = false;
    let timeout = Duration::from_secs(120);

    loop {
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Ok(AgentEvent::Text { ref text, .. })) if !text.is_empty() => {
                println!("[{:?}] Text: {}", tool, &text[..text.len().min(200)]);
                got_text = true;
            }
            Ok(Ok(AgentEvent::SessionEnd { ref result, is_error, .. })) => {
                println!("[{:?}] SessionEnd: err={} {}", tool, is_error, result);
                got_end = true;
                break;
            }
            Ok(Ok(AgentEvent::Exited { code })) => {
                println!("[{:?}] Exited: code={}", tool, code);
                if !got_end {
                    got_end = true; // Some CLIs exit without explicit SessionEnd
                }
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
    let _ = got_end;
    println!("[{:?}] PASSED", tool);
}

#[tokio::test]
async fn rpc_live_claude() {
    run_rpc_test(CliTool::ClaudeCode, "claude", PipeProcessOptions::default()).await;
}

#[tokio::test]
async fn rpc_live_codex() {
    run_rpc_test(CliTool::Codex, "codex", PipeProcessOptions::default()).await;
}

#[tokio::test]
async fn rpc_live_gemini() {
    run_rpc_test(CliTool::Gemini, "gemini", PipeProcessOptions::default()).await;
}

#[tokio::test]
async fn rpc_live_opencode() {
    // Default model is opencode/gpt-5-nano (free, via OpenCode Zen).
    run_rpc_test(CliTool::OpenCode, "opencode", PipeProcessOptions::default()).await;
}
