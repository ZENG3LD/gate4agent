//! ACP-specific message parameter/result structs.
//!
//! These are the typed params used on top of the generic JSON-RPC wire types
//! in [`crate::rpc::message`]. The RPC wire types (`RpcRequest`, `RpcResponse`,
//! `classify_line`) are reused as-is; only the ACP payload shapes live here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::types::AgentEvent;

// ---------------------------------------------------------------------------
// Outbound: host → agent
// ---------------------------------------------------------------------------

/// `initialize` request params (host → agent, id=0 per ACP convention).
///
/// Outbound-only: serialized to JSON and sent to the agent subprocess.
#[derive(Debug, Serialize)]
pub struct InitializeParams {
    /// ACP protocol version — must be an integer (1), not a string.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "clientCapabilities")]
    pub client_capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// Capabilities advertised by the host to the agent during `initialize`.
///
/// Outbound-only.
#[derive(Debug, Serialize)]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

/// File-system capability flags within [`ClientCapabilities`].
///
/// Outbound-only.
#[derive(Debug, Serialize)]
pub struct FsCapabilities {
    #[serde(rename = "readTextFile")]
    pub read_text_file: bool,
    #[serde(rename = "writeTextFile")]
    pub write_text_file: bool,
}

/// Identifies the host client in the `initialize` request.
///
/// Outbound-only.
#[derive(Debug, Serialize)]
pub struct ClientInfo {
    pub name: &'static str,
    /// Human-readable display name (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'static str>,
    pub version: &'static str,
}

/// A single MCP server entry for `session/new`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum McpServerConfig {
    /// stdio-based MCP server launched as a subprocess.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    },
    /// SSE-based MCP server at a URL.
    Sse {
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
    },
}

/// `session/new` request params.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionNewParams {
    pub cwd: String,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// A content block in a `session/prompt` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

/// `session/prompt` request params.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionPromptParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

/// `session/cancel` notification params (host → agent, no response expected).
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionCancelParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// `session/load` request params (host → agent).
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionLoadParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// `session/load` response result (agent → host).
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SessionLoadResult {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// Inbound: agent → host
// ---------------------------------------------------------------------------

/// `session/update` notification params (agent → host streaming events).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUpdateParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub update: SessionUpdate,
}

/// Discriminated union of all known `session/update` payload variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate")]
pub enum SessionUpdate {
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk {
        /// Can be a single content block object, an array of content blocks,
        /// or absent. Some ACP agents send a single object; Claude ACP sends an array.
        #[serde(default)]
        content: Value,
    },
    #[serde(rename = "agent_thought_chunk")]
    AgentThoughtChunk {
        /// Can be `{"thought": "..."}` (some ACP agents) or a plain string.
        #[serde(default)]
        content: Value,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        #[serde(rename = "toolCallId", default)]
        tool_call_id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        status: String,
        #[serde(rename = "rawInput", default)]
        raw_input: Value,
        #[serde(default)]
        locations: Vec<Value>,
    },
    #[serde(rename = "tool_call_update")]
    ToolCallUpdate {
        #[serde(rename = "toolCallId", default)]
        tool_call_id: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        content: Vec<Value>,
    },
    #[serde(rename = "stop")]
    Stop {
        #[serde(rename = "stopReason", default)]
        stop_reason: String,
        #[serde(rename = "inputTokens", default)]
        input_tokens: u64,
        #[serde(rename = "outputTokens", default)]
        output_tokens: u64,
        #[serde(default)]
        usage: Option<Value>,
    },
    #[serde(other)]
    Unknown,
}

