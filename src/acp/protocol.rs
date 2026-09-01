//! ACP-specific message parameter/result structs.
//!
//! These are the typed params used on top of the generic JSON-RPC wire types
//! in [`crate::rpc::message`]. The RPC wire types (`RpcRequest`, `RpcResponse`,
//! `classify_line`) are reused as-is; only the ACP payload shapes live here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::types::{
    AgentEvent, AvailableCommandInfo, ConfigOptionChoiceInfo, ConfigOptionInfo, PlanStep,
    PlanStepPriority, PlanStepStatus,
};
use crate::core::types::ConfigOptionKind as CoreConfigOptionKind;

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

/// `session/load` response result (agent → host) -- also reused to parse
/// `session/new`'s result, since both carry the same "here is the session
/// I made ready for you" shape per the ACP spec: `sessionId`, and
/// optionally `modes` (current + available session modes),
/// `availableCommands` (the agent's slash-command catalog), and
/// `configOptions` (current session configuration -- model, reasoning
/// effort, ...). Every field beyond `sessionId` defaults to empty so an
/// agent that predates any of them still parses.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionLoadResult {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(default)]
    pub modes: SessionModeState,
    #[serde(rename = "availableCommands", default)]
    pub available_commands: Vec<AvailableCommand>,
    #[serde(rename = "configOptions", default)]
    pub config_options: Vec<SessionConfigOption>,
}

/// `session/set_mode` request params (host → agent) -- switch the agent's
/// current session mode. Permitted at any time per the ACP spec, including
/// mid-generation.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSetModeParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "modeId")]
    pub mode_id: String,
}

/// `session/set_config_option` request params (host → agent) -- the
/// generic mechanism that supersedes session modes for per-session
/// settings such as model selection and reasoning effort. Field names
/// (`optionId`, `value`) follow this file's camelCase convention and this
/// crate's own [`SessionConfigOption`] naming; unverified against a live
/// agent capture.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSetConfigOptionParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "optionId")]
    pub option_id: String,
    pub value: Value,
}

// ---------------------------------------------------------------------------
// SessionState — live state assembled at handshake, kept current by
// session/update notifications
// ---------------------------------------------------------------------------

/// Context-window consumption and cost, as last reported by a
/// `usage_update`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsage {
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
}

/// Live ACP session state: modes, command catalog, config options, and
/// context usage. Assembled from the `session/new`/`session/load`
/// handshake result via [`SessionState::from_handshake`] and kept current
/// by `current_mode_update`, `available_commands_update`,
/// `config_option_update`, `session_info_update`, and `usage_update`
/// notifications via [`apply_session_update`]. Exposed to callers through
/// [`super::session::AcpSession`]'s accessor methods.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    pub modes: SessionModeState,
    pub available_commands: Vec<AvailableCommand>,
    pub config_options: Vec<SessionConfigOption>,
    pub title: Option<String>,
    pub usage: Option<SessionUsage>,
}

impl SessionState {
    /// Seed state from a `session/new`/`session/load` handshake result.
    pub(crate) fn from_handshake(result: &SessionLoadResult) -> Self {
        Self {
            modes: result.modes.clone(),
            available_commands: result.available_commands.clone(),
            config_options: result.config_options.clone(),
            title: None,
            usage: None,
        }
    }
}

