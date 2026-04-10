//! Minimal RPC example — spawn Claude with bidirectional JSON-RPC 2.0.
//!
//! Run: cargo run --example rpc_hello

use gate4agent::rpc::{RpcSession, RpcSessionOptions};
use gate4agent::{CliTool, PipeProcessOptions, AgentEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = RpcSession::spawn(
        CliTool::ClaudeCode,
        PipeProcessOptions::default(),
        RpcSessionOptions::default(),
        &std::env::current_dir()?,
        "Say hello",
    )
    .await?;

    let mut rx = session.subscribe();
    println!("Session spawned, listening for events...");

    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::Text { text, .. } => print!("{text}"),
            AgentEvent::SessionEnd { .. } => {
                println!();
                break;
            }
            _ => {}
        }
    }

    Ok(())
}
