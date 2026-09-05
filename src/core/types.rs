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
    #[serde(alias = "kimi")]
    KimiCode,
    /// xAI Grok CLI — ACP transport (`grok agent stdio`).
    #[serde(alias = "grok")]
    Grok,
}

impl std::fmt::Display for CliTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliTool::ClaudeCode => write!(f, "Claude Code"),
            CliTool::Codex => write!(f, "Codex"),
            CliTool::KimiCode => write!(f, "Kimi Code"),
            CliTool::Grok => write!(f, "Grok"),
        }
    }
}

impl CliTool {
    /// Returns the default (compile-time) capability descriptor for this CLI tool.
    ///
    /// Returns an owned [`CliCapabilities`] populated with hardcoded defaults.
    /// Use [`discover_capabilities`](Self::discover_capabilities) if you want
    /// the descriptor enriched by on-disk config files.
    ///
    /// Consumers use this to populate model pickers, permission selectors, and
    /// feature-flag-gated UI panels.
    ///
    /// # Example
    /// ```rust
    /// use gate4agent::CliTool;
    /// let caps = CliTool::ClaudeCode.capabilities();
    /// let default_model = caps.default_model().map(|m| m.id.clone());
    /// let models: Vec<String> = caps.available_models.iter().map(|m| m.id.clone()).collect();
    /// ```
    pub fn capabilities(&self) -> crate::core::capabilities::CliCapabilities {
        use crate::core::capabilities::{
            claude_capabilities, codex_capabilities, grok_capabilities, kimi_capabilities,
        };
        match self {
            CliTool::ClaudeCode => claude_capabilities(),
            CliTool::Codex => codex_capabilities(),
            CliTool::KimiCode => kimi_capabilities(),
            CliTool::Grok => grok_capabilities(),
        }
    }

    /// Returns capability metadata enriched by reading on-disk CLI config files.
    ///
    /// Starts from the compiled-in defaults (same as [`capabilities()`](Self::capabilities)),
    /// then overlays any model configured in tool-specific config files:
    ///
    /// - **Codex**: reads `~/.codex/config.toml` → `model = "…"`
    /// - **Claude / Kimi / Grok**: returns defaults unchanged (no config-based model info).
    ///
    /// Falls back gracefully to defaults if any config file is absent or unreadable.
    /// Performs only synchronous filesystem I/O; safe to call from any thread.
    pub fn discover_capabilities(&self) -> crate::core::capabilities::CliCapabilities {
        crate::core::capabilities::discover(*self)
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
    /// When the limit resets, resolved to an absolute UTC instant.
    ///
    /// Providers print this in the LOCAL wall-clock time of the machine
    /// running the CLI, never with an explicit zone or offset, so this is
    /// filled by resolving that local time against `chrono::Local` (this
    /// process's own OS zone — correct only because the harness node and
    /// the CLI it drives run on the same machine) to the NEAREST FUTURE
    /// occurrence: a bare `HH:MM` that has already passed today resolves
    /// to tomorrow; a `HH:MM on D Mon` whose date has already passed this
    /// year resolves to next year. `None` when no reset time was printed,
    /// or when the local wall-clock time the provider printed does not
    /// exist on this host (a spring-forward DST gap) — `resets_at_text`
    /// carries the verbatim text in every case, including this one.
    pub resets_at: Option<DateTime<Utc>>,
    /// Verbatim reset text as printed by the provider, e.g. `"23:40"` or
    /// `"18:40 on 6 Sep"`, independent of whether `resets_at` above could
    /// be resolved. Populated only by parsers that locate this exact
    /// substring (codex's `/status` quota-state line); `None` for the
    /// generic refusal-message detectors, which have no such fragment to
    /// extract.
    pub resets_at_text: Option<String>,
    /// Percentage of quota CONSUMED (0-100), i.e. usage — not remaining.
    ///
    /// Providers that print remaining quota instead (codex's `/status`:
    /// `"N% left"`) are converted here as `100.0 - N`; this field never
    /// silently holds a raw "percent left" value under the "usage" name.
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

/// Exact current context-window usage reported atomically by a structured provider.
///
/// The five segment fields are normalized and must sum exactly to `used_tokens`.
/// `used_tokens` may exceed `capacity_tokens`; callers must not clamp provider facts.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextWindowUsage {
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub unattributed_tokens: u64,
    pub used_tokens: u64,
    pub capacity_tokens: u64,
}

/// Priority of a single [`PlanStep`] (ACP transport's `plan` update).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStepPriority {
    High,
    Medium,
    Low,
}

