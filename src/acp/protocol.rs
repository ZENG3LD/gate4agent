//! ACP-specific message parameter/result structs.
//!
//! These are the typed params used on top of the generic JSON-RPC wire types
//! in [`crate::rpc::message`]. The RPC wire types (`RpcRequest`, `RpcResponse`,
//! `classify_line`) are reused as-is; only the ACP payload shapes live here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::types::{
    AgentEvent, AnnouncementInfo, AvailableCommandInfo, AvailableModelInfo, ConfigOptionChoiceInfo,
    ConfigOptionInfo, HookRunResult, McpServerSummary, PlanStep, PlanStepPriority, PlanStepStatus,
    ReasoningEffortInfo, StopReason,
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

/// One `{name, value}` pair on the ACP wire -- the shape ACP v1 mandates
/// for both a stdio server's `env` and an SSE server's `headers` (a plain
/// JSON object is not the wire shape; `{"name":...,"value":...}` array
/// entries are).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerEnvVar {
    pub name: String,
    pub value: String,
}

/// A single MCP server entry for `session/new`.
///
/// Matches ACP v1's `McpServer` union exactly, confirmed by reading the
/// spec after a live capture showed a downstream ACP client never receiving
/// `tools/list`: **no `"transport"` discriminator, and stdio carries no
/// tag at all** -- `{name, command, args, env}`. An SSE (or, if ever
/// needed, HTTP) entry is what carries an explicit `"type"` field instead
/// (`"sse"` / `"http"`). `#[serde(untagged)]` reproduces this: it tries
/// each variant against the incoming JSON in declaration order, and the
/// two variants never overlap on required fields (`command` vs. `url`), so
/// there is no ambiguity either way.
///
/// `env`/`headers` are wire-mandated arrays of [`McpServerEnvVar`] pairs,
/// not a JSON object -- a stdio entry serialized with a map there is what
/// that downstream client silently dropped before this fix (nameless,
/// wrong-shaped `env`, no `session/new` field the adapter's registration
/// code recognized). Order is insertion order: whatever order the caller
/// passes to [`McpServerConfig::stdio`] / [`McpServerConfig::sse`], not
/// sorted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// stdio-based MCP server launched as a subprocess.
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<McpServerEnvVar>,
    },
    /// SSE-based MCP server at a URL.
    Sse {
        #[serde(rename = "type")]
        kind: String,
        name: String,
        url: String,
        #[serde(default)]
        headers: Vec<McpServerEnvVar>,
    },
}

impl McpServerConfig {
    /// Build a stdio entry: `{name, command, args, env: [{name,value}]}`.
    /// `env_pairs`' order is preserved as given, not sorted.
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = String>,
        env_pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self::Stdio {
            name: name.into(),
            command: command.into(),
            args: args.into_iter().collect(),
            env: env_pairs
                .into_iter()
                .map(|(name, value)| McpServerEnvVar { name, value })
                .collect(),
        }
    }

    /// Build an SSE entry: `{type:"sse", name, url, headers:
    /// [{name,value}]}`. `header_pairs`' order is preserved as given, not
    /// sorted.
    pub fn sse(
        name: impl Into<String>,
        url: impl Into<String>,
        header_pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self::Sse {
            kind: "sse".to_string(),
            name: name.into(),
            url: url.into(),
            headers: header_pairs
                .into_iter()
                .map(|(name, value)| McpServerEnvVar { name, value })
                .collect(),
        }
    }

    /// The server's registration name -- the only field safe to log:
    /// never `env`/`headers`, which may carry tokens.
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Sse { name, .. } => name,
        }
    }
}

/// `session/new` request params. `additional_directories` is the outbound
/// side of the `sessionCapabilities.additionalDirectories` flag every
/// captured agent advertises (`acp-claude.jsonl`, `acp-codex.jsonl`,
/// `acp-kimi.jsonl`) -- UNVERIFIED wire shape: no live capture ever sends
/// a non-empty request, only the capability flag, so the field name
/// follows this file's camelCase convention and the ACP spec's own
/// `mcpServers` precedent (a sibling array of extra paths the agent may
/// access beyond `cwd`), not a captured request payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionNewParams {
    pub cwd: String,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(rename = "additionalDirectories", default)]
    pub additional_directories: Vec<String>,
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

/// Turn-level token usage as reported on a `session/prompt` response,
/// directly under the top-level `usage` key. Verified live on
/// claude-agent-acp 0.72.0 (`turn-claude.jsonl`): camelCase field names,
/// NOT the snake_case `input_tokens`/`output_tokens` this file's
/// [`extract_token_usage`] previously guessed for an unconfirmed
/// "Claude-nested" shape. `cachedReadTokens`/`cachedWriteTokens` are
/// Anthropic prompt-cache bookkeeping (tokens read from vs. written to the
/// cache this turn); a provider with no such concept simply omits them.
/// codex-acp 1.8.0, Kimi Code CLI 0.39.1, and Grok CLI (build channel)
/// send NO `usage` key at all on this response -- every field here is
/// therefore optional, and [`SessionPromptResult::usage`] itself is
/// `None` for those three.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptUsage {
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: Option<u64>,
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: Option<u64>,
    #[serde(rename = "cachedReadTokens", default)]
    pub cached_read_tokens: Option<u64>,
    #[serde(rename = "cachedWriteTokens", default)]
    pub cached_write_tokens: Option<u64>,
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// The `_meta` sidecar on a `session/prompt` response, when the
/// provider's token breakdown rides there instead of a top-level `usage`
/// key. Verified live on Grok CLI, build channel (`turn-grok.jsonl`): the
/// breakdown sits directly on `_meta` -- `sessionId`, `requestId`,
/// `promptId`, `modelId`, `totalTokens`, `inputTokens`, `outputTokens`,
/// `cachedReadTokens`, `reasoningTokens` -- alongside a nested `_meta.
/// usage` carrying the same numbers again plus a per-model breakdown; that
/// nested object is not modeled separately here (nothing in this build
/// needs the per-model split) and lands in `extra` instead of being
/// dropped. claude-agent-acp additionally sends a `_meta.quota` sidecar
/// with none of these field names -- it also lands in `extra`. codex-acp
/// and Kimi Code CLI send no token-bearing `_meta` on this response at
/// all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptResultMeta {
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    #[serde(rename = "requestId", default)]
    pub request_id: Option<String>,
    #[serde(rename = "promptId", default)]
    pub prompt_id: Option<String>,
    #[serde(rename = "modelId", default)]
    pub model_id: Option<String>,
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: Option<u64>,
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: Option<u64>,
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: Option<u64>,
    #[serde(rename = "cachedReadTokens", default)]
    pub cached_read_tokens: Option<u64>,
    #[serde(rename = "reasoningTokens", default)]
    pub reasoning_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// `session/prompt` response result (agent → host). `stopReason` is the
/// only field all four captured providers agree on
/// (`turn-{claude,codex,grok,kimi}.jsonl`). Token usage rides on two
/// mutually-exclusive-in-practice locations depending on the provider:
/// Claude nests it under `usage` ([`PromptUsage`]); Grok flattens it onto
/// `_meta` ([`PromptResultMeta`]); Codex and Kimi send neither -- both
/// fields are therefore optional, and `super::session::emit_prompt_result`
/// falls back to [`extract_token_usage`] for any shape neither one
/// matches. Losing this breakdown on prompt-response parsing would leave
/// Grok and Claude turns with zeroed-out token counts even though the
/// numbers are right there on the wire -- this struct exists so neither
/// caller has to re-derive the shape by hand.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptResult {
    #[serde(rename = "stopReason", default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub usage: Option<PromptUsage>,
    #[serde(rename = "_meta", default)]
    pub meta: Option<PromptResultMeta>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
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
/// `availableCommands` (the agent's slash-command catalog), `configOptions`
/// (current session configuration -- model, reasoning effort, ...), and
/// `models` (the agent's model catalog -- verified live on codex-acp 1.8.0
/// and Grok CLI 1.0.13, `acp-codex.jsonl`/`acp-grok.jsonl`; previously not
/// parsed at all). Every field beyond `sessionId` defaults to empty so an
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
    #[serde(default)]
    pub models: SessionModelState,
}

/// One reasoning-effort level offered for a [`SessionModel`]. Verified
/// live on Grok CLI 1.0.13, identical on both `session/new`'s `models`
/// field and the vendor `_x.ai/models/update` notification
/// (`acp-grok.jsonl`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningEffortOption {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

/// A model's `_meta` sidecar, as carried per-entry in Grok's `models`
/// field and `_x.ai/models/update` notification (`acp-grok.jsonl`).
/// Codex's model catalog entries carry no `_meta` at all; every field
/// here defaults to absent so that shape still parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionModelMeta {
    #[serde(rename = "totalContextTokens", default)]
    pub total_context_tokens: Option<u64>,
    #[serde(rename = "agentType", default)]
    pub agent_type: Option<String>,
    #[serde(rename = "supportsReasoningEffort", default)]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(rename = "reasoningEffort", default)]
    pub reasoning_effort: Option<String>,
    #[serde(rename = "reasoningEfforts", default)]
    pub reasoning_efforts: Vec<ReasoningEffortOption>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// One model the agent can select for a session. Field names verified
/// live on codex-acp 1.8.0 (`session/new`'s `models.availableModels`) and
/// Grok CLI 1.0.13 (`session/new`'s `models.availableModels` and the
/// vendor `_x.ai/models/update` notification) -- `acp-codex.jsonl`,
/// `acp-grok.jsonl`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionModel {
    #[serde(rename = "modelId", default)]
    pub model_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "_meta", default)]
    pub meta: Option<SessionModelMeta>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// The model catalog block returned by `session/new`/`session/load` and
/// kept current by the vendor `_x.ai/models/update` notification (Grok).
/// Field names verified live on codex-acp 1.8.0 and Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionModelState {
    #[serde(rename = "currentModelId", default)]
    pub current_model_id: Option<String>,
    #[serde(rename = "availableModels", default)]
    pub available_models: Vec<SessionModel>,
}

/// `session/list` request params (host → agent). Verified live by direct
/// invocation on claude-agent-acp 0.71.0 and Grok CLI 1.0.13: both an
/// empty object and `{"cwd": "<path>"}` are accepted, and Grok answered
/// identically either way -- `cwd` is an optional filter, not a required
/// field. `#[serde(skip_serializing_if)]` keeps the default call shaped
/// exactly as verified (`{}`) when no filter is requested.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// One session summary entry in a `session/list` response. Verified live
/// by direct invocation on claude-agent-acp 0.71.0 and Grok CLI 1.0.13:
/// both agree on `sessionId`, `cwd`, `updatedAt`; Claude additionally
/// sends `title` (Grok does not), and Grok additionally sends `_meta`
/// (`x.ai/session` with `kind` and `facets` -- `branch`/`cwd`/`gitRoot`/
/// `repo`; Claude does not send `_meta` at all) -- both are therefore
/// optional. `updatedAt` is kept as a raw string rather than parsed: the
/// two providers use different timestamp formats on the same field
/// (Grok `"2026-09-01T19:00:20.154340+00:00"`, six-digit microseconds
/// and an explicit `+00:00` offset; Claude
/// `"2026-09-01T19:02:19.365Z"`, three-digit milliseconds and a `Z`
/// suffix) -- both are valid RFC 3339 but a single fixed-precision parser
/// would reject one of them, so this build keeps the field opaque
/// instead of picking a parser that only works for one provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSummary {
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Claude-only.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<String>,
    /// Grok-only (`x.ai/session`: `kind`, `facets` -- `branch`, `cwd`,
    /// `gitRoot`, `repo`). Kept as raw JSON: this build has no use for
    /// the facet breakdown today, only for not dropping it.
    #[serde(rename = "_meta", default)]
    pub meta: Option<Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// `session/list` response result (agent → host). Verified live: both
