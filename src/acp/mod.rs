//! ACP (Agent Client Protocol) transport module.
//!
//! Provides [`AcpSession`] — a multi-turn bidirectional JSON-RPC 2.0 session
//! over a subprocess stdio transport. Suitable for CLI tools that implement
//! the Agent Client Protocol specification (Gemini, OpenCode, Cursor, and the
//! Claude ACP adapter via `npx`).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use gate4agent::acp::{AcpSession, AcpSessionOptions};
//! use gate4agent::CliTool;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let session = AcpSession::spawn(
//!     CliTool::Gemini,
//!     std::path::Path::new("."),
//!     AcpSessionOptions::default(),
//! ).await?;
//!
//! let mut events = session.subscribe();
//! session.prompt("Hello, what is 2+2?").await?;
//!
//! // Stream events until TurnComplete or SessionEnd.
//! while let Ok(event) = events.recv().await {
//!     println!("{:?}", event);
//! }
//! # Ok(())
//! # }
//! ```

pub mod host;
pub mod protocol;
pub mod session;
pub(crate) mod reader;
pub(crate) mod spawn;

pub use host::{AcpHostHandler, DefaultAcpHandler};
pub use protocol::McpServerConfig;
pub use session::{AcpError, AcpSession, AcpSessionOptions};
