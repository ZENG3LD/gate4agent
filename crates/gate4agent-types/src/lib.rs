//! Pure data contracts shared by gate4agent engines and shells.

mod control;
mod executable;
mod id;
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
pub use readiness::{AgentReadinessSpec, DraftReadySignal};
pub use spec::{
    AgentCapabilities, AgentCommandMode, AgentSpec, DetectionSpec, InitialPromptMode, LaunchSpec,
    NativeDraftMode, ProcessMatcher, PromptSpec, RuntimePlatform, SpecVerification,
};
