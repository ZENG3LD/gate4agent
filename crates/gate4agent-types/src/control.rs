use crate::{
    AdapterBinding, AdapterFamily, AgentId, InputAction, InputPrepareError, PreparedInput,
    PreparedInputKind,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONTROL_PROTOCOL_VERSION: u16 = 11;
pub const TERMINAL_ROWS_MAX: u16 = 1_000;
pub const TERMINAL_COLUMNS_MAX: u16 = 1_000;
pub const WORKING_DIRECTORY_MAX_BYTES: usize = 32_768;
pub const PROVIDER_INGRESS_EVENTS_MAX: usize = 32;
pub const PROVIDER_EVENT_TEXT_MAX_BYTES: usize = 262_144;
pub const PROVIDER_EVENT_ID_MAX_BYTES: usize = 512;
pub const PROVIDER_EVENT_TOOLS_MAX: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentInstanceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub u64);

#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SessionGeneration(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartRequest {
    pub working_directory: String,
    pub terminal_size: TerminalSize,
    #[serde(default)]
    pub initial_prompt: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TerminalFrame {
    pub sequence: u64,
    pub size: TerminalSize,
    pub cursor_row: u16,
    pub cursor_column: u16,
    pub contents: String,
    pub formatted: Vec<u8>,
}