/// Fold a `session/update` notification into [`SessionState`], for the
/// subset of update kinds that represent durable session state rather
/// than a one-shot stream event. `plan`, message chunks, tool calls, and
/// `stop` do not touch state and are ignored here.
pub(crate) fn apply_session_update(state: &mut SessionState, update: &SessionUpdate) {
    match update {
        SessionUpdate::CurrentModeUpdate { current_mode_id } => {
            state.modes.current_mode_id = Some(current_mode_id.clone());
        }
        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            state.available_commands = available_commands.clone();
        }
        SessionUpdate::ConfigOptionUpdate { config_options } => {
            state.config_options = config_options.clone();
        }
        SessionUpdate::SessionInfoUpdate { title, .. } => {
            if let Some(title) = title {
                state.title = Some(title.clone());
            }
        }
        SessionUpdate::UsageUpdate { used_tokens, context_window, cost, .. } => {
            state.usage = Some(SessionUsage {
                used_tokens: *used_tokens,
                context_window: *context_window,
                cost_amount: cost.as_ref().and_then(|c| c.amount),
                cost_currency: cost.as_ref().and_then(|c| c.currency.clone()),
            });
        }
        _ => {}
    }
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
    /// Echo of a user message chunk -- sent when replaying a loaded
    /// session's history, mirroring `agent_message_chunk`'s content shapes.
    #[serde(rename = "user_message_chunk")]
    UserMessageChunk {
        #[serde(default)]
        content: Value,
    },
    /// The agent's execution plan. Always a full snapshot replacing any
    /// plan sent before it, never a delta.
    #[serde(rename = "plan")]
    Plan {
        #[serde(default)]
        entries: Vec<PlanEntry>,
    },
    /// The agent's slash-command catalog changed.
    #[serde(rename = "available_commands_update")]
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands", default)]
        available_commands: Vec<AvailableCommand>,
    },
    /// The session's active mode changed.
    #[serde(rename = "current_mode_update")]
    CurrentModeUpdate {
        #[serde(rename = "currentModeId", default)]
        current_mode_id: String,
    },
    /// Session metadata changed (e.g. title). Only the fields that
    /// actually changed are present on the wire; anything not named below
    /// lands in `extra` rather than being dropped.
    #[serde(rename = "session_info_update")]
    SessionInfoUpdate {
        #[serde(default)]
        title: Option<String>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    /// Context-window consumption and, optionally, turn cost. Field names
    /// (`usedTokens`, `contextWindow`, nested `cost`) are a best-effort
    /// guess following this file's camelCase convention -- not verified
    /// against a live agent capture. `extra` keeps anything that lands
    /// under a different real key name from being silently dropped.
    #[serde(rename = "usage_update")]
    UsageUpdate {
        #[serde(rename = "usedTokens", default)]
        used_tokens: Option<u64>,
        #[serde(rename = "contextWindow", default)]
        context_window: Option<u64>,
        #[serde(default)]
        cost: Option<UsageCost>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    /// The full current set of session configuration options -- the
    /// mechanism that supersedes session modes. Sent whole on every
    /// update, like `plan`.
    #[serde(rename = "config_option_update")]
    ConfigOptionUpdate {
        #[serde(rename = "configOptions", default)]
        config_options: Vec<SessionConfigOption>,
    },
    #[serde(other)]
    Unknown,
}

/// Priority of a single [`PlanEntry`] within a `plan` update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

/// Status of a single [`PlanEntry`] within a `plan` update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// One step of the agent's execution plan, as carried in
/// [`SessionUpdate::Plan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

/// An input hint attached to an [`AvailableCommand`], e.g.
/// `{"hint": "<file>"}`. The exact shape of this sub-object has not been
/// captured from a live agent; `hint` is read defensively and `extra`
/// keeps anything else intact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AvailableCommandInput {
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// A single slash-style command the agent advertises, either at handshake
/// time (`session/new`/`session/load`'s result) or via
/// `available_commands_update`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AvailableCommand {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input: Option<AvailableCommandInput>,
}

/// A mode the agent can operate in -- one entry of
/// [`SessionModeState::available_modes`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The mode block returned by `session/new`/`session/load` and kept
/// current by `current_mode_update`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionModeState {
    #[serde(rename = "currentModeId", default)]
    pub current_mode_id: Option<String>,
    #[serde(rename = "availableModes", default)]
    pub available_modes: Vec<SessionMode>,
}

