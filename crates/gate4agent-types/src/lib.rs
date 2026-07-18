//! Pure data contracts shared by gate4agent engines and shells.

mod executable;
mod id;
mod readiness;
mod spec;

pub use executable::normalize_executable_name;
pub use id::{AgentId, AgentIdError};
pub use readiness::{AgentReadinessSpec, DraftReadySignal};
pub use spec::{
    AgentCapabilities, AgentCommandMode, AgentSpec, DetectionSpec, InitialPromptMode, LaunchSpec,
    NativeDraftMode, ProcessMatcher, PromptSpec, RuntimePlatform, SpecVerification,
};