pub const FOREGROUND_PROCESS_NAME_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ForegroundProcessKind {
    Agent { agent_id: AgentId },
    Shell,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForegroundProcess {
    pub root_process_id: u32,
    pub process_id: u32,
    pub process_name: String,
    pub kind: ForegroundProcessKind,
}

impl ForegroundProcess {
    pub fn is_valid_for(&self, session_agent_id: &AgentId) -> bool {
        self.root_process_id > 0
            && self.process_id > 0
            && !self.process_name.trim().is_empty()
            && self.process_name.len() <= FOREGROUND_PROCESS_NAME_MAX_BYTES
            && !self.process_name.chars().any(char::is_control)
            && match &self.kind {
                ForegroundProcessKind::Agent { agent_id } => agent_id == session_agent_id,
                ForegroundProcessKind::Shell | ForegroundProcessKind::Other => true,
            }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForegroundAuthority {
    #[default]
    Unknown,
    Confirmed,
    Stale,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForegroundSnapshot {
    pub authority: ForegroundAuthority,
    pub process: Option<ForegroundProcess>,
    pub stale_reason: Option<String>,
}

impl TerminalSize {
    pub fn is_valid(self) -> bool {
        (1..=TERMINAL_ROWS_MAX).contains(&self.rows)
            && (1..=TERMINAL_COLUMNS_MAX).contains(&self.columns)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    Pty,
    Pipe,
    Acp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub protocol_version: u16,
    pub id: CommandId,
    pub command: ControlCommand,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlCommand {
    Register {
        instance_id: AgentInstanceId,
        agent_id: AgentId,
        transport: TransportKind,
    },
    Start {
        instance_id: AgentInstanceId,
        request: StartRequest,
    },
    Stop {
        instance_id: AgentInstanceId,
        force: bool,
    },
    SendInput {
        instance_id: AgentInstanceId,
        action: InputAction,
    },
    Resize {
        instance_id: AgentInstanceId,
        size: TerminalSize,
    },
    RefreshForeground {
        instance_id: AgentInstanceId,
    },
    IngestProvider {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        source: ProviderSource,
        source_sequence: u64,
        events: Vec<ProviderEvent>,
    },
    Remove {
        instance_id: AgentInstanceId,
    },
}

impl ControlCommand {
    pub fn instance_id(&self) -> AgentInstanceId {
        match self {
            Self::Register { instance_id, .. }
            | Self::Start { instance_id, .. }
            | Self::Stop { instance_id, .. }
            | Self::SendInput { instance_id, .. }
            | Self::Resize { instance_id, .. }
            | Self::RefreshForeground { instance_id }
            | Self::IngestProvider { instance_id, .. }
            | Self::Remove { instance_id } => *instance_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EffectEnvelope {
    pub protocol_version: u16,
    pub operation_id: OperationId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub effect: ControlEffect,
}

/// Route proof an effect executor must obtain immediately before a PTY write.
///
/// This is carried by the effect rather than inferred by a product shell so
/// local, hosted, and future browser-facing executors enforce the same rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ForegroundRequirement {
    /// Explicit terminal text and controls are direct user terminal input.
    Any,
    /// Semantic input must target the session's configured agent.
    Agent { agent_id: AgentId },
    /// Intentional shell syntax may be written only while a shell owns the PTY.
    Shell,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlEffect {
    Spawn {
        agent_id: AgentId,
        transport: TransportKind,
        request: StartRequest,
    },
    Stop {
        force: bool,
    },
    WriteInput {
        input: PreparedInput,
        required_foreground: ForegroundRequirement,
    },
    SubmitPrompt {
        prompt: String,
    },
    Interrupt,
    Resize {
        size: TerminalSize,
    },
    ObserveForeground,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservationEnvelope {
    pub protocol_version: u16,
    pub operation_id: Option<OperationId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub observation: ControlObservation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlObservation {
    Spawned {
        process_id: Option<u32>,
    },
    SpawnFailed {
        message: String,
    },
    ProcessExited {
        exit_code: Option<i32>,
        final_terminal: Option<TerminalFrame>,
    },
    StopCompleted {
        forced: bool,
        exit_code: Option<i32>,
        final_terminal: Option<TerminalFrame>,
    },
    StopFailed {
        message: String,
    },
    InputCompleted,
    InputFailed {
        message: String,
    },
    ResizeCompleted {
        size: TerminalSize,
    },
    ResizeFailed {
        message: String,
    },
    ForegroundObserved {
        process: ForegroundProcess,
    },
    ForegroundFailed {
        message: String,
    },
    TerminalFrame {
        frame: TerminalFrame,
    },
    TerminalStale {
        message: String,
    },
    ProviderEvent {
        source: ProviderSource,
        sequence: u64,
        event: ProviderEvent,
    },
    ProviderGap {
        source: ProviderSource,
        missed: u64,
    },
}

impl ControlObservation {
    pub fn requires_operation_id(&self) -> bool {
        !matches!(
            self,
            Self::ProcessExited { .. }
                | Self::TerminalFrame { .. }
                | Self::TerminalStale { .. }
                | Self::ProviderEvent { .. }
                | Self::ProviderGap { .. }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionStatus {
    Registered,
    Starting,
    Running,
    Stopping,
    Exited { exit_code: Option<i32> },
    Failed { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub instance_id: AgentInstanceId,
    pub agent_id: AgentId,
    pub transport: TransportKind,
    pub generation: SessionGeneration,
    pub status: SessionStatus,
    pub pending_operation: Option<OperationId>,
    pub pending_input: Option<PreparedInputKind>,
    pub process_id: Option<u32>,
    pub terminal_size: Option<TerminalSize>,
    pub terminal_frame: Option<TerminalFrame>,
    pub terminal_stale: Option<String>,
    pub foreground: ForegroundSnapshot,
    pub provider: ProviderSnapshot,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProviderEvent {
    SessionStarted {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },
    TurnStarted {
        prompt: Option<String>,
    },
    Text {
        text: String,
        is_delta: bool,
    },
    Thinking {
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
        input_json: String,
    },
    ToolCompleted {
        id: String,
        output: String,
        is_error: bool,
        duration_ms: Option<u64>,
    },
    TurnCompleted {
        usage: TokenUsage,
        is_cumulative: bool,
    },
    SessionEnded {
        result: String,
        cost_usd: Option<String>,
        is_error: bool,
    },
    Error {
        message: String,
    },
    Ready,
    ApprovalRequested {
        tool_name: String,
        description: Option<String>,
    },
    RateLimited {
        limit_type: String,
        resets_at: Option<String>,
        usage_percent: Option<String>,
        raw_message: String,
    },
}

impl ProviderEvent {
    pub fn validate_ingress(&self) -> Result<(), ProviderEventValidationError> {
        match self {
            Self::SessionStarted {
                session_id,
                model,
                tools,
            } => {
                validate_required("session_id", session_id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_identifier("model", model, PROVIDER_EVENT_ID_MAX_BYTES)?;
                if tools.len() > PROVIDER_EVENT_TOOLS_MAX {
                    return Err(ProviderEventValidationError::TooManyTools {
                        count: tools.len(),
                        max: PROVIDER_EVENT_TOOLS_MAX,
                    });
                }
                for tool in tools {
                    validate_required("tool", tool, PROVIDER_EVENT_ID_MAX_BYTES)?;
                }
            }
            Self::TurnStarted { prompt } => {
                if let Some(prompt) = prompt {
                    validate_text("prompt", prompt, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                }
            }
            Self::Text { text, .. } | Self::Thinking { text } => {
                validate_text("text", text, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::ToolStarted {
                id,
                name,
                input_json,
            } => {
                validate_required("tool id", id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_required("tool name", name, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_text("tool input", input_json, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::ToolCompleted { id, output, .. } => {
                validate_required("tool id", id, PROVIDER_EVENT_ID_MAX_BYTES)?;
                validate_text("tool output", output, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::SessionEnded {
                result, cost_usd, ..
            } => {
                validate_text("session result", result, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
                if let Some(cost) = cost_usd {
                    validate_identifier("cost", cost, PROVIDER_EVENT_ID_MAX_BYTES)?;
                }
            }
            Self::Error { message } => {
                validate_required_text("error", message, PROVIDER_EVENT_TEXT_MAX_BYTES)?;
            }
            Self::ApprovalRequested {
                tool_name,
                description,
            } => {
                validate_required("approval tool", tool_name, PROVIDER_EVENT_ID_MAX_BYTES)?;
                if let Some(description) = description {
                    validate_text(
                        "approval description",
                        description,
                        PROVIDER_EVENT_TEXT_MAX_BYTES,
                    )?;
                }
            }
            Self::RateLimited {
                limit_type,
                resets_at,
                usage_percent,
                raw_message,
            } => {
                validate_required("limit type", limit_type, PROVIDER_EVENT_ID_MAX_BYTES)?;
                for (field, value) in [
                    ("reset time", resets_at.as_deref()),
                    ("usage percent", usage_percent.as_deref()),
                ] {
                    if let Some(value) = value {
                        validate_identifier(field, value, PROVIDER_EVENT_ID_MAX_BYTES)?;
                    }
                }
                validate_text(
                    "rate limit message",
                    raw_message,
                    PROVIDER_EVENT_TEXT_MAX_BYTES,
                )?;
            }
            Self::TurnCompleted { .. } | Self::Ready => {}
        }
        Ok(())
    }
}

fn validate_required(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    if value.trim().is_empty() {
        return Err(ProviderEventValidationError::Empty { field });
    }
    validate_identifier(field, value, max)
}

fn validate_required_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    if value.trim().is_empty() {
        return Err(ProviderEventValidationError::Empty { field });
    }
    validate_text(field, value, max)
}

fn validate_identifier(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(ProviderEventValidationError::InvalidField { field, max });
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ProviderEventValidationError> {
    let has_unsafe_control = value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'));
    if value.len() > max || has_unsafe_control {
        return Err(ProviderEventValidationError::InvalidField { field, max });
    }
    Ok(())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProviderEventValidationError {
    #[error("provider event field '{field}' is required")]
    Empty { field: &'static str },
    #[error("provider event field '{field}' contains controls or exceeds {max} bytes")]
    InvalidField { field: &'static str, max: usize },
    #[error("provider event tool count {count} exceeds {max}")]
    TooManyTools { count: usize, max: usize },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ProviderSource {
    pub family: AdapterFamily,
    pub binding: AdapterBinding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSourceCursor {
    pub source: ProviderSource,
    pub sequence: u64,
    pub gap_count: u64,
    pub stale: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderActivity {
    #[default]
    Idle,
    Working,
    WaitingForInput,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveProviderTool {
    pub id: String,
    pub name: String,
    pub input_json: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub sequence: u64,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub completed_turns: u64,
    pub usage: TokenUsage,
    pub activity: ProviderActivity,
    pub current_prompt: Option<String>,
    pub active_tools: Vec<ActiveProviderTool>,
    pub sources: Vec<ProviderSourceCursor>,
    pub last_event: Option<ProviderEvent>,
    pub gap_count: u64,
    pub stale: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub protocol_version: u16,
    pub revision: u64,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlEvent {
    pub protocol_version: u16,
    pub sequence: u64,
    pub command_id: Option<CommandId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub event: ControlEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlEventKind {
    CommandRejected {
        message: String,
    },
    Registered,
    StartRequested {
        operation_id: OperationId,
    },
    Running {
        process_id: Option<u32>,
    },
    StopRequested {
        operation_id: OperationId,
        force: bool,
    },
    InputRequested {
        operation_id: OperationId,
        input_kind: PreparedInputKind,
    },
    InputCompleted {
        input_kind: PreparedInputKind,
    },
    InputFailed {
        input_kind: PreparedInputKind,
        message: String,
    },
    ResizeRequested {
        operation_id: OperationId,
        size: TerminalSize,
    },
    Resized {
        size: TerminalSize,
    },
    ResizeFailed {
        message: String,
    },
    ForegroundRefreshRequested {
        operation_id: OperationId,
    },
    ForegroundObserved {
        process: ForegroundProcess,
    },
    ForegroundFailed {
        message: String,
    },
    TerminalStale {
        message: String,
    },
    ProviderEvent {
        sequence: u64,
        source: ProviderSource,
        source_sequence: u64,
        event: ProviderEvent,
    },
    ProviderGap {
        sequence: u64,
        source: ProviderSource,
        missed: u64,
    },
    Exited {
        exit_code: Option<i32>,
        forced: bool,
    },
    Failed {
        message: String,
    },
    Removed,
    ObservationIgnored {
        reason: ObservationIgnoredReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationIgnoredReason {
    UnsupportedProtocolVersion,
    UnknownInstance,
    StaleGeneration,
    MissingOperation,
    OperationMismatch,
    InvalidState,
    StaleTerminalFrame,
    StaleProviderEvent,
    InvalidForegroundObservation,
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ControlError {
    #[error("control protocol version {actual} is unsupported; expected {expected}")]
    UnsupportedProtocolVersion { expected: u16, actual: u16 },
    #[error("agent instance {instance_id:?} is already registered")]
    DuplicateInstance { instance_id: AgentInstanceId },
    #[error("agent instance {instance_id:?} is not registered")]
    UnknownInstance { instance_id: AgentInstanceId },
    #[error("agent instance {instance_id:?} already has pending operation {operation_id:?}")]
    OperationPending {
        instance_id: AgentInstanceId,
        operation_id: OperationId,
    },
    #[error("agent input was rejected: {error}")]
    InputRejected { error: InputPrepareError },
    #[error("terminal size is outside the supported bounded range")]
    InvalidTerminalSize,
    #[error("working directory is empty, too large, or contains a NUL byte")]
    InvalidWorkingDirectory,
    #[error("pipe transport requires a non-empty initial prompt")]
    MissingInitialPrompt,
    #[error("transport {transport:?} does not support {action}")]
    UnsupportedTransportOperation {
        transport: TransportKind,
        action: String,
    },
    #[error("agent instance {instance_id:?} cannot {action} while in state {status:?}")]
    InvalidTransition {
        instance_id: AgentInstanceId,
        action: String,
        status: SessionStatus,
    },
    #[error("provider ingress generation {actual:?} is stale; expected {expected:?}")]
    StaleProviderGeneration {
        expected: SessionGeneration,
        actual: SessionGeneration,
    },
    #[error("provider ingress source sequence must be greater than the current sequence")]
    StaleProviderSequence,
    #[error("provider ingress batch must contain between 1 and {max} events")]
    InvalidProviderBatch { max: usize },
    #[error("invalid provider ingress event: {message}")]
    InvalidProviderEvent { message: String },
}

impl Default for ControlSnapshot {
    fn default() -> Self {
        Self {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            revision: 0,
            sessions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ForegroundProcess, ForegroundProcessKind, ProviderEvent, ProviderEventValidationError,
        FOREGROUND_PROCESS_NAME_MAX_BYTES,
    };
    use crate::AgentId;

    #[test]
    fn foreground_process_is_bounded_and_agent_bound() {
        let claude = AgentId::new("claude").unwrap();
        let process = ForegroundProcess {
            root_process_id: 1,
            process_id: 2,
            process_name: "claude".to_owned(),
            kind: ForegroundProcessKind::Agent {
                agent_id: claude.clone(),
            },
        };
        assert!(process.is_valid_for(&claude));
        assert!(!process.is_valid_for(&AgentId::new("codex").unwrap()));
        assert!(!ForegroundProcess {
            process_name: "x".repeat(FOREGROUND_PROCESS_NAME_MAX_BYTES + 1),
            ..process
        }
        .is_valid_for(&claude));
    }

    #[test]
    fn provider_ingress_allows_multiline_text_but_rejects_control_bytes() {
        ProviderEvent::Text {
            text: "first line\n\tsecond line".to_owned(),
            is_delta: false,
        }
        .validate_ingress()
        .unwrap();

        assert!(matches!(
            ProviderEvent::Text {
                text: "unsafe\u{0000}text".to_owned(),
                is_delta: false,
            }
            .validate_ingress(),
            Err(ProviderEventValidationError::InvalidField { field: "text", .. })
        ));
        assert!(ProviderEvent::SessionStarted {
            session_id: "session\nother".to_owned(),
            model: "model".to_owned(),
            tools: Vec::new(),
        }
        .validate_ingress()
        .is_err());
    }
}
