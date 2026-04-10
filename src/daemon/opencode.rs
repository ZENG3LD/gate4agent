//! OpenCode daemon adapter.
//!
//! Connects to `opencode serve` HTTP API:
//! - POST /session — create session
//! - POST /session/:id/message — send prompt (blocking)
//! - POST /session/:id/prompt_async — send prompt (async)
//! - GET /session — list sessions
//! - GET /event — SSE event stream (all sessions, filter by sessionID)
//!
//! Default port: 4096
//! Auth: HTTP Basic (optional, via OPENCODE_SERVER_PASSWORD env var)
//!
//! NOT YET IMPLEMENTED — this module documents the API surface for future work.

use crate::core::types::AgentEvent;

/// Map an OpenCode SSE event to an AgentEvent.
///
/// Known SSE event types (unverified — need live testing):
/// - `message.part.delta` with text → AgentEvent::Text
/// - `message.part.delta` with tool_use → AgentEvent::ToolStart
/// - `session.idle` → AgentEvent::TurnComplete
/// - `session.error` → AgentEvent::SessionEnd { is_error: true }
pub fn parse_sse_event(_event_type: &str, _data: &str) -> Option<AgentEvent> {
    // TODO: Implement when testing against live `opencode serve`
    None
}