/// claude-agent-acp 0.71.0 and Grok CLI 1.0.13 wrap the summaries in a
/// `sessions` array under this exact key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionListResult {
    #[serde(default)]
    pub sessions: Vec<SessionSummary>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// `session/close` request params (host → agent). Verified live by
/// direct invocation on claude-agent-acp 0.71.0 and Grok CLI 1.0.13:
/// `{"sessionId": "<id>"}`, matching this file's `{sessionId}`-request
/// convention (`session/cancel`, `session/load`, ...).
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionCloseParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// `session/close` response result (agent → host). Verified live: Claude
/// returns an empty object; Grok returns `{"_meta": {"x.ai/
/// closeOutcome": "closed"}}`. Both parse here -- `meta` is `None` for
/// Claude's empty response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCloseResult {
    #[serde(rename = "_meta", default)]
    pub meta: Option<Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// `session/delete` request params (host → agent) -- UNVERIFIED wire
/// shape (see [`SessionCloseParams`]). Not exercised by any live
/// invocation -- only `session/list`, `session/close`, and `session/fork`
/// were called against a real agent.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionDeleteParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// `session/fork` request params (host → agent). Verified live by direct
/// invocation on claude-agent-acp 0.71.0: calling with only `{"sessionId":
/// "<id>"}` (this file's earlier guess) was REJECTED with a `-32602
/// Invalid params` error naming the missing field explicitly:
/// `{"_errors": [], "cwd": {"_errors": ["Invalid input: expected string,
/// received undefined"]}}}`. `cwd` is therefore required, not optional --
/// forking creates a new session rooted at a working directory, not a
/// bare clone by id, mirroring `session/new`'s own required `cwd`. The
/// success response shape remains UNVERIFIED (the only live call made
/// errored on the missing field before returning one); this file still
/// parses it with [`SessionLoadResult`], the same "here is a session I
/// made ready for you" shape `session/new`/`session/load` use, since
/// nothing contradicts that guess -- just nothing confirms it either.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionForkParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
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
/// `usage_update`. Field names here are this crate's own Rust
/// identifiers, decoupled from the wire key names -- see
/// [`SessionUpdate::UsageUpdate`] for what is actually verified live on
/// the wire.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsage {
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
}

