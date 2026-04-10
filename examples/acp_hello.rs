//! ACP session example — spawn Claude Code via Agent Client Protocol.
//!
//! AcpSession provides bidirectional JSON-RPC 2.0 over stdio with:
//! - Multi-turn sessions (call prompt() repeatedly without respawning)
//! - Host handler for agent → host requests (fs, terminal, permissions)
//! - Structured session/update streaming
//!
//! Run: cargo run --example acp_hello
//!
//! Requires `claude` CLI installed, authenticated, and the ACP adapter:
//!   npx @agentclientprotocol/claude-agent-acp

use gate4agent::acp::{AcpSession, AcpSessionOptions};
use gate4agent::{AcpHostHandler, CliTool, AgentEvent};

/// Minimal host handler: auto-allows all permission requests, denies fs reads.
struct ExampleHandler;

impl AcpHostHandler for ExampleHandler {
    fn request_permission(
        &self,
        tool_name: &str,
        _description: &str,
        _session_id: &str,
    ) -> Result<bool, String> {
        println!("[host] permission request for tool: {}", tool_name);
        Ok(true)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = AcpSession::spawn(
        CliTool::ClaudeCode,
        &std::env::current_dir()?,
        AcpSessionOptions {
            host_handler: Some(Box::new(ExampleHandler)),
            ..Default::default()
        },
    )
    .await?;

    println!("ACP session started: {}", session.session_id());

    let mut rx = session.subscribe();

    session.prompt("Say exactly: hello from ACP session. Nothing else.").await?;

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
                    AgentEvent::TurnComplete { .. } => {
                        println!("\n[turn complete]");
                        break;
                    }
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

    session.kill().await.ok();
    Ok(())
}
