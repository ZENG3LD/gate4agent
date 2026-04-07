//! gate4agent — Universal wrapper for CLI agents (Claude Code, Codex, Gemini).
//!
//! Two transport modes:
//! - PTY mirror: spawns agent in real PTY, captures raw output, vt100 parsing
//! - Pipe mode: `claude -p --output-format stream-json`, plain OS pipes, NDJSON events
//!
//! Both modes produce `AgentEvent` values on a `tokio::sync::broadcast` channel.

pub use error::AgentError;
pub use types::{AgentEvent, CliTool, PtyEvent, SessionConfig};

pub mod pty;
pub mod pipe;
pub mod parser;
pub mod ndjson;
pub mod detection;
pub mod cli;
pub mod snapshot;
pub mod history;
pub mod manager;

pub use manager::{MultiCliManager, ManagerConfig};
pub use snapshot::{
    AgentCli, AgentRenderSnapshot, AgentSnapshotMode, ChatMessage, ChatRole, TermCell, TermGrid,
};
pub use history::{HistoryReader, SessionMeta, reader_for};

pub(crate) mod error;
pub(crate) mod types;
pub(crate) mod utils;
