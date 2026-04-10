//! RPC session example — spawn Claude Code with bidirectional JSON-RPC 2.0.
//!
//! The RpcSession wraps PipeProcess, adding:
//! - Legacy NDJSON fallback (parses Claude's native output)
//! - Bidirectional JSON-RPC 2.0 support (for ACP-compatible CLIs)
//! - Host handler for agent → host requests
//!
//! Run: cargo run --example rpc_hello
//!
//! Requires `claude` CLI installed and authenticated.

use gate4agent::rpc::{RpcSession, RpcSessionOptions, MethodRouter};
use gate4agent::{CliTool, PipeProcessOptions, AgentEvent};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up a host handler that responds to "ping" requests from the agent.
    let handler = MethodRouter::new()
        .on("ping", |_| Ok(json!({"pong": true})));

    let session = RpcSession::spawn(
        CliTool::ClaudeCode,
        PipeProcessOptions::default(),
        RpcSessionOptions {
            host_handler: Some(Box::new(handler)),
            legacy_fallback: true,
            channel_capacity: 256,
        },
        &std::env::current_dir()?,
        "Say exactly: hello from RPC session. Nothing else.",
    )
    .await?;

    println!("RPC session started: {}", session.session_id());

    let mut rx = session.subscribe();

    loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            rx.recv(),
        ).await {
            Ok(Ok(event)) => {
                match &event {
                    AgentEvent::Text { text, .. } => print!("{text}"),
                    AgentEvent::ToolStart { name, .. } => println!("\n[tool] {name}"),
                    AgentEvent::ToolResult { output, is_error, .. } => {
                        println!("[tool result] err={is_error} {}", &output[..output.len().min(200)]);
                    }
                    AgentEvent::Thinking { text } => println!("[thinking] {text}"),
                    AgentEvent::SessionEnd { result, is_error, .. } => {
                        println!("\n[session end] err={is_error} {result}");
                        break;
                    }
                    AgentEvent::Exited { code } => {
                        println!("[exited] code={code}");
                        break;
                    }
                    AgentEvent::RpcNotification { method, params } => {
                        println!("[rpc notif] {method}: {params}");
                    }
                    AgentEvent::RpcIncomingRequest { method, params, .. } => {
                        println!("[rpc request] {method}: {params:?}");
                    }
                    _ => {}
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {
                println!("\n[timeout] 120s elapsed");
                break;
            }
        }
    }

    Ok(())
}