/// Live ACP session state: modes, command catalog, config options, model
/// catalog, and context usage. Assembled from the `session/new`/`session/
/// load` handshake result via [`SessionState::from_handshake`] and kept
/// current by `current_mode_update`, `available_commands_update`,
/// `config_option_update`, `session_info_update`, and `usage_update`
/// notifications via [`apply_session_update`], plus (for `models`) Grok's
/// vendor `_x.ai/models/update` notification via
/// `super::reader::acp_reader_loop`. Exposed to callers through
/// [`super::session::AcpSession`]'s accessor methods.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    pub modes: SessionModeState,
    pub available_commands: Vec<AvailableCommand>,
    pub config_options: Vec<SessionConfigOption>,
    pub models: SessionModelState,
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
            models: result.models.clone(),
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
        /// A single content block object `{"type": "text", "text": "..."}`
        /// on every real single-turn capture from all four providers,
        /// claude-agent-acp 0.72.0 included (`turn-{claude,codex,grok,
        /// kimi}.jsonl`) -- contradicting this field's earlier "Claude ACP
        /// sends an array" note, which no live capture ever confirmed. The
        /// array-of-content-blocks case is kept as an unverified fallback
        /// (`extract_text_from_content` handles both), not removed --
        /// nothing disproves an agent sending it under different
        /// circumstances, only that none of these four turns did.
        #[serde(default)]
        content: Value,
    },
    #[serde(rename = "agent_thought_chunk")]
    AgentThoughtChunk {
        /// A single content block object `{"type": "text", "text": "..."}`
        /// on every real thought chunk observed -- Grok CLI and Kimi Code
        /// CLI both (`turn-grok.jsonl`, `turn-kimi.jsonl`), the SAME shape
        /// as `agent_message_chunk`, NOT the `{"thought": "..."}` this
        /// field previously guessed and had never confirmed live.
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
        /// Claude's own sidecar naming WHY the tool never actually ran --
        /// see [`ToolCallUpdateMeta`]'s own doc comment.
        #[serde(rename = "_meta", default)]
        meta: Option<ToolCallUpdateMeta>,
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
    /// Echo of the user's own prompt content. Verified live on Grok CLI
    /// (`turn-grok.jsonl`): sent as the very first `session/update` of an
    /// ordinary turn, not only when replaying a loaded session's history
    /// as this file previously assumed -- `{"type": "text", "text":
    /// "..."}`, the same content shape as `agent_message_chunk`, plus a
    /// Grok-only `_meta` sidecar (`modelId`, `promptIndex`) that this
    /// variant has no named or `extra` slot for and so drops -- nothing in
    /// this build needs it today.
    #[serde(rename = "user_message_chunk")]
    UserMessageChunk {
        #[serde(default)]
        content: Value,
    },
    /// The agent's execution plan. Always a full snapshot replacing any
    /// plan sent before it, never a delta. NOT OBSERVED in any of the four
    /// providers' single-turn captures (`turn-{claude,codex,grok,
    /// kimi}.jsonl`) -- a plain "reply with one word" turn never produces
    /// a plan, so this shape remains an unverified guess pending a
    /// capture that does exercise planning.
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
    /// The session's active mode changed. NOT OBSERVED in any of the four
    /// providers' single-turn captures (`turn-{claude,codex,grok,
    /// kimi}.jsonl`) -- a plain "reply with one word" turn never switches
    /// mode mid-flight, so this shape remains an unverified guess pending a
    /// capture that does exercise a mode switch.
    #[serde(rename = "current_mode_update")]
    CurrentModeUpdate {
        #[serde(rename = "currentModeId", default)]
        current_mode_id: String,
    },
    /// Session metadata changed (e.g. title). Only the fields that
    /// actually changed are present on the wire; anything not named below
    /// lands in `extra` rather than being dropped. All four providers
    /// disagree on the exact shape, confirmed by direct comparison
    /// (`turn-{claude,codex,grok,kimi}.jsonl`): Grok and Kimi send bare
    /// `{"title": "..."}`; Claude additionally sends `updatedAt` (kept in
    /// `extra`, not modeled -- see [`SessionSummary`] for why this file
    /// does not parse ACP timestamp strings); codex-acp sends
    /// NO `title` at all, only `{"_meta": {"codex": {"threadStatus":
    /// {...}}}}` -- `title: Option<String>` with `#[serde(default)]`
    /// already tolerates that (defaults to `None`), and `extra` keeps the
    /// `_meta` sidecar intact.
    #[serde(rename = "session_info_update")]
    SessionInfoUpdate {
        #[serde(default)]
        title: Option<String>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    /// Context-window consumption and, when present, turn cost. Field
    /// names verified live on claude-agent-acp 0.72.0 and Kimi Code CLI
    /// 0.39.1 (`turn-claude.jsonl`, `turn-kimi.jsonl`): the wire keys are
    /// the bare `used`/`size`, NOT the previously guessed camelCase
    /// `usedTokens`/`contextWindow` -- that guess never matched a real
    /// payload, so the context-usage bar silently stayed empty on every
    /// turn. `cost` is real too, but rarer: claude-agent-acp sends it only
    /// on the last `usage_update` of a turn (two earlier ones in the same
    /// turn had no `cost` at all), matching `{"amount", "currency"}`
    /// exactly as this file had guessed; Kimi Code CLI never sends it;
    /// Grok CLI and codex-acp never send a `usage_update` notification at
    /// all -- both report usage on the `session/prompt` response instead
    /// ([`SessionPromptResult`]). `extra` keeps anything that lands under
    /// a different real key name from being silently dropped.
    #[serde(rename = "usage_update")]
    UsageUpdate {
        #[serde(rename = "used", default)]
        used_tokens: Option<u64>,
        #[serde(rename = "size", default)]
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

/// The `_meta` sidecar on a `tool_call_update`, sourced from the Claude
/// Agent SDK's `tool_result_meta` (`claude-agent-acp` src,
/// `docs/gate4agent/research/gate4agent-blocked-action-signals-2026-09-02.md`
/// §1a): `nonExecutionKind` is WHY the tool never actually ran --
/// `"user-rejected"`, `"permission-rule"`, `"interrupted"`, `"cancelled"` --
/// so a client can render the denial/cancellation distinctly from a real
/// tool failure. The exact wire key for a companion user-typed rejection
/// comment is UNCONFIRMED from the research (only "a companion free-text
/// field" is documented, no field name); every plausible spelling is tried
/// here so whichever one a live agent actually sends still surfaces, rather
/// than committing to a guess and silently dropping the others.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallUpdateMeta {
    #[serde(rename = "nonExecutionKind", default)]
    pub non_execution_kind: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(rename = "userComment", default)]
    pub user_comment: Option<String>,
    #[serde(rename = "user_comment", default)]
    pub user_comment_snake: Option<String>,
}

impl ToolCallUpdateMeta {
    /// The first user-typed rejection comment found under any of the
    /// plausible wire spellings -- see this type's own doc comment.
    fn comment_text(&self) -> Option<&str> {
        self.comment
            .as_deref()
            .or(self.user_comment.as_deref())
            .or(self.user_comment_snake.as_deref())
    }
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
/// `available_commands_update`. `extra` keeps vendor-specific sidecar
/// data intact -- verified live shapes include Codex's `_meta.
/// commandAction` (`/plan`'s config-option wiring) and Grok's `_meta.
/// scope`/`path`/`bareName`/`qualifiedName` (skill-backed commands).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AvailableCommand {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input: Option<AvailableCommandInput>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
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

/// One selectable value of a `select`-kind [`SessionConfigOption`]. Field
/// names verified live against `session/new`'s result on
/// claude-agent-acp 0.71.0, codex-acp 1.8.0, and Kimi Code CLI 0.39.1
/// (`acp-claude.jsonl`, `acp-codex.jsonl`, `acp-kimi.jsonl`): the choice's
/// display label is the wire key `"name"`, not `"label"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigOptionChoice {
    #[serde(default)]
    pub value: Value,
    #[serde(rename = "name", default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// One session configuration setting -- the mechanism that supersedes
/// session modes for things like model selection and reasoning effort.
/// `config_option_update` carries the FULL current set on every update,
/// same as `plan`. Field names verified live against `session/new`'s
/// result on claude-agent-acp 0.71.0, codex-acp 1.8.0, and Kimi Code CLI
/// 0.39.1 (`acp-claude.jsonl`, `acp-codex.jsonl`, `acp-kimi.jsonl`): the
/// kind discriminator is the wire key `"type"` (previously guessed as
/// `"kind"`, which never matched a real payload and silently defaulted
/// every option to `ConfigOptionKind::Unknown`), and the current
/// selection is the wire key `"currentValue"` (previously guessed as
/// `"value"`, which likewise never matched and left every option's value
/// at `Value::Null`). `extra` keeps anything under a different real key
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
    #[serde(rename = "type", default)]
    pub kind: ConfigOptionKind,
    #[serde(rename = "currentValue", default)]
    pub value: Value,
    #[serde(default)]
    pub options: Vec<ConfigOptionChoice>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Amount/currency pair attached to a `usage_update`, when the agent
/// reports cost. Field names verified live on claude-agent-acp 0.72.0
/// (`turn-claude.jsonl`, the last `usage_update` of the turn:
/// `{"amount": 0.17858, "currency": "USD"}`). Kimi Code CLI 0.39.1 never
/// sends this sub-object; Grok CLI and codex-acp never send `usage_update`
/// at all.
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
/// [`PermissionOption::kind`], plus [`Unknown`](Self::Unknown) for any
/// `kind` string a third-party agent sends that is none of them. An agent
/// is not required to offer all four -- see [`PermissionRequestParams`]
/// and the host policy that selects among whichever subset arrives.
///
/// `Unknown` matters for the same reason `ToolKind::Other` does: without a
/// `#[serde(other)]` catch-all here, ONE option in the offered list with an
/// unfamiliar `kind` string used to fail this enum's `Deserialize`, which
/// failed the whole [`PermissionOption`], which failed the whole
/// `Vec<PermissionOption>`, which failed the whole
/// [`PermissionRequestParams`] -- so a host never even saw the request, let
/// alone the kinds it DID recognise, and every `session/request_permission`
/// call from an adapter that words even one option differently ended up
/// answered `PermissionOutcome::Cancelled` (a refusal) with the actual
/// cause logged nowhere. `Unknown` is never selected by kind preference
/// (`super::host::select_offered_option` only ever matches one of the
/// other four); the only path that can still pick an option shaped like
/// this is the allow-ish name/`optionId` fallback in
/// `super::host::HostPolicy::select_permission_option`, and only when
/// policy prefers allow and none of the four known kinds were offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// Any `kind` string the four variants above don't name. Serializes
    /// back out as the literal `"unknown"` (`rename_all = "snake_case"`
    /// applies to every variant including this one); nothing round-trips
    /// through that string today since a host only ever deserializes this
    /// type off an incoming agent request and never sends its own value
    /// for it.
    #[serde(other)]
    Unknown,
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
    /// Per-session lifecycle capabilities. Verified live on
    /// claude-agent-acp 0.71.0, codex-acp 1.8.0, Kimi Code CLI 0.39.1, and
    /// Grok CLI 1.0.13 (`acp-claude.jsonl`, `acp-codex.jsonl`,
    /// `acp-kimi.jsonl`, `acp-grok.jsonl`).
    #[serde(rename = "sessionCapabilities", default)]
    pub session_capabilities: SessionCapabilities,
    /// Remaining capability fields (future-proofing).
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

/// The `sessionCapabilities` block inside `agentCapabilities` -- each key
/// present means the agent advertises that lifecycle operation; its value
/// is an (currently always empty) object reserved for future per-
/// capability configuration, so presence/absence of the key is the signal
/// this build reads, not its contents. Verified live: claude-agent-acp
/// 0.71.0 and codex-acp 1.8.0 advertise all seven keys including
/// `subagents`; Kimi Code CLI 0.39.1 advertises six (no `subagents`); Grok
/// CLI 1.0.13 advertises only `list`/`resume`/`close`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCapabilities {
    #[serde(default)]
    pub list: Option<Value>,
    #[serde(default)]
    pub resume: Option<Value>,
    #[serde(default)]
    pub close: Option<Value>,
    #[serde(default)]
    pub delete: Option<Value>,
    #[serde(default)]
    pub fork: Option<Value>,
    #[serde(rename = "additionalDirectories", default)]
    pub additional_directories: Option<Value>,
    /// Whether the agent may spawn subordinate agent turns ("subagents")
    /// during a session. Verified live as an advertised capability flag on
    /// claude-agent-acp 0.71.0 and codex-acp 1.8.0 -- but neither capture
    /// exercises a full prompt turn, so no subagent lifecycle event or
    /// request shape has been observed on the wire; see
    /// `super::session::AcpSession::supports_subagents` for what this
    /// build can and cannot do with the flag today.
    #[serde(default)]
    pub subagents: Option<Value>,
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

/// Extract token counts from a raw ACP response value. This is the
/// generic fallback for shapes that predate the typed
/// [`SessionPromptResult`]/[`PromptUsage`]/[`PromptResultMeta`] parse
/// (`super::session::emit_prompt_result` tries those first); it is also
/// used by the still-unverified `SessionUpdate::Stop` variant, which no
/// live capture has ever exercised.
///
/// Tries multiple known shapes:
/// 1. ACP canonical camelCase: `{"inputTokens": N, "outputTokens": N}`
/// 2. Nested under `usage`, camelCase -- verified live on claude-agent-acp
///    0.72.0 (`turn-claude.jsonl`): `{"usage": {"inputTokens": N,
///    "outputTokens": N}}`.
/// 3. Nested under `usage`, snake_case -- UNVERIFIED, kept from this
///    function's earlier guess; no live capture has ever sent this exact
///    shape, only the Anthropic Messages API's own history-file format
///    (`src/history/claude.rs`), which is a different transport entirely.
/// 4. Stats-nested: `{"stats": {"input_tokens": N, "output_tokens": N}}`
///    -- likewise UNVERIFIED.
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

    if let Some(usage) = v.get("usage") {
        // 2. Nested under "usage", camelCase (verified: claude-agent-acp)
        if let (Some(i), Some(o)) = (
            usage.get("inputTokens").and_then(|x| x.as_u64()),
            usage.get("outputTokens").and_then(|x| x.as_u64()),
        ) {
            return (i, o);
        }
        // 3. Nested under "usage", snake_case (unverified)
        if let (Some(i), Some(o)) = (
            usage.get("input_tokens").and_then(|x| x.as_u64()),
            usage.get("output_tokens").and_then(|x| x.as_u64()),
        ) {
            return (i, o);
        }
    }

    // 4. Nested under "stats" (unverified)
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
            // `{"thought": "..."}` was this file's original guess and is
            // checked first so it still wins if some agent sends it; real
            // Grok CLI and Kimi Code CLI both send the SAME content shape
            // as `agent_message_chunk` instead --
            // `{"type": "text", "text": "..."}` (`turn-grok.jsonl`,
            // `turn-kimi.jsonl`) -- which the `thought`-only lookup below
            // used to miss entirely, silently emitting empty thinking text
            // on every real thought chunk from either provider.
            let text = content
                .get("thought")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| extract_text_from_content(content));
            vec![AgentEvent::Thinking { text }]
        }

        SessionUpdate::ToolCall { tool_call_id, title, raw_input, .. } => {
            vec![AgentEvent::ToolStart {
                id: tool_call_id.clone(),
                name: title.clone(),
                input: raw_input.clone(),
            }]
        }

        SessionUpdate::ToolCallUpdate { tool_call_id, status, content, meta } => {
            let mut output = content
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("");
            let non_execution_kind = meta.as_ref().and_then(|m| m.non_execution_kind.clone());
            if let Some(comment) = meta.as_ref().and_then(ToolCallUpdateMeta::comment_text) {
                let comment = crate::utils::truncate_str(
                    comment,
                    gate4agent_types::PROVIDER_EVENT_TEXT_MAX_BYTES,
                );
                if !comment.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(comment);
                }
            }
            // ACP's own status vocabulary is `pending | in_progress |
            // completed | failed` (this file's `ToolCallStatus`-shaped
            // uses elsewhere) -- a declined/denied call resolves as
            // `"failed"` on every provider surveyed (Codex's codex-acp
            // maps a decline to `ToolCallStatus::Failed` the same way a
            // real execution failure is). `"error"` is kept as a
            // tolerated alias: no live capture has ever sent it, but
            // nothing in the spec forbids a future/vendor agent doing so,
            // and accepting it costs nothing a real `"failed"`/`"error"`
            // provider would notice.
            let is_error = status == "failed" || status == "error";
            vec![AgentEvent::ToolResult {
                id: tool_call_id.clone(),
                output,
                is_error,
                duration_ms: None,
                non_execution_kind,
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
            let parsed_stop_reason = StopReason::from_wire_str(stop_reason);
            let is_error = parsed_stop_reason.is_refusal();
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
                    is_error,
                    stop_reason: Some(parsed_stop_reason),
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
// Vendor extensions: Grok's `_x.ai/*` namespace
// ---------------------------------------------------------------------------
//
// xAI's Grok CLI (ACP transport) sends a family of provider-specific
// notifications outside the ACP spec's own method names, all under the
// `_x.ai/` prefix -- captured live from a real `grok agent stdio` run
// (`acp-grok.jsonl`). [`parse_vendor_notification`] is the single entry
// point the reader loop calls for any notification method that is not
// `session/update`; it returns `None` for anything it does not
// recognize -- either a method outside `_x.ai/` entirely, or an
// `_x.ai/*` method this build has never seen, or one whose payload
// failed to parse -- so an unrecognized extension always falls back to
// the generic `AgentEvent::RpcNotification` passthrough rather than
// breaking parsing.

/// `_x.ai/models/update` notification params -- the flat sibling of
/// [`SessionModelState`] (no wrapping `models` key). Verified live on
/// Grok CLI 1.0.13 (`acp-grok.jsonl`).
#[derive(Debug, Clone, Deserialize)]
struct VendorModelsUpdateParams {
    #[serde(rename = "currentModelId", default)]
    current_model_id: Option<String>,
    #[serde(rename = "availableModels", default)]
    available_models: Vec<SessionModel>,
}

/// Status of one hook run inside `_x.ai/session_notification`'s
/// `hook_execution` update. Verified live on Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorHookStatus {
    #[serde(default)]
    status: String,
    #[serde(rename = "elapsed_ms", default)]
    elapsed_ms: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// One hook run inside `_x.ai/session_notification`'s `hook_execution`
/// update. Verified live on Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorHookRun {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: VendorHookStatus,
}

/// `_x.ai/session_notification`'s `update` sub-object -- a discriminated
/// union of the two kinds verified live on Grok CLI 1.0.13
/// (`hook_execution`, `model_changed`). `Unknown` covers any other kind
/// this build has never seen.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "sessionUpdate")]
enum VendorSessionNotification {
    #[serde(rename = "hook_execution")]
    HookExecution {
        #[serde(default)]
        event_name: String,
        #[serde(default)]
        runs: Vec<VendorHookRun>,
    },
    #[serde(rename = "model_changed")]
    ModelChanged {
        #[serde(rename = "model_id", default)]
        model_id: String,
        #[serde(rename = "reasoning_effort", default)]
        reasoning_effort: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// `_x.ai/session_notification` notification params. Verified live on
/// Grok CLI 1.0.13.
#[derive(Debug, Clone, Deserialize)]
struct VendorSessionNotificationParams {
    update: VendorSessionNotification,
}

/// One entry of `_x.ai/mcp/servers_updated`'s `mcpServers` array.
/// Verified live on Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorMcpServerEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    source: String,
    #[serde(rename = "type", default)]
    transport: String,
}

/// `_x.ai/mcp/servers_updated` notification params. Verified live on
/// Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorMcpServersUpdatedParams {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: Vec<VendorMcpServerEntry>,
}

/// `_x.ai/mcp/init_progress` notification params. Verified live on Grok
/// CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorMcpInitProgressParams {
    #[serde(default)]
    total: u32,
    #[serde(default)]
    connected: u32,
}

