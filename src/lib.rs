//! gate4agent — Universal wrapper for CLI agents (Claude Code, Codex, Gemini, Cursor, OpenCode).
//!
//! Two transport modes:
//! - Pipe mode: NDJSON-streaming pipe sessions (Claude, Codex, Gemini, Cursor, OpenCode)
//! - PTY mirror: spawns agent in real PTY, captures raw output, vt100 parsing
//!
//! All modes produce `AgentEvent` values on a `tokio::sync::broadcast` channel.
//!
//! # Entry points
//!
//! - [`TransportSession`] — thin dispatch router: pipe or PTY
//! - [`PipeSession`] — direct pipe session entry point (restored for 0.1.x compatibility)
//! - [`pty::PtySession::spawn`] — PTY mirror mode (unchanged)
//! - [`MultiCliManager`] — high-level session manager for the chart app

pub use error::AgentError;
pub use types::{AgentEvent, CliTool, SessionConfig};
pub use transport::{SpawnOptions, TransportSession};
pub use pipe::{PipeSession, PipeProcessOptions, ClaudeOptions};

pub mod transport;
pub mod pty;
pub mod pipe;
pub mod parser;
pub mod ndjson;
pub mod detection;
pub mod cli;
pub mod snapshot;
pub mod history;
pub mod manager;

pub use manager::{MultiCliManager, ManagerConfig, InstanceId, InstanceMode};
pub use snapshot::{
    AgentCli, AgentRenderSnapshot, AgentSnapshotMode, BuddyArt, ChatMessage, ChatRole, TermCell, TermGrid,
};
pub use history::{HistoryReader, SessionMeta, reader_for};

pub(crate) mod error;
pub(crate) mod types;
pub(crate) mod utils;
