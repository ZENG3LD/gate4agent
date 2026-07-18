use crate::{AgentId, InputAction, InputPrepareError, PreparedInput, PreparedInputKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONTROL_PROTOCOL_VERSION: u16 = 6;
pub const TERMINAL_ROWS_MAX: u16 = 1_000;
pub const TERMINAL_COLUMNS_MAX: u16 = 1_000;
pub const WORKING_DIRECTORY_MAX_BYTES: usize = 32_768;

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
    },
    SubmitPrompt {
        prompt: String,
    },
    Interrupt,
    Resize {
        size: TerminalSize,
    },
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
    TerminalFrame {
        frame: TerminalFrame,
    },
    TerminalStale {
        message: String,
    },
    ProviderEvent {
        sequence: u64,
        event: ProviderEvent,
    },
    ProviderGap {
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub sequence: u64,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub completed_turns: u64,
    pub usage: TokenUsage,
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
    TerminalStale {
        message: String,
    },
    ProviderEvent {
        sequence: u64,
        event: ProviderEvent,
    },
    ProviderGap {
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
