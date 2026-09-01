//! ACP (Agent Client Protocol) transport module.
//!
//! Provides [`AcpSession`] — a multi-turn bidirectional JSON-RPC 2.0 session
//! over a subprocess stdio transport. Suitable for CLI tools that implement
//! the Agent Client Protocol specification: Claude Code and Codex (via `npx`
//! ACP adapters), Grok (native `grok agent stdio`), and Kimi Code (native
//! `kimi acp`).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use gate4agent::acp::{AcpSession, AcpSessionOptions};
//! use gate4agent::CliTool;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let session = AcpSession::spawn(
//!     CliTool::Grok,
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

mod gate;
mod host;
pub mod protocol;
pub mod session;
pub(crate) mod reader;
pub(crate) mod spawn;
mod terminal;

pub use gate::DangerousCommandGate;
pub use host::HostPolicy;
pub use session::{AcpError, AcpSession, AcpSessionOptions, OperatorPermissionChoice};