/// `_x.ai/mcp_initialized` notification params. Verified live on Grok CLI
/// 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorMcpInitializedParams {
    #[serde(rename = "mcpToolCount", default)]
    mcp_tool_count: u32,
    #[serde(rename = "elapsedMs", default)]
    elapsed_ms: u64,
}

/// One entry of `_x.ai/announcements/update`'s `announcements` array.
/// Verified live on Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorAnnouncement {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    message: String,
    #[serde(default)]
    severity: Option<String>,
}

/// `_x.ai/announcements/update` notification params. Verified live on
/// Grok CLI 1.0.13.
#[derive(Debug, Clone, Default, Deserialize)]
struct VendorAnnouncementsUpdateParams {
    #[serde(default)]
    announcements: Vec<VendorAnnouncement>,
}

/// Convert a [`SessionModel`] (the `session/new`/`session/load`/vendor
/// wire shape) into the transport-neutral [`AvailableModelInfo`] carried
/// on [`AgentEvent::ModelsUpdate`].
fn session_model_to_info(model: SessionModel) -> AvailableModelInfo {
    let meta = model.meta.unwrap_or_default();
    AvailableModelInfo {
        model_id: model.model_id,
        name: model.name,
        description: model.description,
        context_tokens: meta.total_context_tokens,
        reasoning_efforts: meta
            .reasoning_efforts
            .into_iter()
            .map(|effort| ReasoningEffortInfo {
                id: effort.id,
                label: effort.label,
                description: effort.description,
                is_default: effort.default,
            })
            .collect(),
    }
}