/// Status of a single [`PlanStep`] (ACP transport's `plan` update).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// One step of an agent's execution plan (ACP transport only). Always
/// delivered as a full snapshot replacing any previously emitted plan,
/// never a delta -- see `AgentEvent::Plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub content: String,
    pub priority: PlanStepPriority,
    pub status: PlanStepStatus,
}

/// A single slash-style command the agent advertises (ACP transport
/// only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommandInfo {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

/// The kind of a [`ConfigOptionInfo`] -- `select` (choose one of
/// `choices`) or `boolean` (toggle `value`). `Unknown` is the fallback for
/// a kind string this build does not recognize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigOptionKind {
    Select,
    Boolean,
    Unknown,
}

/// One selectable value of a `select`-kind [`ConfigOptionInfo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigOptionChoiceInfo {
    pub value: serde_json::Value,
    pub label: Option<String>,
}

/// One session configuration setting -- the mechanism ACP uses to change
/// model, reasoning effort, and similar settings, superseding session
/// modes (ACP transport only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigOptionInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub kind: ConfigOptionKind,
    pub value: serde_json::Value,
    pub choices: Vec<ConfigOptionChoiceInfo>,
}

/// One reasoning-effort level offered for an [`AvailableModelInfo`] (ACP
/// transport only) -- carried on Grok's per-model `_meta.
/// reasoningEfforts` array, both in `session/new`'s `models` field and in
/// the vendor `_x.ai/models/update` notification (identical shape in
/// both, verified live on Grok CLI 1.0.13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningEffortInfo {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
}

/// One model the agent can select for a session (ACP transport only), as
/// carried in `session/new`'s `models` field (verified live on codex-acp
/// 1.8.0 and Grok CLI 1.0.13) and Grok's vendor `_x.ai/models/update`
/// notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableModelInfo {
    pub model_id: String,
    pub name: String,
    pub description: Option<String>,
    /// Total context window, when the agent reports one per model
    /// (Grok's `_meta.totalContextTokens`; Codex's model catalog entries
    /// carry no `_meta` at all).
    pub context_tokens: Option<u64>,
    pub reasoning_efforts: Vec<ReasoningEffortInfo>,
}

/// Result of one hook script the provider ran for a session lifecycle
/// event (ACP transport only) -- Grok's vendor `_x.ai/session_
/// notification` with `sessionUpdate: "hook_execution"`, verified live on
/// Grok CLI 1.0.13.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookRunResult {
    pub name: String,
    pub status: String,
    pub elapsed_ms: Option<u64>,
    pub error: Option<String>,
}

/// One MCP server the provider has configured (ACP transport only) --
/// Grok's vendor `_x.ai/mcp/servers_updated` notification, verified live
/// on Grok CLI 1.0.13.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerSummary {
    pub name: String,
    pub source: String,
    pub transport: String,
}

/// One announcement banner the provider is showing (ACP transport only)
/// -- Grok's vendor `_x.ai/announcements/update` notification, verified
/// live on Grok CLI 1.0.13.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnouncementInfo {
    pub id: String,
    pub title: Option<String>,
    pub message: String,
    pub severity: Option<String>,
}

