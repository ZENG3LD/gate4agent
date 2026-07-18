//! Pure data contracts shared by gate4agent engines and shells.

mod adapter;
mod control;
mod executable;
mod id;
mod input;
mod readiness;
mod spec;

pub use adapter::{
    AdapterBinding, AdapterBindingError, AdapterFamily, AdapterId, AdapterIdError,
    AdapterVerification, MAX_ADAPTER_REVISION_LEN,
};
pub use control::{
    AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEffect, ControlError,
    ControlEvent, ControlEventKind, ControlObservation, ControlSnapshot, EffectEnvelope,
    ObservationEnvelope, ObservationIgnoredReason, OperationId, ProviderEvent, ProviderSnapshot,
    SessionGeneration, SessionSnapshot, SessionStatus, StartRequest, TerminalFrame, TerminalSize,
    TokenUsage, TransportKind, CONTROL_PROTOCOL_VERSION, TERMINAL_COLUMNS_MAX, TERMINAL_ROWS_MAX,
    WORKING_DIRECTORY_MAX_BYTES,
};
pub use executable::normalize_executable_name;
pub use id::{AgentId, AgentIdError};
pub use input::{
    normalize_semantic_prompt, prepare_agent_command, prepare_input, prepare_input_with_limits,
    sanitize_prompt_text, AgentCommand, InputAction, InputPrepareError, PreparedInput,
    PreparedInputKind, PreparedWrite, PreparedWriteKind, PromptFraming, PromptPayload,
    ShellCommand, TerminalControl, TerminalText, BRACKETED_PASTE_END, BRACKETED_PASTE_START,
    SEMANTIC_PROMPT_MAX_BYTES, TERMINAL_INPUT_CHUNK_MAX_BYTES, TERMINAL_INPUT_MAX_BYTES,
    TERMINAL_SUBMIT_DELAY_MS, TERMINAL_WRITE_DELAY_MAX_MS,
};
pub use readiness::{AgentReadinessSpec, DraftReadySignal};
pub use spec::{
    AcpTransportSpec, AgentAdapterCapabilities, AgentCapabilities, AgentCommandMode, AgentSpec,
    AgentTransportCapabilities, DetectionSpec, InitialPromptMode, LaunchSpec, NativeDraftMode,
    PipePromptDelivery, PipeTransportSpec, ProcessMatcher, PromptSpec, RuntimePlatform,
    SpecVerification,
};