/// `fs/read_text_file` request params (agent → host).
///
/// `line` (1-based) and `limit` restrict the read to a line window, matching
/// the ACP spec's optional partial-read fields; both default to "whole
/// file" when absent, which is what every agent that predates this feature
/// sends.
#[derive(Debug, Serialize, Deserialize)]
pub struct FsReadParams {
    pub path: String,
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `fs/write_text_file` request params (agent → host).
#[derive(Debug, Serialize, Deserialize)]
pub struct FsWriteParams {
    pub path: String,
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    pub content: String,
}

/// A `name`/`value` environment variable entry, as the ACP wire form for
/// `terminal/create`'s `env` uses (an array of pairs, not a JSON object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

/// `terminal/create` request params (agent → host).
#[derive(Debug, Serialize, Deserialize)]
pub struct TerminalCreateParams {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVariable>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(rename = "outputByteLimit", default)]
    pub output_byte_limit: Option<u64>,
}

/// Shared `{sessionId, terminalId}` params for `terminal/output`,
/// `terminal/wait_for_exit`, `terminal/kill`, and `terminal/release`.
#[derive(Debug, Serialize, Deserialize)]
pub struct TerminalIdParams {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(rename = "terminalId", default)]
    pub terminal_id: String,
}

/// A terminal's exit condition — result of `terminal/wait_for_exit`, and
/// carried inside `terminal/output`'s result once the command has finished.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalExitStatus {
    #[serde(rename = "exitCode", default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
}

/// `terminal/output` result (host → agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalOutputResult {
    pub output: String,
    pub truncated: bool,
    #[serde(rename = "exitStatus", default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<TerminalExitStatus>,
}

/// The kind of operation a tool call performs, carried on
/// [`PermissionToolCall::kind`] so a host policy can decide by intent (e.g.
/// "read" is safe to auto-allow, "execute" is not) without inspecting
/// vendor-specific tool names. `Other` is both the literal wire value for
/// "none of the above" AND the fallback for any kind string a future ACP
/// revision adds that this build does not know yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    #[serde(other)]
    #[default]
    Other,
}

impl ToolKind {
    /// Whether this kind only observes state rather than changing it or
    /// running arbitrary code — the set a `ReadOnly` host policy auto-allows.
    pub(crate) fn is_read_only(self) -> bool {
        matches!(self, ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Fetch)
    }
}

/// A file-system location a tool call touches, as carried on
/// [`PermissionToolCall::locations`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(default)]
    pub line: Option<u32>,
}

/// The tool-call summary embedded in a `session/request_permission` request
/// — just enough for a host policy to decide, not the full `session/update`
/// tool-call shape. Every field defaults so an agent that omits some of
/// them (or all of them) still parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionToolCall {
    #[serde(rename = "toolCallId", default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: ToolKind,
    #[serde(default)]
    pub locations: Vec<ToolCallLocation>,
    /// Raw tool-specific input the agent attached to this permission
    /// request, when it chose to include one -- e.g. `{"command": "rm -rf
    /// /"}` for an `execute`-kind tool call. The ACP spec does not mandate
    /// this field be present on a permission request the way it is on a
    /// `tool_call` session update; many agents omit it here. Defaults to
    /// `Value::Null`, which the dangerous-command gate
    /// (`super::gate::evaluate_permission_tool_call`) treats as "nothing to
    /// inspect" rather than as an empty/safe command.
    #[serde(rename = "rawInput", default)]
    pub raw_input: Value,
}

/// The four option kinds the ACP spec defines for
/// [`PermissionOption::kind`]. An agent is not required to offer all four —
/// see [`PermissionRequestParams`] and the host policy that selects among
/// whichever subset arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One option the agent is offering the host for a `session/request_
/// permission` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionOption {
    #[serde(rename = "optionId")]
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// `session/request_permission` request params (agent → host) — the real
/// ACP shape: a `toolCall` summary of what is being asked for, and the
/// concrete `options` the agent is willing to accept a decision from. There
/// is no bare `allowed: bool` on the wire; the host must pick one of
/// `options` (or decline to pick any of them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequestParams {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(rename = "toolCall", default)]
    pub tool_call: PermissionToolCall,
    #[serde(default)]
    pub options: Vec<PermissionOption>,
}

