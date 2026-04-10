//! Configuration types for daemon transport connections.

/// Daemon target type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonType {
    /// OpenCode (`opencode serve`) — HTTP + SSE
    OpenCode,
    /// OpenClaw — HTTP + WebSocket
    OpenClaw,
}

/// Authentication method for daemon connection.
#[derive(Debug, Clone)]
pub enum DaemonAuth {
    /// HTTP Basic Auth (OpenCode: OPENCODE_SERVER_PASSWORD)
    Basic { password: String },
    /// Bearer token (OpenClaw)
    Bearer { token: String },
}

/// Configuration for connecting to a daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Daemon type
    pub daemon_type: DaemonType,
    /// Host (default "127.0.0.1")
    pub host: String,
    /// Port (OpenCode: 4096, OpenClaw: 18789)
    pub port: u16,
    /// Authentication (None for unauthenticated localhost)
    pub auth: Option<DaemonAuth>,
    /// Resume existing session by ID
    pub session_id: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            daemon_type: DaemonType::OpenCode,
            host: "127.0.0.1".to_string(),
            port: 4096,
            auth: None,
            session_id: None,
        }
    }
}
