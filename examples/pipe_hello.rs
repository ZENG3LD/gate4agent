//! Quick smoke test: spawn Claude in pipe mode, send a prompt, print events.

use gate4agent::pipe::{PipeProcess, PipeProcessOptions};
use gate4agent::pipe::{create_ndjson_parser, CliEvent};
use gate4agent::CliTool;
use std::time::{Duration, Instant};

fn main() {
    println!("=== gate4agent pipe smoke test ===");
    println!("Spawning Claude in pipe mode...");

    let cwd = std::env::current_dir().unwrap();
    let mut pipe = PipeProcess::new_with_options(
        CliTool::ClaudeCode,
        &cwd,
        "Say exactly: hello from gate4agent. Nothing else.",
        PipeProcessOptions::default(),
    )
    .expect("Failed to spawn Claude pipe process");

    println!("Claude spawned, reading NDJSON events...\n");

    let mut parser = create_ndjson_parser(CliTool::ClaudeCode);
    let start = Instant::now();
    let timeout = Duration::from_secs(60);
    let mut got_response = false;

    loop {
        if start.elapsed() > timeout {
            println!("\n[TIMEOUT after 60s]");
            break;
        }

        if let Some(line) = pipe.try_recv() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Print raw NDJSON line
            println!("[RAW] {}", &trimmed[..trimmed.len().min(200)]);

            // Parse through NDJSON parser
            let events = parser.parse_line(trimmed);
            for event in events {
                match &event {
                    CliEvent::AssistantText { text, is_delta } => {
                        if *is_delta {
                            print!("{}", text);
                        } else {
                            println!("\n[FULL TEXT] {}", text);
                        }
                        got_response = true;
                    }
                    CliEvent::SessionStart { session_id, model, .. } => {
                        println!("[SESSION] id={} model={}", session_id, model);
                    }
                    CliEvent::ToolCallStart { name, .. } => {
                        println!("[TOOL] {}", name);
                    }
                    CliEvent::TurnComplete { input_tokens, output_tokens, .. } => {
                        println!("\n[TOKENS] in={} out={}", input_tokens, output_tokens);
                    }
                    CliEvent::SessionEnd { result, cost_usd, is_error } => {
                        println!("[END] error={} cost={:?}", is_error, cost_usd);
                        if !result.is_empty() {
                            println!("[RESULT] {}", &result[..result.len().min(200)]);
                        }
                    }
                    CliEvent::Error { message } => {
                        println!("[ERROR] {}", message);
                    }
                    _ => {
                        println!("[EVENT] {:?}", event);
                    }
                }
            }
        }

        if !pipe.is_running() {
            println!("\n[Process exited]");
            break;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    if got_response {
        println!("\n=== SUCCESS: got response from Claude ===");
    } else {
        println!("\n=== FAIL: no response received ===");
    }
}