/// `session/request_permission` result (host → agent) — the host either
/// selects one of the request's `options` by ID, or cancels without
/// selecting any of them (used e.g. when none of the offered options match
/// the host's policy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

// ---------------------------------------------------------------------------
// Agent capabilities returned by `initialize`
// ---------------------------------------------------------------------------

/// Full `initialize` response returned by the agent.
///
/// The ACP spec wraps capabilities under `agentCapabilities`; this struct
/// mirrors the top-level response shape. All fields are `#[serde(default)]`
/// so that we tolerate agents that omit optional fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentCapabilities {
    /// Protocol version echoed by the agent (integer, e.g. `1`).
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: u32,
    /// Agent-specific capability flags.
    #[serde(rename = "agentCapabilities", default)]
    pub agent_capabilities: AgentCapabilityFlags,
    /// Information about the agent binary.
    #[serde(rename = "agentInfo", default)]
    pub agent_info: AgentInfo,
    /// Authentication methods supported by the agent.
    #[serde(rename = "authMethods", default)]
    pub auth_methods: Vec<Value>,
}

/// Flags inside `agentCapabilities`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentCapabilityFlags {
    #[serde(rename = "loadSession", default)]
    pub load_session: bool,
    /// Remaining capability fields (future-proofing).
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

/// Agent identity returned in the `initialize` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub version: String,
}

// ---------------------------------------------------------------------------
// update_to_event
// ---------------------------------------------------------------------------

/// Extract token counts from a raw ACP response value.
///
/// Tries multiple known shapes:
/// 1. ACP canonical camelCase: `{"inputTokens": N, "outputTokens": N}`
/// 2. Claude-nested usage: `{"usage": {"input_tokens": N, "output_tokens": N}}`
/// 3. Stats-nested: `{"stats": {"input_tokens": N, "output_tokens": N}}`
///
/// Returns `(0, 0)` if nothing matches.
pub(crate) fn extract_token_usage(v: &Value) -> (u64, u64) {
    // 1. Top-level camelCase (ACP canonical)
    if let (Some(i), Some(o)) = (
        v.get("inputTokens").and_then(|x| x.as_u64()),
        v.get("outputTokens").and_then(|x| x.as_u64()),
    ) {
        return (i, o);
    }

    // 2. Nested under "usage" (Claude ACP)
    if let Some(usage) = v.get("usage") {
        if let (Some(i), Some(o)) = (
            usage.get("input_tokens").and_then(|x| x.as_u64()),
            usage.get("output_tokens").and_then(|x| x.as_u64()),
        ) {
            return (i, o);
        }
    }

    // 3. Nested under "stats"
    if let Some(stats) = v.get("stats") {
        if let (Some(i), Some(o)) = (
            stats.get("input_tokens").and_then(|x| x.as_u64()),
            stats.get("output_tokens").and_then(|x| x.as_u64()),
        ) {
            return (i, o);
        }
    }

    (0, 0)
}

/// Extract text from a content value that may be:
/// - A single object `{"type": "text", "text": "..."}` (some ACP agents)
/// - An array of content blocks `[{"type": "text", "text": "..."}, ...]` (Claude ACP)
/// - A plain string
fn extract_text_from_content(content: &Value) -> String {
    // Case 1: single object with a "text" field
    if let Some(t) = content.get("text").and_then(|v| v.as_str()) {
        return t.to_owned();
    }
    // Case 2: array of content blocks
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("");
    }
    // Case 3: plain string
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    String::new()
}

