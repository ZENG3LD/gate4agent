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
use gate4agent::pipe::{PipeProcess, PipeProcessOptions};
use gate4agent::CliTool;

/// Spawn the given CLI tool via PipeProcess and validate NDJSON output.
fn run_pipe_test(tool: CliTool) {
    let cwd = std::env::current_dir().unwrap();
    let prompt = "Say exactly: hello from gate4agent. Nothing else.";

    println!("[{:?}] Spawning...", tool);

    let mut pipe = PipeProcess::new_with_options(
        tool,
        &cwd,
        prompt,
        PipeProcessOptions::default(),
    )
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
            // Drain remaining lines
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
    run_pipe_test(CliTool::ClaudeCode);
}

#[test]
#[ignore]
fn pipe_live_codex() {
    run_pipe_test(CliTool::Codex);
}

#[test]
#[ignore]
fn pipe_live_gemini() {
    run_pipe_test(CliTool::Gemini);
}

#[test]
#[ignore]
fn pipe_live_cursor() {
    run_pipe_test(CliTool::Cursor);
}

#[test]
#[ignore]
fn pipe_live_opencode() {
    run_pipe_test(CliTool::OpenCode);
}
