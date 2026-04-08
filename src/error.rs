//! Unified error enum for the cli2pty4agent crate.

/// Unified error type for all operations in this crate.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("PTY creation failed: {0}")]
    PtyCreate(String),

    #[error("PTY spawn failed: {0}")]
    PtySpawn(String),

    #[error("PTY I/O error: {source}")]
    PtyIo {
        #[from]
        source: std::io::Error,
    },

    #[error("PTY operation failed: {0}")]
    Pty(String),

    #[error("Process spawn failed: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("Broadcast send error (no receivers)")]
    BroadcastSend,

    #[error("JSON parse error: {source}")]
    Json {
        #[from]
        source: serde_json::Error,
    },

    #[error("required daemon is not running at {host}:{port}: {detail}")]
    DaemonNotRunning { host: String, port: u16, detail: String },

    #[error("daemon probe timed out after {timeout_ms}ms at {host}:{port}")]
    DaemonProbeTimeout { host: String, port: u16, timeout_ms: u64 },
}