/// WHO decided a host request from the agent -- see
/// [`AgentEvent::RpcIncomingRequest`] and [`HostRequestDecision`].
/// `Operator` and `DeadlinePolicy` only ever apply to a `session/
/// request_permission` call that was first left
/// [`HostRequestDecision::Deferred`] and answered later, out of band, by
/// `AcpSession::resolve_pending_request` or `AcpSession::expire_deadlines`
/// (`acp/session.rs`) -- `Gate` and `Policy` both decide immediately, on
/// the reader loop's own thread, and never see a `Deferred` request at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDecisionAuthority {
    /// The dangerous-command gate (`acp::gate`) forced this outcome ahead
    /// of `HostPolicy` -- for `terminal/create` and `execute`-kind
    /// `session/request_permission` calls only, and only when the gate is
    /// `Enforced` (`acp::gate::DangerousCommandGate`). The gate outranks
    /// every `HostPolicy` value including `Yolo`, so a gate-forced decision
    /// is always a denial.
    Gate,
    /// `HostPolicy` (`Yolo`/`Auto`/`ReadOnly`/`Deny`) decided the request
    /// the instant it arrived -- today's default path for every request
    /// that is neither gate-blocked nor deferred.
    Policy,
    /// An operator answered a `session/request_permission` call that had
    /// been left `Deferred`, through `AcpSession::resolve_pending_request`.
    Operator,
    /// A `session/request_permission` call that had been left `Deferred`
    /// reached its deadline with no operator answer, so `HostPolicy` --
    /// the SAME policy that would have answered it immediately had
    /// deferral never been enabled, see `AcpSession::expire_deadlines` --
    /// decided it instead.
    ///
    /// Deliberately its own variant rather than `Policy`: folding this into
    /// `Policy` would make it indistinguishable from a request `HostPolicy`
    /// answered on arrival, erasing the fact that an operator was asked
    /// first and nobody answered in time -- two different operational
    /// stories that happen to end in the same `HostPolicy` call. Equally
    /// deliberately not `Operator`: no human made this choice.
    DeadlinePolicy,
}

/// A typed answer to "what happened to this host request" -- replaces a
/// plain `bool` that had no honest value for "arrived, not yet decided",
/// see [`AgentEvent::RpcIncomingRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRequestDecision {
    /// The request was allowed. `by` is who made that call.
    Granted { by: HostDecisionAuthority },
    /// The request was refused. `by` is who made that call.
    Denied { by: HostDecisionAuthority },
    /// The request has arrived and been recorded, but nothing has decided
    /// it yet -- only reachable for a `session/request_permission` call on
    /// a session with deferral enabled (see `acp::session::
    /// AcpSessionOptions::defer_permission_requests`), and only when the
    /// dangerous-command gate did not already force an immediate `Denied
    /// { by: Gate }`. A later `AgentEvent::RpcIncomingRequest` for the SAME
    /// `id`, carrying `Granted` or `Denied`, reports the eventual outcome
    /// once one exists.
    Deferred,
}

/// Whether a host request's underlying operation actually ran without an
/// I/O or execution problem -- orthogonal to [`HostRequestDecision`], which
/// answers "was this allowed", not "did doing it succeed". Introduced
/// because an `Err` a method like `terminal/create` or `fs/read_text_file`
/// returns AFTER the gate/policy already authorized it (a spawn failure, a
/// missing file) is not a refusal -- collapsing it into `HostRequestDecision
/// ::Denied` mints a policy block that never happened (see
/// `acp::host::HostCallOutcome`, where the two are first told apart, and
/// `acp::reader`'s dispatch loop, which reads this off the SAME call that
/// decided [`HostRequestDecision::Granted`], never a second one).
///
/// Only meaningful paired with `Granted`: a `Denied` or `Deferred` request
/// never attempted its underlying operation, so it is always `Executed`
/// here for lack of anything to have failed running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRequestOutcome {
    /// No execution problem -- either the request ran cleanly, or (for
    /// `Denied`/`Deferred`) no execution was ever attempted to fail.
    Executed,
    /// The request was authorized but failed while running it. `error` is
    /// the bounded underlying I/O/execution failure text (e.g. an OS error
    /// from a spawn call) -- never a policy/gate refusal message, which
    /// stays on `AgentEvent::RpcIncomingRequest`'s own `reason` field
    /// instead, exactly as it did before this type existed.
    Failed { error: String },
}

