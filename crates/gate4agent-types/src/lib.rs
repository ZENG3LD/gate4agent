//! Pure data contracts shared by gate4agent engines and shells.

mod control;
mod executable;
mod id;
mod input;
mod readiness;
mod spec;

pub use control::{
    AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEffect, ControlError,
    ControlEvent, ControlEventKind, ControlObservation, ControlSnapshot, EffectEnvelope,
    ObservationEnvelope, ObservationIgnoredReason, OperationId, SessionGeneration,
    SessionSnapshot, SessionStatus, TransportKind, CONTROL_PROTOCOL_VERSION,
};
pub use executable::normalize_executable_name;
pub use id::{AgentId, AgentIdError};
pub use input::{
    prepare_agent_command, prepare_input, prepare_input_with_limits, sanitize_prompt_text,
    AgentCommand, InputAction, InputPrepareError, PreparedInput, PreparedInputKind, PreparedWrite,
    PreparedWriteKind, PromptFraming, PromptPayload, ShellCommand, TerminalControl, TerminalText,
    BRACKETED_PASTE_END, BRACKETED_PASTE_START, TERMINAL_INPUT_CHUNK_MAX_BYTES,
    TERMINAL_INPUT_MAX_BYTES, TERMINAL_SUBMIT_DELAY_MS, TERMINAL_WRITE_DELAY_MAX_MS,
};
pub use readiness::{AgentReadinessSpec, DraftReadySignal};
pub use spec::{
    AgentCapabilities, AgentCommandMode, AgentSpec, DetectionSpec, InitialPromptMode, LaunchSpec,
    NativeDraftMode, ProcessMatcher, PromptSpec, RuntimePlatform, SpecVerification,
};
