//! Provider knowledge and shell-free launch planning for gate4agent.

mod builtin;
mod launch;
mod registry;

pub use builtin::{builtin_registry, builtin_specs, ORCA_REFERENCE_REVISION};
pub use gate4agent_types::{
    AcpTransportSpec, AgentCapabilities, AgentCommandMode, AgentId, AgentIdError,
    AgentReadinessSpec, AgentSpec, AgentTransportCapabilities, DetectionSpec, DraftReadySignal,
    InitialPromptMode, LaunchSpec, NativeDraftMode, PipePromptDelivery, PipeTransportSpec,
    ProcessMatcher, PromptSpec, ProviderAdapter, RuntimePlatform, SpecVerification,
};
pub use launch::{
    plan_draft_launch, plan_launch, EnvMutation, LaunchPlan, LaunchPlanError, LaunchRequest,
    MAX_LAUNCH_PROMPT_BYTES, WINDOWS_INLINE_LAUNCH_MAX_CHARS,
};
pub use registry::{AgentRegistry, RegistryError};