/// Convert `session/update` notification params to zero or more [`AgentEvent`]s.
///
/// Returns a `Vec` because `stop` with reason `end_turn` maps to both
/// `TurnComplete` and `SessionEnd`. Returns an empty vec for unknown update
/// types — callers should emit an `AgentEvent::RpcNotification` passthrough
/// in that case.
pub(crate) fn update_to_event(params: &SessionUpdateParams) -> Vec<AgentEvent> {
    match &params.update {
        SessionUpdate::AgentMessageChunk { content } => {
            let text = extract_text_from_content(content);
            if text.is_empty() {
                vec![]
            } else {
                vec![AgentEvent::Text { text, is_delta: true }]
            }
        }

        SessionUpdate::AgentThoughtChunk { content } => {
            let text = content
                .get("thought")
                .and_then(|v| v.as_str())
                .or_else(|| content.as_str())
                .unwrap_or("")
                .to_owned();
            vec![AgentEvent::Thinking { text }]
        }

        SessionUpdate::ToolCall { tool_call_id, title, raw_input, .. } => {
            vec![AgentEvent::ToolStart {
                id: tool_call_id.clone(),
                name: title.clone(),
                input: raw_input.clone(),
            }]
        }

        SessionUpdate::ToolCallUpdate { tool_call_id, status, content } => {
            let output = content
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("");
            let is_error = status == "error";
            vec![AgentEvent::ToolResult {
                id: tool_call_id.clone(),
                output,
                is_error,
                duration_ms: None,
            }]
        }

        SessionUpdate::Stop { stop_reason, input_tokens, output_tokens, usage } => {
            // Prefer direct fields; fall back to the usage sub-object.
            let (tok_in, tok_out) = if *input_tokens > 0 || *output_tokens > 0 {
                (*input_tokens, *output_tokens)
            } else if let Some(u) = usage {
                extract_token_usage(u)
            } else {
                (0, 0)
            };
            vec![
                AgentEvent::TurnComplete {
                    input_tokens: tok_in,
                    output_tokens: tok_out,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: 0,
                    context_window: None,
                    is_cumulative: false,
                },
                AgentEvent::SessionEnd {
                    result: stop_reason.clone(),
                    cost_usd: None,
                    is_error: false,
                },
            ]
        }

        SessionUpdate::Unknown => vec![],
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_update(update: SessionUpdate) -> SessionUpdateParams {
        SessionUpdateParams { session_id: "s1".to_string(), update }
    }

    #[test]
    fn update_to_event_text_delta_array() {
        // Claude ACP: content is an array of content blocks
        let p = make_update(SessionUpdate::AgentMessageChunk {
            content: json!([{"type": "text", "text": "hello"}]),
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Text { text, is_delta: true } if text == "hello"));
    }

    #[test]
    fn update_to_event_text_delta_single_object() {
        // Some ACP agents: content is a single object, not an array
        let p = make_update(SessionUpdate::AgentMessageChunk {
            content: json!({"type": "text", "text": "hello"}),
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Text { text, is_delta: true } if text == "hello"));
    }

    #[test]
    fn update_to_event_text_delta_plain_string() {
        // Fallback: content is a plain string
        let p = make_update(SessionUpdate::AgentMessageChunk {
            content: json!("hello"),
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Text { text, is_delta: true } if text == "hello"));
    }

    #[test]
    fn update_to_event_text_delta_empty_returns_no_events() {
        let p = make_update(SessionUpdate::AgentMessageChunk {
            content: json!(null),
        });
        let events = update_to_event(&p);
        assert!(events.is_empty());
    }

    #[test]
    fn update_to_event_thinking_plain_string() {
        let p = make_update(SessionUpdate::AgentThoughtChunk {
            content: json!("thinking..."),
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Thinking { text } if text == "thinking..."));
    }

    #[test]
    fn update_to_event_thinking_thought_field() {
        // Some ACP agents: thought wrapped in {"thought": "..."}
        let p = make_update(SessionUpdate::AgentThoughtChunk {
            content: json!({"thought": "deep thought"}),
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Thinking { text } if text == "deep thought"));
    }

    #[test]
    fn update_to_event_tool_start() {
        let p = make_update(SessionUpdate::ToolCall {
            tool_call_id: "t1".to_string(),
            title: "bash".to_string(),
            kind: "bash".to_string(),
            status: "pending".to_string(),
            raw_input: json!({"cmd": "ls"}),
            locations: vec![],
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentEvent::ToolStart { id, name, .. } if id == "t1" && name == "bash")
        );
    }

    #[test]
    fn update_to_event_tool_result() {
        let p = make_update(SessionUpdate::ToolCallUpdate {
            tool_call_id: "t1".to_string(),
            status: "done".to_string(),
            content: vec![json!("ok")],
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { id, output, is_error, .. }
                if id == "t1" && output == "ok" && !is_error)
        );
    }

    #[test]
    fn update_to_event_stop_emits_two_events() {
        let p = make_update(SessionUpdate::Stop {
            stop_reason: "end_turn".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            usage: None,
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::TurnComplete { .. }));
        assert!(matches!(&events[1], AgentEvent::SessionEnd { is_error: false, .. }));
    }

    #[test]
    fn extract_token_usage_acp_canonical() {
        let v = json!({"inputTokens": 10, "outputTokens": 5});
        assert_eq!(extract_token_usage(&v), (10, 5));
    }

    #[test]
    fn extract_token_usage_claude_nested() {
        let v = json!({"usage": {"input_tokens": 10, "output_tokens": 5}});
        assert_eq!(extract_token_usage(&v), (10, 5));
    }

    #[test]
    fn extract_token_usage_stats_nested() {
        let v = json!({"stats": {"input_tokens": 10, "output_tokens": 5}});
        assert_eq!(extract_token_usage(&v), (10, 5));
    }

    #[test]
    fn extract_token_usage_missing() {
        let v = json!({});
        assert_eq!(extract_token_usage(&v), (0, 0));
    }

    #[test]
    fn update_to_event_unknown_returns_empty() {
        let p = make_update(SessionUpdate::Unknown);
        let events = update_to_event(&p);
        assert!(events.is_empty());
    }

    #[test]
    fn session_update_params_round_trip() {
        let original = SessionUpdateParams {
            session_id: "abc".to_string(),
            update: SessionUpdate::AgentMessageChunk {
                content: json!([{"type": "text", "text": "hi"}]),
            },
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: SessionUpdateParams = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.session_id, "abc");
        assert!(matches!(
            deserialized.update,
            SessionUpdate::AgentMessageChunk { .. }
        ));
    }

    #[test]
    fn session_update_params_single_object_content_round_trip() {
        // Simulate an ACP agent whose wire format uses a single object, not an array
        let raw = r#"{"sessionId":"s1","update":{"content":{"text":"hello","type":"text"},"sessionUpdate":"agent_message_chunk"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Text { text, is_delta: true } if text == "hello"));
    }

    #[test]
    fn initialize_params_serialize_camel_case() {
        let p = InitializeParams {
            protocol_version: 1,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities { read_text_file: true, write_text_file: true },
                terminal: true,
            },
            client_info: ClientInfo { name: "gate4agent", title: Some("Gate4Agent"), version: "0.2.0" },
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("protocolVersion"), "must use protocolVersion");
        assert!(s.contains("clientInfo"), "must use clientInfo");
        assert!(s.contains("clientCapabilities"), "must include clientCapabilities");
        // protocolVersion must be an integer, not a quoted string
        assert!(s.contains(r#""protocolVersion":1"#), "protocolVersion must be integer 1");
    }

    #[test]
    fn session_new_params_serialize() {
        let p = SessionNewParams { cwd: "/home/user".to_string(), mcp_servers: vec![] };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"cwd\""), "must use cwd");
        assert!(s.contains("\"mcpServers\""), "must use mcpServers");
    }

    #[test]
    fn session_prompt_params_wraps_content_blocks() {
        let p = SessionPromptParams {
            session_id: "s1".to_string(),
            prompt: vec![ContentBlock::Text { text: "hello".to_string() }],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"prompt\""), "must have prompt field");
        assert!(s.contains("\"type\":\"text\""), "content block must have type=text");
        assert!(s.contains("\"text\":\"hello\""), "must have text content");
    }

    #[test]
    fn session_load_params_serialize() {
        let p = SessionLoadParams { session_id: "prior-session-123".to_string() };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"sessionId\""), "must use sessionId");
        assert!(s.contains("prior-session-123"), "must contain the session id value");

        // Round-trip
        let p2: SessionLoadParams = serde_json::from_str(&s).unwrap();
        assert_eq!(p2.session_id, "prior-session-123");
    }

    #[test]
    fn session_load_result_deserialize_with_session_id() {
        let raw = r#"{"sessionId":"new-session-456"}"#;
        let r: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert_eq!(r.session_id, "new-session-456");
    }

    #[test]
    fn session_load_result_deserialize_without_session_id() {
        // Agent may omit sessionId — should default to empty string
        let raw = r#"{}"#;
        let r: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert!(r.session_id.is_empty());
    }

    #[test]
    fn mcp_server_config_stdio_serialize() {
        let cfg = McpServerConfig::Stdio {
            command: "my-mcp-server".to_string(),
            args: vec!["--port".to_string(), "8080".to_string()],
            env: std::collections::HashMap::new(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(s.contains(r#""transport":"stdio""#), "must tag as stdio");
        assert!(s.contains("my-mcp-server"), "must contain command");
    }

    #[test]
    fn mcp_server_config_sse_serialize() {
        let cfg = McpServerConfig::Sse {
            url: "https://example.com/mcp".to_string(),
            headers: std::collections::HashMap::new(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(s.contains(r#""transport":"sse""#), "must tag as sse");
        assert!(s.contains("https://example.com/mcp"), "must contain url");
    }

    #[test]
    fn mcp_server_config_roundtrip() {
        let original = McpServerConfig::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
            env: {
                let mut m = std::collections::HashMap::new();
                m.insert("HOME".to_string(), "/home/user".to_string());
                m
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: McpServerConfig = serde_json::from_str(&json).unwrap();
        match decoded {
            McpServerConfig::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@modelcontextprotocol/server-filesystem"]);
                assert_eq!(env.get("HOME").map(String::as_str), Some("/home/user"));
            }
            McpServerConfig::Sse { .. } => panic!("expected Stdio variant"),
        }
    }

    // -----------------------------------------------------------------------
    // session/request_permission — protocol-accurate shape
    // -----------------------------------------------------------------------

    #[test]
    fn permission_request_params_parses_the_real_wire_shape() {
        let raw = r#"{
            "sessionId": "s1",
            "toolCall": {
                "toolCallId": "tc1",
                "title": "Edit config.toml",
                "kind": "edit",
                "locations": [{"path": "/repo/config.toml", "line": 12}]
            },
            "options": [
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject-once", "name": "Reject once", "kind": "reject_once"}
            ]
        }"#;
        let params: PermissionRequestParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.session_id, "s1");
        assert_eq!(params.tool_call.tool_call_id, "tc1");
        assert_eq!(params.tool_call.kind, ToolKind::Edit);
        assert_eq!(params.tool_call.locations.len(), 1);
        assert_eq!(params.tool_call.locations[0].path, "/repo/config.toml");
        assert_eq!(params.tool_call.locations[0].line, Some(12));
        assert_eq!(params.options.len(), 2);
        assert_eq!(params.options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(params.options[1].kind, PermissionOptionKind::RejectOnce);
    }

    #[test]
    fn permission_request_params_survives_an_agent_missing_reject_always() {
        // Some agents only ever offer allow_once/allow_always/reject_once —
        // no reject_always. The parser must not require all four kinds.
        let raw = r#"{
            "sessionId": "s1",
            "toolCall": {"toolCallId": "tc1", "kind": "execute"},
            "options": [
                {"optionId": "a1", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "a2", "name": "Allow always", "kind": "allow_always"},
                {"optionId": "r1", "name": "Reject once", "kind": "reject_once"}
            ]
        }"#;
        let params: PermissionRequestParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.options.len(), 3);
        assert!(params
            .options
            .iter()
            .all(|option| option.kind != PermissionOptionKind::RejectAlways));
    }

    #[test]
    fn permission_request_params_tolerates_a_missing_tool_call_and_options() {
        // Defensive parsing: an agent that sends only sessionId still parses,
        // it just carries no usable tool-call detail or options.
        let raw = r#"{"sessionId": "s1"}"#;
        let params: PermissionRequestParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.tool_call.kind, ToolKind::Other);
        assert!(params.options.is_empty());
    }

    #[test]
    fn tool_kind_unknown_string_falls_back_to_other() {
        let raw = r#"{"toolCallId": "tc1", "kind": "not-a-real-kind"}"#;
        let tool_call: PermissionToolCall = serde_json::from_str(raw).unwrap();
        assert_eq!(tool_call.kind, ToolKind::Other);
    }

    #[test]
    fn tool_kind_serializes_the_nine_canonical_wire_values() {
        let cases = [
            (ToolKind::Read, "\"read\""),
            (ToolKind::Edit, "\"edit\""),
            (ToolKind::Delete, "\"delete\""),
            (ToolKind::Move, "\"move\""),
            (ToolKind::Search, "\"search\""),
            (ToolKind::Execute, "\"execute\""),
            (ToolKind::Think, "\"think\""),
            (ToolKind::Fetch, "\"fetch\""),
            (ToolKind::Other, "\"other\""),
        ];
        for (kind, expected) in cases {
            assert_eq!(serde_json::to_string(&kind).unwrap(), expected);
        }
    }

    #[test]
    fn permission_outcome_selected_serializes_with_option_id() {
        let outcome = PermissionOutcome::Selected { option_id: "allow-once".to_owned() };
        let s = serde_json::to_string(&outcome).unwrap();
        assert!(s.contains(r#""outcome":"selected""#));
        assert!(s.contains(r#""optionId":"allow-once""#));
    }

    #[test]
    fn permission_outcome_cancelled_serializes_without_an_option_id() {
        let outcome = PermissionOutcome::Cancelled;
        let s = serde_json::to_string(&outcome).unwrap();
        assert_eq!(s, r#"{"outcome":"cancelled"}"#);
    }

    // -----------------------------------------------------------------------
    // terminal/* — protocol-accurate shapes
    // -----------------------------------------------------------------------

    #[test]
    fn terminal_create_params_parses_command_args_and_env_array() {
        let raw = r#"{
            "sessionId": "s1",
            "command": "pytest",
            "args": ["-x"],
            "env": [{"name": "CI", "value": "1"}],
            "cwd": "/repo",
            "outputByteLimit": 4096
        }"#;
        let params: TerminalCreateParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.command, "pytest");
        assert_eq!(params.args, vec!["-x"]);
        assert_eq!(params.env.len(), 1);
        assert_eq!(params.env[0].name, "CI");
        assert_eq!(params.env[0].value, "1");
        assert_eq!(params.cwd.as_deref(), Some("/repo"));
        assert_eq!(params.output_byte_limit, Some(4096));
    }

    #[test]
    fn terminal_id_params_round_trip() {
        let params = TerminalIdParams {
            session_id: "s1".to_owned(),
            terminal_id: "term-1".to_owned(),
        };
        let s = serde_json::to_string(&params).unwrap();
        assert!(s.contains("\"terminalId\":\"term-1\""));
        let decoded: TerminalIdParams = serde_json::from_str(&s).unwrap();
        assert_eq!(decoded.terminal_id, "term-1");
    }

    #[test]
    fn fs_write_params_parses_content() {
        let raw = r#"{"sessionId": "s1", "path": "/tmp/x.txt", "content": "hello"}"#;
        let params: FsWriteParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.path, "/tmp/x.txt");
        assert_eq!(params.content, "hello");
    }
}
