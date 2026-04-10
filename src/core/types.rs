//! Core shared types for gate4agent.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Supported CLI tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum CliTool {
    #[serde(alias = "claude")]
    #[default]
    ClaudeCode,
    Codex,
    Gemini,
    /// OpenCode (sst/opencode) — PIPE transport, own 5-event NDJSON schema.
    OpenCode,
}

impl std::fmt::Display for CliTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliTool::ClaudeCode => write!(f, "Claude Code"),
            CliTool::Codex => write!(f, "Codex"),
            CliTool::Gemini => write!(f, "Gemini"),
            CliTool::OpenCode => write!(f, "OpenCode"),
        }
    }
}

/// Session configuration for spawning an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// CLI tool to use.
    pub tool: CliTool,
    /// Working directory.
    pub working_dir: PathBuf,
    /// Environment variables to set.
    pub env_vars: Vec<(String, String)>,
    /// Session name/identifier.
    pub name: Option<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            tool: CliTool::ClaudeCode,
            working_dir: std::env::current_dir().unwrap_or_default(),
            env_vars: Vec::new(),
            name: None,
        }
    }
}

/// Detected rate limit information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// Type of rate limit.
    pub limit_type: RateLimitType,
    /// When the limit resets (if known).
    pub resets_at: Option<DateTime<Utc>>,
    /// Usage percentage (if known).
    pub usage_percent: Option<f64>,
    /// Raw message from CLI.
    pub raw_message: String,
    /// When detected.
    pub detected_at: DateTime<Utc>,
}

/// Type of rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateLimitType {
    /// Session/hourly limit.
    Session,
    /// Daily limit.
    Daily,
    /// Weekly limit.
    Weekly,
    /// Unknown limit type.
    Unknown,
}

/// Unified event type produced by both PTY and pipe transports.
///
/// Consumers subscribe to a `broadcast::Receiver<AgentEvent>` and
/// never need to know which transport produced the event.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    // --- Lifecycle ---
    /// Process spawned, session ID assigned.
    Started { session_id: String },
    /// Process exited with code.
    Exited { code: i32 },
    /// Error from transport or parsing.
    Error { message: String },

    // --- PTY mirror mode events ---
    /// Raw byte chunk from PTY (pre-ANSI-strip). Use for vt100 screen emulation.
    /// Uses `Vec<u8>` (not String) so multi-byte UTF-8 sequences split across
    /// reads are not corrupted by `from_utf8_lossy` replacement characters.
    PtyRaw { data: Vec<u8> },
    /// Classified PTY output (post-VTE-strip + OutputParser classification).
    PtyParsed(crate::pty::cli::traits::ParsedMessage),
    /// PTY session ready for input (PromptReady detected).
    PtyReady,
    /// PTY tool approval needed.
    PtyToolApproval { tool_name: String, description: Option<String> },

    // --- Stream events (transport-neutral; formerly Pipe-prefixed) ---
    /// Session initialized. Produced by all PIPE and DaemonHarness transports.
    SessionStart { session_id: String, model: String, tools: Vec<String> },
    /// Streaming text delta from assistant (is_delta=true) or complete turn text.
    Text { text: String, is_delta: bool },
    /// Tool call started by assistant.
    ToolStart { id: String, name: String, input: serde_json::Value },
    /// Tool call completed.
    ToolResult { id: String, output: String, is_error: bool, duration_ms: Option<u64> },
    /// Assistant thinking/reasoning block.
    Thinking { text: String },
    /// Turn complete with token usage.
    TurnComplete { input_tokens: u64, output_tokens: u64 },
    /// Session ended with final result.
    SessionEnd { result: String, cost_usd: Option<f64>, is_error: bool },

    // --- Both modes ---
    /// Rate limit detected (from text pattern matching).
    RateLimit(RateLimitInfo),

    // --- JSON-RPC 2.0 (RpcSession) ---
    /// Raw JSON-RPC notification from agent that did not map to a structured
    /// event. Consumers can inspect `method` and parse `params` themselves.
    RpcNotification { method: String, params: serde_json::Value },

    /// Agent sent a JSON-RPC request to the host (for observer purposes).
    ///
    /// The `RpcSession` reader loop has already handled it via `HostHandler`
    /// and sent the response. This variant lets subscribers audit what the
    /// agent requested without needing their own handler.
    RpcIncomingRequest {
        id: crate::rpc::message::RpcId,
        method: String,
        params: Option<serde_json::Value>,
    },
}
