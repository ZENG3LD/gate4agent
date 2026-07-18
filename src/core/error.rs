//! Unified error enum for gate4agent.

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

    #[error("PTY process observation failed: {source}")]
    PtyProcessProbe {
        #[from]
        source: crate::pty::PtyProcessProbeError,
    },

    #[error("PTY process-tree termination failed: {source}")]
    PtyTreeTermination {
        #[from]
        source: crate::pty::PtyTreeTerminationError,
    },

    #[error("PTY shutdown did not reach an ordered exit within {timeout_ms}ms")]
    PtyShutdownTimedOut { timeout_ms: u64 },

    #[error("agent launch plan failed: {source}")]
    LaunchPlan {
        #[from]
        source: crate::agent::LaunchPlanError,
    },

    #[error("terminal input preparation failed: {source}")]
    InputPrepare {
        #[from]
        source: crate::agent::InputPrepareError,
    },

    #[error(
        "readiness permit belongs to agent '{permit_agent}', not session agent '{session_agent}'"
    )]
    PtyReadinessAgentMismatch {
        session_agent: crate::agent::AgentId,
        permit_agent: crate::agent::AgentId,
    },

    #[error("readiness permit intent {actual:?} cannot authorize {required:?}")]
    PtyReadinessIntentMismatch {
        required: crate::agent::ReadinessIntent,
        actual: crate::agent::ReadinessIntent,
    },

    #[error("agent '{agent}' does not declare capability '{capability}'")]
    AgentCapabilityUnsupported {
        agent: crate::agent::AgentId,
        capability: &'static str,
    },

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

    #[error("Spawn failed: {0}")]
    SpawnFailed(String),
}
