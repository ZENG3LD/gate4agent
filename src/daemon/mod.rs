//! Daemon transport — connect to long-running HTTP/WebSocket AI agent servers.
//!
//! One daemon target supported:
//! - **OpenClaw** — HTTP REST + WebSocket on port 18789

pub mod session;
pub mod config;
pub mod openclaw;

pub use config::{DaemonConfig, DaemonType, DaemonAuth};
pub use session::DaemonSession;
