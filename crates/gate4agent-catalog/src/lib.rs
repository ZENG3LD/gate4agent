//! Provider knowledge and shell-free launch planning for gate4agent.

mod builtin;
mod capability;
mod launch;
mod registry;
mod session_options;

pub use builtin::{builtin_registry, builtin_specs, ORCA_REFERENCE_REVISION};
pub use capability::{
    parse_capability_models_for, resolve_capability_probe_for, CapabilityProbeCatalogError,
    ResolvedCapabilityProbePlan,
};
pub use gate4agent_adapters::{
    builtin_adapter_registry, merge_session_option_models, one_shot_spec, one_shot_specs,
    resolve_one_shot_plan, AdapterDescriptor, AdapterRegistry, AdapterRegistryError,
    AgentSessionOptionCatalog, OneShotAdapterError, OneShotAdapterSpec, OneShotModelSource,
    OneShotModelSpec, OneShotPlan, OneShotPromptDelivery, OneShotThinkingLevel,
    ResolvedSessionOptionLaunch, SessionOption, SessionOptionAdapterError, SessionOptionApply,
    SessionOptionArgumentOverride, SessionOptionCategory, SessionOptionChoice,
    SessionOptionInteractionDetection, SessionOptionKind, SessionOptionLaunchApplication,
    SessionOptionMidSessionApplication, SessionOptionMidSessionPlan, SessionOptionModel,
    SessionOptionModelListSpec, BUILTIN_ADAPTER_REVISION, CAPABILITY_PROBE_OUTPUT_MAX_BYTES,
    CAPABILITY_PROBE_REVISION, CLAUDE_CODE_INLINE_REVISION, CODEX_CLI_INLINE_REVISION,
    KIMI_CODE_INLINE_REVISION, MANAGED_HOOK_REVISION, ONE_SHOT_OUTPUT_MAX_BYTES,
    ONE_SHOT_REVISION, ONE_SHOT_THINKING_OPTION_ID, ONE_SHOT_TIMEOUT_SECONDS,
    QWEN_CODE_INLINE_REVISION, SESSION_OPTION_CATALOG_REVISION,
};
pub use gate4agent_types::{
    AcpTransportSpec, AdapterBinding, AdapterBindingError, AdapterFamily, AdapterId,
    AdapterIdError, AdapterVerification, AgentAdapterCapabilities, AgentCapabilities,
    AgentCommandMode, AgentId, AgentIdError, AgentReadinessSpec, AgentSpec,
    AgentTransportCapabilities, DetectionSpec, DraftReadySignal, InitialPromptMode, LaunchSpec,
    NativeDraftMode, PipePromptDelivery, PipeProtocol, PipeTransportSpec, ProcessMatcher,
    PromptSpec, RuntimePlatform, SessionOptionSelection, SessionOptionValue, SpecVerification,
};
pub use launch::{
    plan_draft_launch, plan_launch, EnvMutation, LaunchPlan, LaunchPlanError, LaunchRequest,
    MAX_LAUNCH_PROMPT_BYTES, WINDOWS_INLINE_LAUNCH_MAX_CHARS,
};
pub use registry::{AgentRegistry, RegistryError};
pub use session_options::{
    parse_session_option_models_for, plan_mid_session_action_control_for,
    plan_mid_session_action_for, plan_mid_session_control_for, plan_mid_session_option_for,
    resolve_session_option_launch_for, session_option_catalog_for, SessionOptionCatalogError,
    SessionOptionControlPlan,
};