/// ACP's `stopReason` vocabulary (`agentclientprotocol.com/protocol/
/// prompt-turn`): `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`,
/// `cancelled`. An unrecognized wire string is kept verbatim as `Other`,
/// never dropped -- a future ACP revision or a vendor extension stays
/// visible to a consumer that only wants the five canonical values.
///
/// `ProviderError` is NOT part of ACP's own vocabulary -- it is synthesized
/// locally (`acp::session`) for the one case ACP has no `stopReason` for at
/// all: the `session/prompt` call itself answered with a JSON-RPC error
/// instead of a normal response (e.g. codex-acp's `-32603` "usage limit
/// exceeded", measured live 2026-09-05, `data: {"message": "...",
/// "codexErrorInfo": "usageLimitExceeded"}}`). `message` is the error text
/// verbatim -- the RPC error's own `data.message` when the error carries
/// one (Codex's shape: `error.message` itself is only the generic
/// "Internal error", the real text rides on `data.message`), else the bare
/// `error.message`. `vendor_code` is the provider's own machine code for
/// the error when `data` names one (Codex's `data.codexErrorInfo`), or any
/// OTHER string-valued field found directly on `data` when `codexErrorInfo`
/// itself is absent -- never guessed beyond what the wire actually named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
    ProviderError {
        code: i32,
        message: String,
        vendor_code: Option<String>,
    },
}

impl StopReason {
    /// Parse ACP's raw `stopReason` wire string. Never fails: an
    /// unrecognized value becomes `Other`, exactly as a future ACP revision
    /// or a vendor's own extension should be treated -- visible, never
    /// rejected.
    pub fn from_wire_str(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "max_tokens" => Self::MaxTokens,
            "max_turn_requests" => Self::MaxTurnRequests,
            "refusal" => Self::Refusal,
            "cancelled" => Self::Cancelled,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The wire string this value round-trips to -- `Other`'s own inner
    /// string for an unrecognized value, and a fixed descriptive slug for
    /// `ProviderError`, which never arrived as a `stopReason` string in the
    /// first place (see this type's own doc comment).
    pub fn as_wire_str(&self) -> &str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::MaxTurnRequests => "max_turn_requests",
            Self::Refusal => "refusal",
            Self::Cancelled => "cancelled",
            Self::Other(s) => s,
            Self::ProviderError { .. } => "provider_error",
        }
    }

    /// Whether this stop reason is itself a block an operator needs to see
    /// -- true only for ACP's own `refusal`. `ProviderError` is NOT
    /// unconditionally a block here (a quota/limit exhaustion is, a random
    /// transient RPC error is not) -- that finer classification belongs to
    /// the mint site that reads `vendor_code`/`message`, not this type.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refusal)
    }
}

