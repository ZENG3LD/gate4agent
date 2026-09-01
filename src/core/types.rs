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
    ///
    /// `granted` is the host's decision on the request, read off the same
    /// `Result<Value, RpcError>` the reader loop already computed by calling
    /// the handler -- `Err(_)` (e.g. `fs/read_text_file`'s or `terminal/
    /// create`'s `PERMISSION_DENIED` refusal) means denied; `Ok(value)`
    /// means granted, UNLESS `method` is `session/request_permission`, in
    /// which case granted is recovered by cross-referencing the selected
    /// `optionId` in `value`'s `outcome` against the original request's
    /// `options` list (a `Cancelled` outcome, or a `selected` outcome that
    /// picked a `reject_once`/`reject_always` option, is not a grant even
    /// though the RPC call itself succeeded -- ACP models a decline as a
    /// normal response, not an RPC error). This does not change what the
    /// host does -- it only lets a subscriber see the request and the
    /// decision the host already made.
    RpcIncomingRequest {
        id: crate::rpc::message::RpcId,
        method: String,
        params: Option<serde_json::Value>,
        granted: bool,
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
}
