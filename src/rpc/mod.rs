//! Bidirectional JSON-RPC 2.0 protocol layer for gate4agent.
//!
//! Sits on top of [`PipeProcess`](crate::pipe::PipeProcess), adding:
//!
//! - **Agent → host requests**: agent asks host to read files, run commands,
//!   approve tools — dispatched to a [`HostHandler`] implementation.
//! - **Host → agent requests**: host sends requests and awaits typed responses
//!   via [`RpcSession::rpc_call`].
//! - **JSON-RPC notifications** mapped to [`AgentEvent`](crate::core::types::AgentEvent) broadcast.
//! - **Legacy fallback** to per-CLI NDJSON parsing for non-JSON-RPC lines.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use gate4agent::rpc::{RpcSession, RpcSessionOptions, MethodRouter};
//! use gate4agent::{CliTool, PipeProcessOptions};
//! use serde_json::json;
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let session = RpcSession::spawn(
//!     CliTool::ClaudeCode,
//!     PipeProcessOptions::default(),
//!     RpcSessionOptions {
//!         host_handler: Some(Box::new(
//!             MethodRouter::new().on("ping", |_| Ok(json!({"pong": true})))
//!         )),
//!         ..Default::default()
//!     },
//!     std::path::Path::new("."),
//!     "Hello",
//! ).await?;
//!
//! let result = session
//!     .rpc_call("echo", Some(json!({"hello": "world"})), Duration::from_secs(10))
//!     .await?;
//!
//! println!("Agent responded: {}", result);
//! # Ok(())
//! # }
//! ```

pub mod handler;
pub mod id;
pub mod message;
pub mod pending;
pub mod session;

pub use handler::{HostHandler, MethodRouter, RejectAllHandler};
pub use id::IdGen;
pub use message::{
    classify_line, IncomingMessage, RpcError, RpcId, RpcNotification, RpcRequest, RpcResponse,
};
pub use pending::PendingRequests;
pub use session::{RpcSession, RpcSessionError, RpcSessionOptions};