impl Serialize for StopReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::ProviderError { code, message, vendor_code } => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("StopReason", 4)?;
                state.serialize_field("kind", "provider_error")?;
                state.serialize_field("code", code)?;
                state.serialize_field("message", message)?;
                state.serialize_field("vendor_code", vendor_code)?;
                state.end()
            }
            other => serializer.serialize_str(other.as_wire_str()),
        }
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Only the plain wire-string shape round-trips here -- this build
        // never receives a `ProviderError` back off the wire (it is
        // synthesized locally, never parsed), so a bare string is the only
        // inbound shape that needs to deserialize.
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_wire_str(&s))
    }
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
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
        duration_ms: Option<u64>,
        /// ACP `tool_call_update._meta.nonExecutionKind` -- Claude's own
        /// vocabulary for WHY the tool never actually ran:
        /// `"user-rejected"`, `"permission-rule"`, `"interrupted"`,
        /// `"cancelled"`. `None` for a provider that sends no such field
        /// (every provider except Claude, and Claude itself for a call
        /// that genuinely ran and either succeeded or failed for real).
        /// Never guessed from `output` text -- read off the typed `_meta`
        /// field only, or absent.
        non_execution_kind: Option<String>,
    },
    /// Assistant thinking/reasoning block.
    Thinking { text: String },
    /// Turn complete with token usage.
    TurnComplete {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        reasoning_tokens: u64,
        context_window: Option<u64>,
        is_cumulative: bool,
    },
    /// Exact current context-window usage from a structured provider event.
    ContextWindowUsage { usage: ContextWindowUsage },
    /// Session ended with final result.
    SessionEnd {
        result: String,
        cost_usd: Option<f64>,
        is_error: bool,
        /// The typed reason the turn stopped, when the transport is ACP
        /// and one exists -- `Some(StopReason::Refusal)` and
        /// `Some(StopReason::ProviderError { .. })` are both `is_error:
        /// true` (see [`StopReason`]'s own doc comment). `None` for a
        /// non-ACP transport (pipe, one-shot), which has no such concept
        /// on the wire at all, and for the ACP process-exit path, which
        /// has no `stopReason` to report either.
        stop_reason: Option<StopReason>,
    },
    /// The current turn ended abnormally, without a matching `TurnComplete`/
    /// `SessionEnd` -- ACP transport only, synthesized locally by
    /// `acp::session::AcpSession::start_prompt` (never sent by an agent on
    /// the wire) for the three ways a `session/prompt` call can fail to
    /// produce one: the agent answered with a JSON-RPC error (e.g. codex-acp
    /// 1.10.0's `-32603 usageLimitExceeded` when the account's quota is
    /// exhausted, measured live), the pending call was cancelled because the
    /// session closed mid-turn, or `AcpSessionOptions::prompt_timeout`
    /// elapsed with no response at all. `reason` is the bounded, human-
    /// readable text explaining which of those three happened.
    ///
    /// This is a DISTINCT signal from `Error`, not a replacement for it:
    /// `Error` still carries the same failure text for anything that only
    /// wants to display it (e.g. `manager.rs`'s chat transcript). This event
    /// exists because `Error` alone left the turn's own state stuck --
    /// downstream (`gate4agent-shell-native`'s `provider_event`,
    /// `gate4agent-engine`'s snapshot reducer) had no event here that reset
    /// `ProviderActivity` away from `Blocked`, so `gate4agent-node`'s
    /// turn-in-flight precondition
    /// (`NodeShared::require_session_runtime_policy`) refused every
    /// subsequent `session/prompt` as `TurnInFlight` forever, even though no
    /// turn was actually still running -- measured live against codex-acp
    /// 1.10.0 on 2026-09-05: a `session/prompt` RPC error landed within
    /// seconds, but the session then refused every further prompt for the
    /// rest of its life.
    TurnInterrupted { reason: String },

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
    /// and sent the response -- or, for a `session/request_permission` call
    /// left [`HostRequestDecision::Deferred`], recorded it and sent nothing
    /// yet. This variant lets subscribers audit what the agent requested
    /// without needing their own handler.
    ///
    /// `decision` is the host's answer: for `terminal/create`,
    /// `fs/read_text_file`, `fs/write_text_file`, and the other `terminal/*`
    /// methods, `acp::host::AcpHostAdapter::dispatch` tells apart a gate/
    /// policy refusal decided BEFORE any operation ran (`Denied`) from an
    /// operation that WAS authorized (`Granted`, regardless of whether it
    /// then succeeded or failed -- see `outcome` below for that). For
    /// `session/request_permission`, there is no execution step to fail --
    /// `Granted`/`Denied` is recovered by cross-referencing the selected
    /// `optionId` in the outcome against the original request's `options`
    /// list (a `Cancelled` outcome, or a `selected` outcome that picked a
    /// `reject_once`/`reject_always` option, is `Denied` even though the RPC
    /// call itself succeeded -- ACP models a decline as a normal response,
    /// not an RPC error). A `session/request_permission` call left
    /// `Deferred` is broadcast twice under the SAME `id`: once here as
    /// `Deferred` when it arrives, then again as `Granted`/`Denied` once
    /// `AcpSession::resolve_pending_request` or `AcpSession::
    /// expire_deadlines` decides it. This does not change what the host
    /// does -- it only lets a subscriber see the request and the decision
    /// the host already made, or that none exists yet.
    RpcIncomingRequest {
        id: crate::rpc::message::RpcId,
        method: String,
        params: Option<serde_json::Value>,
        decision: HostRequestDecision,
        /// Whether an authorized (`Granted`) request actually ran cleanly
        /// or failed doing so -- see [`HostRequestOutcome`]'s own doc
        /// comment for why this is a separate field from `decision` rather
        /// than a third flavor of `Denied`. Always `Executed` for `Denied`/
        /// `Deferred` (nothing ran to fail).
        outcome: HostRequestOutcome,
        /// The refusal text behind a `Denied` decision, when this build
        /// actually computed one -- for `fs/read_text_file`,
        /// `fs/write_text_file`, `terminal/create`, `terminal/output`,
        /// `terminal/wait_for_exit`, `terminal/kill`, `terminal/release`,
        /// read off `acp::host::HostCallOutcome::Denied`'s own message
        /// (the gate's or `HostPolicy`'s refusal text, computed BEFORE any
        /// operation ran); for `session/request_permission`, read off
        /// `AcpHostAdapter::permission_refusal_reason` instead, the one
        /// method that answers a decline with an `Ok` outcome (see that
        /// method's own doc comment). `None` for every `Granted`/`Deferred`
        /// decision -- an execution failure on a `Granted` request explains
        /// itself through `outcome` above, never through this field, so a
        /// `Denied` decision's refusal text and a `Granted` request's I/O
        /// failure text can never be confused for one another by a
        /// consumer that only reads `reason`.
        reason: Option<String>,
    },

    // --- ACP session/update: structured session state (ACP transport only) ---
    /// Echo of a user message, replayed when resuming a loaded session
    /// (`session/update`'s `user_message_chunk`).
    UserMessage { text: String, is_delta: bool },
    /// The agent's full execution plan, replacing any plan emitted before
    /// it (`session/update`'s `plan`).
    Plan { steps: Vec<PlanStep> },
    /// The agent's slash-command catalog changed
    /// (`available_commands_update`).
    AvailableCommandsUpdate { commands: Vec<AvailableCommandInfo> },
    /// The session's active mode changed (`current_mode_update`).
    ModeChanged { mode_id: String },
    /// Session metadata changed; only the fields that actually changed
    /// are populated (`session_info_update`).
    SessionInfoUpdate { title: Option<String> },
    /// Context-window consumption and, when reported, turn cost
    /// (`usage_update`).
    UsageUpdate {
        used_tokens: Option<u64>,
        context_window: Option<u64>,
        cost_amount: Option<f64>,
        cost_currency: Option<String>,
    },
    /// The full current set of session configuration options
    /// (`config_option_update`).
    ConfigOptionsUpdate { options: Vec<ConfigOptionInfo> },

    // --- Grok vendor `_x.ai/*` extensions (ACP transport only) ---
    /// The agent's available model catalog and/or current selection
    /// changed (Grok's vendor `_x.ai/models/update` notification).
    /// `session/new`'s own `models` field seeds the same data without
    /// emitting this event -- see `AcpSession::available_models`.
    ModelsUpdate {
        current_model_id: Option<String>,
        available_models: Vec<AvailableModelInfo>,
    },
    /// The provider switched models mid-session (Grok's vendor
    /// `_x.ai/session_notification` with `sessionUpdate:
    /// "model_changed"`).
    ProviderModelChanged {
        model_id: String,
        reasoning_effort: Option<String>,
    },
    /// A provider-specific session setting changed (Grok's vendor
    /// `_x.ai/settings/update` notification). Only `permission_mode` and
    /// `auto_permission_mode_enabled` are pulled out by name -- the two
    /// that affect host-visible behavior; `raw` carries the full payload
    /// verbatim so nothing else in it is silently dropped.
    SettingsUpdate {
        permission_mode: Option<String>,
        auto_permission_mode_enabled: Option<bool>,
        raw: serde_json::Value,
    },
    /// Hook scripts ran for a session lifecycle event, with per-hook
    /// success/failure (Grok's vendor `_x.ai/session_notification` with
    /// `sessionUpdate: "hook_execution"`).
    HookExecutionUpdate {
        event_name: String,
        runs: Vec<HookRunResult>,
    },
    /// The provider's configured MCP server list changed (Grok's vendor
    /// `_x.ai/mcp/servers_updated` notification).
    McpServersUpdate { servers: Vec<McpServerSummary> },
    /// MCP server connection progress during startup (Grok's vendor
    /// `_x.ai/mcp/init_progress` notification).
    McpInitProgress { total: u32, connected: u32 },
    /// MCP server initialization finished (Grok's vendor
    /// `_x.ai/mcp_initialized` notification).
    McpInitialized { tool_count: u32, elapsed_ms: u64 },
    /// The provider's announcement banners changed (Grok's vendor
    /// `_x.ai/announcements/update` notification).
    AnnouncementsUpdate { announcements: Vec<AnnouncementInfo> },
}
