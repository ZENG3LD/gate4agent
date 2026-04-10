//! Live pipe integration tests — spawn real CLIs and validate NDJSON parsing.
//!
//! Based on the working pattern from examples/pipe_hello.rs (0.1.18).
//! Uses PipeProcess directly — it handles cmd /C wrapping, stdin for Claude, etc.
//!
//! Run with:
//!   cargo test --test pipe_live -- --ignored --nocapture

use std::time::{Duration, Instant};

use gate4agent::pipe::cli::traits::CliEvent;
use gate4agent::pipe::cli::create_ndjson_parser;
use gate4agent::pipe::{PipeProcess, PipeProcessOptions, ClaudeOptions, PipeSession};
use gate4agent::{AgentEvent, CliTool};
use gate4agent::core::types::SessionConfig;

/// Spawn the given CLI tool via PipeProcess and validate NDJSON output.
fn run_pipe_test(tool: CliTool, extra_args: Vec<String>) {
    let cwd = std::env::current_dir().unwrap();
    let prompt = "Say exactly: hello from gate4agent. Nothing else.";

    println!("[{:?}] Spawning...", tool);

    let opts = PipeProcessOptions {
        extra_args,
        claude: ClaudeOptions::default(),
    };

    let mut pipe = PipeProcess::new_with_options(tool, &cwd, prompt, opts)
        .unwrap_or_else(|e| panic!("{:?} failed to spawn: {}", tool, e));

    println!("[{:?}] Spawned, reading NDJSON events...", tool);

    let mut parser = create_ndjson_parser(tool);
    let mut all_events: Vec<CliEvent> = Vec::new();
    let start = Instant::now();
    let timeout = Duration::from_secs(120);

    loop {
        if start.elapsed() > timeout {
            println!("[{:?}] TIMEOUT after 120s", tool);
            break;
        }

        if let Some(line) = pipe.try_recv() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            println!("[{:?}] raw: {}", tool, &trimmed[..trimmed.len().min(300)]);

            let events = parser.parse_line(trimmed);
            for ev in &events {
                println!("[{:?}] event: {:?}", tool, ev);
            }
            all_events.extend(events);
        }

        if !pipe.is_running() {
            while let Some(line) = pipe.try_recv() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                println!("[{:?}] raw (drain): {}", tool, &trimmed[..trimmed.len().min(300)]);
                let events = parser.parse_line(trimmed);
                for ev in &events {
                    println!("[{:?}] event (drain): {:?}", tool, ev);
                }
                all_events.extend(events);
            }
            println!("[{:?}] Process exited", tool);
            break;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    println!("\n[{:?}] Total events: {}", tool, all_events.len());

    if let Some(sid) = parser.session_id() {
        println!("[{:?}] Session ID: {}", tool, sid);
    }

    assert!(
        !all_events.is_empty(),
        "{:?}: parser produced no events — check CLI is installed and outputs NDJSON",
        tool,
    );

    let has_text = all_events.iter().any(|e| {
        matches!(e, CliEvent::AssistantText { text, .. } if !text.is_empty())
    });
    assert!(
        has_text,
        "{:?}: no AssistantText event with non-empty text",
        tool,
    );

    println!("[{:?}] PASSED", tool);
}

#[test]
#[ignore]
fn pipe_live_claude() {
    run_pipe_test(CliTool::ClaudeCode, vec![]);
}

#[test]
#[ignore]
fn pipe_live_codex() {
    run_pipe_test(CliTool::Codex, vec![]);
}

#[test]
#[ignore]
fn pipe_live_gemini() {
    run_pipe_test(CliTool::Gemini, vec![]);
}

#[test]
#[ignore]
fn pipe_live_opencode() {
    // Use free built-in model to avoid API key requirements.
    run_pipe_test(
        CliTool::OpenCode,
        vec!["-m".into(), "opencode/nemotron-3-super-free".into()],
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// PipeSession integration test (higher-level API over PipeProcess)
// ─────────────────────────────────────────────────────────────────────────────

/// Full PipeSession lifecycle: spawn → subscribe → receive events → session ends.
///
/// Verifies:
/// 1. `PipeSession::spawn` succeeds (Claude CLI must be installed).
/// 2. `session.session_id()` is non-empty immediately after spawn.
/// 3. At least one `AgentEvent::Text` event with non-empty text arrives.
/// 4. The session eventually sends `AgentEvent::SessionEnd`.
#[test]
#[ignore] // Requires real `claude` CLI installed and authenticated
fn pipe_session_full_lifecycle() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let config = SessionConfig {
            tool: CliTool::ClaudeCode,
            working_dir: std::env::current_dir().unwrap(),
            env_vars: Vec::new(),
            name: None,
        };

        let session = PipeSession::spawn(
            config,
            "Say exactly: hello from gate4agent pipe session. Nothing else.",
            PipeProcessOptions::default(),
        )
        .await
        .expect("PipeSession::spawn must succeed — is claude CLI installed?");

        // session_id is set immediately (it is a UUID assigned at spawn time,
        // not the CLI-native session ID from the NDJSON stream).
        assert!(
            !session.session_id().is_empty(),
            "session_id must be non-empty immediately after spawn"
        );

        let mut rx = session.subscribe();

        let mut got_text = false;
        let mut got_session_end = false;

        let deadline = std::time::Duration::from_secs(120);

        loop {
            match tokio::time::timeout(deadline, rx.recv()).await {
                Ok(Ok(AgentEvent::Text { ref text, .. })) if !text.is_empty() => {
                    println!("[pipe_session] Text: {}", text);
                    got_text = true;
                }
                Ok(Ok(AgentEvent::SessionEnd { ref result, .. })) => {
                    println!("[pipe_session] SessionEnd: {}", result);
                    got_session_end = true;
                    break;
                }
                Ok(Ok(AgentEvent::Exited { code })) => {
                    println!("[pipe_session] Exited with code {}", code);
                    break;
                }
                Ok(Ok(ev)) => {
                    println!("[pipe_session] event: {:?}", ev);
                }
                Ok(Err(e)) => {
                    println!("[pipe_session] recv error (channel closed): {}", e);
                    break;
                }
                Err(_) => {
                    println!("[pipe_session] TIMEOUT after 120s");
                    break;
                }
            }
        }

        assert!(
            got_text,
            "must receive at least one AgentEvent::Text with non-empty text"
        );
        assert!(
            got_session_end,
            "must receive AgentEvent::SessionEnd before channel closes"
        );
    });
}
