//! Daemon session — connection to a running HTTP/WebSocket AI agent daemon.

use tokio::sync::broadcast;
use crate::core::types::AgentEvent;
use crate::core::error::AgentError;
use super::config::DaemonConfig;

/// Session connected to a daemon (OpenCode serve or OpenClaw).
///
/// Parallel to `PipeSession` but for HTTP/WebSocket daemons.
/// NOT YET FUNCTIONAL — skeleton for future implementation.
pub struct DaemonSession {
    config: DaemonConfig,
    session_id: Option<String>,
    tx: broadcast::Sender<AgentEvent>,
}

impl DaemonSession {
    /// Connect to a running daemon.
    ///
    /// # Errors
    /// Returns `AgentError` if the daemon is unreachable.
    pub async fn connect(config: DaemonConfig) -> Result<Self, AgentError> {
        let (tx, _) = broadcast::channel(256);
        // TODO: Actually connect to daemon, create/resume session
        Ok(Self {
            config,
            session_id: None,
            tx,
        })
    }

    /// Subscribe to agent events from this daemon session.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Send a follow-up prompt to the daemon session.
    pub async fn send_prompt(&self, _prompt: &str) -> Result<(), AgentError> {
        // TODO: POST /session/:id/message (OpenCode) or WS frame (OpenClaw)
        Err(AgentError::SpawnFailed(
            "DaemonSession::send_prompt not yet implemented".into(),
        ))
    }

    /// Get the current session ID.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Get a reference to the daemon configuration.
    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// Kill the daemon session.
    pub async fn kill(&self) -> Result<(), AgentError> {
        // TODO: DELETE session or close WS
        Ok(())
    }
}
