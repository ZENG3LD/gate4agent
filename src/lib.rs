//! gate4agent — Universal wrapper for CLI agents (Claude Code, Codex, Gemini, Cursor, OpenCode, OpenClaw).
//!
//! Three transport modes:
//! - PTY mirror: spawns agent in real PTY, captures raw output, vt100 parsing
//! - Pipe mode: NDJSON-streaming pipe sessions (Claude, Codex, Gemini, Cursor, OpenCode)
//! - Daemon harness: pipe client over a pre-running daemon (OpenClaw via acpx)
//!
//! All modes produce `AgentEvent` values on a `tokio::sync::broadcast` channel.
//!
//! # Entry points
//!
//! - [`TransportSession::spawn`] — unified entry for all 6 pipe/daemon CLIs
//! - [`pty::PtySession::spawn`] — PTY mirror mode (unchanged)
//! - [`MultiCliManager`] — high-level session manager for the chart app

pub use error::AgentError;
pub use types::{AgentEvent, CliTool, PtyEvent, SessionConfig};
pub use transport::{SpawnOptions, DaemonProbe, DaemonSpec, ensure_daemon_running, TransportSession};

pub mod transport;
pub mod daemon;
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
