//! Pure data contracts shared by gate4agent engines and shells.

mod adapter;
mod capability;
mod control;
mod executable;
mod history;
mod id;
mod input;
mod readiness;
mod resume;
mod session_options;
mod spec;

pub use adapter::{
    AdapterBinding, AdapterBindingError, AdapterFamily, AdapterId, AdapterIdError,
    AdapterVerification, MAX_ADAPTER_REVISION_LEN,
};
pub use capability::{
    validate_capability_models, CapabilityModelSummary, CapabilityProbeFailure,
    CapabilityProbeRequest, CapabilitySnapshot, CapabilityValidationError, PendingCapabilityProbe,
    CAPABILITY_MODELS_MAX, CAPABILITY_MODEL_ID_MAX_BYTES, CAPABILITY_MODEL_LABEL_MAX_BYTES,
};
pub use control::{
    ActiveProviderTool, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEffect,
    ControlError, ControlEvent, ControlEventKind, ControlObservation, ControlSnapshot,
    EffectEnvelope, ForegroundAuthority, ForegroundProcess, ForegroundProcessKind,
    ForegroundRequirement, ForegroundSnapshot, ObservationEnvelope, ObservationIgnoredReason,
    OperationId, ProviderActivity, ProviderEvent, ProviderEventValidationError,
    ProviderInteraction, ProviderInteractionId, ProviderInteractionKind,
    ProviderInteractionOutcome, ProviderInteractionStatus, ProviderSessionIdentity,
    ProviderSessionKey, ProviderSnapshot, ProviderSource, ProviderSourceCursor, ProviderSubagent,
    SessionGeneration, SessionSnapshot, SessionStatus, StartRequest, TerminalFrame, TerminalSize,
    TokenUsage, TransportKind, CONTROL_PROTOCOL_VERSION, FOREGROUND_PROCESS_NAME_MAX_BYTES,
    PROVIDER_EVENT_ID_MAX_BYTES, PROVIDER_EVENT_TEXT_MAX_BYTES, PROVIDER_EVENT_TOOLS_MAX,
    PROVIDER_INGRESS_EVENTS_MAX, PROVIDER_INTERACTIONS_MAX, PROVIDER_SESSION_LOCATOR_MAX_BYTES,
    PROVIDER_SUBAGENTS_MAX, TERMINAL_COLUMNS_MAX, TERMINAL_ROWS_MAX, WORKING_DIRECTORY_MAX_BYTES,
};
pub use executable::normalize_executable_name;
pub use history::{
    validate_candidate_id, validate_history_error, HistoryCandidateSummary, HistoryMessageRecord,
    HistoryMessageRole, HistoryOperation, HistoryQuery, HistorySessionRecord, HistorySnapshot,
    HistoryValidationError, PendingHistoryOperation, HISTORY_CANDIDATE_ID_MAX_BYTES,
    HISTORY_DISCOVERY_LIMIT_MAX, HISTORY_ERROR_MAX_BYTES, HISTORY_MESSAGES_MAX,
    HISTORY_MESSAGE_MAX_BYTES, HISTORY_MODEL_MAX_BYTES, HISTORY_SESSION_ID_MAX_BYTES,
    HISTORY_TITLE_MAX_BYTES,
};
pub use id::{AgentId, AgentIdError};
pub use input::{
    normalize_semantic_prompt, prepare_agent_command, prepare_input, prepare_input_with_limits,
    prepare_shell_command, sanitize_prompt_text, AgentCommand, InputAction, InputPrepareError,
    PreparedInput, PreparedInputKind, PreparedWrite, PreparedWriteKind, PromptFraming,
    PromptPayload, ShellCommand, TerminalControl, TerminalText, BRACKETED_PASTE_END,
    BRACKETED_PASTE_START, SEMANTIC_PROMPT_MAX_BYTES, TERMINAL_INPUT_CHUNK_MAX_BYTES,
    TERMINAL_INPUT_MAX_BYTES, TERMINAL_SUBMIT_DELAY_MS, TERMINAL_WRITE_DELAY_MAX_MS,
};
pub use readiness::{AgentReadinessSpec, DraftReadySignal};
pub use resume::{
    validate_resume_error, PendingResumeOperation, ResumeAuthorityTarget, ResumeLaunchRequest,
    ResumePhase, ResumeSessionSummary, ResumeSnapshot, ResumeTarget, ResumeValidationError,
    RESUME_ERROR_MAX_BYTES,
};
pub use session_options::{
    SessionOptionSelection, SessionOptionValidationError, SessionOptionValue,
    SESSION_OPTION_ID_MAX_BYTES, SESSION_OPTION_VALUES_MAX, SESSION_OPTION_VALUE_MAX_BYTES,
};
pub use spec::{
    AcpTransportSpec, AgentAdapterCapabilities, AgentCapabilities, AgentCommandMode, AgentSpec,
    AgentTransportCapabilities, DetectionSpec, InitialPromptMode, LaunchSpec, NativeDraftMode,
    PipePromptDelivery, PipeTransportSpec, ProcessMatcher, PromptSpec, RuntimePlatform,
    SpecVerification,
};
