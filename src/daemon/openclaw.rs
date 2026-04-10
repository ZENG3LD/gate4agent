//! OpenClaw daemon adapter.
//!
//! Connects to OpenClaw WebSocket + HTTP API:
//! - HTTP: 12 REST endpoints on port 18789
//! - WS: `ws://host:port/ws` — protocol v3 (typed req/res/event frames)
//! - Auth: Bearer token
//!
//! Key endpoints:
//! - POST /sessions — create session
//! - POST /sessions/:id/messages — send message
//! - WS event frames for streaming
//!
//! ACP bridge (acpx) — manages CLI agents as sub-processes:
//! - `sessions_spawn` with `runtime: "acp"` + `agentId: "claude"`
//! - Persistent multi-turn sessions with structured streaming
//!
//! NOT YET IMPLEMENTED — this module documents the API surface for future work.

use crate::core::types::AgentEvent;

/// Map an OpenClaw WebSocket event frame to an AgentEvent.
///
/// WS protocol v3 frame types (unverified):
/// - `event` with `type: "text"` → AgentEvent::Text
/// - `event` with `type: "tool_start"` → AgentEvent::ToolStart
/// - `event` with `type: "tool_result"` → AgentEvent::ToolResult
/// - `res` with completion → AgentEvent::TurnComplete
pub fn parse_ws_event(_frame: &str) -> Option<AgentEvent> {
    // TODO: Implement when testing against live OpenClaw
    None
}