/// The kind of a [`SessionConfigOption`] -- `select` (choose one of
/// `options`) or `boolean` (toggle `value`). Falls back to `Unknown` for a
/// kind string a future ACP revision adds that this build does not know
/// yet, matching the [`ToolKind`] tolerance pattern used elsewhere in this
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionKind {
    Select,
    Boolean,
    #[serde(other)]
    #[default]
    Unknown,
}

/// One selectable value of a `select`-kind [`SessionConfigOption`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigOptionChoice {
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// One session configuration setting -- the mechanism that supersedes
/// session modes for things like model selection and reasoning effort.
/// `config_option_update` carries the FULL current set on every update,
/// same as `plan`. Field names beyond `id`/`kind` are best-effort guesses
/// following this file's camelCase convention -- not verified against a
/// live agent capture; `extra` keeps anything under a different real key
/// name from being silently dropped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfigOption {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub kind: ConfigOptionKind,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub options: Vec<ConfigOptionChoice>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Amount/currency pair attached to a `usage_update`, when the agent
/// reports cost. Field names (`amount`/`currency`) are a best-effort
/// guess; unverified against a live agent capture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageCost {
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
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
    SwitchMode,
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

        SessionUpdate::UserMessageChunk { content } => {
            let text = extract_text_from_content(content);
            if text.is_empty() {
                vec![]
            } else {
                vec![AgentEvent::UserMessage { text, is_delta: true }]
            }
        }

        SessionUpdate::Plan { entries } => {
            let steps = entries
                .iter()
                .map(|entry| PlanStep {
                    content: entry.content.clone(),
                    priority: match entry.priority {
                        PlanEntryPriority::High => PlanStepPriority::High,
                        PlanEntryPriority::Medium => PlanStepPriority::Medium,
                        PlanEntryPriority::Low => PlanStepPriority::Low,
                    },
                    status: match entry.status {
                        PlanEntryStatus::Pending => PlanStepStatus::Pending,
                        PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
                        PlanEntryStatus::Completed => PlanStepStatus::Completed,
                    },
                })
                .collect();
            vec![AgentEvent::Plan { steps }]
        }

        SessionUpdate::AvailableCommandsUpdate { available_commands } => {
            let commands = available_commands
                .iter()
                .map(|command| AvailableCommandInfo {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    input_hint: command.input.as_ref().and_then(|input| input.hint.clone()),
                })
                .collect();
            vec![AgentEvent::AvailableCommandsUpdate { commands }]
        }

        SessionUpdate::CurrentModeUpdate { current_mode_id } => {
            vec![AgentEvent::ModeChanged { mode_id: current_mode_id.clone() }]
        }

        SessionUpdate::SessionInfoUpdate { title, .. } => {
            vec![AgentEvent::SessionInfoUpdate { title: title.clone() }]
        }

        SessionUpdate::UsageUpdate { used_tokens, context_window, cost, .. } => {
            vec![AgentEvent::UsageUpdate {
                used_tokens: *used_tokens,
                context_window: *context_window,
                cost_amount: cost.as_ref().and_then(|c| c.amount),
                cost_currency: cost.as_ref().and_then(|c| c.currency.clone()),
            }]
        }

        SessionUpdate::ConfigOptionUpdate { config_options } => {
            let options = config_options
                .iter()
                .map(|option| ConfigOptionInfo {
                    id: option.id.clone(),
                    name: option.name.clone(),
                    description: option.description.clone(),
                    category: option.category.clone(),
                    kind: match option.kind {
                        ConfigOptionKind::Select => CoreConfigOptionKind::Select,
                        ConfigOptionKind::Boolean => CoreConfigOptionKind::Boolean,
                        ConfigOptionKind::Unknown => CoreConfigOptionKind::Unknown,
                    },
                    value: option.value.clone(),
                    choices: option
                        .options
                        .iter()
                        .map(|choice| ConfigOptionChoiceInfo {
                            value: choice.value.clone(),
                            label: choice.label.clone(),
                        })
                        .collect(),
                })
                .collect();
            vec![AgentEvent::ConfigOptionsUpdate { options }]
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
    fn tool_kind_serializes_the_ten_canonical_wire_values() {
        let cases = [
            (ToolKind::Read, "\"read\""),
            (ToolKind::Edit, "\"edit\""),
            (ToolKind::Delete, "\"delete\""),
            (ToolKind::Move, "\"move\""),
            (ToolKind::Search, "\"search\""),
            (ToolKind::Execute, "\"execute\""),
            (ToolKind::Think, "\"think\""),
            (ToolKind::Fetch, "\"fetch\""),
            (ToolKind::SwitchMode, "\"switch_mode\""),
            (ToolKind::Other, "\"other\""),
        ];
        for (kind, expected) in cases {
            assert_eq!(serde_json::to_string(&kind).unwrap(), expected);
        }
    }

    #[test]
    fn tool_kind_switch_mode_round_trips() {
        let raw = r#"{"toolCallId": "tc1", "kind": "switch_mode"}"#;
        let tool_call: PermissionToolCall = serde_json::from_str(raw).unwrap();
        assert_eq!(tool_call.kind, ToolKind::SwitchMode);
        assert!(!tool_call.kind.is_read_only());
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

    // -----------------------------------------------------------------------
    // session/update — the six kinds this file previously left unparsed
    // -----------------------------------------------------------------------

    #[test]
    fn session_update_user_message_chunk_parses_and_emits_event() {
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hi from the user"}}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        assert!(matches!(params.update, SessionUpdate::UserMessageChunk { .. }));
        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::UserMessage { text, is_delta: true } if text == "hi from the user"
        ));
    }

    #[test]
    fn session_update_plan_parses_full_snapshot() {
        let raw = r#"{"sessionId":"s1","update":{
            "sessionUpdate":"plan",
            "entries":[
                {"content":"read the file","priority":"high","status":"completed"},
                {"content":"write the fix","priority":"medium","status":"in_progress"},
                {"content":"run the tests","priority":"low","status":"pending"}
            ]
        }}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::Plan { entries } = &params.update else {
            panic!("expected Plan variant");
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].priority, PlanEntryPriority::High);
        assert_eq!(entries[0].status, PlanEntryStatus::Completed);
        assert_eq!(entries[1].status, PlanEntryStatus::InProgress);

        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        let AgentEvent::Plan { steps } = &events[0] else {
            panic!("expected AgentEvent::Plan");
        };
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].content, "read the file");
        assert_eq!(steps[0].priority, PlanStepPriority::High);
        assert_eq!(steps[2].status, PlanStepStatus::Pending);
    }

    #[test]
    fn session_update_available_commands_update_parses() {
        let raw = r#"{"sessionId":"s1","update":{
            "sessionUpdate":"available_commands_update",
            "availableCommands":[
                {"name":"review","description":"Review the diff","input":{"hint":"<file>"}},
                {"name":"explain","description":"Explain the code"}
            ]
        }}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::AvailableCommandsUpdate { available_commands } = &params.update else {
            panic!("expected AvailableCommandsUpdate variant");
        };
        assert_eq!(available_commands.len(), 2);
        assert_eq!(available_commands[0].name, "review");
        assert_eq!(
            available_commands[0].input.as_ref().and_then(|i| i.hint.clone()),
            Some("<file>".to_owned())
        );
        assert!(available_commands[1].input.is_none());

        let events = update_to_event(&params);
        let AgentEvent::AvailableCommandsUpdate { commands } = &events[0] else {
            panic!("expected AgentEvent::AvailableCommandsUpdate");
        };
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].input_hint.as_deref(), Some("<file>"));
        assert_eq!(commands[1].input_hint, None);
    }

    #[test]
    fn session_update_current_mode_update_parses() {
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"current_mode_update","currentModeId":"architect"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            &params.update,
            SessionUpdate::CurrentModeUpdate { current_mode_id } if current_mode_id == "architect"
        ));
        let events = update_to_event(&params);
        assert!(matches!(
            &events[0],
            AgentEvent::ModeChanged { mode_id } if mode_id == "architect"
        ));
    }

    #[test]
    fn session_update_session_info_update_parses_only_changed_fields() {
        // Only "title" changed on this update -- no other field present.
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"session_info_update","title":"New title"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            &params.update,
            SessionUpdate::SessionInfoUpdate { title: Some(t), .. } if t == "New title"
        ));
        let events = update_to_event(&params);
        assert!(matches!(
            &events[0],
            AgentEvent::SessionInfoUpdate { title: Some(t) } if t == "New title"
        ));
    }

    #[test]
    fn session_update_session_info_update_keeps_unnamed_fields_via_extra() {
        // A field this build doesn't have a named slot for must not be
        // dropped silently -- it must show up in `extra`.
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"session_info_update","someFutureField":"value"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::SessionInfoUpdate { title, extra } = &params.update else {
            panic!("expected SessionInfoUpdate variant");
        };
        assert!(title.is_none());
        assert_eq!(extra.get("someFutureField").and_then(Value::as_str), Some("value"));
    }

    #[test]
    fn session_update_usage_update_parses_with_cost() {
        let raw = r#"{"sessionId":"s1","update":{
            "sessionUpdate":"usage_update",
            "usedTokens":12345,
            "contextWindow":200000,
            "cost":{"amount":0.42,"currency":"USD"}
        }}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::UsageUpdate { used_tokens, context_window, cost, .. } = &params.update
        else {
            panic!("expected UsageUpdate variant");
        };
        assert_eq!(*used_tokens, Some(12345));
        assert_eq!(*context_window, Some(200_000));
        assert_eq!(cost.as_ref().and_then(|c| c.amount), Some(0.42));
        assert_eq!(cost.as_ref().and_then(|c| c.currency.clone()), Some("USD".to_owned()));

        let events = update_to_event(&params);
        assert!(matches!(
            &events[0],
            AgentEvent::UsageUpdate {
                used_tokens: Some(12345),
                context_window: Some(200_000),
                cost_amount: Some(amount),
                cost_currency: Some(currency),
            } if (*amount - 0.42).abs() < f64::EPSILON && currency == "USD"
        ));
    }

    #[test]
    fn session_update_usage_update_parses_without_cost() {
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"usage_update","usedTokens":100}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert!(matches!(
            &events[0],
            AgentEvent::UsageUpdate {
                used_tokens: Some(100),
                context_window: None,
                cost_amount: None,
                cost_currency: None,
            }
        ));
    }

    #[test]
    fn session_update_config_option_update_parses_select_and_boolean() {
        let raw = r#"{"sessionId":"s1","update":{
            "sessionUpdate":"config_option_update",
            "configOptions":[
                {
                    "id":"model",
                    "name":"Model",
                    "description":"Which model to use",
                    "category":"generation",
                    "kind":"select",
                    "value":"opus",
                    "options":[
                        {"value":"opus","label":"Opus"},
                        {"value":"sonnet","label":"Sonnet"}
                    ]
                },
                {
                    "id":"extended-thinking",
                    "name":"Extended thinking",
                    "kind":"boolean",
                    "value":true
                }
            ]
        }}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::ConfigOptionUpdate { config_options } = &params.update else {
            panic!("expected ConfigOptionUpdate variant");
        };
        assert_eq!(config_options.len(), 2);
        assert_eq!(config_options[0].kind, ConfigOptionKind::Select);
        assert_eq!(config_options[0].options.len(), 2);
        assert_eq!(config_options[1].kind, ConfigOptionKind::Boolean);
        assert_eq!(config_options[1].value, Value::Bool(true));

        let events = update_to_event(&params);
        let AgentEvent::ConfigOptionsUpdate { options } = &events[0] else {
            panic!("expected AgentEvent::ConfigOptionsUpdate");
        };
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].choices.len(), 2);
        assert_eq!(options[0].choices[0].label.as_deref(), Some("Opus"));
    }

    #[test]
    fn session_update_config_option_kind_unknown_string_falls_back() {
        let raw = r#"{"id":"x","name":"X","kind":"not-a-real-kind","value":null}"#;
        let option: SessionConfigOption = serde_json::from_str(raw).unwrap();
        assert_eq!(option.kind, ConfigOptionKind::Unknown);
    }

    #[test]
    fn session_update_unrecognized_future_kind_falls_back_to_unknown_and_events_are_empty() {
        // A `sessionUpdate` tag this build has never heard of must not
        // break parsing -- it must land in the `Unknown` variant, and
        // `update_to_event` must return no events for it (the reader loop
        // then passes it through as a raw `RpcNotification` rather than
        // dropping it -- see `reader::acp_reader_loop`).
        let raw = r#"{"sessionId":"s1","update":{"sessionUpdate":"totally_new_kind_from_the_future","someField":42}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        assert!(matches!(params.update, SessionUpdate::Unknown));
        assert!(update_to_event(&params).is_empty());
    }

    // -----------------------------------------------------------------------
    // session/new & session/load handshake result — modes, commands, config
    // -----------------------------------------------------------------------

    #[test]
    fn session_load_result_parses_modes_commands_and_config_options() {
        let raw = r#"{
            "sessionId":"s1",
            "modes":{
                "currentModeId":"code",
                "availableModes":[
                    {"id":"code","name":"Code"},
                    {"id":"architect","name":"Architect","description":"Plan before editing"}
                ]
            },
            "availableCommands":[
                {"name":"review","description":"Review the diff"}
            ],
            "configOptions":[
                {"id":"model","name":"Model","kind":"select","value":"opus"}
            ]
        }"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.session_id, "s1");
        assert_eq!(result.modes.current_mode_id.as_deref(), Some("code"));
        assert_eq!(result.modes.available_modes.len(), 2);
        assert_eq!(result.modes.available_modes[1].id, "architect");
        assert_eq!(result.available_commands.len(), 1);
        assert_eq!(result.config_options.len(), 1);
        assert_eq!(result.config_options[0].id, "model");
    }

    #[test]
    fn session_load_result_defaults_when_agent_predates_modes_and_commands() {
        let raw = r#"{"sessionId":"s1"}"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert!(result.modes.current_mode_id.is_none());
        assert!(result.modes.available_modes.is_empty());
        assert!(result.available_commands.is_empty());
        assert!(result.config_options.is_empty());
    }

    #[test]
    fn session_state_from_handshake_seeds_modes_commands_and_config_options() {
        let raw = r#"{
            "sessionId":"s1",
            "modes":{"currentModeId":"code","availableModes":[{"id":"code","name":"Code"}]},
            "availableCommands":[{"name":"review","description":"Review the diff"}],
            "configOptions":[{"id":"model","name":"Model","kind":"select","value":"opus"}]
        }"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        let state = SessionState::from_handshake(&result);
        assert_eq!(state.modes.current_mode_id.as_deref(), Some("code"));
        assert_eq!(state.available_commands.len(), 1);
        assert_eq!(state.config_options.len(), 1);
        assert!(state.title.is_none());
        assert!(state.usage.is_none());
    }

    // -----------------------------------------------------------------------
    // apply_session_update — SessionState kept current by *_update
    // -----------------------------------------------------------------------

    #[test]
    fn apply_session_update_sets_current_mode_id() {
        let mut state = SessionState::default();
        apply_session_update(
            &mut state,
            &SessionUpdate::CurrentModeUpdate { current_mode_id: "architect".to_owned() },
        );
        assert_eq!(state.modes.current_mode_id.as_deref(), Some("architect"));
    }

    #[test]
    fn apply_session_update_replaces_available_commands() {
        let mut state = SessionState::default();
        state.available_commands.push(AvailableCommand {
            name: "stale".to_owned(),
            description: String::new(),
            input: None,
        });
        let fresh = vec![AvailableCommand {
            name: "review".to_owned(),
            description: "Review the diff".to_owned(),
            input: None,
        }];
        apply_session_update(
            &mut state,
            &SessionUpdate::AvailableCommandsUpdate { available_commands: fresh.clone() },
        );
        assert_eq!(state.available_commands.len(), 1);
        assert_eq!(state.available_commands[0].name, "review");
    }

    #[test]
    fn apply_session_update_replaces_config_options_wholesale() {
        let mut state = SessionState::default();
        let options = vec![SessionConfigOption {
            id: "model".to_owned(),
            name: "Model".to_owned(),
            kind: ConfigOptionKind::Select,
            value: Value::String("opus".to_owned()),
            ..Default::default()
        }];
        apply_session_update(
            &mut state,
            &SessionUpdate::ConfigOptionUpdate { config_options: options },
        );
        assert_eq!(state.config_options.len(), 1);
        assert_eq!(state.config_options[0].id, "model");
    }

    #[test]
    fn apply_session_update_sets_title_only_when_present() {
        let mut state = SessionState::default();
        apply_session_update(
            &mut state,
            &SessionUpdate::SessionInfoUpdate { title: None, extra: HashMap::new() },
        );
        assert!(state.title.is_none());

        apply_session_update(
            &mut state,
            &SessionUpdate::SessionInfoUpdate {
                title: Some("New title".to_owned()),
                extra: HashMap::new(),
            },
        );
        assert_eq!(state.title.as_deref(), Some("New title"));
    }

    #[test]
    fn apply_session_update_sets_usage() {
        let mut state = SessionState::default();
        apply_session_update(
            &mut state,
            &SessionUpdate::UsageUpdate {
                used_tokens: Some(500),
                context_window: Some(200_000),
                cost: Some(UsageCost {
                    amount: Some(1.5),
                    currency: Some("USD".to_owned()),
                    extra: HashMap::new(),
                }),
                extra: HashMap::new(),
            },
        );
        let usage = state.usage.expect("usage must be set");
        assert_eq!(usage.used_tokens, Some(500));
        assert_eq!(usage.context_window, Some(200_000));
        assert_eq!(usage.cost_amount, Some(1.5));
        assert_eq!(usage.cost_currency.as_deref(), Some("USD"));
    }

    #[test]
    fn apply_session_update_ignores_stream_only_updates() {
        let mut state = SessionState::default();
        let before = state.clone();
        apply_session_update(&mut state, &SessionUpdate::Plan { entries: vec![] });
        apply_session_update(
            &mut state,
            &SessionUpdate::Stop {
                stop_reason: "end_turn".to_owned(),
                input_tokens: 0,
                output_tokens: 0,
                usage: None,
            },
        );
        assert_eq!(before.modes.current_mode_id, state.modes.current_mode_id);
        assert!(state.title.is_none());
        assert!(state.usage.is_none());
    }

    // -----------------------------------------------------------------------
    // session/set_mode & session/set_config_option — new outbound methods
    // -----------------------------------------------------------------------

    #[test]
    fn session_set_mode_params_serialize() {
        let params = SessionSetModeParams {
            session_id: "s1".to_owned(),
            mode_id: "architect".to_owned(),
        };
        let s = serde_json::to_string(&params).unwrap();
        assert!(s.contains("\"sessionId\":\"s1\""));
        assert!(s.contains("\"modeId\":\"architect\""));
    }

    #[test]
    fn session_set_config_option_params_serialize() {
        let params = SessionSetConfigOptionParams {
            session_id: "s1".to_owned(),
            option_id: "model".to_owned(),
            value: Value::String("sonnet".to_owned()),
        };
        let s = serde_json::to_string(&params).unwrap();
        assert!(s.contains("\"sessionId\":\"s1\""));
        assert!(s.contains("\"optionId\":\"model\""));
        assert!(s.contains("\"value\":\"sonnet\""));
    }
}
