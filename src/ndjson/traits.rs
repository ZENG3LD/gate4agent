//! NDJSON parser trait and unified CliEvent type.

use serde_json::Value;

/// Unified event type from any AI CLI tool's NDJSON stream.
#[derive(Debug, Clone)]
pub enum CliEvent {
    /// Session initialized (model, session ID, tools list).
    SessionStart {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },
    /// Assistant text (complete message or streaming delta).
    AssistantText {
        text: String,
        is_delta: bool,
    },
    /// Tool call initiated by the assistant.
    ToolCallStart {
        id: String,
        name: String,
        input: Value,
    },
    /// Tool call result returned.
    ToolCallResult {
        id: String,
        output: String,
        is_error: bool,
        duration_ms: Option<u64>,
    },
    /// Thinking/reasoning content.
    Thinking {
        text: String,
    },
    /// Turn completed with token usage.
    TurnComplete {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Session ended.
    SessionEnd {
        result: String,
        cost_usd: Option<f64>,
        is_error: bool,
    },
    /// Error event.
    Error {
        message: String,
    },
}

/// Trait for parsing NDJSON lines into CliEvents.
pub trait NdjsonParser: Send {
    /// Parse a single JSON line into zero or more events.
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent>;

    /// Get the session ID if known.
    fn session_id(&self) -> Option<&str>;
}