/// Parse an `_x.ai/*` vendor notification into an [`AgentEvent`],
/// updating `state.models` when the method is `_x.ai/models/update` so
/// `AcpSession::available_models`/`current_model_id` stay current the
/// same way `session/update` keeps the rest of [`SessionState`] current.
/// Returns `None` for a method this build does not recognize -- see this
/// section's module-level doc comment above.
pub(crate) fn parse_vendor_notification(
    state: &mut SessionState,
    method: &str,
    params: &Value,
) -> Option<AgentEvent> {
    match method {
        "_x.ai/models/update" => {
            let parsed: VendorModelsUpdateParams = serde_json::from_value(params.clone()).ok()?;
            state.models = SessionModelState {
                current_model_id: parsed.current_model_id.clone(),
                available_models: parsed.available_models.clone(),
            };
            Some(AgentEvent::ModelsUpdate {
                current_model_id: parsed.current_model_id,
                available_models: parsed
                    .available_models
                    .into_iter()
                    .map(session_model_to_info)
                    .collect(),
            })
        }
        "_x.ai/settings/update" => Some(AgentEvent::SettingsUpdate {
            permission_mode: params
                .get("permission_mode")
                .and_then(Value::as_str)
                .map(str::to_owned),
            auto_permission_mode_enabled: params
                .get("auto_permission_mode_enabled")
                .and_then(Value::as_bool),
            raw: params.clone(),
        }),
        "_x.ai/session_notification" => {
            let parsed: VendorSessionNotificationParams =
                serde_json::from_value(params.clone()).ok()?;
            match parsed.update {
                VendorSessionNotification::HookExecution { event_name, runs } => {
                    Some(AgentEvent::HookExecutionUpdate {
                        event_name,
                        runs: runs
                            .into_iter()
                            .map(|run| HookRunResult {
                                name: run.name,
                                status: run.status.status,
                                elapsed_ms: run.status.elapsed_ms,
                                error: run.status.error,
                            })
                            .collect(),
                    })
                }
                VendorSessionNotification::ModelChanged { model_id, reasoning_effort } => {
                    Some(AgentEvent::ProviderModelChanged { model_id, reasoning_effort })
                }
                VendorSessionNotification::Unknown => None,
            }
        }
        "_x.ai/mcp/servers_updated" => {
            let parsed: VendorMcpServersUpdatedParams =
                serde_json::from_value(params.clone()).ok()?;
            Some(AgentEvent::McpServersUpdate {
                servers: parsed
                    .mcp_servers
                    .into_iter()
                    .map(|entry| McpServerSummary {
                        name: entry.name,
                        source: entry.source,
                        transport: entry.transport,
                    })
                    .collect(),
            })
        }
        "_x.ai/mcp/init_progress" => {
            let parsed: VendorMcpInitProgressParams =
                serde_json::from_value(params.clone()).ok()?;
            Some(AgentEvent::McpInitProgress {
                total: parsed.total,
                connected: parsed.connected,
            })
        }
        "_x.ai/mcp_initialized" => {
            let parsed: VendorMcpInitializedParams =
                serde_json::from_value(params.clone()).ok()?;
            Some(AgentEvent::McpInitialized {
                tool_count: parsed.mcp_tool_count,
                elapsed_ms: parsed.elapsed_ms,
            })
        }
        "_x.ai/announcements/update" => {
            let parsed: VendorAnnouncementsUpdateParams =
                serde_json::from_value(params.clone()).ok()?;
            Some(AgentEvent::AnnouncementsUpdate {
                announcements: parsed
                    .announcements
                    .into_iter()
                    .map(|a| AnnouncementInfo {
                        id: a.id,
                        title: a.title,
                        message: a.message,
                        severity: a.severity,
                    })
                    .collect(),
            })
        }
        _ => None,
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
    fn update_to_event_thinking_grok_verbatim_text_field_not_thought_field() {
        // `turn-grok.jsonl` -- the REAL shape: same content object as
        // `agent_message_chunk`, `{"type": "text", "text": "..."}`. Before
        // this fix, the `{"thought": "..."}`-only lookup missed this
        // entirely and every real Grok thought chunk rendered as empty
        // text.
        let raw = r#"{"sessionId":"01a05e6a-aa4d-7a13-9e9c-2077aa244389","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"The"}}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Thinking { text } if text == "The"));
    }

    #[test]
    fn update_to_event_thinking_kimi_verbatim_text_field_not_thought_field() {
        // `turn-kimi.jsonl` -- same real shape as Grok's, confirming it is
        // not a Grok-only quirk.
        let raw = r#"{"sessionId":"session_ff1eb29e-0ba7-40aa-b654-bbf327c39463","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Reply"}}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Thinking { text } if text == "Reply"));
    }

    #[test]
    fn update_to_event_user_message_chunk_grok_verbatim() {
        // `turn-grok.jsonl` -- the user's own prompt, echoed back as the
        // first `session/update` of the turn. Same content shape as
        // `agent_message_chunk`; the Grok-only `_meta` sidecar
        // (`modelId`, `promptIndex`) is dropped -- this variant has no
        // named or `extra` slot for it, and nothing in this build needs
        // it.
        let raw = r#"{"sessionId":"01a05e6a-aa4d-7a13-9e9c-2077aa244389","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"Reply with the single word: ok"},"_meta":{"modelId":"grok-4.6","promptIndex":0}}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AgentEvent::UserMessage { text, is_delta: true } if text == "Reply with the single word: ok"
        ));
    }

    #[test]
    fn update_to_event_agent_message_chunk_claude_verbatim_single_object() {
        // `turn-claude.jsonl` -- single content-block object, NOT an
        // array, contradicting this variant's earlier "Claude ACP sends
        // an array" doc note.
        let raw = r#"{"sessionId":"2a82e7d7-ffa6-4ebf-a424-1bd783ec3457","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"},"messageId":"msg_011CedGLEtEZqvzLu9npuaJo"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Text { text, is_delta: true } if text == "ok"));
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
            meta: None,
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { id, output, is_error, non_execution_kind, .. }
                if id == "t1" && output == "ok" && !is_error && non_execution_kind.is_none())
        );
    }

    #[test]
    fn update_to_event_tool_result_failed_status_is_error() {
        // ACP's own status vocabulary is `pending | in_progress | completed
        // | failed` -- a `"done"`/`"error"`-shaped guess never matches a
        // real provider's `"failed"`. No `_meta` on this update -- a plain
        // execution failure, not a block.
        let p = make_update(SessionUpdate::ToolCallUpdate {
            tool_call_id: "t1".to_string(),
            status: "failed".to_string(),
            content: vec![json!("boom")],
            meta: None,
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentEvent::ToolResult { is_error, non_execution_kind, .. }
                if *is_error && non_execution_kind.is_none())
        );
    }

    #[test]
    fn update_to_event_tool_result_classifier_permission_rule_sample() {
        // The owner's classifier sample, verbatim
        // (`docs/gate4agent/research/gate4agent-blocked-action-signals-2026-09-02.md`
        // appendix), arriving as a `tool_call_update` with
        // `_meta.nonExecutionKind: "permission-rule"` -- the shape
        // claude-agent-acp uses to report a classifier/permission-rule
        // denial.
        let sample = "Permission for this action was denied by the Claude Code auto mode classifier. Reason: Blocked by classifier. If you have other tasks that don't depend on this action, continue working on those.";
        let p = make_update(SessionUpdate::ToolCallUpdate {
            tool_call_id: "t1".to_string(),
            status: "failed".to_string(),
            content: vec![json!(sample)],
            meta: Some(ToolCallUpdateMeta {
                non_execution_kind: Some("permission-rule".to_owned()),
                comment: None,
                user_comment: None,
                user_comment_snake: None,
            }),
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolResult { is_error, non_execution_kind, output, .. } => {
                assert!(*is_error);
                assert_eq!(non_execution_kind.as_deref(), Some("permission-rule"));
                assert_eq!(output, sample);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
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
        assert!(matches!(
            &events[1],
            AgentEvent::SessionEnd { is_error: false, stop_reason: Some(StopReason::EndTurn), .. }
        ));
    }

    #[test]
    fn update_to_event_stop_refusal_is_error() {
        let p = make_update(SessionUpdate::Stop {
            stop_reason: "refusal".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            usage: None,
        });
        let events = update_to_event(&p);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[1],
            AgentEvent::SessionEnd { is_error: true, stop_reason: Some(StopReason::Refusal), .. }
        ));
    }

    #[test]
    fn extract_token_usage_acp_canonical() {
        let v = json!({"inputTokens": 10, "outputTokens": 5});
        assert_eq!(extract_token_usage(&v), (10, 5));
    }

    #[test]
    fn extract_token_usage_nested_camel_case_verified_claude_shape() {
        // Verbatim shape from `turn-claude.jsonl` line 9 (the `session/
        // prompt` response's `usage` object): camelCase, not the
        // snake_case this function's "Claude-nested" case originally
        // guessed and never matched a real payload.
        let v = json!({"usage": {"inputTokens": 2, "outputTokens": 4, "cachedReadTokens": 15320, "cachedWriteTokens": 17081, "totalTokens": 32407}});
        assert_eq!(extract_token_usage(&v), (2, 4));
    }

    #[test]
    fn extract_token_usage_nested_snake_case_unverified_fallback() {
        // No live capture has ever sent this exact shape -- see the
        // function's own doc comment. Kept only as a defensive fallback.
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

    // -----------------------------------------------------------------------
    // SessionPromptResult -- `session/prompt` response, all four providers
    // -----------------------------------------------------------------------

    #[test]
    fn session_prompt_result_claude_verbatim_usage() {
        // `turn-claude.jsonl` line 9 -- the id=2 result. `stopReason` plus
        // a camelCase `usage` object; `_meta.quota` duplicates the same
        // numbers in a different shape and is not modeled, only kept via
        // `extra`.
        let raw = r#"{"stopReason":"end_turn","usage":{"inputTokens":2,"outputTokens":4,"cachedReadTokens":15320,"cachedWriteTokens":17081,"totalTokens":32407},"_meta":{"quota":{"token_count":{"totalTokens":32407,"inputTokens":2,"cachedInputTokens":15320,"cachedWriteTokens":17081,"outputTokens":4,"reasoningOutputTokens":0},"model_usage":[{"model":"claude-opus-5[1m]","token_count":{"totalTokens":32407,"inputTokens":2,"cachedInputTokens":15320,"cachedWriteTokens":17081,"outputTokens":4,"reasoningOutputTokens":0}}]}}}"#;
        let result: SessionPromptResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        let usage = result.usage.expect("claude sends usage");
        assert_eq!(usage.input_tokens, Some(2));
        assert_eq!(usage.output_tokens, Some(4));
        assert_eq!(usage.cached_read_tokens, Some(15320));
        assert_eq!(usage.cached_write_tokens, Some(17081));
        assert_eq!(usage.total_tokens, Some(32407));
        // `_meta` is present on the wire (Claude's own `quota` sidecar),
        // so it parses to `Some` -- but none of its NAMED token fields
        // are populated by `quota` (those are Grok's field names), only
        // `extra`.
        let meta = result.meta.expect("claude sends _meta.quota");
        assert!(meta.total_tokens.is_none());
        assert!(meta.input_tokens.is_none());
        assert!(meta.extra.contains_key("quota"));
    }

    #[test]
    fn session_prompt_result_grok_verbatim_meta() {
        // `turn-grok.jsonl` -- the id=2 result. No top-level `usage`; the
        // breakdown rides directly on `_meta` instead, alongside a
        // duplicate nested `_meta.usage` this build does not model
        // separately (kept intact via `extra`).
        let raw = r#"{"stopReason":"end_turn","_meta":{"sessionId":"01a05e6a-aa4d-7a13-9e9c-2077aa244389","requestId":"97238a1c-461a-4f9a-ba2f-4acc677b762b","promptId":"97238a1c-461a-4f9a-ba2f-4acc677b762b","totalTokens":19885,"modelId":"grok-4.6","inputTokens":19807,"outputTokens":78,"cachedReadTokens":1408,"reasoningTokens":73,"usage":{"inputTokens":19807,"outputTokens":78,"totalTokens":19885,"cachedReadTokens":1408,"cacheCreationTokens":0,"reasoningTokens":73,"modelCalls":1,"apiDurationMs":3284,"costUsdTicks":64549000,"numTurns":1}}}"#;
        let result: SessionPromptResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(result.usage.is_none());
        let meta = result.meta.expect("grok sends _meta");
        assert_eq!(meta.session_id.as_deref(), Some("01a05e6a-aa4d-7a13-9e9c-2077aa244389"));
        assert_eq!(meta.request_id.as_deref(), Some("97238a1c-461a-4f9a-ba2f-4acc677b762b"));
        assert_eq!(meta.prompt_id.as_deref(), Some("97238a1c-461a-4f9a-ba2f-4acc677b762b"));
        assert_eq!(meta.model_id.as_deref(), Some("grok-4.6"));
        assert_eq!(meta.total_tokens, Some(19885));
        assert_eq!(meta.input_tokens, Some(19807));
        assert_eq!(meta.output_tokens, Some(78));
        assert_eq!(meta.cached_read_tokens, Some(1408));
        assert_eq!(meta.reasoning_tokens, Some(73));
        // The nested per-model breakdown must not be silently dropped.
        assert!(meta.extra.contains_key("usage"));
    }

    #[test]
    fn session_prompt_result_kimi_verbatim_bare_stop_reason() {
        // `turn-kimi.jsonl` -- no `usage`, no `_meta` at all.
        let raw = r#"{"stopReason":"end_turn"}"#;
        let result: SessionPromptResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(result.usage.is_none());
        assert!(result.meta.is_none());
    }

    #[test]
    fn session_prompt_result_codex_verbatim_bare_stop_reason() {
        // `turn-codex.jsonl` -- identical bare shape to Kimi's.
        let raw = r#"{"stopReason":"end_turn"}"#;
        let result: SessionPromptResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert!(result.usage.is_none());
        assert!(result.meta.is_none());
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
        let p = SessionNewParams {
            cwd: "/home/user".to_string(),
            mcp_servers: vec![],
            additional_directories: vec![],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"cwd\""), "must use cwd");
        assert!(s.contains("\"mcpServers\""), "must use mcpServers");
        assert!(s.contains("\"additionalDirectories\""), "must use additionalDirectories");
    }

    #[test]
    fn session_new_params_carries_additional_directories() {
        let p = SessionNewParams {
            cwd: "/home/user".to_string(),
            mcp_servers: vec![],
            additional_directories: vec!["/home/user/other-repo".to_string()],
        };
        let s = serde_json::to_string(&p).unwrap();
        let decoded: SessionNewParams = serde_json::from_str(&s).unwrap();
        assert_eq!(decoded.additional_directories, vec!["/home/user/other-repo".to_string()]);
    }

    /// The generic MCP-server-overlay door (gate4agent-arc-mailbox-and-task-
    /// layer Slice A(ii)): when a caller populates `mcp_servers` with one
    /// stdio entry, `session/new` carries it in ACP v1's actual wire shape --
    /// `name`, `command`, `args`, and `env` (an array of `{name,value}`
    /// pairs, never a map), and no `"transport"` discriminator at all. This
    /// is the shape a live capture once showed an ACP adapter silently
    /// dropping: a nameless entry with `env` as a JSON object never matched
    /// the adapter's own `McpServer` union, so `tools/list` never ran. An
    /// empty `mcp_servers` (no overlay prepared) still serializes to
    /// `"mcpServers":[]`, exactly as `session_new_params_serialize` above
    /// already pins.
    #[test]
    fn session_new_params_serializes_mcp_server_stdio_entry() {
        let command = "C:\\example\\example-mcp-server.exe";
        let endpoint = "\\\\.\\pipe\\example-mcp-server-s1";
        let p = SessionNewParams {
            cwd: "/home/user".to_string(),
            mcp_servers: vec![McpServerConfig::stdio(
                "example-mcp-server",
                command,
                vec!["--session-proxy".to_string()],
                vec![
                    ("EXAMPLE_SESSION_ENDPOINT".to_string(), endpoint.to_string()),
                    ("EXAMPLE_SESSION_TOKEN".to_string(), "tok-abc".to_string()),
                ],
            )],
            additional_directories: vec![],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""mcpServers":[{"#), "must carry the one stdio entry");
        assert!(!s.contains("\"transport\""), "ACP v1 stdio has no discriminator field");
        assert!(s.contains(r#""name":"example-mcp-server""#), "must carry the ACP-visible server name");
        assert!(s.contains(&format!("\"command\":{command:?}")));
        assert!(s.contains(r#""args":["--session-proxy"]"#));
        // env is an array of {name,value} pairs, never a map -- this is
        // exactly the shape a bare `contains("EXAMPLE_SESSION_ENDPOINT")`
        // check (the old assertion) could not have caught, because it
        // passes whether env is an object or an array.
        assert!(s.contains(&format!(
            r#""env":[{{"name":"EXAMPLE_SESSION_ENDPOINT","value":{endpoint:?}}},{{"name":"EXAMPLE_SESSION_TOKEN","value":"tok-abc"}}]"#
        )));

        let empty = SessionNewParams {
            cwd: "/home/user".to_string(),
            mcp_servers: vec![],
            additional_directories: vec![],
        };
        let empty_s = serde_json::to_string(&empty).unwrap();
        assert!(empty_s.contains(r#""mcpServers":[]"#), "no overlay means an empty array, not absence");
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
        // ACP v1's stdio `McpServer` carries no discriminator at all --
        // `name`, `command`, `args`, `env` land directly on the object.
        let cfg = McpServerConfig::stdio(
            "my-server",
            "my-mcp-server",
            vec!["--port".to_string(), "8080".to_string()],
            std::iter::empty(),
        );
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(!s.contains("\"transport\""), "ACP v1 stdio has no discriminator field");
        assert!(s.contains(r#""name":"my-server""#), "must carry the ACP-visible name");
        assert!(s.contains(r#""command":"my-mcp-server""#), "must contain command");
        assert!(s.contains(r#""env":[]"#), "env is an array even when empty, never a map");
    }

    #[test]
    fn mcp_server_config_sse_serialize() {
        // ACP v1's SSE `McpServer` is the variant that DOES carry a
        // discriminator -- `"type":"sse"` -- plus `name`.
        let cfg = McpServerConfig::sse("my-sse-server", "https://example.com/mcp", std::iter::empty());
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(s.contains(r#""type":"sse""#), "must tag as sse");
        assert!(s.contains(r#""name":"my-sse-server""#), "must carry the ACP-visible name");
        assert!(s.contains("https://example.com/mcp"), "must contain url");
        assert!(s.contains(r#""headers":[]"#), "headers is an array even when empty, never a map");
    }

    #[test]
    fn mcp_server_config_roundtrip() {
        let original = McpServerConfig::stdio(
            "fs-server",
            "npx",
            vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
            vec![("HOME".to_string(), "/home/user".to_string())],
        );
        let json = serde_json::to_string(&original).unwrap();
        let decoded: McpServerConfig = serde_json::from_str(&json).unwrap();
        match decoded {
            McpServerConfig::Stdio { name, command, args, env } => {
                assert_eq!(name, "fs-server");
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@modelcontextprotocol/server-filesystem"]);
                assert_eq!(env.len(), 1);
                assert_eq!(env[0].name, "HOME");
                assert_eq!(env[0].value, "/home/user");
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
    fn permission_request_params_tolerates_an_unknown_option_kind() {
        // Regression: before `PermissionOptionKind::Unknown` existed, one
        // option in the list whose `kind` string was not one of the four
        // known ones failed the WHOLE `PermissionRequestParams` to
        // deserialize, hiding every option the agent DID offer.
        let raw = r#"{
            "sessionId": "s1",
            "toolCall": {"toolCallId": "tc1", "kind": "execute"},
            "options": [
                {"optionId": "always_allow", "name": "Always allow", "kind": "allow_forever"},
                {"optionId": "reject-once", "name": "Reject once", "kind": "reject_once"}
            ]
        }"#;
        let params: PermissionRequestParams = serde_json::from_str(raw).unwrap();
        assert_eq!(params.options.len(), 2);
        assert_eq!(params.options[0].kind, PermissionOptionKind::Unknown);
        assert_eq!(params.options[1].kind, PermissionOptionKind::RejectOnce);
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
    fn session_update_session_info_update_grok_verbatim() {
        // `turn-grok.jsonl` -- bare title, nothing else. (The wire message
        // is wrapped in the usual `{"jsonrpc", "method": "session/update",
        // "params": {...}}` envelope; `params` is what
        // `SessionUpdateParams` deserializes.)
        let raw = r#"{"sessionId":"01a05e6a-aa4d-7a13-9e9c-2077aa244389","update":{"sessionUpdate":"session_info_update","title":"Reply with single word ok"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            &params.update,
            SessionUpdate::SessionInfoUpdate { title: Some(t), .. } if t == "Reply with single word ok"
        ));
    }

    #[test]
    fn session_update_session_info_update_claude_verbatim_carries_updated_at_in_extra() {
        // `turn-claude.jsonl` -- title plus `updatedAt`, which this build
        // does not model as a named field (see the variant's doc comment)
        // and must therefore keep in `extra` rather than drop.
        let raw = r#"{"sessionId":"2a82e7d7-ffa6-4ebf-a424-1bd783ec3457","update":{"sessionUpdate":"session_info_update","title":"Reply with 'ok'","updatedAt":"2026-09-01T19:23:20.355Z"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::SessionInfoUpdate { title, extra } = &params.update else {
            panic!("expected SessionInfoUpdate variant");
        };
        assert_eq!(title.as_deref(), Some("Reply with 'ok'"));
        assert_eq!(
            extra.get("updatedAt").and_then(Value::as_str),
            Some("2026-09-01T19:23:20.355Z")
        );
    }

    #[test]
    fn session_update_session_info_update_kimi_verbatim() {
        // `turn-kimi.jsonl` -- bare title, same shape as Grok's.
        let raw = r#"{"sessionId":"session_ff1eb29e-0ba7-40aa-b654-bbf327c39463","update":{"sessionUpdate":"session_info_update","title":"Reply with the single word: ok"}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            &params.update,
            SessionUpdate::SessionInfoUpdate { title: Some(t), .. } if t == "Reply with the single word: ok"
        ));
    }

    #[test]
    fn session_update_session_info_update_codex_verbatim_has_no_title_at_all() {
        // `turn-codex.jsonl` -- the one provider that sends this update
        // WITHOUT a title, only a `_meta.codex.threadStatus` sidecar.
        // Parsing must survive it: `title` defaults to `None` and the
        // sidecar lands in `extra`.
        let raw = r#"{"sessionId":"01a05e70-55a1-7010-9c10-79ae25c4f531","update":{"sessionUpdate":"session_info_update","_meta":{"codex":{"threadStatus":{"type":"active","activeFlags":[]}}}}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::SessionInfoUpdate { title, extra } = &params.update else {
            panic!("expected SessionInfoUpdate variant");
        };
        assert!(title.is_none());
        assert!(extra.contains_key("_meta"));
        let events = update_to_event(&params);
        assert!(matches!(&events[0], AgentEvent::SessionInfoUpdate { title: None }));
    }

    #[test]
    fn session_update_usage_update_claude_verbatim_bare() {
        // `turn-claude.jsonl` line 5 -- the first of three `usage_update`s
        // in the turn, no `_meta`, no `cost`. `used`/`size` are the real
        // wire keys; this file previously guessed camelCase
        // `usedTokens`/`contextWindow`, which never matched.
        let raw = r#"{"sessionId":"2a82e7d7-ffa6-4ebf-a424-1bd783ec3457","update":{"sessionUpdate":"usage_update","used":32407,"size":1000000}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::UsageUpdate { used_tokens, context_window, cost, .. } = &params.update
        else {
            panic!("expected UsageUpdate variant");
        };
        assert_eq!(*used_tokens, Some(32407));
        assert_eq!(*context_window, Some(1_000_000));
        assert!(cost.is_none());
    }

    #[test]
    fn session_update_usage_update_claude_verbatim_final_carries_cost() {
        // `turn-claude.jsonl` line 8 -- the LAST `usage_update` of the
        // turn, right before the `session/prompt` response: same
        // `used`/`size`, now with `cost` attached.
        let raw = r#"{"sessionId":"2a82e7d7-ffa6-4ebf-a424-1bd783ec3457","update":{"sessionUpdate":"usage_update","used":32407,"size":1000000,"cost":{"amount":0.17858,"currency":"USD"},"_meta":{"_claude/origin":{"kind":"human"}}}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let SessionUpdate::UsageUpdate { used_tokens, context_window, cost, .. } = &params.update
        else {
            panic!("expected UsageUpdate variant");
        };
        assert_eq!(*used_tokens, Some(32407));
        assert_eq!(*context_window, Some(1_000_000));
        assert_eq!(cost.as_ref().and_then(|c| c.amount), Some(0.17858));
        assert_eq!(cost.as_ref().and_then(|c| c.currency.clone()), Some("USD".to_owned()));

        let events = update_to_event(&params);
        assert!(matches!(
            &events[0],
            AgentEvent::UsageUpdate {
                used_tokens: Some(32407),
                context_window: Some(1_000_000),
                cost_amount: Some(amount),
                cost_currency: Some(currency),
            } if (*amount - 0.17858).abs() < f64::EPSILON && currency == "USD"
        ));
    }

    #[test]
    fn session_update_usage_update_kimi_verbatim() {
        // `turn-kimi.jsonl` -- same `used`/`size` shape, no `cost`, no
        // `_meta` at all.
        let raw = r#"{"sessionId":"session_ff1eb29e-0ba7-40aa-b654-bbf327c39463","update":{"sessionUpdate":"usage_update","used":23382,"size":1048576}}"#;
        let params: SessionUpdateParams = serde_json::from_str(raw).unwrap();
        let events = update_to_event(&params);
        assert!(matches!(
            &events[0],
            AgentEvent::UsageUpdate {
                used_tokens: Some(23382),
                context_window: Some(1_048_576),
                cost_amount: None,
                cost_currency: None,
            }
        ));
    }

    #[test]
    fn session_update_config_option_update_parses_select_and_boolean() {
        // Per-option field names (`type`, `currentValue`, choice `name`)
        // match the live wire shape verified on `session/new`'s
        // `configOptions` (claude-agent-acp 0.71.0, codex-acp 1.8.0, Kimi
        // Code CLI 0.39.1 -- `acp-claude.jsonl`, `acp-codex.jsonl`,
        // `acp-kimi.jsonl`); the outer `config_option_update` notification
        // wrapper itself is not exercised by any live capture (none of
        // them runs a full prompt turn).
        let raw = r#"{"sessionId":"s1","update":{
            "sessionUpdate":"config_option_update",
            "configOptions":[
                {
                    "id":"model",
                    "name":"Model",
                    "description":"Which model to use",
                    "category":"generation",
                    "type":"select",
                    "currentValue":"opus",
                    "options":[
                        {"value":"opus","name":"Opus"},
                        {"value":"sonnet","name":"Sonnet"}
                    ]
                },
                {
                    "id":"extended-thinking",
                    "name":"Extended thinking",
                    "type":"boolean",
                    "currentValue":true
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
        let raw = r#"{"id":"x","name":"X","type":"not-a-real-kind","currentValue":null}"#;
        let option: SessionConfigOption = serde_json::from_str(raw).unwrap();
        assert_eq!(option.kind, ConfigOptionKind::Unknown);
    }

    #[test]
    fn session_config_option_reads_the_live_wire_key_names() {
        // Verbatim shape of one `configOptions` entry from `session/new`'s
        // result on claude-agent-acp 0.71.0 (`acp-claude.jsonl`) -- the
        // discriminator is `"type"`, not `"kind"`, and the current
        // selection is `"currentValue"`, not `"value"`. Before this fix
        // both fields silently defaulted (`kind` -> `Unknown`, `value` ->
        // `Value::Null`) on every real agent response.
        let raw = r#"{
            "id": "mode",
            "name": "Mode",
            "description": "Session permission mode",
            "category": "mode",
            "type": "select",
            "currentValue": "auto",
            "options": [
                {"value": "default", "name": "Manual", "description": "Always ask before making changes"},
                {"value": "auto", "name": "Auto", "description": "Claude handles permission decisions"}
            ]
        }"#;
        let option: SessionConfigOption = serde_json::from_str(raw).unwrap();
        assert_eq!(option.kind, ConfigOptionKind::Select);
        assert_eq!(option.value, Value::String("auto".to_owned()));
        assert_eq!(option.options.len(), 2);
        assert_eq!(option.options[1].label.as_deref(), Some("Auto"));
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
                {"id":"model","name":"Model","type":"select","currentValue":"opus"}
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
        assert_eq!(result.config_options[0].kind, ConfigOptionKind::Select);
        assert_eq!(result.config_options[0].value, Value::String("opus".to_owned()));
        assert!(result.models.available_models.is_empty());
    }

    #[test]
    fn session_load_result_defaults_when_agent_predates_modes_and_commands() {
        let raw = r#"{"sessionId":"s1"}"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert!(result.modes.current_mode_id.is_none());
        assert!(result.modes.available_modes.is_empty());
        assert!(result.available_commands.is_empty());
        assert!(result.config_options.is_empty());
        assert!(result.models.current_model_id.is_none());
        assert!(result.models.available_models.is_empty());
    }

    #[test]
    fn session_load_result_parses_the_live_codex_models_field() {
        // Trimmed to two entries from codex-acp 1.8.0's real `session/new`
        // result (`acp-codex.jsonl`) -- Codex's model catalog entries
        // carry no `_meta` at all.
        let raw = r#"{
            "sessionId":"01a05e3a-f474-7c53-9e8e-68b60a606d39",
            "models":{
                "availableModels":[
                    {"modelId":"gpt-5.6-sol[low]","name":"GPT-5.6-Sol (low)","description":"Fast responses with lighter reasoning"},
                    {"modelId":"gpt-5.4-mini[xhigh]","name":"GPT-5.4-Mini (xhigh)","description":"Extra high reasoning depth for complex problems"}
                ],
                "currentModelId":"gpt-5.6-sol[ultra]"
            }
        }"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.models.current_model_id.as_deref(), Some("gpt-5.6-sol[ultra]"));
        assert_eq!(result.models.available_models.len(), 2);
        assert_eq!(result.models.available_models[0].model_id, "gpt-5.6-sol[low]");
        assert!(result.models.available_models[0].meta.is_none());
    }

    #[test]
    fn session_load_result_parses_the_live_grok_models_field_with_reasoning_efforts() {
        // Verbatim from Grok CLI 1.0.13's real `session/new` result
        // (`acp-grok.jsonl`), trimmed to one model's `_meta` block.
        let raw = r#"{
            "sessionId":"01a05e41-1437-7231-9625-5fb30b8a02cd",
            "models":{
                "currentModelId":"grok-4.6",
                "availableModels":[
                    {
                        "modelId":"grok-4.6",
                        "name":"Grok 4.6",
                        "description":"SpaceXAI's latest frontier model",
                        "_meta":{
                            "totalContextTokens":500000,
                            "agentType":"grok-build-plan",
                            "supportsReasoningEffort":true,
                            "reasoningEffort":"high",
                            "reasoningEfforts":[
                                {"id":"xhigh","value":"xhigh","label":"Extra High Effort","description":"Highest effort and reasoning level","default":false},
                                {"id":"high","value":"high","label":"High Effort","description":"Higher implementation quality with extensive reasoning","default":true}
                            ]
                        }
                    }
                ]
            }
        }"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.models.current_model_id.as_deref(), Some("grok-4.6"));
        let model = &result.models.available_models[0];
        assert_eq!(model.model_id, "grok-4.6");
        let meta = model.meta.as_ref().expect("grok models carry _meta");
        assert_eq!(meta.total_context_tokens, Some(500_000));
        assert_eq!(meta.reasoning_efforts.len(), 2);
        assert_eq!(meta.reasoning_efforts[1].id, "high");
        assert!(meta.reasoning_efforts[1].default);
    }

    #[test]
    fn session_state_from_handshake_seeds_modes_commands_and_config_options() {
        let raw = r#"{
            "sessionId":"s1",
            "modes":{"currentModeId":"code","availableModes":[{"id":"code","name":"Code"}]},
            "availableCommands":[{"name":"review","description":"Review the diff"}],
            "configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"opus"}],
            "models":{"currentModelId":"grok-4.6","availableModels":[{"modelId":"grok-4.6","name":"Grok 4.6"}]}
        }"#;
        let result: SessionLoadResult = serde_json::from_str(raw).unwrap();
        let state = SessionState::from_handshake(&result);
        assert_eq!(state.modes.current_mode_id.as_deref(), Some("code"));
        assert_eq!(state.available_commands.len(), 1);
        assert_eq!(state.config_options.len(), 1);
        assert_eq!(state.models.current_model_id.as_deref(), Some("grok-4.6"));
        assert_eq!(state.models.available_models.len(), 1);
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
            extra: HashMap::new(),
        });
        let fresh = vec![AvailableCommand {
            name: "review".to_owned(),
            description: "Review the diff".to_owned(),
            input: None,
            extra: HashMap::new(),
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

    // -----------------------------------------------------------------------
    // sessionCapabilities — verified live against the initialize response
    // -----------------------------------------------------------------------

    #[test]
    fn session_capabilities_parses_the_live_claude_and_codex_shape() {
        // Verbatim `agentCapabilities.sessionCapabilities` from
        // claude-agent-acp 0.71.0 and codex-acp 1.8.0 (`acp-claude.jsonl`,
        // `acp-codex.jsonl`): all seven keys present, including `subagents`.
        let raw = r#"{
            "additionalDirectories": {},
            "close": {},
            "delete": {},
            "fork": {},
            "list": {},
            "resume": {},
            "subagents": {}
        }"#;
        let caps: SessionCapabilities = serde_json::from_str(raw).unwrap();
        assert!(caps.list.is_some());
        assert!(caps.resume.is_some());
        assert!(caps.close.is_some());
        assert!(caps.delete.is_some());
        assert!(caps.fork.is_some());
        assert!(caps.additional_directories.is_some());
        assert!(caps.subagents.is_some());
    }

    #[test]
    fn session_capabilities_parses_the_live_kimi_shape_without_subagents() {
        // Verbatim from Kimi Code CLI 0.39.1 (`acp-kimi.jsonl`): six keys,
        // no `subagents`.
        let raw = r#"{
            "list": {},
            "resume": {},
            "close": {},
            "delete": {},
            "fork": {},
            "additionalDirectories": {}
        }"#;
        let caps: SessionCapabilities = serde_json::from_str(raw).unwrap();
        assert!(caps.fork.is_some());
        assert!(caps.subagents.is_none());
    }

    #[test]
    fn session_capabilities_parses_the_live_grok_shape_with_only_three_keys() {
        // Verbatim from Grok CLI 1.0.13 (`acp-grok.jsonl`): only
        // `list`/`resume`/`close` -- no `delete`, `fork`,
        // `additionalDirectories`, or `subagents`.
        let raw = r#"{"list": {}, "resume": {}, "close": {}}"#;
        let caps: SessionCapabilities = serde_json::from_str(raw).unwrap();
        assert!(caps.list.is_some());
        assert!(caps.resume.is_some());
        assert!(caps.close.is_some());
        assert!(caps.delete.is_none());
        assert!(caps.fork.is_none());
        assert!(caps.additional_directories.is_none());
        assert!(caps.subagents.is_none());
    }

    #[test]
    fn agent_capability_flags_parses_session_capabilities_nested_under_the_live_key() {
        let raw = r#"{
            "loadSession": true,
            "sessionCapabilities": {"list": {}, "resume": {}, "close": {}, "subagents": {}}
        }"#;
        let flags: AgentCapabilityFlags = serde_json::from_str(raw).unwrap();
        assert!(flags.load_session);
        assert!(flags.session_capabilities.list.is_some());
        assert!(flags.session_capabilities.subagents.is_some());
    }

    // -----------------------------------------------------------------------
    // session/list, session/close, session/delete, session/fork — outbound,
    // verified by direct live invocation on claude-agent-acp 0.71.0 and
    // Grok CLI 1.0.13 (`session/delete` was NOT exercised and stays
    // UNVERIFIED, unchanged from the earlier guess).
    // -----------------------------------------------------------------------

    #[test]
    fn session_list_params_serialize_with_no_filter_matches_the_call_that_was_actually_made() {
        // Verified: an empty object is accepted by both providers.
        let s = serde_json::to_string(&SessionListParams::default()).unwrap();
        assert_eq!(s, "{}");
    }

    #[test]
    fn session_list_params_serialize_with_a_cwd_filter_matches_the_call_that_was_actually_made() {
        // Verified: Grok accepted `{"cwd": "<path>"}` and answered
        // identically to the filterless call.
        let params = SessionListParams { cwd: Some("/repo".to_owned()) };
        let s = serde_json::to_string(&params).unwrap();
        assert_eq!(s, r#"{"cwd":"/repo"}"#);
    }

    #[test]
    fn session_list_result_parses_the_live_claude_session_summary_shape() {
        // Claude's `session/list` entry: sessionId, cwd, title, updatedAt
        // -- no `_meta`. `updatedAt` uses milliseconds + `Z`.
        let raw = r#"{"sessions":[{
            "sessionId": "5932f0f1-b2c0-4651-8041-1e7d64be1d64",
            "cwd": "/repo",
            "title": "Fix the parser",
            "updatedAt": "2026-09-01T19:02:19.365Z"
        }]}"#;
        let result: SessionListResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.sessions.len(), 1);
        let entry = &result.sessions[0];
        assert_eq!(entry.session_id, "5932f0f1-b2c0-4651-8041-1e7d64be1d64");
        assert_eq!(entry.cwd.as_deref(), Some("/repo"));
        assert_eq!(entry.title.as_deref(), Some("Fix the parser"));
        assert_eq!(entry.updated_at.as_deref(), Some("2026-09-01T19:02:19.365Z"));
        assert!(entry.meta.is_none());
    }

    #[test]
    fn session_list_result_parses_the_live_grok_session_summary_shape_with_meta_facets() {
        // Grok's `session/list` entry: sessionId, cwd, updatedAt, _meta
        // (`x.ai/session`: kind + facets branch/cwd/gitRoot/repo) -- no
        // `title`. `updatedAt` uses six-digit microseconds + `+00:00`.
        // Sub-values of `kind`/`facets` were reported by key set, not
        // literal values, so this fixture uses illustrative content for
        // those; the key names and outer shape are as reported.
        let raw = r#"{"sessions":[{
            "sessionId": "01a05e41-1437-7231-9625-5fb30b8a02cd",
            "cwd": "C:\\repo\\gate4agent",
            "updatedAt": "2026-09-01T19:00:20.154340+00:00",
            "_meta": {
                "x.ai/session": {
                    "kind": "build",
                    "facets": {
                        "branch": "main",
                        "cwd": "C:\\repo\\gate4agent",
                        "gitRoot": "C:/repo/gate4agent",
                        "repo": "gate4agent"
                    }
                }
            }
        }]}"#;
        let result: SessionListResult = serde_json::from_str(raw).unwrap();
        let entry = &result.sessions[0];
        assert_eq!(entry.session_id, "01a05e41-1437-7231-9625-5fb30b8a02cd");
        assert_eq!(entry.updated_at.as_deref(), Some("2026-09-01T19:00:20.154340+00:00"));
        assert!(entry.title.is_none());
        let meta = entry.meta.as_ref().expect("grok entries carry _meta");
        assert_eq!(meta["x.ai/session"]["kind"], "build");
        assert_eq!(meta["x.ai/session"]["facets"]["repo"], "gate4agent");
    }

    #[test]
    fn session_close_params_serialize() {
        // Verified: identical `{"sessionId": "<id>"}` shape on both
        // claude-agent-acp 0.71.0 and Grok CLI 1.0.13.
        let params = SessionCloseParams { session_id: "s1".to_owned() };
        let s = serde_json::to_string(&params).unwrap();
        assert_eq!(s, r#"{"sessionId":"s1"}"#);
    }

    #[test]
    fn session_close_result_parses_the_live_claude_empty_response() {
        // Verbatim: Claude's `session/close` response is `{}`.
        let result: SessionCloseResult = serde_json::from_str("{}").unwrap();
        assert!(result.meta.is_none());
    }

    #[test]
    fn session_close_result_parses_the_live_grok_close_outcome_response() {
        // Verbatim: Grok's `session/close` response.
        let raw = r#"{"_meta": {"x.ai/closeOutcome": "closed"}}"#;
        let result: SessionCloseResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.meta.unwrap()["x.ai/closeOutcome"], "closed");
    }

    #[test]
    fn session_delete_params_serialize() {
        // UNVERIFIED -- not exercised by any live invocation, unchanged
        // from the earlier `{sessionId}`-request guess.
        let params = SessionDeleteParams { session_id: "s1".to_owned() };
        let s = serde_json::to_string(&params).unwrap();
        assert_eq!(s, r#"{"sessionId":"s1"}"#);
    }

    #[test]
    fn session_fork_params_serialize_carries_both_session_id_and_the_required_cwd() {
        // Verified: calling with only `sessionId` (this file's earlier
        // guess) was rejected live by claude-agent-acp 0.71.0 with:
        //   {"code": -32602, "message": "Invalid params",
        //    "data": {"_errors": [],
        //             "cwd": {"_errors": ["Invalid input: expected string, received undefined"]}}}
        // `cwd` is a required field on `SessionForkParams` -- the type
        // system itself now makes the earlier bug (a call missing `cwd`)
        // impossible to construct.
        let params = SessionForkParams { session_id: "s1".to_owned(), cwd: "/repo".to_owned() };
        let s = serde_json::to_string(&params).unwrap();
        assert_eq!(s, r#"{"sessionId":"s1","cwd":"/repo"}"#);
    }

    // -----------------------------------------------------------------------
    // Grok vendor `_x.ai/*` extensions — parsed from verbatim capture JSON
    // -----------------------------------------------------------------------

    #[test]
    fn parse_vendor_notification_models_update_updates_state_and_emits_event() {
        // Verbatim `_x.ai/models/update` params from Grok CLI 1.0.13
        // (`acp-grok.jsonl`), trimmed to one model.
        let raw = serde_json::from_str::<Value>(
            r#"{
                "currentModelId": "grok-4.6",
                "availableModels": [
                    {
                        "modelId": "grok-4.6",
                        "name": "Grok 4.6",
                        "description": "SpaceXAI's latest frontier model",
                        "_meta": {
                            "totalContextTokens": 500000,
                            "supportsReasoningEffort": true,
                            "reasoningEffort": "high",
                            "reasoningEfforts": [
                                {"id": "high", "value": "high", "label": "High Effort", "description": "Higher implementation quality with extensive reasoning", "default": true}
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/models/update", &raw)
            .expect("must parse the live models/update shape");
        assert_eq!(state.models.current_model_id.as_deref(), Some("grok-4.6"));
        assert_eq!(state.models.available_models.len(), 1);
        match event {
            AgentEvent::ModelsUpdate { current_model_id, available_models } => {
                assert_eq!(current_model_id.as_deref(), Some("grok-4.6"));
                assert_eq!(available_models.len(), 1);
                assert_eq!(available_models[0].model_id, "grok-4.6");
                assert_eq!(available_models[0].context_tokens, Some(500_000));
                assert_eq!(available_models[0].reasoning_efforts.len(), 1);
                assert_eq!(available_models[0].reasoning_efforts[0].id, "high");
            }
            other => panic!("expected AgentEvent::ModelsUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_vendor_notification_settings_update_pulls_permission_fields() {
        // Trimmed from Grok CLI 1.0.13's real `_x.ai/settings/update`
        // (`acp-grok.jsonl`) -- the live capture itself has both fields
        // `null`; exercised here with concrete values to prove the
        // extraction path, since `raw` still carries the full payload
        // either way.
        let raw = json!({
            "show_resolved_model": false,
            "permission_mode": "auto",
            "auto_permission_mode_enabled": true,
            "subscription_tier_display": "SuperGrok Plus"
        });
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/settings/update", &raw)
            .expect("must parse settings/update");
        match event {
            AgentEvent::SettingsUpdate { permission_mode, auto_permission_mode_enabled, raw: echoed } => {
                assert_eq!(permission_mode.as_deref(), Some("auto"));
                assert_eq!(auto_permission_mode_enabled, Some(true));
                assert_eq!(echoed, raw, "raw must carry the full payload verbatim");
            }
            other => panic!("expected AgentEvent::SettingsUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_vendor_notification_settings_update_tolerates_null_permission_fields() {
        // Verbatim from `acp-grok.jsonl`: both fields are actually `null`
        // on the wire in this capture.
        let raw = json!({"permission_mode": null, "auto_permission_mode_enabled": null});
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/settings/update", &raw).unwrap();
        assert!(matches!(
            event,
            AgentEvent::SettingsUpdate { permission_mode: None, auto_permission_mode_enabled: None, .. }
        ));
    }

    #[test]
    fn parse_vendor_notification_session_notification_hook_execution() {
        // Verbatim `_x.ai/session_notification` params from Grok CLI
        // 1.0.13 (`acp-grok.jsonl`), including the mangled-encoding error
        // text from a failed hook exactly as captured.
        let raw = serde_json::from_str::<Value>(
            r#"{
                "sessionId": "01a05e41-1437-7231-9625-5fb30b8a02cd",
                "update": {
                    "sessionUpdate": "hook_execution",
                    "event_name": "session_start",
                    "runs": [
                        {"name": "global/gate4agent-status:session_start[0].hooks[0]", "status": {"status": "success", "elapsed_ms": 539}},
                        {"name": "global/settings:session_start[0].hooks[0]", "status": {"status": "failed", "error": "exit code 1", "elapsed_ms": 701}},
                        {"name": "global/settings:session_start[1].hooks[0]", "status": {"status": "success", "elapsed_ms": 548}}
                    ]
                }
            }"#,
        )
        .unwrap();
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/session_notification", &raw)
            .expect("must parse hook_execution");
        match event {
            AgentEvent::HookExecutionUpdate { event_name, runs } => {
                assert_eq!(event_name, "session_start");
                assert_eq!(runs.len(), 3);
                assert_eq!(runs[0].status, "success");
                assert_eq!(runs[0].elapsed_ms, Some(539));
                assert_eq!(runs[1].status, "failed");
                assert_eq!(runs[1].error.as_deref(), Some("exit code 1"));
            }
            other => panic!("expected AgentEvent::HookExecutionUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_vendor_notification_session_notification_model_changed() {
        // Verbatim from Grok CLI 1.0.13 (`acp-grok.jsonl`).
        let raw = json!({
            "sessionId": "01a05e41-1437-7231-9625-5fb30b8a02cd",
            "update": {"sessionUpdate": "model_changed", "model_id": "grok-4.6", "reasoning_effort": "high"}
        });
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/session_notification", &raw)
            .expect("must parse model_changed");
        match event {
            AgentEvent::ProviderModelChanged { model_id, reasoning_effort } => {
                assert_eq!(model_id, "grok-4.6");
                assert_eq!(reasoning_effort.as_deref(), Some("high"));
            }
            other => panic!("expected AgentEvent::ProviderModelChanged, got {other:?}"),
        }
    }

    #[test]
    fn parse_vendor_notification_mcp_servers_updated() {
        // Verbatim from Grok CLI 1.0.13 (`acp-grok.jsonl`).
        let raw = json!({
            "mcpServers": [
                {"name": "Puppeteer", "source": "local", "type": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-puppeteer"]}
            ]
        });
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/mcp/servers_updated", &raw)
            .expect("must parse mcp/servers_updated");
        match event {
            AgentEvent::McpServersUpdate { servers } => {
                assert_eq!(servers.len(), 1);
                assert_eq!(servers[0].name, "Puppeteer");
                assert_eq!(servers[0].transport, "stdio");
            }
            other => panic!("expected AgentEvent::McpServersUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_vendor_notification_mcp_init_progress() {
        // Verbatim from Grok CLI 1.0.13 (`acp-grok.jsonl`).
        let raw = json!({"total": 1, "connected": 0, "sessionId": "01a05e41-1437-7231-9625-5fb30b8a02cd"});
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/mcp/init_progress", &raw).unwrap();
        assert!(matches!(event, AgentEvent::McpInitProgress { total: 1, connected: 0 }));
    }

    #[test]
    fn parse_vendor_notification_mcp_initialized() {
        // Verbatim from Grok CLI 1.0.13 (`acp-grok.jsonl`).
        let raw = json!({
            "sessionId": "01a05e41-1437-7231-9625-5fb30b8a02cd",
            "mcpToolCount": 7,
            "elapsedMs": 3887
        });
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/mcp_initialized", &raw).unwrap();
        assert!(matches!(
            event,
            AgentEvent::McpInitialized { tool_count: 7, elapsed_ms: 3887 }
        ));
    }

    #[test]
    fn parse_vendor_notification_announcements_update() {
        // Verbatim from Grok CLI 1.0.13 (`acp-grok.jsonl`).
        let raw = json!({
            "gen": 1788287666i64,
            "announcements": [
                {"id": "team", "message": "Select 'Grok 4.6' under /model.", "severity": "info", "title": "Grok 4.6 is here!", "cta": null, "updated_at": null, "expires_at": null, "dismissible": null, "persistent": null}
            ]
        });
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/announcements/update", &raw)
            .expect("must parse announcements/update");
        match event {
            AgentEvent::AnnouncementsUpdate { announcements } => {
                assert_eq!(announcements.len(), 1);
                assert_eq!(announcements[0].id, "team");
                assert_eq!(announcements[0].title.as_deref(), Some("Grok 4.6 is here!"));
                assert_eq!(announcements[0].severity.as_deref(), Some("info"));
            }
            other => panic!("expected AgentEvent::AnnouncementsUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_vendor_notification_unknown_x_ai_method_returns_none() {
        // An `_x.ai/*` method this build has never seen must not break
        // parsing -- the reader loop falls back to the generic
        // `RpcNotification` passthrough for it.
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/some_future_method", &json!({}));
        assert!(event.is_none());
    }

    #[test]
    fn parse_vendor_notification_unrelated_namespace_returns_none() {
        // A notification method entirely outside `_x.ai/` -- e.g. another
        // vendor's own future extension -- must likewise fall through.
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_some.other.vendor/thing", &json!({}));
        assert!(event.is_none());
    }

    #[test]
    fn parse_vendor_notification_malformed_x_ai_payload_returns_none() {
        // A recognized `_x.ai/*` method with a payload that fails to
        // parse against its known shape must fall through to the generic
        // passthrough rather than panicking or fabricating an event.
        let mut state = SessionState::default();
        let event = parse_vendor_notification(&mut state, "_x.ai/mcp_initialized", &json!("not an object"));
        assert!(event.is_none());
    }

    // -----------------------------------------------------------------------
    // available_commands_update — `_meta` sidecar data is preserved
    // -----------------------------------------------------------------------

    #[test]
    fn available_command_keeps_meta_sidecar_data_in_extra() {
        // Trimmed from codex-acp 1.8.0's real `available_commands_update`
        // (`acp-codex.jsonl`) -- the `/plan` command's `_meta.
        // commandAction` wiring.
        let raw = r#"{
            "name": "plan",
            "description": "Turn plan mode on.",
            "input": null,
            "_meta": {"commandAction": {"kind": "setConfigOption", "configId": "collaboration_mode", "value": "plan"}}
        }"#;
        let command: AvailableCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(command.name, "plan");
        assert!(command.input.is_none());
        assert!(command.extra.contains_key("_meta"));
    }
}
