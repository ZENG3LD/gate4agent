//! Daemon transport — connect to long-running HTTP/WebSocket AI agent servers.
//!
//! Two daemon targets supported:
//! - **OpenCode** (`opencode serve`) — HTTP REST + SSE on port 4096
//! - **OpenClaw** — HTTP REST + WebSocket on port 18789

pub mod session;
pub mod config;
pub mod opencode;
pub mod openclaw;

pub use config::{DaemonConfig, DaemonType, DaemonAuth};
pub use session::DaemonSession;
