use crate::git_worktree::{
    create_worktree as create_git_worktree,
    create_worktree_with_timeout as create_git_worktree_with_timeout,
    list_worktrees as list_git_worktrees,
    list_worktrees_with_deadline as list_git_worktrees_with_deadline,
    paths_equal as worktree_paths_equal, remove_worktree as remove_git_worktree,
    removal_lookup_path as normalize_worktree_removal_target,
    resolve_base_commit_with_timeout,
    run_git_read_bounded, GitCommandOutput, GitWorktreeError, GitWorktreeErrorKind,
    NativeGitWorktreeSnapshot,
};
use crate::host_directory_browser::{
    browse_host_directories, HostDirectoryBrowseError, HostDirectoryBrowseErrorKind,
};
use crate::environment_profiles::{
    EnvironmentProfileBinding, NodeEnvironmentProfile, MAX_NODE_ENVIRONMENT_PROFILES,
};
use crate::bundle_catalog::{BundleCatalog, NodeBundle};
use self::bundle_delivery::{DeliveryStore, DeliveryStoreError};
use self::context_pack_store::ContextPackStore;
use crate::bundle_provider::{
    bundle_launch_arguments, validate_bundle_binding, BundleProviderLayout,
};
use crate::context_pack::{
    ContextPackCatalog, ContextPackRepository, ContextPackRepositoryFileSource,
    ContextPackSelectedFileSkipReason, NodeContextPack, CONTEXT_PACK_SELECTED_FILES,
};
use crate::harness_mcp_proxy::{
    HarnessMcpProxyError, HarnessMcpProxyRegistry, PreparedHarnessMcpSpawn,
    ReviewedHarnessMcpProgram,
};
use crate::session_environment::{
    MaterializationId, MaterializationOwner, MaterializationOwnershipRecord,
    MaterializationState, NodeSecretResolver, NodeSessionMaterializationProfile,
    SessionEnvironmentMaterializeError, SessionEnvironmentMaterializer,
};
use crate::worktree_service::{
    exact_created_worktree, exact_owned_worktree, ManagedWorktreeLeaseRecord,
    ManagedWorktreeProfile, ManagedWorktreeRegistry, ManagedWorktreeSessionHolder,
};
use crate::session_registry::{
    self, validate_display_name, LoadedNodeState, ManagedWorktreeSpawnReplayRecordV10,
    ManagedWorktreeSpawnReplayStateV10, MAX_MANAGED_SESSION_RECORDS,
    MAX_MANAGED_WORKTREE_SPAWN_REPLAYS,
};
#[cfg(unix)]
use crate::workspace_file_unix::{
    create_workspace_directory as create_workspace_directory_on_disk,
    create_workspace_file as create_workspace_file_on_disk,
    read_workspace_file as read_workspace_file_from_disk,
    write_workspace_file as write_workspace_file_to_disk, WorkspaceFileBytes,
    WorkspaceFileReadError, WorkspaceFileReadErrorKind,
    WORKSPACE_ENTRY_CREATE_CANCELED, WORKSPACE_ENTRY_CREATE_COMMITTING,
    WORKSPACE_ENTRY_CREATE_PENDING,
};
#[cfg(windows)]
use crate::workspace_file_windows::{
    create_workspace_directory as create_workspace_directory_on_disk,
    create_workspace_file as create_workspace_file_on_disk,
    read_workspace_file as read_workspace_file_from_disk,
    write_workspace_file as write_workspace_file_to_disk, WorkspaceFileBytes,
    WorkspaceFileReadError, WorkspaceFileReadErrorKind,
    WORKSPACE_ENTRY_CREATE_CANCELED, WORKSPACE_ENTRY_CREATE_COMMITTING,
    WORKSPACE_ENTRY_CREATE_PENDING,
};
use crate::platform;
use crate::provider_runtime::{
    require_policy, ProviderRuntimeAdmissionError, ProviderRuntimeMonitor,
    ProviderRuntimeRequirement,
};
use crate::spawn_spec::SpawnProfileRegistry;
use crate::standalone_workspace::{
    prepare_standalone_workspace, StandaloneWorkspaceError, StandaloneWorkspaceErrorKind,
};
use crate::protocol::{
    read_json_frame_limited_body_timeout, write_json_frame, write_json_frame_limited,
    validate_node_negotiated_handshake_capacity, validate_provider_contract_manifest,
    provider_id_is_legacy, AdapterContractRevision, CapabilityId, ClientFrame, ClientRole,
    ControllerState, DeliveryBlobChunkHexV1, DeliveryBlobDigestV1,
    DeliveryBundleManifestV2, DeliveryCommitReceiptV1, DeliveryStageId,
    AgentProgressAttentionKindV1, AgentProgressAttentionV1, AgentProgressCurrentV1,
    AgentProgressEventKindV1, AgentProgressUsageV1, AgentProgressV1, FrameError,
    GitCommitDetails, GitDiff, GitDiffMode, GitDiffRequest, GitHistoryPage,
    GitObjectId, GitCommitSummary, GitSignatureStatus, GitSnapshot, GitStatusEntry,
    GitWorktreeSnapshot, HarnessMcpActivationDigest, HarnessMcpReservationId,
    HostDirectoryListing,
    NodeCompatibilitySupport, NodeEvent,
    NodeEventEnvelope, NodeFailure, NodeFailureCode, NodeHello, NodeId, NodeIncarnationId,
    NegotiatedNodeCompatibility, NodeRequest, NodeResponse, NodeSnapshot, OpaqueHostPath,
    ProtocolRange,
    ProviderAdapterContractSupport,
    ProviderContractRevision, ProviderContractSupport, RepositoryPath, RequestEnvelope,
    ProviderRuntimeStatuses, ResolvedBundleReceipt, ResolvedEnvironmentProfileReceipt,
    ContextPackLineageReceipt, ResolvedContextPackReceipt,
    ResolvedHarnessMcpProxyReceiptV1, ResolvedSpawnReceipt,
    ResolvedSpawnSpec, ResponseEnvelope,
    ServerChallenge, ServerFrame, SessionAddress, SessionAgentProgress, SessionKey, SessionMode,
    ManagedSessionRecord,
    ManagedSessionState, ManagedWorktreeCleanupFailure, ManagedWorktreeLeaseId,
    ManagedWorktreeLeaseSnapshot, ManagedWorktreeLeaseState, ManagedWorktreeRetention,
    ManagedWorktreeSpawnReceipt, ManagedWorktreeSpawnRequest,
    ManagedWorktreeSpawnRequestV2, SessionRecordId,
    NativeSessionCatalogRoute, NativeSessionSelection, SessionTaskBindingV1,
    SessionTaskTargetV1, TaskId,
    ObservationCapabilitiesV1, ObservationEvidenceV1, ObservationInteractionOutcomeV1,
    ObservationKindV1, ObservationSourceFamilyV1, ObservationV1,
    StateSchemaSupport, WorkspaceEntry, WorkspaceEntryKind, WorktreeProfileId,
    SpawnContextId, SpawnEnvironmentProfileId, SpawnIdempotencyKey,
    SpawnProfileDefaults, SpawnRequiredCapabilities, SpawnSpec, SpawnSpecResolveError,
    WorkspaceFileContent, WorkspaceFileRevision,
    WorkspaceFileRead, WorkspaceId, WorkspaceInspection, WorkspaceInspectionTruncationV1,
    WorkspaceSnapshot,
    WorktreeServiceMode,
    LaunchInventory, ManagedWorktreeProfileSummary, SpawnProfileSummary,
    WorktreeProfileInventory, MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE,
    DEFAULT_CONTROLLER_LEASE_MS,
    MAX_CONTROLLER_LEASE_MS, MIN_CONTROLLER_LEASE_MS, NODE_COMPATIBILITY_METADATA_CAPABILITY,
    CAPABILITY_HOST_DIRECTORY_BROWSE_V1,
    NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, NODE_REPOSITORY_PATH_CAPABILITY,
    NODE_PROVIDER_ID_OPEN_CAPABILITY,
    NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY,
    NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY, NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
    NODE_HISTORY_CONTEXT_PACK_CAPABILITY, NODE_SESSION_RECORD_CONTEXT_EXPORT_CAPABILITY,
    NODE_NATIVE_SESSION_CATALOG_CAPABILITY, NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY,
    NODE_NATIVE_SESSION_INDEX_CAPABILITY, NODE_NATIVE_SESSION_PREVIEW_CAPABILITY,
    NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
    NODE_OBSERVATION_EVENTS_CAPABILITY,
    NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY,
    NODE_HARNESS_MCP_READ_PROXY_CAPABILITY,
    NODE_SESSION_TASK_CORRELATION_CAPABILITY,
    NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY,
    NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
    NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
    NODE_MANAGED_WORKTREE_SPAWN_V2_CAPABILITY,
    NODE_SPAWN_PROFILE_REVISION_CAPABILITY, NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
    NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
    NODE_GIT_READ_CAPABILITY, NODE_WORKSPACE_FILE_READ_CAPABILITY,
    NODE_WORKSPACE_FILE_WRITE_CAPABILITY, NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY,
    NODE_WORKTREE_SELECTION_CAPABILITY,
    NODE_PROTOCOL_VERSION, MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    MAX_AGENT_PROGRESS_ACTIVE_TOOL_LABELS, MAX_AGENT_PROGRESS_ENTRY_BYTES,
    MAX_NODE_TERMINAL_BYTES, MAX_NODE_TEXT_BYTES,
    MAX_REPOSITORY_PATH_BYTES,
    MAX_GIT_DIFF_BYTES, MAX_GIT_HISTORY_COMMITS, MAX_WORKSPACE_FILE_BYTES,
    MAX_WORKSPACE_ROOT_BYTES, NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V10,
    SPAWN_RUNTIME_PROVIDER_SESSION_IDENTITY, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
    SPAWN_RUNTIME_SEMANTIC_READINESS, SPAWN_RUNTIME_SEMANTIC_RESUME,
    SPAWN_RUNTIME_STRUCTURED_PROMPT,
};
#[cfg(test)]
use crate::protocol::WorktreeProfileRevision;
use ring::digest::{digest, SHA256};
use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_handle::{EventSubscription, Gate4AgentHandle, PortDispatchError};
use gate4agent_runtime_native::{
    HookIngressConfig, NativeHarnessMcpLaunchOverlay, NativeInstanceLaunchOverlay,
    NativeLaunchEnvironmentOverlay,
    NativeHistoryConfig, NativeLaunchProfileControl, NativeRuntime, NativeRuntimeConfig,
    NativeSessionCatalogAuthority, NativeSessionCatalogError, NativeSessionPreviewError,
    ScopedNativeSessionCatalogEntry,
};
#[cfg(test)]
use gate4agent_runtime_native::{HistorySourceLayout, NativeHistoryRoot};
use gate4agent_node_wire::{
    auth_proof, negotiated_auth_proof, proofs_match, random_incarnation_id, random_nonce,
    AuthDirection, LocalServerStream, OwnerOnlyLocalListener,
};
use gate4agent_types::{
    AdapterBinding, AdapterFamily, AgentId, AgentInstanceId, AgentSpec, CommandEnvelope, CommandId,
    validate_candidate_id, ControlCommand, ControlEvent, ControlEventKind,
    HistoryCandidateSummary, HistoryOperation, HistoryQuery, HistorySessionRecord, InputAction,
    PromptFraming, PromptPayload, ResumeLaunchRequest,
    ProviderEvent, ProviderInteractionKind, ProviderInteractionStatus,
    ProviderRuntimeCapability, ProviderRuntimePolicy, ProviderSessionIdentity, ProviderSessionKey,
    ProviderSnapshot,
    ResumeTarget,
    SessionGeneration, StartRequest,
    TerminalControl, TerminalText,
    SessionStatus, TerminalFrame, TransportKind, CONTROL_PROTOCOL_VERSION, CONTROL_SESSIONS_MAX,
    HISTORY_DISCOVERY_LIMIT_MAX, WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::{BTreeMap, VecDeque};
#[cfg(feature = "fixture")]
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{sleep, timeout};

mod http_api;

#[path = "bundle_delivery.rs"]
mod bundle_delivery;

#[path = "context_pack_store.rs"]
mod context_pack_store;

#[cfg(windows)]
pub use crate::platform::DEFAULT_NODE_ENDPOINT;

const NODE_EVENT_HISTORY_MAX: usize = 4_096;
const NODE_BROADCAST_CAPACITY: usize = 1_024;
const NODE_TERMINAL_BROADCAST_CAPACITY: usize = 1;
const NODE_CONNECTION_EVENT_BURST_MAX: usize = 16;
const CONTROL_EVENT_SUBSCRIPTION_CAPACITY: usize = 1_024;
const MAX_PREAUTH_CONNECTIONS: usize = 32;
const MAX_AUTHENTICATED_CONNECTIONS: usize = 16;
const AUTH_FRAME_TIMEOUT_MS: u64 = 5_000;
const FRAME_BODY_TIMEOUT_MS: u64 = 5_000;
const CONNECTION_SHUTDOWN_GRACE_MS: u64 = 250;
const SPAWN_DISPATCH_TIMEOUT_MS: u64 = 2_000;
const SPAWN_IDEMPOTENCY_MAX_ENTRIES: usize = 256;
const SPAWN_IDEMPOTENCY_TTL_MS: u64 = 15 * 60 * 1_000;
const PROVIDER_RUNTIME_ADMISSION_TIMEOUT_MS: u64 = 2_500;
const MUTATION_SETTLE_TIMEOUT_MS: u64 = 5_000;
const READINESS_SETTLE_HEADROOM_MS: u64 = 2_000;
const MANAGED_RESUME_SETTLE_TIMEOUT_MS: u64 = 30_000;
const WORKSPACE_INSPECTION_MAX_CONCURRENCY: usize = 4;
const WORKSPACE_FILE_READ_TIMEOUT_MS: u64 = 2_000;
const HOST_DIRECTORY_BROWSE_TIMEOUT_MS: u64 = 2_000;
const NATIVE_SESSION_CATALOG_TIMEOUT_MS: u64 = 30_000;
const WORKSPACE_TREE_MAX_DEPTH: usize = 6;
const WORKSPACE_TREE_MAX_ENTRIES: usize = 512;
/// Default total wall-clock budget for `inspect_workspace`'s walk+git
/// chain, overridable via `GATE4AGENT_NODE_WORKSPACE_INSPECTION_BUDGET_MS`.
const WORKSPACE_INSPECTION_TIME_BUDGET_MS_DEFAULT: u64 = 8_000;
/// The harness relay enforces its own flat response deadline
/// (`HOST_RUN_READ_RESPONSE_DEADLINE`, `gate4agent-harness-service::runtime`,
/// 12s) above this node-side budget. The two constants live in different
/// crates with no shared definition, so clamp any override here comfortably
/// below that external ceiling — a misconfigured value above it would
/// reopen the exact deadline race this budget exists to close.
const WORKSPACE_INSPECTION_TIME_BUDGET_MS_MAX: u64 = 11_000;
/// Default cap on directory entries visited (not just returned) by one
/// walk, overridable via `GATE4AGENT_NODE_WORKSPACE_INSPECTION_ENTRY_CAP`.
const WORKSPACE_INSPECTION_ENTRY_CAP_DEFAULT: usize = 50_000;
const WORKSPACE_INSPECTION_ENTRY_CAP_MAX: usize = 1_000_000;
const GIT_STATUS_MAX_ENTRIES: usize = 128;
const GIT_COMMIT_MAX_ENTRIES: usize = 12;
const GIT_OUTPUT_MAX_BYTES: usize = 64 * 1_024;
const GIT_DIAGNOSTIC_MAX_BYTES: usize = 1_024;
const WORKSPACE_FILE_WRITE_TIMEOUT_MS: u64 = 5_000;
const WORKSPACE_ENTRY_CREATE_TIMEOUT_MS: u64 = 5_000;
const GIT_COMMAND_TIMEOUT_MS: u64 = 1_500;
const WORKSPACE_UNAVAILABLE_ERROR: &str = "workspace-unavailable";
const PROVIDER_SESSION_SCOPE_CONFLICT_ERROR: &str = "provider-session-scope-conflict";
const PROVIDER_SESSION_LIVE_CONFLICT_ERROR: &str = "provider-session-live-conflict";
const MANAGED_SESSION_CAPACITY_ERROR: &str = "managed-session-capacity";
const PROVIDER_IDENTITY_ALLOCATION_ERROR: &str =
    "provider-session-identity-allocation-failed";
const PROVIDER_RESUME_REJECTED_ERROR: &str = "provider-resume-rejected";
const DURABLE_STATE_COMMIT_FAILED_ERROR: &str = "durable-state-commit-failed";
const DURABLE_STATE_LOCK_FAILED_ERROR: &str = "durable-state-lock-failed";
const DURABLE_STATE_LOAD_FAILED_ERROR: &str = "durable-state-load-failed";
const DURABLE_STATE_CONFLICT_ERROR: &str = "durable-state-conflict";
const DURABLE_STATE_SCHEMA_UNSUPPORTED_ERROR: &str = "durable-state-schema-unsupported";
const DURABLE_STATE_PATH_SEMANTICS_UNSUPPORTED_ERROR: &str =
    "durable-state-path-semantics-unsupported";

fn opaque_windows_path(value: String) -> OpaqueHostPath {
    OpaqueHostPath::utf8(value).expect("validated Windows node path must remain wire-safe")
}

fn agent_progress_from_provider_snapshot(
    address: SessionAddress,
    provider: &ProviderSnapshot,
) -> Option<SessionAgentProgress> {
    let mut truncated = false;
    let active_tool_count = bounded_progress_count(provider.active_tools.len(), &mut truncated);
    let mut active_tool_labels = Vec::with_capacity(
        provider
            .active_tools
            .len()
            .min(MAX_AGENT_PROGRESS_ACTIVE_TOOL_LABELS),
    );
    for tool in &provider.active_tools {
        if active_tool_labels.len() == MAX_AGENT_PROGRESS_ACTIVE_TOOL_LABELS {
            truncated = true;
            break;
        }
        if let Some(label) = sanitize_progress_tool_label(&tool.name, &mut truncated) {
            active_tool_labels.push(label);
        }
    }
    let attention = provider
        .interactions
        .iter()
        .find(|interaction| {
            matches!(
                interaction.status,
                ProviderInteractionStatus::Pending | ProviderInteractionStatus::Resolving { .. }
            )
        })
        .map(|interaction| AgentProgressAttentionV1 {
            kind: match interaction.interaction_kind {
                ProviderInteractionKind::Approval => AgentProgressAttentionKindV1::Approval,
                ProviderInteractionKind::Question => AgentProgressAttentionKindV1::Question,
            },
            tool_label: sanitize_progress_tool_label(&interaction.tool_name, &mut truncated),
        });
    let subagent_count = bounded_progress_count(provider.subagents.len(), &mut truncated);
    let usage = (provider.completed_turns > 0
        || provider.usage.input_tokens > 0
        || provider.usage.output_tokens > 0
        || provider.usage.cache_read_tokens > 0
        || provider.usage.cache_write_tokens > 0
        || provider.usage.reasoning_tokens > 0)
        .then_some(AgentProgressUsageV1 {
            input_tokens: provider.usage.input_tokens,
            output_tokens: provider.usage.output_tokens,
            cache_read_tokens: provider.usage.cache_read_tokens,
            cache_write_tokens: provider.usage.cache_write_tokens,
            reasoning_tokens: provider.usage.reasoning_tokens,
        });
    let progress = AgentProgressV1 {
        provider_sequence: provider.sequence,
        activity: provider.activity,
        completed_turns: provider.completed_turns,
        usage,
        current: AgentProgressCurrentV1::from(provider.activity),
        active_tool_labels,
        active_tool_count,
        attention,
        subagent_count,
        last_event_kind: provider.last_event.as_ref().and_then(agent_progress_event_kind),
        gap_count: provider.gap_count,
        stale: provider.stale,
        truncated,
    };
    let entry = SessionAgentProgress { address, progress };
    serde_json::to_vec(&entry)
        .ok()
        .filter(|encoded| encoded.len() <= MAX_AGENT_PROGRESS_ENTRY_BYTES)
        .map(|_| entry)
}

fn bounded_progress_count(count: usize, truncated: &mut bool) -> u32 {
    match u32::try_from(count) {
        Ok(count) => count,
        Err(_) => {
            *truncated = true;
            u32::MAX
        }
    }
}

fn sanitize_progress_tool_label(value: &str, truncated: &mut bool) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        *truncated = true;
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase();
    let class = if ["read", "view", "open", "get"].iter().any(|term| normalized.contains(term)) {
        "Read"
    } else if ["write", "create", "save"].iter().any(|term| normalized.contains(term)) {
        "Write"
    } else if ["edit", "patch", "replace"].iter().any(|term| normalized.contains(term)) {
        "Edit"
    } else if ["shell", "bash", "powershell", "terminal", "exec", "command"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Shell"
    } else if ["search", "find", "grep", "query"].iter().any(|term| normalized.contains(term)) {
        "Search"
    } else if ["browser", "web", "http", "fetch"].iter().any(|term| normalized.contains(term)) {
        "Browse"
    } else if ["git", "commit", "diff"].iter().any(|term| normalized.contains(term)) {
        "Git"
    } else if ["ask", "question", "approval", "input"].iter().any(|term| normalized.contains(term)) {
        "Ask"
    } else if ["task", "agent", "spawn"].iter().any(|term| normalized.contains(term)) {
        "Task"
    } else {
        "Tool"
    };
    if trimmed != class {
        *truncated = true;
    }
    Some(class.to_owned())
}

fn agent_progress_event_kind(event: &ProviderEvent) -> Option<AgentProgressEventKindV1> {
    if matches!(event, ProviderEvent::ContextWindowUsage { .. }) {
        return None;
    }
    Some(match event {
        ProviderEvent::SessionStarted { .. } => AgentProgressEventKindV1::SessionStarted,
        ProviderEvent::SessionIdentityObserved { .. } => {
            AgentProgressEventKindV1::SessionIdentityObserved
        }
        ProviderEvent::TurnStarted { .. } => AgentProgressEventKindV1::TurnStarted,
        ProviderEvent::WorkingObserved => AgentProgressEventKindV1::WorkingObserved,
        ProviderEvent::Text { .. } => AgentProgressEventKindV1::Text,
        ProviderEvent::Thinking { .. } => AgentProgressEventKindV1::Thinking,
        ProviderEvent::ToolStarted { .. } => AgentProgressEventKindV1::ToolStarted,
        ProviderEvent::ToolCompleted { .. } => AgentProgressEventKindV1::ToolCompleted,
        ProviderEvent::TurnCompleted { .. } => AgentProgressEventKindV1::TurnCompleted,
        ProviderEvent::TurnInterrupted => AgentProgressEventKindV1::TurnInterrupted,
        ProviderEvent::SessionEnded { .. } => AgentProgressEventKindV1::SessionEnded,
        ProviderEvent::Error { .. } => AgentProgressEventKindV1::Error,
        ProviderEvent::Ready => AgentProgressEventKindV1::Ready,
        ProviderEvent::InteractionRequested { .. } => {
            AgentProgressEventKindV1::InteractionRequested
        }
        ProviderEvent::InteractionResolved { .. } => AgentProgressEventKindV1::InteractionResolved,
        ProviderEvent::SubagentStarted { .. } => AgentProgressEventKindV1::SubagentStarted,
        ProviderEvent::SubagentStopped { .. } => AgentProgressEventKindV1::SubagentStopped,
        ProviderEvent::RateLimited { .. } => AgentProgressEventKindV1::RateLimited,
        ProviderEvent::ContextWindowUsage { .. } => unreachable!("handled above"),
    })
}

fn observation_evidence(family: AdapterFamily) -> Option<ObservationEvidenceV1> {
    match family {
        AdapterFamily::PtySemantic => Some(ObservationEvidenceV1::PtyHint),
        AdapterFamily::Hook | AdapterFamily::ManagedHook => {
            Some(ObservationEvidenceV1::ManagedHook)
        }
        AdapterFamily::Pipe | AdapterFamily::OneShot | AdapterFamily::Acp => {
            Some(ObservationEvidenceV1::StructuredProvider)
        }
        AdapterFamily::History
        | AdapterFamily::Resume
        | AdapterFamily::SessionOptions
        | AdapterFamily::CapabilityProbe => None,
    }
}

fn observation_source_capabilities(
    source: &gate4agent_types::ProviderSource,
) -> (ObservationSourceFamilyV1, ObservationCapabilitiesV1) {
    let adapter = source.binding.id.as_str();
    match source.family {
        AdapterFamily::PtySemantic => (
            ObservationSourceFamilyV1::PtySemantic,
            ObservationCapabilitiesV1::default(),
        ),
        AdapterFamily::Pipe => (
            ObservationSourceFamilyV1::Pipe,
            ObservationCapabilitiesV1 {
                tools: matches!(
                    adapter,
                    "claude-code" | "codex" | "gemini" | "opencode" | "kimi" | "qwen-code"
                ),
                attention: adapter == "qwen-code",
                usage: matches!(
                    adapter,
                    "claude-code" | "codex" | "gemini" | "opencode" | "qwen-code"
                ),
                ..ObservationCapabilitiesV1::default()
            },
        ),
        AdapterFamily::Hook => (
            ObservationSourceFamilyV1::Hook,
            hook_observation_capabilities(adapter),
        ),
        AdapterFamily::ManagedHook => (
            ObservationSourceFamilyV1::ManagedHook,
            hook_observation_capabilities(adapter),
        ),
        AdapterFamily::OneShot => (
            ObservationSourceFamilyV1::OneShot,
            ObservationCapabilitiesV1::default(),
        ),
        AdapterFamily::Acp => (
            ObservationSourceFamilyV1::Acp,
            ObservationCapabilitiesV1 {
                tools: true,
                usage: true,
                ..ObservationCapabilitiesV1::default()
            },
        ),
        AdapterFamily::History
        | AdapterFamily::Resume
        | AdapterFamily::SessionOptions
        | AdapterFamily::CapabilityProbe => unreachable!("non-live source has no observation evidence"),
    }
}

fn hook_observation_capabilities(adapter: &str) -> ObservationCapabilitiesV1 {
    ObservationCapabilitiesV1 {
        tools: matches!(
            adapter,
            "claude-code"
                | "codex"
                | "gemini"
                | "pi"
                | "omp"
                | "antigravity"
                | "amp"
                | "command-code"
                | "hermes"
                | "devin"
                | "grok"
                | "kimi"
                | "copilot"
                | "droid"
                | "cursor"
        ),
        attention: matches!(
            adapter,
            "claude-code"
                | "codex"
                | "opencode"
                | "mimo-code"
                | "antigravity"
                | "hermes"
                | "devin"
                | "grok"
                | "kimi"
                | "copilot"
                | "droid"
        ),
        subagents: matches!(
            adapter,
            "claude-code" | "grok" | "kimi" | "copilot" | "droid" | "cursor"
        ),
        ..ObservationCapabilitiesV1::default()
    }
}

fn token_usage_is_observed(usage: &gate4agent_types::TokenUsage) -> bool {
    usage.input_tokens != 0
        || usage.output_tokens != 0
        || usage.cache_read_tokens != 0
        || usage.cache_write_tokens != 0
        || usage.reasoning_tokens != 0
        || usage.context_window.is_some()
}

fn source_capabilities_observation(
    source: &gate4agent_types::ProviderSource,
    source_sequence: u64,
    evidence: ObservationEvidenceV1,
) -> ObservationV1 {
    let (source_family, capabilities) = observation_source_capabilities(source);
    ObservationV1 {
        source_sequence,
        observed_at_unix_ms: Some(unix_time_ms()),
        evidence,
        kind: ObservationKindV1::SourceCapabilities {
            source_family,
            source_adapter: source.binding.id.as_str().to_owned(),
            capabilities,
        },
        truncated: false,
    }
}

fn history_summary_observations(
    preview: &gate4agent_types::SessionRecordPreview,
) -> [ObservationV1; 2] {
    let observed_at_unix_ms = unix_time_ms().max(1);
    let source_sequence = preview
        .modified_at_unix_ms
        .unwrap_or(observed_at_unix_ms)
        .max(1);
    [
        ObservationV1 {
            source_sequence,
            observed_at_unix_ms: Some(observed_at_unix_ms),
            evidence: ObservationEvidenceV1::HistoryProjection,
            kind: ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::History,
                source_adapter: "native-history".to_owned(),
                capabilities: ObservationCapabilitiesV1 {
                    history_summary: true,
                    ..ObservationCapabilitiesV1::default()
                },
            },
            truncated: false,
        },
        ObservationV1 {
            source_sequence,
            observed_at_unix_ms: Some(observed_at_unix_ms),
            evidence: ObservationEvidenceV1::HistoryProjection,
            kind: ObservationKindV1::HistorySnapshot {
                message_count: preview.message_count,
                message_count_exact: preview.message_count_exact,
                completed_turn_count: preview.completed_turn_count,
                total_tokens: preview.total_tokens,
            },
            // The observation intentionally carries aggregate metrics only.
            // Omitted preview messages therefore do not make this aggregate partial.
            truncated: false,
        },
    ]
}

fn observation_tool_class(name: &str) -> String {
    let mut truncated = false;
    sanitize_progress_tool_label(name, &mut truncated).unwrap_or_else(|| "Tool".to_owned())
}

fn opaque_subagent_correlation(
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    source: &gate4agent_types::ProviderSource,
    provider_agent_id: &str,
) -> String {
    let mut material = Vec::with_capacity(16 + provider_agent_id.len());
    material.extend_from_slice(&instance_id.0.to_le_bytes());
    material.extend_from_slice(&generation.0.to_le_bytes());
    material.extend_from_slice(
        &serde_json::to_vec(source).expect("validated provider source must serialize"),
    );
    material.extend_from_slice(provider_agent_id.as_bytes());
    let digest = digest(&SHA256, &material);
    let mut correlation = String::with_capacity(20);
    correlation.push_str("sub-");
    for byte in &digest.as_ref()[..8] {
        use std::fmt::Write as _;
        write!(&mut correlation, "{byte:02x}").expect("writing to a String cannot fail");
    }
    correlation
}

fn opaque_tool_correlation(
    event: &ControlEvent,
    source: &gate4agent_types::ProviderSource,
    provider_tool_id: &str,
) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(&event.instance_id.0.to_le_bytes());
    material.extend_from_slice(&event.generation.0.to_le_bytes());
    material.extend_from_slice(
        &serde_json::to_vec(source).expect("validated provider source must serialize"),
    );
    material.extend_from_slice(provider_tool_id.as_bytes());
    opaque_correlation("tool-", &material)
}

fn opaque_interaction_correlation(event: &ControlEvent, interaction_id: u64) -> String {
    let mut material = Vec::with_capacity(24);
    material.extend_from_slice(b"interaction");
    material.extend_from_slice(&event.instance_id.0.to_le_bytes());
    material.extend_from_slice(&event.generation.0.to_le_bytes());
    material.extend_from_slice(&interaction_id.to_le_bytes());
    opaque_correlation("int-", &material)
}

fn opaque_correlation(prefix: &str, material: &[u8]) -> String {
    let digest = digest(&SHA256, material);
    let mut correlation = String::with_capacity(prefix.len() + 16);
    correlation.push_str(prefix);
    for byte in &digest.as_ref()[..8] {
        use std::fmt::Write as _;
        write!(&mut correlation, "{byte:02x}").expect("writing to a String cannot fail");
    }
    correlation
}

fn observation_interaction_outcome(
    outcome: gate4agent_types::ProviderInteractionOutcome,
) -> ObservationInteractionOutcomeV1 {
    match outcome {
        gate4agent_types::ProviderInteractionOutcome::Approved => {
            ObservationInteractionOutcomeV1::Approved
        }
        gate4agent_types::ProviderInteractionOutcome::Answered => {
            ObservationInteractionOutcomeV1::Answered
        }
        gate4agent_types::ProviderInteractionOutcome::Denied => {
            ObservationInteractionOutcomeV1::Denied
        }
        gate4agent_types::ProviderInteractionOutcome::Interrupted => {
            ObservationInteractionOutcomeV1::Interrupted
        }
        gate4agent_types::ProviderInteractionOutcome::TurnEnded => {
            ObservationInteractionOutcomeV1::TurnEnded
        }
        gate4agent_types::ProviderInteractionOutcome::Superseded => {
            ObservationInteractionOutcomeV1::Superseded
        }
    }
}

fn provider_observations(event: &ControlEvent) -> Vec<ObservationV1> {
    let (source, source_sequence, provider_sequence, provider_event) = match &event.event {
        ControlEventKind::ProviderEvent {
            sequence,
            source,
            source_sequence,
            event,
        } => (source, *source_sequence, *sequence, event),
        ControlEventKind::ProviderGap {
            source,
            source_sequence,
            missed,
            ..
        } => {
            let Some(evidence) = observation_evidence(source.family) else {
                return Vec::new();
            };
            return vec![
                source_capabilities_observation(source, *source_sequence, evidence),
                ObservationV1 {
                    source_sequence: *source_sequence,
                    observed_at_unix_ms: Some(unix_time_ms()),
                    evidence,
                    kind: ObservationKindV1::Gap { missed: *missed },
                    truncated: false,
                },
            ];
        }
        ControlEventKind::InteractionResolved {
            interaction_id,
            outcome,
        } => {
            return vec![ObservationV1 {
                source_sequence: event.sequence,
                observed_at_unix_ms: Some(unix_time_ms()),
                evidence: ObservationEvidenceV1::NodeLifecycle,
                kind: ObservationKindV1::InteractionResolved {
                    correlation_id: opaque_interaction_correlation(event, interaction_id.0),
                    outcome: observation_interaction_outcome(*outcome),
                },
                truncated: false,
            }];
        }
        _ => return node_lifecycle_observations(event),
    };
    let Some(evidence) = observation_evidence(source.family) else {
        return Vec::new();
    };
    let is_pty_hint = evidence == ObservationEvidenceV1::PtyHint;
    let mut kinds = Vec::with_capacity(2);
    match provider_event {
        ProviderEvent::SessionStarted { .. } => kinds.push(ObservationKindV1::SessionStarted),
        ProviderEvent::TurnStarted { .. } => kinds.push(ObservationKindV1::TurnStarted),
        ProviderEvent::WorkingObserved => kinds.push(ObservationKindV1::Working),
        ProviderEvent::Thinking { .. } => kinds.push(ObservationKindV1::Working),
        ProviderEvent::ToolStarted { id, name, .. } => kinds.push(ObservationKindV1::ToolStarted {
            correlation_id: opaque_tool_correlation(event, source, id),
            class: observation_tool_class(name),
        }),
        ProviderEvent::ToolCompleted { id, is_error, duration_ms, .. } if !is_pty_hint => {
            kinds.push(ObservationKindV1::ToolCompleted {
                correlation_id: opaque_tool_correlation(event, source, id),
                class: "Tool".to_owned(),
                success: !is_error,
                duration_ms: *duration_ms,
            });
        }
        ProviderEvent::TurnCompleted { usage, is_cumulative } if !is_pty_hint => {
            kinds.push(ObservationKindV1::TurnCompleted);
            if token_usage_is_observed(usage) {
                kinds.push(ObservationKindV1::Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                    context_window: usage.context_window,
                    is_cumulative: *is_cumulative,
                });
            }
        }
        ProviderEvent::ContextWindowUsage { usage }
            if evidence == ObservationEvidenceV1::StructuredProvider =>
        {
            kinds.push(ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: usage.uncached_input_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                output_tokens: usage.output_tokens,
                unattributed_tokens: usage.unattributed_tokens,
                used_tokens: usage.used_tokens,
                capacity_tokens: usage.capacity_tokens,
            });
        }
        ProviderEvent::TurnInterrupted => kinds.push(ObservationKindV1::TurnInterrupted),
        ProviderEvent::SessionEnded { is_error, .. } => kinds.push(ObservationKindV1::Exited {
            success: Some(!is_error),
        }),
        ProviderEvent::Error { .. } => kinds.push(ObservationKindV1::Error {
            detail: "provider-error".to_owned(),
        }),
        ProviderEvent::Ready => kinds.push(ObservationKindV1::Ready),
        ProviderEvent::InteractionRequested {
            interaction_kind,
            tool_name,
            ..
        } => kinds.push(match interaction_kind {
            ProviderInteractionKind::Approval => ObservationKindV1::ApprovalRequested {
                correlation_id: opaque_interaction_correlation(event, provider_sequence),
                tool_class: observation_tool_class(tool_name),
            },
            ProviderInteractionKind::Question => ObservationKindV1::QuestionRequested {
                correlation_id: opaque_interaction_correlation(event, provider_sequence),
                tool_class: observation_tool_class(tool_name),
            },
        }),
        ProviderEvent::SubagentStarted { agent_id, agent_type, .. } => {
            kinds.push(ObservationKindV1::SubagentStarted {
                correlation_id: opaque_subagent_correlation(
                    event.instance_id,
                    event.generation,
                    source,
                    agent_id,
                ),
                class: agent_type
                    .as_deref()
                    .map(observation_tool_class)
                    .unwrap_or_else(|| "Task".to_owned()),
            });
        }
        ProviderEvent::SubagentStopped { agent_id } if !is_pty_hint => {
            kinds.push(ObservationKindV1::SubagentCompleted {
                correlation_id: opaque_subagent_correlation(
                    event.instance_id,
                    event.generation,
                    source,
                    agent_id,
                ),
                success: None,
            });
        }
        ProviderEvent::RateLimited { .. } => kinds.push(ObservationKindV1::RateLimited),
        ProviderEvent::SessionIdentityObserved { .. }
        | ProviderEvent::Text { .. }
        | ProviderEvent::InteractionResolved { .. }
        | ProviderEvent::ToolCompleted { .. }
        | ProviderEvent::TurnCompleted { .. }
        | ProviderEvent::ContextWindowUsage { .. }
        | ProviderEvent::SubagentStopped { .. } => {}
    }
    let reports_capabilities = source_sequence == 1
        || matches!(provider_event, ProviderEvent::SessionStarted { .. });
    let mut observations = kinds.into_iter().map(|kind| ObservationV1 {
            source_sequence,
            observed_at_unix_ms: Some(unix_time_ms()),
            evidence,
            kind,
            truncated: false,
        })
        .filter(|observation| observation.validate().is_ok())
        .collect::<Vec<_>>();
    // The observation engine coalesces this declaration by source and excludes it
    // from timeline rows. Emit it with the first source receipt (or an explicit
    // session restart), while gaps carry their own declaration for repair.
    if reports_capabilities {
        observations.insert(0, source_capabilities_observation(source, source_sequence, evidence));
    }
    observations
}

fn opaque_process_correlation(
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
) -> String {
    let mut material = Vec::with_capacity(24);
    material.extend_from_slice(b"provider-session");
    material.extend_from_slice(&instance_id.0.to_le_bytes());
    material.extend_from_slice(&generation.0.to_le_bytes());
    let digest = digest(&SHA256, &material);
    let mut correlation = String::with_capacity(21);
    correlation.push_str("proc-");
    for byte in &digest.as_ref()[..8] {
        use std::fmt::Write as _;
        write!(&mut correlation, "{byte:02x}").expect("writing to a String cannot fail");
    }
    correlation
}

fn node_lifecycle_observations(event: &ControlEvent) -> Vec<ObservationV1> {
    let correlation_id = opaque_process_correlation(event.instance_id, event.generation);
    let kinds = match &event.event {
        ControlEventKind::Running { .. } => vec![
            ObservationKindV1::SessionStarted,
            ObservationKindV1::OwnedProcessStarted {
                correlation_id,
                class: "provider-session".to_owned(),
            },
        ],
        ControlEventKind::Exited { exit_code, .. } => vec![
            ObservationKindV1::OwnedProcessExited {
                correlation_id,
                success: exit_code.map(|code| code == 0),
                exit_code: *exit_code,
            },
            ObservationKindV1::Exited {
                success: exit_code.map(|code| code == 0),
            },
        ],
        ControlEventKind::Failed { .. } => vec![
            ObservationKindV1::OwnedProcessExited {
                correlation_id,
                success: Some(false),
                exit_code: None,
            },
            ObservationKindV1::Error {
                detail: "session-failed".to_owned(),
            },
        ],
        ControlEventKind::Removed => vec![ObservationKindV1::Stopped],
        _ => return Vec::new(),
    };
    std::iter::once(ObservationV1 {
        source_sequence: event.sequence,
        observed_at_unix_ms: Some(unix_time_ms()),
        evidence: ObservationEvidenceV1::NodeLifecycle,
        kind: ObservationKindV1::SourceCapabilities {
            source_family: ObservationSourceFamilyV1::NodeLifecycle,
            source_adapter: "node".to_owned(),
            capabilities: ObservationCapabilitiesV1 {
                owned_processes: true,
                ..ObservationCapabilitiesV1::default()
            },
        },
        truncated: false,
    })
        .chain(kinds.into_iter().map(|kind| ObservationV1 {
            source_sequence: event.sequence,
            observed_at_unix_ms: Some(unix_time_ms()),
            evidence: ObservationEvidenceV1::NodeLifecycle,
            kind,
            truncated: false,
        }))
        .filter(|observation| observation.validate().is_ok())
        .collect()
}

async fn run_native_session_catalog_operation<T, E, F>(
    catalog: Arc<Mutex<NativeSessionCatalogAuthority>>,
    operation: &'static str,
    execute: F,
) -> Result<T, NodeFailure>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(&mut NativeSessionCatalogAuthority) -> Result<T, E> + Send + 'static,
{
    let task = tokio::task::spawn_blocking(move || {
        let mut catalog = match catalog.try_lock() {
            Ok(catalog) => catalog,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    "native session catalog operation is already running",
                ));
            }
        };
        execute(&mut catalog).map_err(|error| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                &format!("{operation} failed: {error}"),
            )
        })
    });
    timeout(Duration::from_millis(NATIVE_SESSION_CATALOG_TIMEOUT_MS), task)
        .await
        .map_err(|_| {
            failure(
                NodeFailureCode::BackendBusy,
                &format!("{operation} exceeded its bounded deadline"),
            )
        })?
        .map_err(|_| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                &format!("{operation} task failed"),
            )
        })?
}

fn windows_path_text(path: &OpaqueHostPath) -> &str {
    path.as_utf8()
        .expect("Windows node only creates UTF-8 opaque host paths")
}

fn require_windows_path(path: OpaqueHostPath) -> Result<String, NodeFailure> {
    path.as_utf8()
        .map(str::to_owned)
        .ok_or_else(|| failure(
            NodeFailureCode::InvalidRequest,
            "this node requires UTF-8 host paths",
        ))
}

fn protocol_worktree(mut worktree: NativeGitWorktreeSnapshot) -> GitWorktreeSnapshot {
    GitWorktreeSnapshot {
        path: opaque_windows_path(worktree.path),
        head: worktree.head,
        branch: worktree.branch,
        is_bare: worktree.is_bare,
        is_main: worktree.is_main,
        locked: worktree.locked,
        lock_reason: worktree.lock_reason,
        prunable: worktree.prunable,
        prunable_reason: worktree.prunable_reason,
        workspace_id: worktree.workspace_id.take(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceConfig {
    workspace_id: WorkspaceId,
    canonical_root: String,
    worktree_service_mode: WorktreeServiceMode,
    managed_worktree_profiles: BTreeMap<WorktreeProfileId, ManagedWorktreeProfile>,
}

impl WorkspaceConfig {
    pub fn new(
        workspace_id: WorkspaceId,
        root: impl AsRef<Path>,
    ) -> Result<Self, NodeServerError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(NodeServerError::InvalidWorkspaceRoot {
                workspace_id,
                path: root.to_string_lossy().into_owned(),
                message: "path is not absolute".to_owned(),
            });
        }
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            NodeServerError::InvalidWorkspaceRoot {
                workspace_id: workspace_id.clone(),
                path: root.to_string_lossy().into_owned(),
                message: error.to_string(),
            }
        })?;
        if !canonical.is_dir() {
            return Err(NodeServerError::InvalidWorkspaceRoot {
                workspace_id,
                path: root.to_string_lossy().into_owned(),
                message: "path is not a directory".to_owned(),
            });
        }
        let canonical_root = canonical.into_os_string().into_string().map_err(|path| {
            NodeServerError::InvalidWorkspaceRoot {
                workspace_id: workspace_id.clone(),
                path: path.to_string_lossy().into_owned(),
                message: "canonical path is not valid Unicode".to_owned(),
            }
        })?;
        let canonical_root = platform::normalize_canonical_root(canonical_root);
        validate_workspace_root(&workspace_id, &canonical_root)?;
        Ok(Self {
            workspace_id,
            canonical_root,
            worktree_service_mode: WorktreeServiceMode::Manual,
            managed_worktree_profiles: BTreeMap::new(),
        })
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn canonical_root(&self) -> &str {
        &self.canonical_root
    }

    pub fn with_worktree_service_mode(mut self, mode: WorktreeServiceMode) -> Self {
        self.worktree_service_mode = mode;
        self
    }

    pub fn with_managed_worktree_profile(
        mut self,
        profile: ManagedWorktreeProfile,
    ) -> Result<Self, NodeServerError> {
        profile.validate_for_workspace(&self.canonical_root).map_err(|message| {
            NodeServerError::InvalidManagedWorktreeProfile {
                workspace_id: self.workspace_id.clone(),
                profile_id: profile.profile_id().clone(),
                message,
            }
        })?;
        if self.managed_worktree_profiles.contains_key(profile.profile_id()) {
            return Err(NodeServerError::DuplicateManagedWorktreeProfile {
                workspace_id: self.workspace_id,
                profile_id: profile.profile_id().clone(),
            });
        }
        if self.managed_worktree_profiles.len()
            == MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE
        {
            return Err(NodeServerError::ManagedWorktreeProfileCapacity {
                workspace_id: self.workspace_id,
                max: MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE,
            });
        }
        self.managed_worktree_profiles
            .insert(profile.profile_id().clone(), profile);
        Ok(self)
    }
}

#[derive(Clone)]
pub struct NodeServerConfig {
    pub endpoint: String,
    api_listen: Option<std::net::SocketAddr>,
    pub node_id: NodeId,
    pub workspaces: Vec<WorkspaceConfig>,
    access_token: String,
    pub runtime: NativeRuntimeConfig,
    state_path: Option<PathBuf>,
    spawn_profiles: SpawnProfileRegistry,
    session_environment: Option<NodeSessionEnvironmentConfig>,
    history: Option<NativeHistoryConfig>,
    harness_mcp_helper: Option<ReviewedHarnessMcpProgram>,
    #[cfg(feature = "fixture")]
    fixture_raw_pty_runtime: bool,
}

#[derive(Clone)]
struct NodeSessionEnvironmentConfig {
    root: PathBuf,
    resolver: Arc<dyn NodeSecretResolver>,
}

impl NodeServerConfig {
    pub fn new(
        endpoint: impl Into<String>,
        access_token: impl Into<String>,
        node_id: NodeId,
        workspaces: impl IntoIterator<Item = WorkspaceConfig>,
    ) -> Result<Self, NodeServerError> {
        let endpoint = endpoint.into();
        let access_token = access_token.into();
        if !platform::validate_endpoint(&endpoint) {
            return Err(NodeServerError::InvalidEndpoint);
        }
        if access_token.is_empty() || access_token.len() > 4_096 {
            return Err(NodeServerError::InvalidAccessToken);
        }
        let workspaces = workspaces.into_iter().collect::<Vec<_>>();
        if workspaces.is_empty() {
            return Err(NodeServerError::NoWorkspaces);
        }
        let mut workspace_ids = BTreeMap::new();
        let mut workspace_roots = BTreeMap::new();
        for workspace in &workspaces {
            if workspace.worktree_service_mode == WorktreeServiceMode::Managed
                && workspace.managed_worktree_profiles.is_empty()
            {
                return Err(NodeServerError::ManagedWorktreeProfileRequired(
                    workspace.workspace_id.clone(),
                ));
            }
            if workspace.worktree_service_mode != WorktreeServiceMode::Managed
                && !workspace.managed_worktree_profiles.is_empty()
            {
                return Err(NodeServerError::ManagedWorktreeProfileModeMismatch(
                    workspace.workspace_id.clone(),
                ));
            }
            if workspace_ids
                .insert(workspace.workspace_id.clone(), ())
                .is_some()
            {
                return Err(NodeServerError::DuplicateWorkspaceId(
                    workspace.workspace_id.clone(),
                ));
            }
            let root_key = platform::root_identity(&workspace.canonical_root);
            if let Some(existing) = workspace_roots.insert(
                root_key,
                workspace.workspace_id.clone(),
            ) {
                return Err(NodeServerError::DuplicateWorkspaceRoot {
                    first: existing,
                    second: workspace.workspace_id.clone(),
                    root: workspace.canonical_root.clone(),
                });
            }
        }
        Ok(Self {
            endpoint,
            api_listen: None,
            node_id,
            workspaces,
            access_token,
            runtime: NativeRuntimeConfig::default(),
            state_path: None,
            spawn_profiles: SpawnProfileRegistry::default(),
            session_environment: None,
            history: None,
            harness_mcp_helper: None,
            #[cfg(feature = "fixture")]
            fixture_raw_pty_runtime: false,
        })
    }

    pub fn with_api_listen(
        mut self,
        api_listen: std::net::SocketAddr,
    ) -> Result<Self, NodeServerError> {
        if !api_listen.ip().is_loopback() {
            return Err(NodeServerError::InvalidApiListen(api_listen));
        }
        self.api_listen = Some(api_listen);
        Ok(self)
    }

    pub fn with_state_path(
        mut self,
        state_path: impl Into<PathBuf>,
    ) -> Result<Self, NodeServerError> {
        let state_path = state_path.into();
        if !state_path.is_absolute() || state_path.file_name().is_none() {
            return Err(NodeServerError::InvalidStatePath(
                state_path.to_string_lossy().into_owned(),
            ));
        }
        self.state_path = Some(state_path);
        Ok(self)
    }

    pub fn with_spawn_profiles(mut self, spawn_profiles: SpawnProfileRegistry) -> Self {
        self.spawn_profiles = spawn_profiles;
        self
    }

    pub fn with_session_environment_materialization(
        mut self,
        root: impl Into<PathBuf>,
        resolver: Arc<dyn NodeSecretResolver>,
    ) -> Result<Self, NodeServerError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(NodeServerError::InvalidSessionEnvironmentRoot);
        }
        self.session_environment = Some(NodeSessionEnvironmentConfig { root, resolver });
        Ok(self)
    }

    /// Enables bounded provider-history discovery and loading from explicit
    /// host-owned roots. Ambient vendor homes are never inferred here.
    pub fn with_history(mut self, history: NativeHistoryConfig) -> Self {
        self.history = Some(history);
        self
    }

    /// Enables the H3B read proxy with one exact operator-reviewed helper file.
    pub fn with_harness_mcp_helper(
        mut self,
        helper_program: impl Into<PathBuf>,
    ) -> Result<Self, NodeServerError> {
        #[cfg(not(windows))]
        {
            let _ = helper_program;
            return Err(NodeServerError::InvalidHarnessMcpHelper);
        }
        #[cfg(windows)]
        {
        self.harness_mcp_helper = Some(
            ReviewedHarnessMcpProgram::review(helper_program.into())
                .map_err(|_| NodeServerError::InvalidHarnessMcpHelper)?,
        );
        Ok(self)
        }
    }
}

pub fn default_state_path(node_id: &NodeId) -> Result<PathBuf, NodeServerError> {
    platform::default_state_path(node_id.as_str())
        .ok_or(NodeServerError::LocalStateDirectoryUnavailable)
}

pub fn default_node_endpoint() -> Result<PathBuf, NodeServerError> {
    platform::default_node_endpoint().ok_or(NodeServerError::LocalRuntimeDirectoryUnavailable)
}

fn delivery_store_root_for_state_path(state_path: &Path) -> Option<PathBuf> {
    let parent = state_path.parent()?;
    let mut name = state_path.file_name()?.to_os_string();
    name.push(".delivery-store");
    Some(parent.join(name))
}

fn context_pack_store_root_for_state_path(state_path: &Path) -> Option<PathBuf> {
    let parent = state_path.parent()?;
    let mut name = state_path.file_name()?.to_os_string();
    name.push(".context-pack-store");
    Some(parent.join(name))
}

pub struct NodeServer {
    config: NodeServerConfig,
    runtime: NativeRuntime,
    shared: Arc<NodeShared>,
    events: EventSubscription,
    state_path_lock: Option<session_registry::StatePathLock>,
}

#[cfg(feature = "fixture")]
#[derive(Clone, Default)]
pub struct SpawnManagedWorktreeV2FailureProbe {
    latest_code: Arc<Mutex<Option<NodeFailureCode>>>,
}

#[cfg(feature = "fixture")]
impl SpawnManagedWorktreeV2FailureProbe {
    pub fn latest_code(&self) -> Option<NodeFailureCode> {
        *self
            .latest_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record(&self, code: Option<NodeFailureCode>) {
        *self
            .latest_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = code;
    }
}

#[cfg(all(feature = "fixture", windows))]
struct IsolatedCodexHomeFixtureEnvironment;

#[cfg(all(feature = "fixture", windows))]
impl gate4agent_runtime_native::NativeChildEnvironmentResolver
    for IsolatedCodexHomeFixtureEnvironment
{
    fn resolve_child_environment(
        &self,
    ) -> Result<
        Vec<gate4agent_catalog::EnvMutation>,
        gate4agent_runtime_native::NativeChildEnvironmentResolveError,
    > {
        Ok(vec![gate4agent_catalog::EnvMutation {
            key: OsString::from("USERPROFILE"),
            value: None,
        }])
    }
}

impl NodeServer {
    pub fn new(config: NodeServerConfig) -> Result<Self, NodeServerError> {
        let catalog = active_registry()?;
        Self::new_with_registry(config, catalog)
    }

    #[cfg(feature = "fixture")]
    pub fn new_fixture(config: NodeServerConfig) -> Result<Self, NodeServerError> {
        let mut spec = gate4agent_testkit::interactive_agent_spec();
        spec.id = AgentId::new("claude").map_err(|error| NodeServerError::Registry(error.to_string()))?;
        spec.display_name = "Controlled Claude PTY fixture".to_owned();
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(feature = "fixture")]
    pub fn new_clean_exit_fixture(
        config: NodeServerConfig,
        fixture_root: PathBuf,
        started_marker: PathBuf,
        release_signal: PathBuf,
    ) -> Result<Self, NodeServerError> {
        let mut spec = gate4agent_testkit::controlled_clean_exit_agent_spec(
            &fixture_root,
            &started_marker,
            &release_signal,
        )
        .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        spec.id = AgentId::new("claude")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        spec.display_name = "Controlled Claude clean-exit fixture".to_owned();
        Self::new_fixture_with_spec(config, spec)
    }

    /// A `claude` source whose session establishes a verified provider
    /// identity via an early `SessionStart` Hook post (so it is eligible for
    /// auto-export-at-exit and its native history is independently
    /// loadable), waits for `release_signal`, then exits cleanly — paired
    /// on the same Node with a plain `codex` target that never validates
    /// anything about what it receives. Built for A2 durable ContextPack
    /// E2E coverage: the test itself inspects materialized files and
    /// managed-session-record fields directly, rather than having the
    /// target script self-validate.
    #[cfg(all(feature = "fixture", windows))]
    pub fn new_durable_context_pack_clean_exit_fixture(
        config: NodeServerConfig,
        fixture_root: PathBuf,
        started_marker: PathBuf,
        release_signal: PathBuf,
        session_id: &str,
    ) -> Result<Self, NodeServerError> {
        let mut source_spec = gate4agent_testkit::identified_clean_exit_agent_spec(
            &fixture_root,
            &started_marker,
            &release_signal,
            session_id,
        )
        .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let source_id = AgentId::new("claude")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let source_provider = builtin_registry().get(&source_id).ok_or_else(|| {
            NodeServerError::Registry(
                "durable context pack clean-exit fixture source adapter is unavailable"
                    .to_owned(),
            )
        })?;
        source_spec.capabilities.adapters.history =
            source_provider.capabilities.adapters.history.clone();
        source_spec.id = source_id;
        source_spec.display_name =
            "Controlled Claude durable context-pack clean-exit fixture".to_owned();
        // Both specs launch via `powershell.exe`; the registry's ambiguity
        // check keys on `detection.command` (+ aliases), not `launch.program`
        // -- give each a distinct detection identity, same precedent as
        // `context_pack_fixture_catalog`'s per-provider
        // `gate4agent-{provider}-context-fixture.cmd` commands.
        source_spec.detection.command =
            "gate4agent-claude-durable-context-pack-fixture.cmd".to_owned();
        source_spec.detection.aliases.clear();

        let mut target_spec = gate4agent_testkit::interactive_agent_spec();
        target_spec.id = AgentId::new("codex")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        target_spec.display_name =
            "Controlled Codex durable context-pack target fixture".to_owned();
        target_spec.detection.command =
            "gate4agent-codex-durable-context-pack-fixture.cmd".to_owned();
        target_spec.detection.aliases.clear();

        let catalog = AgentRegistry::new([source_spec, target_spec])
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let mut server = Self::new_with_registry(config, catalog)?;
        Arc::get_mut(&mut server.shared)
            .expect("fixture Node shared state must remain uniquely owned before run")
            .fixture_semantic_hook_policy = true;
        Ok(server)
    }

    #[cfg(feature = "fixture")]
    pub fn new_monitoring_hook_fixture(
        config: NodeServerConfig,
    ) -> Result<Self, NodeServerError> {
        let mut spec = gate4agent_testkit::monitoring_hook_agent_spec();
        spec.id = AgentId::new("claude")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let mut server = Self::new_fixture_with_spec(config, spec)?;
        Arc::get_mut(&mut server.shared)
            .expect("fixture Node shared state must remain uniquely owned before run")
            .fixture_semantic_hook_policy = true;
        Ok(server)
    }

    #[cfg(all(feature = "fixture", windows))]
    pub fn new_provider_bundle_argv_fixture(
        config: NodeServerConfig,
        agent_id: AgentId,
        proof_path: PathBuf,
    ) -> Result<Self, NodeServerError> {
        let spec = Self::provider_bundle_argv_fixture_spec(agent_id, proof_path, None)?;
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(all(feature = "fixture", windows))]
    pub fn new_provider_bundle_argv_hold_fixture(
        config: NodeServerConfig,
        agent_id: AgentId,
        proof_path: PathBuf,
        release_signal: PathBuf,
    ) -> Result<Self, NodeServerError> {
        let spec = Self::provider_bundle_argv_fixture_spec(
            agent_id,
            proof_path,
            Some(release_signal),
        )?;
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(all(feature = "fixture", windows))]
    fn provider_bundle_argv_fixture_spec(
        agent_id: AgentId,
        proof_path: PathBuf,
        release_signal: Option<PathBuf>,
    ) -> Result<gate4agent_types::AgentSpec, NodeServerError> {
        if !matches!(agent_id.as_str(), "claude" | "codex" | "kimi") {
            return Err(NodeServerError::Registry(
                "provider bundle argv fixture requires Claude, Codex, or Kimi".to_owned(),
            ));
        }
        let mut spec = gate4agent_testkit::interactive_agent_spec();
        spec.id = agent_id.clone();
        spec.display_name = format!("Controlled {agent_id} bundle argv fixture");
        let provider = builtin_registry()
            .get(&agent_id)
            .ok_or_else(|| {
                NodeServerError::Registry(
                    "provider bundle argv fixture adapter is unavailable".to_owned(),
                )
            })?;
        spec.capabilities.adapters.history = provider.capabilities.adapters.history.clone();
        if !proof_path.is_absolute()
            || !proof_path
                .parent()
                .is_some_and(|parent| parent.is_dir())
        {
            return Err(NodeServerError::Registry(
                "provider bundle argv proof path requires an existing absolute parent"
                    .to_owned(),
            ));
        }
        if let Some(release_signal) = release_signal.as_ref() {
            if !release_signal.is_absolute()
                || !release_signal
                    .parent()
                    .is_some_and(|parent| parent.is_dir())
            {
                return Err(NodeServerError::Registry(
                    "provider bundle argv release path requires an existing absolute parent"
                        .to_owned(),
                ));
            }
            if release_signal.exists() {
                return Err(NodeServerError::Registry(
                    "provider bundle argv release path must not exist".to_owned(),
                ));
            }
            let proof_parent = std::fs::canonicalize(
                proof_path
                    .parent()
                    .expect("validated provider bundle proof parent"),
            )
            .map_err(|_| {
                NodeServerError::Registry(
                    "provider bundle argv proof parent is unavailable".to_owned(),
                )
            })?;
            let release_parent = std::fs::canonicalize(
                release_signal
                    .parent()
                    .expect("validated provider bundle release parent"),
            )
            .map_err(|_| {
                NodeServerError::Registry(
                    "provider bundle argv release parent is unavailable".to_owned(),
                )
            })?;
            if proof_parent != release_parent {
                return Err(NodeServerError::Registry(
                    "provider bundle argv release path must share the proof parent".to_owned(),
                ));
            }
        }
        let proof_path = proof_path.into_os_string().into_string().map_err(|_| {
            NodeServerError::Registry(
                "provider bundle argv proof path is not valid Unicode".to_owned(),
            )
        })?;
        let release_signal = release_signal.map(|release_signal| {
            release_signal.into_os_string().into_string().map_err(|_| {
                NodeServerError::Registry(
                    "provider bundle argv release path is not valid Unicode".to_owned(),
                )
            })
        }).transpose()?;
        let script = spec
            .launch
            .fixed_args
            .pop()
            .ok_or_else(|| NodeServerError::Registry("PTY fixture script is unavailable".to_owned()))?;
        let bundle_validation = if agent_id.as_str() == "codex" {
            "if ($bundleArgs.Count -ne 0) { exit 91 }; $codexHome = [Environment]::GetEnvironmentVariable('CODEX_HOME', 'Process'); $cwd = (Get-Location).ProviderPath; if ([string]::IsNullOrEmpty($codexHome)) { [IO.File]::WriteAllLines($proofPath, [string[]]@('unbundled', $cwd)); [Console]::Out.WriteLine('F63_CODEX_UNBUNDLED_VALIDATED') } elseif (-not [IO.Path]::IsPathRooted($codexHome) -or -not (Test-Path -LiteralPath $codexHome -PathType Container)) { exit 92 } else { $skillPath = Join-Path $codexHome 'skills/review-code/SKILL.md'; if (-not (Test-Path -LiteralPath $skillPath -PathType Leaf)) { exit 93 }; $files = @(Get-ChildItem -LiteralPath $codexHome -Recurse -Force -File); if ($files.Count -ne 1 -or [IO.Path]::GetFullPath($files[0].FullName) -ine [IO.Path]::GetFullPath($skillPath)) { exit 94 }; $sha = [Security.Cryptography.SHA256]::Create(); try { $skillHash = ([BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($skillPath)))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }; if ($skillHash -cne 'd78fe03be106b673fcf5415c8359e99f0298b5071cd11822c58ebcb27e52a68d') { exit 95 }; [IO.File]::WriteAllLines($proofPath, [string[]]@('bundled', $codexHome, $cwd, $skillHash)); [Console]::Out.WriteLine('F63_CODEX_BUNDLE_HOME_VALIDATED') }"
        } else if agent_id.as_str() == "claude" {
            "if ($bundleArgs.Count -eq 0) { [IO.File]::WriteAllLines($proofPath, [string[]]@()); [Console]::Out.WriteLine('F62_NO_BUNDLE_ARGV') } elseif ($bundleArgs.Count -eq 2 -and $bundleArgs[0] -ceq '--plugin-dir' -and [IO.Path]::IsPathRooted($bundleArgs[1]) -and (Test-Path -LiteralPath $bundleArgs[1] -PathType Container)) { if (-not (Test-Path -LiteralPath (Join-Path $bundleArgs[1] '.claude-plugin/plugin.json') -PathType Leaf)) { exit 92 }; [IO.File]::WriteAllLines($proofPath, [string[]]@($bundleArgs[0], $bundleArgs[1])); [Console]::Out.WriteLine('F62_BUNDLE_ARGV_VALIDATED') } else { exit 91 }"
        } else {
            "if ($bundleArgs.Count -eq 0) { [IO.File]::WriteAllLines($proofPath, [string[]]@()); [Console]::Out.WriteLine('F62_NO_BUNDLE_ARGV') } elseif ($bundleArgs.Count -eq 2 -and $bundleArgs[0] -ceq '--skills-dir' -and [IO.Path]::IsPathRooted($bundleArgs[1]) -and (Test-Path -LiteralPath $bundleArgs[1] -PathType Container)) { if ((Split-Path -Leaf $bundleArgs[1]) -ne 'skills') { exit 93 }; [IO.File]::WriteAllLines($proofPath, [string[]]@($bundleArgs[0], $bundleArgs[1])); [Console]::Out.WriteLine('F62_BUNDLE_ARGV_VALIDATED') } else { exit 91 }"
        };
        if let Some(release_signal) = release_signal {
            spec.launch.fixed_args.push(format!(
                "& {{ param([string]$proofPath, [string]$releaseSignal, [Parameter(ValueFromRemainingArguments=$true)][string[]]$bundleArgs) {bundle_validation}; $deadline = [DateTime]::UtcNow.AddSeconds(45); while (-not (Test-Path -LiteralPath $releaseSignal -PathType Leaf)) {{ if ([DateTime]::UtcNow -ge $deadline) {{ exit 96 }}; Start-Sleep -Milliseconds 20 }}; {script} }}",
            ));
            spec.launch.fixed_args.push(proof_path);
            spec.launch.fixed_args.push(release_signal);
        } else {
            spec.launch.fixed_args.push(format!(
                "& {{ param([string]$proofPath, [Parameter(ValueFromRemainingArguments=$true)][string[]]$bundleArgs) {bundle_validation}; {script} }}",
            ));
            spec.launch.fixed_args.push(proof_path);
        }
        Ok(spec)
    }

    #[cfg(all(feature = "fixture", windows))]
    pub fn new_context_pack_fixture(
        config: NodeServerConfig,
        proof_path: PathBuf,
    ) -> Result<Self, NodeServerError> {
        let catalog = Self::context_pack_fixture_catalog(proof_path, None, false)?;
        Self::new_with_registry(config, catalog)
    }

    #[cfg(all(feature = "fixture", windows))]
    pub fn new_monitoring_context_pack_fixture(
        config: NodeServerConfig,
        proof_path: PathBuf,
    ) -> Result<Self, NodeServerError> {
        let catalog = Self::context_pack_fixture_catalog(proof_path, None, true)?;
        let mut server = Self::new_with_registry(config, catalog)?;
        Arc::get_mut(&mut server.shared)
            .expect("fixture Node shared state must remain uniquely owned before run")
            .fixture_semantic_hook_policy = true;
        Ok(server)
    }

    #[cfg(all(feature = "fixture", windows))]
    pub fn install_isolated_codex_home_environment_fixture(
        &mut self,
        profile_id: SpawnEnvironmentProfileId,
        profile_revision: crate::protocol::SpawnEnvironmentProfileRevision,
    ) -> Result<(), NodeServerError> {
        let provider = AgentId::new("codex")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let native_profile = gate4agent_runtime_native::NativeLaunchProfile::new(
            gate4agent_runtime_native::NativeLaunchProfileId::new(format!(
                "{}-pty",
                profile_id.as_str(),
            ))
            .map_err(|error| NodeServerError::Registry(error.to_string()))?,
            provider.clone(),
            TransportKind::Pty,
            vec![OsString::from("USERPROFILE")],
            Arc::new(IsolatedCodexHomeFixtureEnvironment),
        )
        .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let materialization = NodeSessionMaterializationProfile::new(
            Vec::new(),
            vec![crate::session_environment::NodeSessionPathBinding::new(
                "CODEX_HOME",
                crate::session_environment::NodeSessionPathClass::ProviderHome,
            )
            .map_err(|error| NodeServerError::Registry(error.to_string()))?],
            Vec::new(),
        )
        .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        self.install_environment_profile(
            NodeEnvironmentProfile::new_with_materialization(
                profile_id,
                profile_revision,
                provider,
                [native_profile],
                Some(materialization),
            )
            .map_err(|error| NodeServerError::Registry(error.to_string()))?,
        )
    }

    #[cfg(all(feature = "fixture", windows))]
    pub fn new_context_only_proof_fixture(
        config: NodeServerConfig,
        target_provider: AgentId,
        proof_path: PathBuf,
    ) -> Result<Self, NodeServerError> {
        if target_provider.as_str() != "kimi" {
            return Err(NodeServerError::Registry(
                "context-only proof fixture requires Kimi".to_owned(),
            ));
        }
        let catalog = Self::context_pack_fixture_catalog(
            proof_path,
            Some(&target_provider),
            false,
        )?;
        Self::new_with_registry(config, catalog)
    }

    #[cfg(all(feature = "fixture", windows))]
    fn context_pack_fixture_catalog(
        proof_path: PathBuf,
        context_only_target: Option<&AgentId>,
        monitoring_claude_source: bool,
    ) -> Result<AgentRegistry, NodeServerError> {
        if !proof_path.is_absolute()
            || !proof_path
                .parent()
                .is_some_and(|parent| parent.is_dir())
        {
            return Err(NodeServerError::Registry(
                "context pack proof path requires an existing absolute parent".to_owned(),
            ));
        }
        let proof_path = proof_path.into_os_string().into_string().map_err(|_| {
            NodeServerError::Registry(
                "context pack proof path is not valid Unicode".to_owned(),
            )
        })?;
        let providers = ["claude", "codex", "grok", "kimi", "qwen-code"];
        let builtins = builtin_registry();
        let mut specs = Vec::with_capacity(providers.len());
        for provider_id in providers {
            let agent_id = AgentId::new(provider_id)
                .map_err(|error| NodeServerError::Registry(error.to_string()))?;
            let provider = builtins.get(&agent_id).ok_or_else(|| {
                NodeServerError::Registry(
                    "context pack fixture provider adapter is unavailable".to_owned(),
                )
            })?;
            let monitored_provider =
                monitoring_claude_source && matches!(provider_id, "claude" | "codex");
            let mut spec = if monitored_provider {
                gate4agent_testkit::monitoring_hook_agent_spec()
            } else {
                gate4agent_testkit::interactive_agent_spec()
            };
            spec.id = agent_id.clone();
            spec.display_name = format!("Controlled {agent_id} context pack fixture");
            spec.detection.command = format!("gate4agent-{provider_id}-context-fixture.cmd");
            spec.detection.aliases.clear();
            spec.capabilities.adapters.history = provider.capabilities.adapters.history.clone();
            if monitored_provider {
                spec.capabilities.adapters.hook = provider.capabilities.adapters.hook.clone();
            }
            if provider_id == "codex" {
                let script = spec.launch.fixed_args.pop().ok_or_else(|| {
                    NodeServerError::Registry("PTY fixture script is unavailable".to_owned())
                })?;
                let validation = r#"
$bundleArgs = @($bundleArgs)
if ($bundleArgs.Count -ne 0) { exit 91 }
$codexHome = [Environment]::GetEnvironmentVariable('CODEX_HOME', 'Process')
$contextRoot = [Environment]::GetEnvironmentVariable('GATE4AGENT_CONTEXT_ROOT', 'Process')
$cwd = (Get-Location).ProviderPath
if ([string]::IsNullOrEmpty($codexHome) -or -not [IO.Path]::IsPathRooted($codexHome) -or -not (Test-Path -LiteralPath $codexHome -PathType Container)) { exit 92 }
$skillPath = Join-Path $codexHome 'skills/review-code/SKILL.md'
if (-not (Test-Path -LiteralPath $skillPath -PathType Leaf)) { exit 93 }
$homeFiles = @(Get-ChildItem -LiteralPath $codexHome -Recurse -Force -File)
if ($homeFiles.Count -ne 1 -or [IO.Path]::GetFullPath($homeFiles[0].FullName) -ine [IO.Path]::GetFullPath($skillPath)) { exit 94 }
$sha = [Security.Cryptography.SHA256]::Create()
try { $skillHash = ([BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($skillPath)))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
if ($skillHash -cne 'd78fe03be106b673fcf5415c8359e99f0298b5071cd11822c58ebcb27e52a68d') { exit 95 }
if ([string]::IsNullOrEmpty($contextRoot) -or -not [IO.Path]::IsPathRooted($contextRoot) -or -not (Test-Path -LiteralPath $contextRoot -PathType Container)) { exit 96 }
$contextPath = Join-Path $contextRoot 'context-pack.json'
if (-not (Test-Path -LiteralPath $contextPath -PathType Leaf)) { exit 97 }
$contextFiles = @(Get-ChildItem -LiteralPath $contextRoot -Recurse -Force -File)
if ($contextFiles.Count -ne 1 -or [IO.Path]::GetFullPath($contextFiles[0].FullName) -ine [IO.Path]::GetFullPath($contextPath)) { exit 98 }
$contextFull = [IO.Path]::GetFullPath($contextRoot).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$codexFull = [IO.Path]::GetFullPath($codexHome).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$cwdFull = [IO.Path]::GetFullPath($cwd).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$separator = [IO.Path]::DirectorySeparatorChar
if ($contextFull.Equals($codexFull, [StringComparison]::OrdinalIgnoreCase) -or $contextFull.StartsWith($codexFull + $separator, [StringComparison]::OrdinalIgnoreCase)) { exit 99 }
if ($contextFull.Equals($cwdFull, [StringComparison]::OrdinalIgnoreCase) -or $contextFull.StartsWith($cwdFull + $separator, [StringComparison]::OrdinalIgnoreCase)) { exit 100 }
$rawContext = [IO.File]::ReadAllText($contextPath)
if ($rawContext.Contains('g4a-private-provider-session-canary') -or $rawContext.Contains('private-provider-login-identity')) { exit 101 }
try { $document = $rawContext | ConvertFrom-Json -ErrorAction Stop } catch { exit 102 }
$allowedTopLevel = @('schema', 'source_provider', 'source_message_count', 'retained_messages', 'repository', 'truncated')
$requiredTopLevel = @('schema', 'source_provider', 'source_message_count', 'retained_messages', 'truncated')
foreach ($property in @($document.PSObject.Properties.Name)) {
    if ($allowedTopLevel -cnotcontains [string]$property) { exit 103 }
}
foreach ($property in $requiredTopLevel) {
    if ($null -eq $document.PSObject.Properties[$property]) { exit 104 }
}
if ($document.schema -cne 'g4a-context-pack-v1' -or $null -ne $document.PSObject.Properties['cwd']) { exit 105 }
if (@('claude', 'codex', 'grok', 'kimi', 'qwen-code') -cnotcontains [string]$document.source_provider) { exit 106 }
$messages = @($document.retained_messages)
if ($messages.Count -eq 0) { exit 107 }
foreach ($message in $messages) {
    $messageProperties = @($message.PSObject.Properties.Name)
    foreach ($property in $messageProperties) {
        if (@('role', 'text') -cnotcontains [string]$property) { exit 108 }
    }
    if ($messageProperties.Count -ne 2 -or @('user', 'assistant') -cnotcontains [string]$message.role -or [string]::IsNullOrEmpty([string]$message.text)) { exit 109 }
}
$sha = [Security.Cryptography.SHA256]::Create()
try { $contextHash = ([BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($contextPath)))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
[IO.File]::WriteAllLines($proofPath, [string[]]@('contextual', $codexHome, $cwd, $skillHash, $contextRoot, $contextHash, [string]$document.schema, [string]$document.source_provider, [string]$messages.Count, [string]$messages[0].text, [string]$messages[$messages.Count - 1].text))
$rawProof = [IO.File]::ReadAllText($proofPath)
if ($rawProof.Contains('g4a-private-provider-session-canary') -or $rawProof.Contains('private-provider-login-identity')) { exit 110 }
[Console]::Out.WriteLine('F7_CODEX_BUNDLE_CONTEXT_VALIDATED')
"#;
                spec.launch.fixed_args.push(format!(
                    "& {{ param([string]$proofPath, [Parameter(ValueFromRemainingArguments=$true)][string[]]$bundleArgs) {validation}; {script} }}",
                ));
                spec.launch.fixed_args.push(proof_path.clone());
            } else if context_only_target.map(AgentId::as_str) == Some(provider_id) {
                let script = spec.launch.fixed_args.pop().ok_or_else(|| {
                    NodeServerError::Registry("PTY fixture script is unavailable".to_owned())
                })?;
                let validation = r#"
$bundleArgs = @($bundleArgs)
if ($bundleArgs.Count -ne 0) { exit 91 }
$contextRoot = [Environment]::GetEnvironmentVariable('GATE4AGENT_CONTEXT_ROOT', 'Process')
$cwd = (Get-Location).ProviderPath
if ([string]::IsNullOrEmpty($contextRoot) -or -not [IO.Path]::IsPathRooted($contextRoot) -or -not (Test-Path -LiteralPath $contextRoot -PathType Container)) { exit 92 }
$contextPath = Join-Path $contextRoot 'context-pack.json'
if (-not (Test-Path -LiteralPath $contextPath -PathType Leaf)) { exit 93 }
$entries = @(Get-ChildItem -LiteralPath $contextRoot -Force)
if ($entries.Count -ne 1 -or $entries[0] -isnot [IO.FileInfo] -or [IO.Path]::GetFullPath($entries[0].FullName) -ine [IO.Path]::GetFullPath($contextPath)) { exit 94 }
$contextFull = [IO.Path]::GetFullPath($contextRoot).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$cwdFull = [IO.Path]::GetFullPath($cwd).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$separator = [IO.Path]::DirectorySeparatorChar
if ($contextFull.Equals($cwdFull, [StringComparison]::OrdinalIgnoreCase) -or $contextFull.StartsWith($cwdFull + $separator, [StringComparison]::OrdinalIgnoreCase) -or $cwdFull.StartsWith($contextFull + $separator, [StringComparison]::OrdinalIgnoreCase)) { exit 95 }
try { $document = [IO.File]::ReadAllText($contextPath) | ConvertFrom-Json -ErrorAction Stop } catch { exit 96 }
if ($document.schema -cne 'g4a-context-pack-v1' -or $null -ne $document.PSObject.Properties['cwd']) { exit 97 }
if (@('claude', 'codex', 'grok', 'kimi', 'qwen-code') -cnotcontains [string]$document.source_provider) { exit 98 }
$messages = @($document.retained_messages)
if ($messages.Count -eq 0) { exit 99 }
$hasUser = $false
$hasAssistant = $false
foreach ($message in $messages) {
    if ([string]::IsNullOrWhiteSpace([string]$message.text)) { exit 100 }
    if ([string]$message.role -ceq 'user') { $hasUser = $true }
    elseif ([string]$message.role -ceq 'assistant') { $hasAssistant = $true }
    else { exit 101 }
}
if (-not $hasUser -or -not $hasAssistant) { exit 102 }
$sha = [Security.Cryptography.SHA256]::Create()
try { $contextHash = ([BitConverter]::ToString($sha.ComputeHash([IO.File]::ReadAllBytes($contextPath)))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
[IO.File]::WriteAllLines($proofPath, [string[]]@('context-only', $contextRoot, $cwd, $contextHash, [string]$document.schema, [string]$document.source_provider, [string]$messages.Count))
[Console]::Out.WriteLine('F7_KIMI_CONTEXT_ONLY_VALIDATED')
"#;
                spec.launch.fixed_args.push(format!(
                    "& {{ param([string]$proofPath, [Parameter(ValueFromRemainingArguments=$true)][string[]]$bundleArgs) {validation}; {script} }}",
                ));
                spec.launch.fixed_args.push(proof_path.clone());
            }
            specs.push(spec);
        }
        let catalog = AgentRegistry::new(specs)
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        Ok(catalog)
    }

    #[cfg(feature = "fixture")]
    pub fn new_exact_launcher_fixture(
        config: NodeServerConfig,
        agent_id: AgentId,
        launcher: String,
    ) -> Result<Self, NodeServerError> {
        if !matches!(agent_id.as_str(), "claude" | "codex" | "grok" | "kimi" | "qwen-code") {
            return Err(NodeServerError::Registry(
                "exact launcher fixture requires a supported provider".to_owned(),
            ));
        }
        if launcher.contains('\0') {
            return Err(NodeServerError::Registry(
                "exact launcher fixture path contains NUL".to_owned(),
            ));
        }
        let launcher_path = Path::new(&launcher);
        if !launcher_path.is_absolute() {
            return Err(NodeServerError::Registry(
                "exact launcher fixture path must be absolute".to_owned(),
            ));
        }
        let metadata = std::fs::metadata(launcher_path).map_err(|_| {
            NodeServerError::Registry("exact launcher fixture path is unavailable".to_owned())
        })?;
        if !metadata.is_file() {
            return Err(NodeServerError::Registry(
                "exact launcher fixture path must be a regular file".to_owned(),
            ));
        }
        let mut spec = builtin_registry()
            .get(&agent_id)
            .ok_or_else(|| {
                NodeServerError::Registry(
                    "exact launcher fixture provider is unavailable".to_owned(),
                )
            })?
            .clone();
        spec.launch.program = launcher;
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(feature = "fixture")]
    pub fn new_harness_mcp_proxy_fixture(
        config: NodeServerConfig,
        provider_program: PathBuf,
        fixed_args: Vec<OsString>,
        harness_mcp_program: PathBuf,
    ) -> Result<Self, NodeServerError> {
        if !provider_program.is_absolute() || fixed_args.len() > 32 {
            return Err(NodeServerError::Registry(
                "H3B fixture requires an absolute provider and at most 32 arguments".to_owned(),
            ));
        }
        let metadata = std::fs::symlink_metadata(&provider_program).map_err(|_| {
            NodeServerError::Registry("H3B fixture provider is unavailable".to_owned())
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(NodeServerError::Registry(
                "H3B fixture provider must be an exact regular file".to_owned(),
            ));
        }
        let program = provider_program.into_os_string().into_string().map_err(|_| {
            NodeServerError::Registry("H3B fixture provider path is not Unicode".to_owned())
        })?;
        let fixed_args = fixed_args.into_iter().map(|argument| {
            argument.into_string().map_err(|_| NodeServerError::Registry(
                "H3B fixture argument is not Unicode".to_owned(),
            ))
        }).collect::<Result<Vec<_>, _>>()?;
        let argument_bytes = fixed_args.iter()
            .try_fold(0usize, |total, argument| total.checked_add(argument.len()));
        if program.contains('\0')
            || fixed_args.iter().any(|argument| argument.contains('\0'))
            || !matches!(argument_bytes, Some(total) if total <= 65_536)
        {
            return Err(NodeServerError::Registry(
                "H3B fixture provider or arguments are invalid".to_owned(),
            ));
        }
        let mut config = config.with_harness_mcp_helper(harness_mcp_program)?;
        let agent_id = AgentId::new("codex")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let mut spec = builtin_registry().get(&agent_id).ok_or_else(|| {
            NodeServerError::Registry("H3B fixture provider is unavailable".to_owned())
        })?.clone();
        spec.display_name = "Controlled H3B MCP proxy fixture".to_owned();
        let process_name = Path::new(&program).file_name().and_then(|name| name.to_str())
            .ok_or_else(|| NodeServerError::Registry(
                "H3B fixture provider basename is unavailable".to_owned(),
            ))?.to_owned();
        spec.detection.command = process_name.clone();
        spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
            name: process_name,
        }];
        spec.launch.program = program;
        spec.launch.fixed_args = fixed_args;
        config.fixture_raw_pty_runtime = true;
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(feature = "fixture")]
    pub fn new_qwen_dual_output_fixture(
        mut config: NodeServerConfig,
        launcher: String,
        fixed_args: Vec<String>,
    ) -> Result<Self, NodeServerError> {
        const FIXTURE_ARGUMENTS_MAX: usize = 32;
        const FIXTURE_ARGUMENT_BYTES_MAX: usize = 65_536;

        if launcher.contains('\0') {
            return Err(NodeServerError::Registry(
                "Qwen dual-output fixture path contains NUL".to_owned(),
            ));
        }
        let launcher_path = Path::new(&launcher);
        if !launcher_path.is_absolute() {
            return Err(NodeServerError::Registry(
                "Qwen dual-output fixture path must be absolute".to_owned(),
            ));
        }
        let metadata = std::fs::metadata(launcher_path).map_err(|_| {
            NodeServerError::Registry(
                "Qwen dual-output fixture path is unavailable".to_owned(),
            )
        })?;
        if !metadata.is_file() {
            return Err(NodeServerError::Registry(
                "Qwen dual-output fixture path must be a regular file".to_owned(),
            ));
        }
        let argument_bytes = fixed_args
            .iter()
            .try_fold(0usize, |total, argument| total.checked_add(argument.len()));
        if fixed_args.len() > FIXTURE_ARGUMENTS_MAX
            || fixed_args.iter().any(|argument| argument.contains('\0'))
            || !matches!(argument_bytes, Some(total) if total <= FIXTURE_ARGUMENT_BYTES_MAX)
        {
            return Err(NodeServerError::Registry(
                "Qwen dual-output fixture arguments are invalid".to_owned(),
            ));
        }
        let qwen_id = AgentId::new("qwen-code")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let mut spec = builtin_registry()
            .get(&qwen_id)
            .ok_or_else(|| {
                NodeServerError::Registry(
                    "Qwen dual-output fixture provider is unavailable".to_owned(),
                )
            })?
            .clone();
        if spec.capabilities.adapters.pty_sidecar.is_none() {
            return Err(NodeServerError::Registry(
                "Qwen dual-output fixture sidecar binding is unavailable".to_owned(),
            ));
        }
        let mut spawn_profile = config
            .spawn_profiles
            .iter()
            .next()
            .cloned()
            .ok_or_else(|| {
                NodeServerError::Registry(
                    "Qwen dual-output fixture spawn profile is unavailable".to_owned(),
                )
            })?;
        spawn_profile.provider = qwen_id;
        config.spawn_profiles = SpawnProfileRegistry::new([spawn_profile])
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        spec.launch.program = launcher;
        spec.launch.fixed_args = fixed_args;
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(feature = "fixture")]
    pub fn new_resume_fixture(config: NodeServerConfig) -> Result<Self, NodeServerError> {
        let mut spec = gate4agent_testkit::interactive_agent_spec();
        let claude_id = AgentId::new("claude")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        let claude = builtin_registry()
            .get(&claude_id)
            .ok_or_else(|| NodeServerError::Registry("Claude fixture adapter is unavailable".to_owned()))?;
        spec.id = claude_id;
        spec.display_name = "Resumable Claude PTY fixture".to_owned();
        spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
            name: "claude".to_owned(),
        }];
        spec.capabilities.transports.pty_adapter = claude
            .capabilities
            .transports
            .pty_adapter
            .clone();
        spec.capabilities.adapters.resume = claude
            .capabilities
            .adapters
            .resume
            .clone();
        let script = spec
            .launch
            .fixed_args
            .pop()
            .ok_or_else(|| NodeServerError::Registry("PTY fixture script is unavailable".to_owned()))?;
        spec.launch.fixed_args.push(format!(
            "& {{ param([Parameter(ValueFromRemainingArguments=$true)][string[]]$ignored) {script} }}",
        ));
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(feature = "fixture")]
    pub fn new_cmd_cwd_fixture(
        config: NodeServerConfig,
        command: String,
    ) -> Result<Self, NodeServerError> {
        let mut spec = gate4agent_testkit::interactive_agent_spec();
        spec.id = AgentId::new("claude")
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        spec.display_name = "Controlled cmd cwd fixture".to_owned();
        spec.detection.command = "gate4agent-cwd-fixture.cmd".to_owned();
        spec.launch.program = command;
        spec.launch.fixed_args.clear();
        Self::new_fixture_with_spec(config, spec)
    }

    #[cfg(feature = "fixture")]
    fn new_fixture_with_spec(
        config: NodeServerConfig,
        spec: gate4agent_types::AgentSpec,
    ) -> Result<Self, NodeServerError> {
        let catalog = AgentRegistry::new([spec])
            .map_err(|error| NodeServerError::Registry(error.to_string()))?;
        Self::new_with_registry(config, catalog)
    }

    fn new_with_registry(
        config: NodeServerConfig,
        catalog: AgentRegistry,
    ) -> Result<Self, NodeServerError> {
        for profile in config.spawn_profiles.iter() {
            if catalog.get(&profile.provider).is_none() {
                return Err(NodeServerError::Registry(
                    "spawn profile references an unavailable provider".to_owned(),
                ));
            }
        }
        let input_settle_timeout_ms = catalog
            .iter()
            .map(|spec| spec.readiness.timeout_ms)
            .max()
            .unwrap_or(MUTATION_SETTLE_TIMEOUT_MS)
            .saturating_add(READINESS_SETTLE_HEADROOM_MS)
            .max(MUTATION_SETTLE_TIMEOUT_MS);
        let (provider_contracts, provider_adapter_contracts) =
            provider_contract_manifest(&catalog)?;
        let compatibility_support = node_compatibility_support_for_manifest(
            &provider_contracts,
            &provider_adapter_contracts,
        )?;
        validate_node_negotiated_handshake_capacity(
            &compatibility_support,
            NODE_PROTOCOL_VERSION,
        )
        .map_err(|error| NodeServerError::ProviderContractManifest(error.to_string()))?;
        let enabled_providers = provider_contracts
            .iter()
            .map(|contract| contract.provider.clone())
            .collect();
        #[cfg(feature = "fixture")]
        let fixture_raw_pty_runtime = config.fixture_raw_pty_runtime;
        #[cfg(not(feature = "fixture"))]
        let fixture_raw_pty_runtime = false;
        let (provider_runtime_monitor, provider_runtime_statuses) = if fixture_raw_pty_runtime {
            let statuses = ProviderRuntimeStatuses::new(catalog.iter().map(|spec| {
                crate::protocol::ProviderRuntimeStatus::raw_passthrough(spec.id.clone(), None)
            })).expect("fixture provider catalog remains bounded");
            (None, statuses)
        } else {
            let monitor = Arc::new(ProviderRuntimeMonitor::new(&catalog));
            let statuses = monitor.collect();
            (Some(monitor), statuses)
        };
        let state_path_lock =
            session_registry::StatePathLock::acquire(config.state_path.as_deref())
                .map_err(|error| {
                    durable_state_server_error(error, DURABLE_STATE_LOCK_FAILED_ERROR)
                })?;
        let (delivery_store, delivered_bundles) = match config.state_path.as_ref() {
            Some(state_path) => {
                let delivery_root = delivery_store_root_for_state_path(state_path)
                    .ok_or(NodeServerError::DeliveryStore)?;
                let (store, bundles) = DeliveryStore::open(delivery_root)
                    .map_err(|_| NodeServerError::DeliveryStore)?;
                (Some(store), bundles)
            }
            None => (None, Vec::new()),
        };
        let delivered_catalog = BundleCatalog::new(delivered_bundles)
            .map_err(|_| NodeServerError::DeliveryStore)?;
        let (context_pack_store, durable_context_packs) = match config.state_path.as_ref() {
            Some(state_path) => {
                let context_pack_root = context_pack_store_root_for_state_path(state_path)
                    .ok_or(NodeServerError::ContextPackStore)?;
                let (store, packs) = ContextPackStore::open(context_pack_root)
                    .map_err(|_| NodeServerError::ContextPackStore)?;
                (Some(store), packs)
            }
            None => (None, Vec::new()),
        };
        let mut durable_context_catalog = ContextPackCatalog::default();
        for pack in durable_context_packs {
            durable_context_catalog
                .insert(pack)
                .map_err(|_| NodeServerError::ContextPackStore)?;
        }
        let session_environment_materializer = config
            .session_environment
            .as_ref()
            .map(|environment| {
                SessionEnvironmentMaterializer::new(
                    environment.root.clone(),
                    Arc::clone(&environment.resolver),
                )
                .map_err(|_| NodeServerError::SessionEnvironmentMaterializer)
            })
            .transpose()?;
        let native_session_catalog = config.history.clone().map(|history| {
            Arc::new(Mutex::new(NativeSessionCatalogAuthority::new(history)))
        });
        let (handle, runtime) = match config.history.clone() {
            Some(history) => NativeRuntime::new_with_history(catalog, config.runtime, history),
            None => NativeRuntime::new(catalog, config.runtime),
        };
        let native_launch_profile_control = runtime.native_launch_profile_control();
        let harness_mcp_registry = config
            .harness_mcp_helper
            .clone()
            .map(HarnessMcpProxyRegistry::new);
        let events = handle.subscribe(CONTROL_EVENT_SUBSCRIPTION_CAPACITY);
        let incarnation_id = random_incarnation_id()
            .map_err(NodeServerError::IncarnationIdentity)?;
        let mut loaded = session_registry::load(config.state_path.as_deref(), &config.node_id)
            .map_err(durable_state_load_error)?;
        let managed_worktrees = std::mem::take(&mut loaded.managed_worktrees);
        let managed_worktree_tombstones =
            std::mem::take(&mut loaded.managed_worktree_tombstones);
        let materializations = std::mem::take(&mut loaded.materializations);
        let managed_spawn_replays = std::mem::take(&mut loaded.managed_spawn_replays);
        let (workspaces, records, persistence_warning) =
            merge_durable_state(&config.workspaces, loaded)?;
        let mut shared = NodeShared::new_with_incarnation(
            handle,
            config.access_token.clone(),
            config.node_id.clone(),
            incarnation_id,
            workspaces,
            enabled_providers,
            provider_runtime_statuses,
            provider_runtime_monitor,
            provider_contracts,
            provider_adapter_contracts,
            config.spawn_profiles.clone(),
            Some(native_launch_profile_control),
            native_session_catalog,
            session_environment_materializer,
            config.state_path.clone(),
            records,
            managed_worktrees,
            managed_worktree_tombstones,
            materializations,
            persistence_warning,
            input_settle_timeout_ms,
        );
        shared.harness_mcp_registry = harness_mcp_registry.clone();
        shared.bundle_catalog = RwLock::new(delivered_catalog);
        shared.delivery_store = Mutex::new(delivery_store);
        shared.context_catalog = RwLock::new(durable_context_catalog);
        shared.context_pack_store = Mutex::new(context_pack_store);
        shared.managed_spawn_replays = Mutex::new(
            managed_spawn_replays
                .into_iter()
                .map(|record| (record.idempotency_key.clone(), record))
                .collect(),
        );
        let shared = Arc::new(shared);
        if let Some(registry) = harness_mcp_registry {
            let weak = Arc::downgrade(&shared);
            registry.set_event_sink(Arc::new(move |event| {
                if let Some(shared) = weak.upgrade() {
                    shared.publish_transient(event);
                }
            }));
        }
        if shared.persistence_error().is_none() {
            shared.persist_state().map_err(|error| {
                durable_state_server_error(error, DURABLE_STATE_COMMIT_FAILED_ERROR)
            })?;
        }
        Ok(Self {
            config,
            runtime,
            shared,
            events,
            state_path_lock,
        })
    }

    pub fn shutdown_handle(&self) -> NodeShutdownHandle {
        NodeShutdownHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    #[cfg(feature = "fixture")]
    pub fn spawn_managed_worktree_v2_failure_probe_fixture(
        &self,
    ) -> SpawnManagedWorktreeV2FailureProbe {
        self.shared
            .fixture_spawn_managed_worktree_v2_failure_probe
            .clone()
    }

    /// Installs one immutable host-local environment profile before the server runs.
    ///
    /// Native resolvers remain inside the runtime; the shared node registry keeps
    /// only public IDs, revisions, provider bindings, and opaque native profile IDs.
    pub fn install_environment_profile(
        &mut self,
        profile: NodeEnvironmentProfile,
    ) -> Result<(), NodeServerError> {
        if !self.shared.enabled_providers.contains(profile.provider()) {
            return Err(NodeServerError::EnvironmentProfileProviderUnavailable(
                profile.provider().clone(),
            ));
        }
        let (binding, native_profiles, materialization) = profile.into_parts();
        if materialization.is_some() && self.shared.session_environment_materializer.is_none() {
            return Err(NodeServerError::SessionEnvironmentMaterializerRequired);
        }
        let mut profiles = self
            .shared
            .environment_profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if profiles.contains_key(&binding.id) {
            return Err(NodeServerError::DuplicateEnvironmentProfile(
                binding.id.clone(),
            ));
        }
        if profiles.len() >= MAX_NODE_ENVIRONMENT_PROFILES {
            return Err(NodeServerError::EnvironmentProfileCapacity {
                max: MAX_NODE_ENVIRONMENT_PROFILES,
            });
        }
        for native_id in binding.native_profile_ids() {
            if profiles.values().any(|current| {
                current.native_profile_ids().any(|current_id| current_id == native_id)
            }) {
                return Err(NodeServerError::DuplicateNativeEnvironmentProfile(
                    native_id.to_string(),
                ));
            }
        }

        let mut installed = Vec::with_capacity(native_profiles.len());
        for native_profile in native_profiles {
            let native_id = native_profile.id().clone();
            if let Err(error) = self.runtime.upsert_native_launch_profile(native_profile) {
                for installed_id in installed.into_iter().rev() {
                    let _ = self.runtime.remove_native_launch_profile(&installed_id);
                }
                return Err(NodeServerError::NativeEnvironmentProfile(error.to_string()));
            }
            installed.push(native_id);
        }
        if let Some(materialization) = materialization {
            self.shared
                .environment_materialization_profiles
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(binding.id.clone(), materialization);
        }
        profiles.insert(binding.id.clone(), binding);
        Ok(())
    }

    /// Installs one immutable host-local bundle before the server runs.
    pub fn install_bundle(&mut self, bundle: NodeBundle) -> Result<(), NodeServerError> {
        if self.shared.session_environment_materializer.is_none() {
            return Err(NodeServerError::SessionEnvironmentMaterializerRequired);
        }
        let mut catalog = self
            .shared
            .bundle_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        catalog
            .insert_idempotent(bundle)
            .map_err(|error| NodeServerError::BundleCatalog(error.to_string()))?;
        Ok(())
    }

    pub async fn run_until_ctrl_signal(self) -> Result<(), NodeServerError> {
        let shutdown = self.shutdown_handle();
        let run = self.run();
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => result,
            signal = wait_for_ctrl_signal() => {
                signal?;
                let shutdown_result = shutdown.request_shutdown().await;
                let run_result = run.await;
                shutdown_result.and(run_result)
            }
        }
    }

    pub async fn run(self) -> Result<(), NodeServerError> {
        let NodeServer {
            config,
            mut runtime,
            shared,
            events,
            state_path_lock: _state_path_lock,
        } = self;
        shared.reconcile_materializations();
        shared.reconcile_managed_worktrees().await;
        shared.reconcile_context_pack_exports().await;
        runtime
            .start_hook_ingress(HookIngressConfig::default())
            .await
            .map_err(|error| NodeServerError::HookIngressStartup(error.to_string()))?;
        let endpoint = config.endpoint.clone();
        let api_listen = config.api_listen;
        let accept_shared = Arc::clone(&shared);
        let api_shared = Arc::clone(&shared);
        let shutdown_shared = Arc::clone(&shared);
        let shutdown_timeout = Duration::from_millis(
            config.runtime.provider_shutdown_timeout_ms.max(1),
        );
        let result = {
            let runtime_loop = drive_runtime_until_shutdown(
                &mut runtime,
                events,
                Arc::clone(&shared),
                shutdown_timeout,
            );
            let accept_loop = accept_connections(&endpoint, accept_shared);
            let api_loop = http_api::run(api_listen, api_shared);
            tokio::pin!(runtime_loop);
            tokio::pin!(accept_loop);
            tokio::pin!(api_loop);
            tokio::select! {
                runtime_result = &mut runtime_loop => {
                    let shutdown_result = shutdown_shared
                        .begin_shutdown()
                        .await
                        .map_err(NodeServerError::ShutdownDispatch);
                    let (accept_result, api_result) = tokio::join!(&mut accept_loop, &mut api_loop);
                    match runtime_result {
                        Err(error) => Err(error),
                        Ok(()) => shutdown_result
                            .and(accept_result)
                            .and(api_result.map_err(NodeServerError::HttpApi)),
                    }
                }
                accept_result = &mut accept_loop => {
                    let shutdown_result = shutdown_shared
                        .begin_shutdown()
                        .await
                        .map_err(NodeServerError::ShutdownDispatch);
                    let (runtime_result, api_result) = tokio::join!(&mut runtime_loop, &mut api_loop);
                    match accept_result {
                        Err(error) => Err(error),
                        Ok(()) => shutdown_result
                            .and(runtime_result)
                            .and(api_result.map_err(NodeServerError::HttpApi)),
                    }
                }
                api_result = &mut api_loop => {
                    let shutdown_result = shutdown_shared
                        .begin_shutdown()
                        .await
                        .map_err(NodeServerError::ShutdownDispatch);
                    let (runtime_result, accept_result) = tokio::join!(&mut runtime_loop, &mut accept_loop);
                    match api_result {
                        Err(error) => Err(NodeServerError::HttpApi(error)),
                        Ok(()) => shutdown_result
                            .and(runtime_result)
                            .and(accept_result),
                    }
                }
            }
        };
        runtime.stop_hook_ingress().await;
        result
    }
}

async fn drive_runtime_until_shutdown(
    runtime: &mut NativeRuntime,
    events: EventSubscription,
    shared: Arc<NodeShared>,
    shutdown_timeout: Duration,
) -> Result<(), NodeServerError> {
    let mut shutdown_started = None;
    loop {
        runtime.tick().await;
        while let Ok(event) = events.try_recv() {
            let clean_exit = matches!(
                event.event,
                ControlEventKind::Exited { exit_code: Some(0), forced: false },
            );
            let instance_id = event.instance_id;
            let generation = event.generation;
            // Resolve and spawn before publish_control: publish_control's own
            // managed-record bookkeeping (reconcile_managed_record) downgrades
            // this exact record from Live to Dormant and clears
            // active_session for every Exited event, inside the same
            // synchronous call. Capturing (record_id, session) here — before
            // that runs — is the only way to observe them still Live and
            // still populated; the export itself must still run on its own
            // detached task (never awaited inline on this loop) because it
            // needs NativeRuntime::tick() to keep running to ever settle, and
            // tick() is only ever driven by this same loop. See
            // reconcile_context_pack_export_for_record's doc comment.
            if clean_exit {
                if let Some((record_id, session)) =
                    shared.managed_record_export_target_for_instance(instance_id, generation)
                {
                    let export_shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        export_shared
                            .reconcile_context_pack_export_for_record(&record_id, session)
                            .await;
                    });
                }
            }
            shared.publish_control(event);
        }
        shared.publish_terminal_frames();
        if shared.shutdown.load(Ordering::Acquire) {
            let started = *shutdown_started.get_or_insert_with(Instant::now);
            let snapshot = shared.handle.snapshot();
            let sessions_settled = snapshot.sessions.iter().all(|session| {
                !matches!(
                    session.status,
                    gate4agent_types::SessionStatus::Starting
                        | gate4agent_types::SessionStatus::Running
                        | gate4agent_types::SessionStatus::Stopping
                )
            });
            if runtime.active_native_sessions() == 0 && sessions_settled {
                return Ok(());
            }
            if started.elapsed() >= shutdown_timeout {
                return Err(NodeServerError::ShutdownTimedOut {
                    active_native_sessions: runtime.active_native_sessions(),
                });
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Clone)]
pub struct NodeShutdownHandle {
    shared: Arc<NodeShared>,
}

impl NodeShutdownHandle {
    pub async fn request_shutdown(&self) -> Result<(), NodeServerError> {
        self.shared
            .begin_shutdown()
            .await
            .map_err(NodeServerError::ShutdownDispatch)
    }
}

async fn wait_for_ctrl_signal() -> Result<(), NodeServerError> {
    #[cfg(windows)]
    {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    return tokio::select! {
        signal = ctrl_c.recv() => signal.ok_or(NodeServerError::SignalStreamClosed),
        signal = ctrl_break.recv() => signal.ok_or(NodeServerError::SignalStreamClosed),
    };
    }
    #[cfg(unix)]
    {
        let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            signal = interrupt.recv() => signal.ok_or(NodeServerError::SignalStreamClosed),
            signal = terminate.recv() => signal.ok_or(NodeServerError::SignalStreamClosed),
        }
    }
}

fn active_registry() -> Result<AgentRegistry, NodeServerError> {
    let specs = builtin_registry()
        .iter()
        .filter(|spec| {
            matches!(
                spec.id.as_str(),
                "claude" | "codex" | "grok" | "kimi" | "qwen-code"
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    AgentRegistry::new(specs).map_err(|error| NodeServerError::Registry(error.to_string()))
}

fn provider_contract_manifest(
    catalog: &AgentRegistry,
) -> Result<
    (
        Vec<ProviderContractSupport>,
        Vec<ProviderAdapterContractSupport>,
    ),
    NodeServerError,
> {
    let mut provider_contracts = Vec::new();
    let mut provider_adapter_contracts = Vec::new();
    for spec in catalog.iter() {
        let provider = spec.id.clone();
        let revision = ProviderContractRevision::new(spec.revision.clone())
            .map_err(|error| NodeServerError::ProviderContractManifest(error.to_string()))?;
        provider_contracts.push(ProviderContractSupport {
            provider: provider.clone(),
            revision,
        });
        for (family, binding) in declared_adapter_bindings(spec)? {
            let revision = AdapterContractRevision::new(binding.revision.clone())
                .map_err(|error| NodeServerError::ProviderContractManifest(error.to_string()))?;
            provider_adapter_contracts.push(ProviderAdapterContractSupport {
                provider: provider.clone(),
                family,
                adapter_id: binding.id.clone(),
                revision,
            });
        }
    }
    validate_provider_contract_manifest(&provider_contracts, &provider_adapter_contracts)
        .map_err(|error| NodeServerError::ProviderContractManifest(error.to_string()))?;
    Ok((provider_contracts, provider_adapter_contracts))
}

fn declared_adapter_bindings(
    spec: &AgentSpec,
) -> Result<Vec<(AdapterFamily, &AdapterBinding)>, NodeServerError> {
    let mut bindings = Vec::new();
    push_declared_binding(
        &mut bindings,
        AdapterFamily::PtySemantic,
        spec.capabilities.transports.pty_adapter.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::Pipe,
        spec.capabilities.adapters.pty_sidecar.as_ref(),
    )?;
    let pipe_transport = spec.capabilities.transports.pipe.as_ref();
    let pipe_family = pipe_transport.map(|transport| match transport.protocol {
        gate4agent_types::PipeProtocol::SemanticNdjson
        | gate4agent_types::PipeProtocol::StructuredJsonl => AdapterFamily::Pipe,
        gate4agent_types::PipeProtocol::OneShotText => AdapterFamily::OneShot,
    });
    push_declared_binding(
        &mut bindings,
        AdapterFamily::Pipe,
        pipe_transport
            .filter(|_| pipe_family == Some(AdapterFamily::Pipe))
            .map(|transport| &transport.adapter),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::Acp,
        spec.capabilities
            .transports
            .acp
            .as_ref()
            .map(|transport| &transport.adapter),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::Hook,
        spec.capabilities.adapters.hook.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::ManagedHook,
        spec.capabilities.adapters.managed_hook.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::OneShot,
        pipe_transport
            .filter(|_| pipe_family == Some(AdapterFamily::OneShot))
            .map(|transport| &transport.adapter),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::OneShot,
        spec.capabilities.adapters.one_shot.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::History,
        spec.capabilities.adapters.history.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::Resume,
        spec.capabilities.adapters.resume.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::SessionOptions,
        spec.capabilities.adapters.session_options.as_ref(),
    )?;
    push_declared_binding(
        &mut bindings,
        AdapterFamily::CapabilityProbe,
        spec.capabilities.adapters.capability_probe.as_ref(),
    )?;
    Ok(bindings)
}

fn push_declared_binding<'a>(
    bindings: &mut Vec<(AdapterFamily, &'a AdapterBinding)>,
    family: AdapterFamily,
    binding: Option<&'a AdapterBinding>,
) -> Result<(), NodeServerError> {
    let Some(binding) = binding else {
        return Ok(());
    };
    if let Some((_, existing)) = bindings
        .iter()
        .find(|(existing_family, _)| *existing_family == family)
    {
        if *existing == binding {
            return Ok(());
        }
        return Err(NodeServerError::ProviderContractManifest(format!(
            "provider declares conflicting {family:?} adapter bindings"
        )));
    }
    bindings.push((family, binding));
    Ok(())
}

fn merge_durable_state(
    configured: &[WorkspaceConfig],
    mut loaded: LoadedNodeState,
) -> Result<(Vec<WorkspaceConfig>, Vec<ManagedSessionRecord>, Option<String>), NodeServerError> {
    let mut workspaces = configured
        .iter()
        .cloned()
        .map(|workspace| (workspace.workspace_id.clone(), workspace))
        .collect::<BTreeMap<_, _>>();
    let mut roots = workspaces
        .values()
        .map(|workspace| {
            (
                platform::root_identity(&workspace.canonical_root),
                workspace.workspace_id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut warning = loaded
        .warning
        .take()
        .map(|message| session_registry::sanitized_persistence_summary(&message));

    for (workspace_id, canonical_root) in loaded.workspaces {
        if let Some(existing) = workspaces.get(&workspace_id) {
            if !platform::roots_equal(&existing.canonical_root, &canonical_root) {
                return Err(NodeServerError::DurableState(io::Error::new(
                    io::ErrorKind::InvalidData,
                    DURABLE_STATE_CONFLICT_ERROR,
                )));
            }
            continue;
        }
        if roots.get(&platform::root_identity(&canonical_root)).is_some() {
            return Err(NodeServerError::DurableState(io::Error::new(
                io::ErrorKind::InvalidData,
                DURABLE_STATE_CONFLICT_ERROR,
            )));
        }
        if !Path::new(&canonical_root).is_dir() {
            warning.get_or_insert_with(|| {
                session_registry::DURABLE_STATE_WORKSPACE_WARNING.to_owned()
            });
            continue;
        }
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &canonical_root).map_err(|_| {
            NodeServerError::DurableState(io::Error::new(
                io::ErrorKind::InvalidData,
                DURABLE_STATE_CONFLICT_ERROR,
            ))
        })?;
        roots.insert(
            platform::root_identity(&workspace.canonical_root),
            workspace.workspace_id.clone(),
        );
        workspaces.insert(workspace_id, workspace);
    }

    for record in &mut loaded.records {
        match workspaces.get(&record.workspace_id) {
            Some(workspace)
                if platform::roots_equal(
                    &workspace.canonical_root,
                    windows_path_text(&record.canonical_root),
                ) => {}
            Some(_) => {
                return Err(NodeServerError::DurableState(io::Error::new(
                    io::ErrorKind::InvalidData,
                    DURABLE_STATE_CONFLICT_ERROR,
                )));
            }
            None => {
                record.state = ManagedSessionState::Unavailable;
                record.active_session = None;
                record.last_error = Some(WORKSPACE_UNAVAILABLE_ERROR.to_owned());
            }
        }
    }
    Ok((workspaces.into_values().collect(), loaded.records, warning))
}

struct NodeShared {
    handle: Gate4AgentHandle,
    access_token: String,
    node_id: NodeId,
    incarnation_id: NodeIncarnationId,
    started_at_unix_ms: u64,
    workspaces: RwLock<BTreeMap<WorkspaceId, String>>,
    worktree_service_modes: BTreeMap<WorkspaceId, WorktreeServiceMode>,
    managed_worktree_profiles:
        BTreeMap<WorkspaceId, BTreeMap<WorktreeProfileId, ManagedWorktreeProfile>>,
    managed_worktrees: Mutex<ManagedWorktreeRegistry>,
    enabled_providers: Vec<AgentId>,
    provider_runtime_statuses: ProviderRuntimeStatuses,
    provider_runtime_status_updates:
        RwLock<BTreeMap<AgentId, crate::protocol::ProviderRuntimeStatus>>,
    provider_runtime_monitor: Option<Arc<ProviderRuntimeMonitor>>,
    provider_contracts: Vec<ProviderContractSupport>,
    provider_adapter_contracts: Vec<ProviderAdapterContractSupport>,
    spawn_profiles: SpawnProfileRegistry,
    environment_profiles:
        RwLock<BTreeMap<SpawnEnvironmentProfileId, EnvironmentProfileBinding>>,
    environment_materialization_profiles:
        RwLock<BTreeMap<SpawnEnvironmentProfileId, NodeSessionMaterializationProfile>>,
    bundle_catalog: RwLock<BundleCatalog>,
    delivery_store: Mutex<Option<DeliveryStore>>,
    context_pack_store: Mutex<Option<ContextPackStore>>,
    context_catalog: RwLock<ContextPackCatalog>,
    native_launch_profile_control: Option<NativeLaunchProfileControl>,
    harness_mcp_registry: Option<HarnessMcpProxyRegistry>,
    native_session_catalog: Option<Arc<Mutex<NativeSessionCatalogAuthority>>>,
    session_environment_materializer: Option<SessionEnvironmentMaterializer>,
    materializations: Mutex<BTreeMap<MaterializationId, MaterializationOwnershipRecord>>,
    spawn_idempotency: Mutex<SpawnIdempotencyCache>,
    managed_spawn_replays:
        Mutex<BTreeMap<SpawnIdempotencyKey, ManagedWorktreeSpawnReplayRecordV10>>,
    controller: Mutex<Option<ControllerLease>>,
    history: Mutex<NodeEventHistory>,
    event_tx: broadcast::Sender<NodeEventEnvelope>,
    terminal_event_tx: broadcast::Sender<Arc<Vec<NodeEventEnvelope>>>,
    terminal_frame_watermarks:
        Mutex<BTreeMap<AgentInstanceId, (SessionAddress, u64)>>,
    next_connection_id: AtomicU64,
    next_instance_id: AtomicU64,
    next_command_id: AtomicU64,
    shutdown: AtomicBool,
    shutdown_notify: Notify,
    preauth_slots: Arc<Semaphore>,
    authenticated_slots: Arc<Semaphore>,
    inspection_slots: Arc<Semaphore>,
    mutation_gate: AsyncMutex<()>,
    session_bindings: Mutex<BTreeMap<AgentInstanceId, SessionBinding>>,
    session_records: Mutex<SessionRecords>,
    state_path: Option<PathBuf>,
    persistence_error: RwLock<Option<String>>,
    state_transaction: Mutex<()>,
    input_settle_timeout_ms: u64,
    #[cfg(feature = "fixture")]
    fixture_semantic_hook_policy: bool,
    #[cfg(feature = "fixture")]
    fixture_spawn_managed_worktree_v2_failure_probe: SpawnManagedWorktreeV2FailureProbe,
}

impl Drop for NodeShared {
    fn drop(&mut self) {
        if let Some(registry) = self.harness_mcp_registry.as_ref() {
            let instances = registry.shutdown();
            if let Some(control) = self.native_launch_profile_control.as_ref() {
                for instance_id in instances {
                    control.clear_native_harness_mcp_launch_overlay(instance_id);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionBinding {
    workspace_id: WorkspaceId,
    generation: SessionGeneration,
    runtime_policy: ProviderRuntimePolicy,
    pending_resume: Option<(SessionGeneration, CommandId, ProviderRuntimePolicy)>,
    record_id: Option<SessionRecordId>,
    managed_worktree_lease_id: Option<ManagedWorktreeLeaseId>,
    environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    bundle: Option<ResolvedBundleReceipt>,
    context: Option<ResolvedContextPackReceipt>,
    materialization_id: Option<MaterializationId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpawnRecordPolicy {
    ProviderIdentityOnly,
    Always,
}

struct NativeEnvironmentSelectionGuard {
    control: Option<NativeLaunchProfileControl>,
    instance_id: AgentInstanceId,
}

struct NativeInstanceOverlayGuard {
    control: Option<NativeLaunchProfileControl>,
    instance_id: AgentInstanceId,
}

struct NativeHarnessMcpOverlayGuard {
    control: NativeLaunchProfileControl,
    instance_id: AgentInstanceId,
}

impl NativeHarnessMcpOverlayGuard {
    fn retain(self) { std::mem::forget(self); }
}

impl Drop for NativeHarnessMcpOverlayGuard {
    fn drop(&mut self) {
        self.control
            .clear_native_harness_mcp_launch_overlay(self.instance_id);
    }
}

impl NativeInstanceOverlayGuard {
    fn retain(mut self) {
        self.control = None;
    }
}

impl Drop for NativeInstanceOverlayGuard {
    fn drop(&mut self) {
        if let Some(control) = self.control.as_ref() {
            control.clear_native_instance_launch_overlay(self.instance_id);
        }
    }
}

enum PreparedNativeLaunchOverlay {
    Environment(NativeLaunchEnvironmentOverlay),
    Instance(NativeInstanceLaunchOverlay),
}

impl NativeEnvironmentSelectionGuard {
    fn retain(mut self) {
        self.control = None;
    }
}

impl Drop for NativeEnvironmentSelectionGuard {
    fn drop(&mut self) {
        if let Some(control) = self.control.as_ref() {
            control.clear_native_launch_profile_selection(self.instance_id);
        }
    }
}

struct SessionMaterializationGuard<'a> {
    shared: &'a NodeShared,
    id: Option<MaterializationId>,
}

impl SessionMaterializationGuard<'_> {
    fn id(&self) -> Option<&MaterializationId> {
        self.id.as_ref()
    }

    fn retain(mut self) {
        self.id = None;
    }
}

impl Drop for SessionMaterializationGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.shared.cleanup_materialization(&id);
        }
    }
}

struct SessionRecords {
    records: BTreeMap<SessionRecordId, ManagedSessionRecord>,
}

struct SpawnIdempotencyCache {
    entries: BTreeMap<SpawnIdempotencyKey, SpawnIdempotencyEntry>,
}

struct SpawnIdempotencyEntry {
    value: SpawnIdempotencyValue,
    expires_at: Instant,
}

enum SpawnIdempotencyValue {
    Standard {
        spec: SpawnSpec,
        result: Result<ResolvedSpawnReceipt, NodeFailure>,
    },
    Managed {
        request: ManagedWorktreeSpawnRequest,
        result: Result<ManagedWorktreeSpawnReceipt, NodeFailure>,
    },
    HarnessMcp,
}

enum DurableManagedSpawnReplayDecision {
    Reserved,
    PendingLinked,
    Committed(ManagedWorktreeSpawnReceipt),
}

impl SpawnIdempotencyValue {
    fn references_context(&self, context_id: &SpawnContextId) -> bool {
        match self {
            Self::Standard { result, .. } => result
                .as_ref()
                .ok()
                .and_then(|receipt| receipt.context.as_ref()),
            Self::Managed { result, .. } => result
                .as_ref()
                .ok()
                .and_then(|receipt| receipt.spawn.context.as_ref()),
            Self::HarnessMcp => None,
        }
        .is_some_and(|receipt| &receipt.id == context_id)
    }
}

#[derive(Clone, Copy)]
struct ControllerLease {
    connection_id: u64,
    expires_at: Instant,
}

struct NodeEventHistory {
    last_sequence: u64,
    replay_floor_sequence: u64,
    events: VecDeque<NodeEventEnvelope>,
    record_providers: BTreeMap<SessionRecordId, AgentId>,
    removed_record_providers: BTreeMap<u64, AgentId>,
}

impl NodeEventHistory {
    fn new(record_providers: BTreeMap<SessionRecordId, AgentId>) -> Self {
        Self {
            last_sequence: 0,
            replay_floor_sequence: 1,
            events: VecDeque::with_capacity(NODE_EVENT_HISTORY_MAX),
            record_providers,
            removed_record_providers: BTreeMap::new(),
        }
    }
}

impl NodeShared {
    fn new_with_incarnation(
        handle: Gate4AgentHandle,
        access_token: String,
        node_id: NodeId,
        incarnation_id: NodeIncarnationId,
        workspaces: Vec<WorkspaceConfig>,
        enabled_providers: Vec<AgentId>,
        provider_runtime_statuses: ProviderRuntimeStatuses,
        provider_runtime_monitor: Option<Arc<ProviderRuntimeMonitor>>,
        provider_contracts: Vec<ProviderContractSupport>,
        provider_adapter_contracts: Vec<ProviderAdapterContractSupport>,
        spawn_profiles: SpawnProfileRegistry,
        native_launch_profile_control: Option<NativeLaunchProfileControl>,
        native_session_catalog: Option<Arc<Mutex<NativeSessionCatalogAuthority>>>,
        session_environment_materializer: Option<SessionEnvironmentMaterializer>,
        state_path: Option<PathBuf>,
        records: Vec<ManagedSessionRecord>,
        managed_worktrees: Vec<ManagedWorktreeLeaseRecord>,
        managed_worktree_tombstones: Vec<ManagedWorktreeLeaseRecord>,
        materializations: Vec<MaterializationOwnershipRecord>,
        persistence_warning: Option<String>,
        input_settle_timeout_ms: u64,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(NODE_BROADCAST_CAPACITY);
        let (terminal_event_tx, _) = broadcast::channel(NODE_TERMINAL_BROADCAST_CAPACITY);
        let record_providers = records
            .iter()
            .map(|record| (record.record_id.clone(), record.provider.clone()))
            .collect();
        let mut worktree_service_modes: BTreeMap<WorkspaceId, WorktreeServiceMode> = workspaces
            .iter()
            .map(|workspace| {
                (workspace.workspace_id.clone(), workspace.worktree_service_mode)
            })
            .collect();
        for lease in &managed_worktrees {
            worktree_service_modes.insert(lease.workspace_id.clone(), WorktreeServiceMode::Off);
        }
        let managed_worktree_profiles = workspaces.iter()
            .map(|workspace| (
                workspace.workspace_id.clone(),
                workspace.managed_worktree_profiles.clone(),
            ))
            .collect();
        let mut managed_worktrees = ManagedWorktreeRegistry::from_records(
            managed_worktrees,
            managed_worktree_tombstones,
        ).expect("durable managed worktree registry was validated while loading");
        managed_worktrees.clear_stale_session_holders(incarnation_id, unix_time_ms());
        managed_worktrees.reattach_record_holders(
            &managed_worktree_record_holders(&records),
            unix_time_ms(),
        );
        Self {
            handle,
            access_token,
            node_id,
            incarnation_id,
            started_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            workspaces: RwLock::new(
                workspaces
                    .into_iter()
                    .map(|workspace| (workspace.workspace_id, workspace.canonical_root))
                    .collect(),
            ),
            worktree_service_modes,
            managed_worktree_profiles,
            managed_worktrees: Mutex::new(managed_worktrees),
            enabled_providers,
            provider_runtime_statuses,
            provider_runtime_status_updates: RwLock::new(BTreeMap::new()),
            provider_runtime_monitor,
            provider_contracts,
            provider_adapter_contracts,
            spawn_profiles,
            environment_profiles: RwLock::new(BTreeMap::new()),
            environment_materialization_profiles: RwLock::new(BTreeMap::new()),
            bundle_catalog: RwLock::new(BundleCatalog::default()),
            delivery_store: Mutex::new(None),
            context_pack_store: Mutex::new(None),
            context_catalog: RwLock::new(ContextPackCatalog::default()),
            native_launch_profile_control,
            harness_mcp_registry: None,
            native_session_catalog,
            session_environment_materializer,
            materializations: Mutex::new(
                materializations
                    .into_iter()
                    .map(|record| (record.id().clone(), record))
                    .collect(),
            ),
            spawn_idempotency: Mutex::new(SpawnIdempotencyCache {
                entries: BTreeMap::new(),
            }),
            managed_spawn_replays: Mutex::new(BTreeMap::new()),
            controller: Mutex::new(None),
            history: Mutex::new(NodeEventHistory::new(record_providers)),
            event_tx,
            terminal_event_tx,
            terminal_frame_watermarks: Mutex::new(BTreeMap::new()),
            next_connection_id: AtomicU64::new(1),
            next_instance_id: AtomicU64::new(1),
            next_command_id: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            preauth_slots: Arc::new(Semaphore::new(MAX_PREAUTH_CONNECTIONS)),
            authenticated_slots: Arc::new(Semaphore::new(MAX_AUTHENTICATED_CONNECTIONS)),
            inspection_slots: Arc::new(Semaphore::new(WORKSPACE_INSPECTION_MAX_CONCURRENCY)),
            mutation_gate: AsyncMutex::new(()),
            session_bindings: Mutex::new(BTreeMap::new()),
            session_records: Mutex::new(SessionRecords {
                records: records
                    .into_iter()
                    .map(|record| (record.record_id.clone(), record))
                    .collect(),
            }),
            state_path,
            persistence_error: RwLock::new(
                persistence_warning
                    .map(|message| session_registry::sanitized_persistence_summary(&message)),
            ),
            state_transaction: Mutex::new(()),
            input_settle_timeout_ms,
            #[cfg(feature = "fixture")]
            fixture_semantic_hook_policy: false,
            #[cfg(feature = "fixture")]
            fixture_spawn_managed_worktree_v2_failure_probe:
                SpawnManagedWorktreeV2FailureProbe::default(),
        }
    }

    #[cfg(test)]
    fn new(
        handle: Gate4AgentHandle,
        access_token: String,
        node_id: NodeId,
        workspaces: Vec<WorkspaceConfig>,
        enabled_providers: Vec<AgentId>,
    ) -> Self {
        Self::new_with_incarnation(
            handle,
            access_token,
            node_id,
            NodeIncarnationId::from_bytes([0; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            workspaces,
            enabled_providers,
            ProviderRuntimeStatuses::default(),
            None,
            Vec::new(),
            Vec::new(),
            SpawnProfileRegistry::default(),
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            MUTATION_SETTLE_TIMEOUT_MS.saturating_add(READINESS_SETTLE_HEADROOM_MS),
        )
    }

    fn persistence_error(&self) -> Option<String> {
        self.persistence_error
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn admit_provider_runtime(
        &self,
        provider: &AgentId,
        requirement: ProviderRuntimeRequirement,
    ) -> Result<ProviderRuntimePolicy, NodeFailure> {
        let admission = if let Some(monitor) = self.provider_runtime_monitor.clone() {
            let provider = provider.clone();
            let refresh = tokio::task::spawn_blocking(move || monitor.evaluate(&provider));
            let (status, admission) = timeout(
                Duration::from_millis(PROVIDER_RUNTIME_ADMISSION_TIMEOUT_MS),
                refresh,
            )
            .await
            .map_err(|_| failure(
                NodeFailureCode::BackendBusy,
                "provider runtime probe exceeded its bounded deadline",
            ))?
            .map_err(|_| failure(
                NodeFailureCode::BackendOperationFailed,
                "provider runtime probe task failed",
            ))?;
            if let Some(status) = status {
                self
                    .provider_runtime_status_updates
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(status.provider().clone(), status);
            }
            admission.and_then(|policy| {
                require_policy(policy, requirement).map(|()| policy)
            })
        } else {
            crate::provider_runtime::admit_status(
                &self.provider_runtime_statuses,
                provider,
                requirement,
            )
        };
        admission.map_err(|error| match error {
            ProviderRuntimeAdmissionError::LauncherUnavailable => failure(
                NodeFailureCode::BackendOperationFailed,
                "provider launcher is unavailable",
            ),
            ProviderRuntimeAdmissionError::SemanticCapabilityUnverified => failure(
                NodeFailureCode::UnsupportedCapability,
                "provider semantic capability is not verified",
            ),
            ProviderRuntimeAdmissionError::ProbeBusy => failure(
                NodeFailureCode::BackendBusy,
                "provider runtime probe is already in progress",
            ),
        })
    }

    fn admit_qwen_sidecar_observation_policy(
        &self,
        provider: &AgentId,
        mode: SessionMode,
        policy: ProviderRuntimePolicy,
    ) -> ProviderRuntimePolicy {
        let exact_qwen_sidecar = mode == SessionMode::Pty
            && provider.as_str() == "qwen-code"
            && self.provider_adapter_contracts.iter().any(|contract| {
                contract.provider == *provider
                    && contract.family == AdapterFamily::Pipe
                    && contract.adapter_id.as_str() == "qwen-code"
            });
        if !exact_qwen_sidecar || policy.semantic_readiness {
            return policy;
        }
        ProviderRuntimePolicy::new(
            policy.raw_pty_lifecycle,
            true,
            policy.structured_prompt,
            policy.provider_session_identity,
            policy.semantic_resume,
        )
        .expect("exact Qwen sidecar observation policy is internally valid")
    }

    fn resolve_environment_profile(
        &self,
        resolved: &ResolvedSpawnSpec,
    ) -> Result<Option<ResolvedEnvironmentProfileReceipt>, NodeFailure> {
        let Some(profile_id) = resolved.environment_profile_id.as_ref() else {
            return Ok(None);
        };
        let profiles = self
            .environment_profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = profiles.get(profile_id).ok_or_else(|| {
            failure(
                NodeFailureCode::UnknownEnvironmentProfile,
                "spawn environment profile is unavailable on this node",
            )
        })?;
        if binding.provider != resolved.provider
            || binding.native_profile_id(resolved.mode).is_none()
        {
            return Err(failure(
                NodeFailureCode::EnvironmentProfileBindingMismatch,
                "spawn environment profile does not match the provider and mode",
            ));
        }
        Ok(Some(ResolvedEnvironmentProfileReceipt {
            profile_id: binding.id.clone(),
            profile_revision: binding.revision.clone(),
        }))
    }

    fn resolve_bundle(
        &self,
        resolved: &ResolvedSpawnSpec,
        environment_profile: Option<&ResolvedEnvironmentProfileReceipt>,
    ) -> Result<Option<ResolvedBundleReceipt>, NodeFailure> {
        let Some(bundle_id) = resolved.bundle_id.as_ref() else {
            return Ok(None);
        };
        let catalog = self.bundle_catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bundle = catalog.get(bundle_id).ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownBundle,
                    "spawn bundle is unavailable on this node",
                )
            })?;
        self.bundle_layout(
            &resolved.provider,
            resolved.mode,
            environment_profile,
            bundle,
        )?;
        Ok(Some(bundle.receipt()))
    }

    fn begin_delivery_stage(
        &self,
        manifest: DeliveryBundleManifestV2,
    ) -> Result<NodeResponse, NodeFailure> {
        let mut store = self
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let store = store.as_mut().ok_or_else(|| {
            failure(
                NodeFailureCode::DeliveryStageStorageFailed,
                "node delivery store is unavailable",
            )
        })?;
        let begun = store.begin(manifest).map_err(delivery_failure)?;
        Ok(NodeResponse::DeliveryStageBegun {
            stage_id: begun.stage_id,
            manifest_digest: begun.manifest_digest,
            missing_blobs: begun.missing_blobs,
        })
    }

    fn put_delivery_blob_chunk(
        &self,
        stage_id: DeliveryStageId,
        blob_digest: DeliveryBlobDigestV1,
        offset: u64,
        chunk_hex: DeliveryBlobChunkHexV1,
    ) -> Result<NodeResponse, NodeFailure> {
        let mut store = self
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let store = store.as_mut().ok_or_else(|| {
            failure(
                NodeFailureCode::DeliveryStageStorageFailed,
                "node delivery store is unavailable",
            )
        })?;
        let accepted = store
            .put_chunk(&stage_id, &blob_digest, offset, &chunk_hex.decode())
            .map_err(delivery_failure)?;
        Ok(NodeResponse::DeliveryBlobChunkAccepted {
            stage_id: accepted.stage_id,
            blob_digest: accepted.blob_digest,
            next_offset: accepted.next_offset,
        })
    }

    fn commit_delivery_stage(
        &self,
        stage_id: DeliveryStageId,
    ) -> Result<DeliveryCommitReceiptV1, NodeFailure> {
        let mut store = self
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let store = store.as_mut().ok_or_else(|| {
            failure(
                NodeFailureCode::DeliveryStageStorageFailed,
                "node delivery store is unavailable",
            )
        })?;
        let prepared = store.prepare_commit(&stage_id).map_err(delivery_failure)?;
        let mut catalog = self
            .bundle_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next_catalog = catalog.clone();
        next_catalog
            .insert_idempotent(prepared.bundle().clone())
            .map_err(|_| {
                failure(
                    NodeFailureCode::DeliveryStageConflict,
                    "delivery bundle identity conflicts with installed content",
                )
            })?;
        let receipt = store.publish_commit(prepared).map_err(delivery_failure)?;
        *catalog = next_catalog;
        Ok(receipt)
    }

    fn abort_delivery_stage(
        &self,
        stage_id: &DeliveryStageId,
    ) -> Result<(), NodeFailure> {
        let mut store = self
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let store = store.as_mut().ok_or_else(|| {
            failure(
                NodeFailureCode::DeliveryStageStorageFailed,
                "node delivery store is unavailable",
            )
        })?;
        store.abort(stage_id).map_err(delivery_failure)
    }

    fn resolve_context(
        &self,
        resolved: &ResolvedSpawnSpec,
    ) -> Result<Option<ResolvedContextPackReceipt>, NodeFailure> {
        let Some(context_id) = resolved.context_id.as_ref() else {
            return Ok(None);
        };
        self.context_catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(context_id)
            .map(|context| context.receipt().clone())
            .ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownContextPack,
                    "spawn context pack is unavailable on this node",
                )
            })
            .map(Some)
    }

    fn bundle_layout(
        &self,
        provider: &AgentId,
        mode: SessionMode,
        environment_profile: Option<&ResolvedEnvironmentProfileReceipt>,
        bundle: &NodeBundle,
    ) -> Result<BundleProviderLayout, NodeFailure> {
        let layout = validate_bundle_binding(provider, mode, bundle).map_err(|_| {
            failure(
                NodeFailureCode::BundleBindingMismatch,
                "spawn bundle does not match the provider and mode",
            )
        })?;
        if layout != BundleProviderLayout::Codex {
            return Ok(layout);
        }
        let profile = environment_profile.and_then(|receipt| {
            let profiles = self
                .environment_profiles
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let binding = profiles.get(&receipt.profile_id)?;
            if binding.revision != receipt.profile_revision
                || &binding.provider != provider
                || binding.native_profile_id(mode).is_none()
            {
                return None;
            }
            self.environment_materialization_profiles
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&receipt.profile_id)
                .cloned()
        });
        if !profile.is_some_and(|profile| profile.supports_bundle_layout(layout)) {
            return Err(failure(
                NodeFailureCode::BundleBindingMismatch,
                "Codex bundle requires an exact isolated CODEX_HOME profile",
            ));
        }
        Ok(layout)
    }

    fn select_environment_profile(
        &self,
        instance_id: AgentInstanceId,
        provider: &AgentId,
        mode: SessionMode,
        environment_profile: Option<&ResolvedEnvironmentProfileReceipt>,
    ) -> Result<Option<NativeEnvironmentSelectionGuard>, NodeFailure> {
        let Some(environment_profile) = environment_profile else {
            return Ok(None);
        };
        let native_profile_id = {
            let profiles = self
                .environment_profiles
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let binding = profiles
                .get(&environment_profile.profile_id)
                .ok_or_else(|| {
                    failure(
                        NodeFailureCode::UnknownEnvironmentProfile,
                        "session environment profile is unavailable on this node",
                    )
                })?;
            if &binding.revision != &environment_profile.profile_revision
                || &binding.provider != provider
            {
                return Err(failure(
                    NodeFailureCode::EnvironmentProfileBindingMismatch,
                    "session environment profile revision or provider changed",
                ));
            }
            binding.native_profile_id(mode).cloned().ok_or_else(|| {
                failure(
                    NodeFailureCode::EnvironmentProfileBindingMismatch,
                    "session environment profile does not support this mode",
                )
            })?
        };
        let control = self.native_launch_profile_control.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                "native environment profile controller is unavailable",
            )
        })?;
        control
            .select_native_launch_profile(instance_id, native_profile_id)
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendOperationFailed,
                    "native environment profile selection failed",
                )
            })?;
        Ok(Some(NativeEnvironmentSelectionGuard {
            control: Some(control),
            instance_id,
        }))
    }

    fn install_prepared_launch_overlay(
        &self,
        instance_id: AgentInstanceId,
        overlay: PreparedNativeLaunchOverlay,
    ) -> Result<Option<NativeInstanceOverlayGuard>, NodeFailure> {
        let control = self.native_launch_profile_control.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                "native launch profile controller is unavailable",
            )
        })?;
        match overlay {
            PreparedNativeLaunchOverlay::Environment(overlay) => {
                control
                    .install_native_launch_environment_overlay(instance_id, overlay)
                    .map_err(|_| failure(
                        NodeFailureCode::BackendOperationFailed,
                        "session environment overlay installation failed",
                    ))?;
                Ok(None)
            }
            PreparedNativeLaunchOverlay::Instance(overlay) => {
                control
                    .install_native_instance_launch_overlay(instance_id, overlay)
                    .map_err(|_| failure(
                        NodeFailureCode::BundleBindingMismatch,
                        "session bundle launch overlay installation failed",
                    ))?;
                Ok(Some(NativeInstanceOverlayGuard {
                    control: Some(control),
                    instance_id,
                }))
            }
        }
    }

    fn environment_materialization_profile(
        &self,
        environment_profile: &ResolvedEnvironmentProfileReceipt,
    ) -> Option<NodeSessionMaterializationProfile> {
        self.environment_materialization_profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&environment_profile.profile_id)
            .cloned()
    }

    fn materialization_profile(
        &self,
        provider: &AgentId,
        mode: SessionMode,
        environment_profile: Option<&ResolvedEnvironmentProfileReceipt>,
        bundle: Option<&ResolvedBundleReceipt>,
        context: Option<&ResolvedContextPackReceipt>,
    ) -> Result<Option<(
        NodeSessionMaterializationProfile,
        bool,
        Option<BundleProviderLayout>,
    )>, NodeFailure> {
        let environment = environment_profile
            .and_then(|receipt| self.environment_materialization_profile(receipt));
        let (bundle, bundle_layout) = match bundle {
            Some(receipt) => {
                let catalog = self.bundle_catalog.read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let installed = catalog.get(&receipt.id).ok_or_else(|| failure(
                    NodeFailureCode::UnknownBundle,
                    "session bundle is unavailable on this node",
                ))?;
                if installed.receipt() != *receipt {
                    return Err(failure(
                        NodeFailureCode::BundleBindingMismatch,
                        "session bundle revision or digest changed",
                    ));
                }
                let layout = self.bundle_layout(
                    provider,
                    mode,
                    environment_profile,
                    installed,
                )?;
                (Some(installed.clone()), Some(layout))
            }
            None => (None, None),
        };
        let has_environment_overlay = environment.is_some() || context.is_some();
        let profile = match (environment, bundle.as_ref(), bundle_layout) {
            (None, None, None) => None,
            (Some(profile), None, None) => Some(Ok(profile)),
            (None, Some(bundle), Some(layout)) => {
                Some(NodeSessionMaterializationProfile::from_bundle(bundle, layout))
            }
            (Some(profile), Some(bundle), Some(layout)) => {
                Some(profile.with_bundle(bundle, layout))
            }
            _ => {
                return Err(failure(
                    NodeFailureCode::BundleBindingMismatch,
                    "session bundle provider layout is unavailable",
                ));
            }
        };
        let profile = profile
            .transpose()
            .map_err(|_| failure(
                NodeFailureCode::BundleMaterializationFailed,
                "session bundle materialization profile is invalid",
            ))?;
        let profile = match (profile, context) {
            (Some(profile), Some(receipt)) => {
                let catalog = self.context_catalog
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let pack = catalog.get(&receipt.id).ok_or_else(|| failure(
                    NodeFailureCode::UnknownContextPack,
                    "session context pack is unavailable on this node",
                ))?;
                if pack.receipt() != receipt {
                    return Err(failure(
                        NodeFailureCode::ContextPackMaterializationFailed,
                        "session context pack identity changed",
                    ));
                }
                profile.with_context(pack)
            }
            (None, Some(receipt)) => {
                let catalog = self.context_catalog
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let pack = catalog.get(&receipt.id).ok_or_else(|| failure(
                    NodeFailureCode::UnknownContextPack,
                    "session context pack is unavailable on this node",
                ))?;
                if pack.receipt() != receipt {
                    return Err(failure(
                        NodeFailureCode::ContextPackMaterializationFailed,
                        "session context pack identity changed",
                    ));
                }
                NodeSessionMaterializationProfile::from_context(pack)
            }
            (Some(profile), None) => Ok(profile),
            (None, None) => return Ok(None),
        }
        .map_err(|_| failure(
            NodeFailureCode::ContextPackMaterializationFailed,
            "session context pack materialization profile is invalid",
        ))?;
        Ok(Some((profile, has_environment_overlay, bundle_layout)))
    }

    fn prepare_session_materialization<'a>(
        &'a self,
        address: &SessionAddress,
        provider: &AgentId,
        mode: SessionMode,
        environment_profile: Option<&ResolvedEnvironmentProfileReceipt>,
        bundle: Option<&ResolvedBundleReceipt>,
        context: Option<&ResolvedContextPackReceipt>,
        managed_lease_id: Option<ManagedWorktreeLeaseId>,
    ) -> Result<Option<(Option<PreparedNativeLaunchOverlay>, SessionMaterializationGuard<'a>)>, NodeFailure> {
        let Some((profile, has_environment_overlay, bundle_layout)) =
            self.materialization_profile(provider, mode, environment_profile, bundle, context)?
        else {
            return Ok(None);
        };
        let materializer = self.session_environment_materializer.as_ref().ok_or_else(|| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                "session environment materializer is unavailable",
            )
        })?;
        let materialization_failure_code = if context.is_some() {
            NodeFailureCode::ContextPackMaterializationFailed
        } else if bundle.is_some() {
            NodeFailureCode::BundleMaterializationFailed
        } else {
            NodeFailureCode::BackendOperationFailed
        };
        let id = MaterializationId::new(format!(
            "mat-{}-{}-{}",
            self.incarnation_id,
            address.session.instance_id.0,
            address.session.generation.0,
        ))
        .map_err(|_| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                "session environment materialization identity failed",
            )
        })?;
        let owner = MaterializationOwner::Session {
            incarnation_id: self.incarnation_id,
            instance_id: address.session.instance_id,
            generation: address.session.generation,
        };
        let mut ownership = materializer
            .begin(
                id.clone(),
                environment_profile.cloned(),
                bundle.cloned(),
                context.cloned(),
                owner,
                managed_lease_id,
                &profile,
                unix_time_ms(),
            )
            .map_err(|_| {
                failure(
                    materialization_failure_code,
                    "session materialization preparation failed",
                )
            })?;
        {
            let _transaction = self
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let duplicate = self
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), ownership.clone());
            if duplicate.is_some() {
                return Err(failure(
                    NodeFailureCode::BackendOperationFailed,
                    "session environment materialization identity conflict",
                ));
            }
            if let Err(error) = self.persist_state_locked() {
                self.materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return Err(persistence_failure(error));
            }
        }
        let environment = match materializer.materialize(&mut ownership, &profile, unix_time_ms()) {
            Ok(environment) => environment,
            Err(_) => {
                let transaction = self
                    .state_transaction
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.clone(), ownership.clone());
                if self.persist_state_locked().is_err() {
                    self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
                }
                drop(transaction);
                if ownership.state() == MaterializationState::CleanupRequired {
                    let _ = self.cleanup_materialization(&id);
                }
                return Err(failure(
                    materialization_failure_code,
                    "session materialization failed",
                ));
            }
        };
        {
            let _transaction = self
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), ownership.clone());
            if let Err(error) = self.persist_state_locked() {
                drop(_transaction);
                let _ = self.cleanup_materialization(&id);
                return Err(persistence_failure(error));
            }
        }
        let guard = SessionMaterializationGuard {
            shared: self,
            id: Some(id),
        };
        let bundle_arguments = match bundle {
            Some(receipt) => {
                let catalog = self.bundle_catalog.read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let installed = catalog.get(&receipt.id).ok_or_else(|| failure(
                    NodeFailureCode::UnknownBundle,
                    "session bundle is unavailable on this node",
                ))?;
                if installed.receipt() != *receipt {
                    return Err(failure(
                        NodeFailureCode::BundleBindingMismatch,
                        "session bundle revision or digest changed",
                    ));
                }
                let layout = bundle_layout.ok_or_else(|| failure(
                    NodeFailureCode::BundleBindingMismatch,
                    "materialized bundle provider layout is unavailable",
                ))?;
                materializer.revalidate_bundle(&ownership, installed, layout).map_err(|_| {
                    failure(
                        NodeFailureCode::BundleMaterializationFailed,
                        "session bundle byte revalidation failed",
                    )
                })?;
                Some(bundle_launch_arguments(
                    layout,
                    ownership.bundle_root(),
                    ownership.provider_home(),
                ).map_err(|_| failure(
                    NodeFailureCode::BundleBindingMismatch,
                    "session bundle launch binding is invalid",
                ))?)
            }
            None => None,
        };
        if let Some(receipt) = context {
            materializer
                .revalidate_context(&ownership, receipt)
                .map_err(|_| failure(
                    NodeFailureCode::ContextPackMaterializationFailed,
                    "session context pack byte revalidation failed",
                ))?;
        }
        let transport = match mode {
            SessionMode::Pty => TransportKind::Pty,
            SessionMode::Inline => TransportKind::Pipe,
        };
        let overlay = if let Some(extra_args) = bundle_arguments {
            Some(PreparedNativeLaunchOverlay::Instance(
                NativeInstanceLaunchOverlay::new(
                    provider.clone(),
                    transport,
                    if has_environment_overlay {
                        environment
                    } else {
                        Vec::new()
                    },
                    extra_args,
                )
                .map_err(|_| {
                    failure(
                        NodeFailureCode::BundleBindingMismatch,
                        "session bundle launch overlay is invalid",
                    )
                })?,
            ))
        } else if has_environment_overlay {
            let context_only = environment_profile.is_none()
                && bundle.is_none()
                && context.is_some();
            let overlay = if context_only {
                NativeLaunchEnvironmentOverlay::new_context_root(
                    provider.clone(),
                    transport,
                    environment,
                )
                .map_err(|_| {
                    failure(
                        NodeFailureCode::ContextPackMaterializationFailed,
                        "context-only launch overlay is invalid",
                    )
                })?
            } else {
                NativeLaunchEnvironmentOverlay::new(
                    provider.clone(),
                    transport,
                    environment,
                )
                .map_err(|_| {
                    failure(
                        NodeFailureCode::BackendOperationFailed,
                        "session environment overlay is invalid",
                    )
                })?
            };
            Some(PreparedNativeLaunchOverlay::Environment(overlay))
        } else {
            None
        };
        Ok(Some((overlay, guard)))
    }

    fn cleanup_materialization(&self, id: &MaterializationId) -> Result<(), NodeFailure> {
        let Some(mut ownership) = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
        else {
            return Ok(());
        };
        if ownership.state() != MaterializationState::CleanupRequired {
            ownership.mark_cleanup_required(unix_time_ms()).map_err(|_| {
                failure(
                    NodeFailureCode::BackendOperationFailed,
                    "session environment cleanup state is invalid",
                )
            })?;
        }
        {
            let _transaction = self
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = self
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), ownership.clone());
            if let Err(error) = self.persist_state_locked() {
                let mut materializations = self
                    .materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(previous) = previous {
                    materializations.insert(id.clone(), previous);
                }
                return Err(persistence_failure(error));
            }
        }
        let cleanup = self
            .session_environment_materializer
            .as_ref()
            .ok_or(())
            .and_then(|materializer| materializer.cleanup(&ownership).map_err(|_| ()));
        if cleanup.is_err() {
            let _ = ownership.mark_recovery_required(unix_time_ms());
            self.materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), ownership);
            if self.persist_state().is_err() {
                self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
            }
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "session environment cleanup requires recovery",
            ));
        }
        let _transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        if let Err(error) = self.persist_state_locked() {
            self.materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), ownership);
            return Err(persistence_failure(error));
        }
        Ok(())
    }

    fn resolve_record_materialization(
        &self,
        record_id: &SessionRecordId,
        provider: &AgentId,
        mode: SessionMode,
        environment_profile: Option<&ResolvedEnvironmentProfileReceipt>,
        bundle: Option<&ResolvedBundleReceipt>,
        context: Option<&ResolvedContextPackReceipt>,
    ) -> Result<Option<(MaterializationId, Option<PreparedNativeLaunchOverlay>)>, NodeFailure> {
        let ownership = {
            let materializations = self
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut matches = materializations.values().filter(|record| {
                record.owner()
                    == &MaterializationOwner::Record {
                        record_id: record_id.clone(),
                    }
            });
            let ownership = matches.next().cloned();
            if matches.next().is_some() {
                return Err(failure(
                    NodeFailureCode::BackendOperationFailed,
                    "managed session has conflicting environment ownership",
                ));
            }
            ownership
        };
        if let (Some(ownership), Some(receipt)) = (ownership.as_ref(), context) {
            let materializer = self.session_environment_materializer.as_ref().ok_or_else(|| {
                failure(
                    NodeFailureCode::ContextPackMaterializationFailed,
                    "session context materializer is unavailable",
                )
            })?;
            let pack = materializer
                .revalidate_context(ownership, receipt)
                .map_err(|_| {
                    self.mark_materialization_recovery_required(ownership.id());
                    failure(
                        NodeFailureCode::ContextPackMaterializationFailed,
                        "managed session context pack revalidation failed",
                    )
                })?;
            self.context_catalog
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(pack)
                .map_err(|_| failure(
                    NodeFailureCode::ContextPackMaterializationFailed,
                    "managed session context pack catalog restore failed",
                ))?;
        }
        let materialization = self.materialization_profile(
            provider,
            mode,
            environment_profile,
            bundle,
            context,
        )?;
        match (ownership, materialization) {
            (None, None) => Ok(None),
            (None, Some(_)) | (Some(_), None) => Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "managed session materialization is unavailable",
            )),
            (Some(ownership), Some((profile, has_environment_materialization, bundle_layout))) => {
                if ownership.environment_profile() != environment_profile
                    || ownership.bundle() != bundle
                    || ownership.context() != context
                    || ownership.state() != MaterializationState::Ready
                {
                    self.mark_materialization_recovery_required(ownership.id());
                    return Err(failure(
                        NodeFailureCode::BackendOperationFailed,
                        "managed session environment ownership requires recovery",
                    ));
                }
                let materializer = self.session_environment_materializer.as_ref().ok_or_else(|| {
                    failure(
                        NodeFailureCode::BackendOperationFailed,
                        "session environment materializer is unavailable",
                    )
                })?;
                if materializer.revalidate(&ownership).is_err() {
                    self.mark_materialization_recovery_required(ownership.id());
                    return Err(failure(
                        NodeFailureCode::BackendOperationFailed,
                        "managed session environment revalidation failed",
                    ));
                }
                let bundle_arguments = if let Some(bundle_receipt) = bundle {
                    let catalog = self.bundle_catalog.read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let Some(installed) = catalog.get(&bundle_receipt.id) else {
                        return Err(failure(
                            NodeFailureCode::UnknownBundle,
                            "managed session bundle is unavailable",
                        ));
                    };
                    let layout = bundle_layout.ok_or_else(|| failure(
                        NodeFailureCode::BundleBindingMismatch,
                        "record bundle provider layout is unavailable",
                    ))?;
                    if materializer.revalidate_bundle(&ownership, installed, layout).is_err() {
                        drop(catalog);
                        self.mark_materialization_recovery_required(ownership.id());
                        return Err(failure(
                            NodeFailureCode::BundleMaterializationFailed,
                            "managed session bundle revalidation failed",
                        ));
                    }
                    Some(bundle_launch_arguments(
                        layout,
                        ownership.bundle_root(),
                        ownership.provider_home(),
                    ).map_err(|_| failure(
                        NodeFailureCode::BundleBindingMismatch,
                        "managed session bundle launch binding is invalid",
                    ))?)
                } else {
                    None
                };
                if !has_environment_materialization {
                    let overlay = bundle_arguments.map(|extra_args| {
                        NativeInstanceLaunchOverlay::new(
                            provider.clone(),
                            TransportKind::Pty,
                            Vec::new(),
                            extra_args,
                        )
                        .map(PreparedNativeLaunchOverlay::Instance)
                        .map_err(|_| failure(
                            NodeFailureCode::BundleBindingMismatch,
                            "managed session bundle launch overlay is invalid",
                        ))
                    }).transpose()?;
                    return Ok(Some((ownership.id().clone(), overlay)));
                }
                let environment = match materializer.resolve_environment(&ownership, &profile) {
                    Ok(environment) => environment,
                    Err(
                        SessionEnvironmentMaterializeError::SecretUnavailable
                        | SessionEnvironmentMaterializeError::SecretDenied
                        | SessionEnvironmentMaterializeError::InvalidSecretValue,
                    ) => {
                        return Err(failure(
                            NodeFailureCode::BackendOperationFailed,
                            "managed session environment resolution failed",
                        ));
                    }
                    Err(
                        SessionEnvironmentMaterializeError::InvalidRoot
                        | SessionEnvironmentMaterializeError::OwnershipMismatch
                        | SessionEnvironmentMaterializeError::Filesystem(_)
                        | SessionEnvironmentMaterializeError::Record(_),
                    ) => {
                        self.mark_materialization_recovery_required(ownership.id());
                        return Err(failure(
                            NodeFailureCode::BackendOperationFailed,
                            "managed session environment resolution failed",
                        ));
                    }
                };
                let transport = match mode {
                    SessionMode::Pty => TransportKind::Pty,
                    SessionMode::Inline => TransportKind::Pipe,
                };
                let overlay = match bundle_arguments {
                    Some(extra_args) => PreparedNativeLaunchOverlay::Instance(
                        NativeInstanceLaunchOverlay::new(
                            provider.clone(),
                            transport,
                            environment,
                            extra_args,
                        )
                        .map_err(|_| failure(
                            NodeFailureCode::BundleBindingMismatch,
                            "managed session bundle launch overlay is invalid",
                        ))?,
                    ),
                    None => PreparedNativeLaunchOverlay::Environment(
                        NativeLaunchEnvironmentOverlay::new(
                            provider.clone(),
                            transport,
                            environment,
                        )
                        .map_err(|_| {
                            failure(
                                NodeFailureCode::BackendOperationFailed,
                                "managed session environment overlay is invalid",
                            )
                        })?,
                    ),
                };
                Ok(Some((ownership.id().clone(), Some(overlay))))
            }
        }
    }

    fn destroy_record_materialization(
        &self,
        record_id: &SessionRecordId,
    ) -> Result<bool, NodeFailure> {
        let ids = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter_map(|record| {
                (record.owner()
                    == &MaterializationOwner::Record {
                        record_id: record_id.clone(),
                    })
                .then(|| record.id().clone())
            })
            .collect::<Vec<_>>();
        if ids.len() > 1 {
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "managed session has conflicting environment ownership",
            ));
        }
        let Some(id) = ids.first().cloned() else {
            return Ok(false);
        };
        let (terminal_record, mut cleanup_ownership) = {
            let _transaction = self
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original_record = self.record(record_id)?;
            let mut terminal_record = original_record.clone();
            terminal_record.provider_session = None;
            terminal_record.active_session = None;
            terminal_record.state = ManagedSessionState::Unavailable;
            terminal_record.last_error = Some("environment-profile-unavailable".to_owned());
            terminal_record.updated_at_unix_ms = unix_time_ms();
            let original_ownership = self
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&id)
                .cloned()
                .ok_or_else(|| {
                    failure(
                        NodeFailureCode::BackendOperationFailed,
                        "managed session environment ownership disappeared during cleanup",
                    )
                })?;
            let mut cleanup_ownership = original_ownership.clone();
            if cleanup_ownership.state() != MaterializationState::CleanupRequired {
                cleanup_ownership.mark_cleanup_required(unix_time_ms()).map_err(|_| {
                    failure(
                        NodeFailureCode::BackendOperationFailed,
                        "managed session environment cleanup state is invalid",
                    )
                })?;
            }
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .insert(record_id.clone(), terminal_record.clone());
            self.materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), cleanup_ownership.clone());
            if let Err(error) = self.persist_state_locked() {
                self.session_records
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .records
                    .insert(record_id.clone(), original_record);
                self.materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.clone(), original_ownership);
                return Err(persistence_failure(error));
            }
            (terminal_record, cleanup_ownership)
        };
        let cleanup = self
            .session_environment_materializer
            .as_ref()
            .ok_or(())
            .and_then(|materializer| materializer.cleanup(&cleanup_ownership).map_err(|_| ()));
        if cleanup.is_err() {
            let _ = cleanup_ownership.mark_recovery_required(unix_time_ms());
            self.materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), cleanup_ownership);
            if self.persist_state().is_err() {
                self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
            }
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "managed session environment cleanup requires recovery",
            ));
        }
        let _transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.remove_record_memory(record_id);
        self.materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        if let Err(error) = self.persist_state_locked() {
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .insert(record_id.clone(), terminal_record);
            self.materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id, cleanup_ownership);
            return Err(persistence_failure(error));
        }
        Ok(true)
    }

    fn mark_materialization_recovery_required(&self, id: &MaterializationId) {
        let changed = {
            let mut materializations = self
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(ownership) = materializations.get_mut(id) else {
                return;
            };
            ownership.mark_recovery_required(unix_time_ms()).is_ok()
        };
        if changed && self.persist_state().is_err() {
            self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
        }
    }

    fn reconcile_materializations(&self) {
        let records = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for mut ownership in records {
            let result = match (ownership.state(), ownership.owner()) {
                (MaterializationState::Preparing, _)
                | (MaterializationState::CleanupRequired, _) => {
                    self.cleanup_materialization(ownership.id())
                }
                (MaterializationState::Ready, MaterializationOwner::Session { .. }) => {
                    if ownership.mark_recovery_required(unix_time_ms()).is_ok() {
                        self.materializations
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(ownership.id().clone(), ownership.clone());
                        self.persist_state().map_err(persistence_failure)
                    } else {
                        Err(failure(
                            NodeFailureCode::BackendOperationFailed,
                            "session environment reconciliation failed",
                        ))
                    }
                }
                (MaterializationState::Ready, MaterializationOwner::Record { record_id }) => self
                    .session_environment_materializer
                    .as_ref()
                    .ok_or_else(|| {
                        failure(
                            NodeFailureCode::BackendOperationFailed,
                            "session environment materializer is unavailable",
                        )
                    })
                    .and_then(|materializer| {
                        materializer.revalidate(&ownership).map_err(|_| failure(
                            NodeFailureCode::BackendOperationFailed,
                            "session materialization revalidation failed",
                        ))?;
                        let record = self.record(record_id)?;
                        if record.environment_profile.as_ref() != ownership.environment_profile()
                            || record.bundle.as_ref() != ownership.bundle()
                            || record.context.as_ref() != ownership.context()
                        {
                            return Err(failure(
                                NodeFailureCode::BackendOperationFailed,
                                "session materialization ownership changed after restart",
                            ));
                        }
                        if let Some(receipt) = ownership.bundle() {
                            let catalog = self.bundle_catalog.read()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            let bundle = catalog.get(&receipt.id).ok_or_else(|| failure(
                                NodeFailureCode::UnknownBundle,
                                "session bundle is unavailable after restart",
                            ))?;
                            if bundle.receipt() != *receipt {
                                return Err(failure(
                                    NodeFailureCode::BundleBindingMismatch,
                                    "session bundle changed after restart",
                                ));
                            }
                            let layout = self.bundle_layout(
                                &record.provider,
                                record.mode,
                                record.environment_profile.as_ref(),
                                bundle,
                            )?;
                            materializer
                                .revalidate_bundle(&ownership, bundle, layout)
                                .map_err(|_| failure(
                                    NodeFailureCode::BundleMaterializationFailed,
                                    "session bundle revalidation failed",
                                ))?;
                        }
                        if let Some(receipt) = ownership.context() {
                            let pack = materializer
                                .revalidate_context(&ownership, receipt)
                                .map_err(|_| {
                                    failure(
                                        NodeFailureCode::ContextPackMaterializationFailed,
                                        "session context revalidation failed",
                                    )
                                })?;
                            self.context_catalog
                                .write()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .insert(pack)
                                .map_err(|_| {
                                    failure(
                                        NodeFailureCode::ContextPackMaterializationFailed,
                                        "session context catalog restore failed",
                                    )
                                })?;
                        }
                        Ok(())
                    }),
                (MaterializationState::RecoveryRequired, _) => Ok(()),
            };
            if result.is_err() && ownership.state() != MaterializationState::RecoveryRequired {
                let _ = ownership.mark_recovery_required(unix_time_ms());
                self.materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(ownership.id().clone(), ownership);
                if self.persist_state().is_err() {
                    self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
                }
            }
        }
        let materialized_profiles = self
            .environment_materialization_profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let ownership = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let changed = {
            let mut session_records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut changed = false;
            for record in session_records.records.values_mut() {
                let environment_profile = record.environment_profile.as_ref();
                if environment_profile.is_none()
                    && record.bundle.is_none()
                    && record.context.is_none()
                {
                    continue;
                }
                if record.bundle.is_none()
                    && environment_profile.is_some_and(|profile| {
                        !materialized_profiles.contains(&profile.profile_id)
                    })
                {
                    continue;
                }
                let has_exact_ownership = ownership.iter().any(|candidate| {
                    candidate.state() == MaterializationState::Ready
                        && candidate.environment_profile() == environment_profile
                        && candidate.bundle() == record.bundle.as_ref()
                        && candidate.context() == record.context.as_ref()
                        && candidate.owner()
                            == &MaterializationOwner::Record {
                                record_id: record.record_id.clone(),
                            }
                });
                if !has_exact_ownership {
                    record.provider_session = None;
                    record.active_session = None;
                    record.state = ManagedSessionState::Unavailable;
                    record.last_error = Some(if record.bundle.is_some() {
                        "bundle-unavailable".to_owned()
                    } else if record.context.is_some() {
                        "context-unavailable".to_owned()
                    } else {
                        "environment-profile-unavailable".to_owned()
                    });
                    record.updated_at_unix_ms = unix_time_ms();
                    changed = true;
                }
            }
            changed
        };
        if changed && self.persist_state().is_err() {
            self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
        }
    }

    fn replay_spawn_spec(
        &self,
        spec: &SpawnSpec,
        now: Instant,
    ) -> Result<Option<Result<ResolvedSpawnReceipt, NodeFailure>>, NodeFailure> {
        let mut cache = self
            .spawn_idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.entries.retain(|_, entry| entry.expires_at > now);
        if let Some(entry) = cache.entries.get(&spec.idempotency_key) {
            let SpawnIdempotencyValue::Standard { spec: existing, result } = &entry.value else {
                return Err(failure(
                    NodeFailureCode::SpawnIdempotencyConflict,
                    "spawn idempotency key was reused across spawn kinds",
                ));
            };
            if existing != spec {
                return Err(failure(
                    NodeFailureCode::SpawnIdempotencyConflict,
                    "spawn idempotency key was reused with a different specification",
                ));
            }
            return Ok(Some(result.clone()));
        }
        if cache.entries.len() >= SPAWN_IDEMPOTENCY_MAX_ENTRIES {
            return Err(failure(
                NodeFailureCode::SpawnIdempotencyCapacity,
                "spawn idempotency cache reached its bounded capacity",
            ));
        }
        Ok(None)
    }

    fn remember_spawn_spec(
        &self,
        spec: SpawnSpec,
        result: Result<ResolvedSpawnReceipt, NodeFailure>,
        accepted_at: Instant,
    ) {
        self
            .spawn_idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .insert(
                spec.idempotency_key.clone(),
                SpawnIdempotencyEntry {
                    value: SpawnIdempotencyValue::Standard { spec, result },
                    expires_at: accepted_at + Duration::from_millis(SPAWN_IDEMPOTENCY_TTL_MS),
                },
            );
    }

    fn claim_harness_mcp_spawn(
        &self,
        _reservation_id: &HarnessMcpReservationId,
        _activation_digest: &HarnessMcpActivationDigest,
        spec: &SpawnSpec,
        now: Instant,
    ) -> Result<(), NodeFailure> {
        let mut cache = self.spawn_idempotency.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.entries.retain(|_, entry| entry.expires_at > now);
        if cache.entries.contains_key(&spec.idempotency_key) {
            return Err(failure(
                NodeFailureCode::SpawnIdempotencyConflict,
                "spawn idempotency key was reused across spawn kinds",
            ));
        }
        if cache.entries.len() >= SPAWN_IDEMPOTENCY_MAX_ENTRIES {
            return Err(failure(
                NodeFailureCode::SpawnIdempotencyCapacity,
                "spawn idempotency cache reached its bounded capacity",
            ));
        }
        cache.entries.insert(spec.idempotency_key.clone(), SpawnIdempotencyEntry {
            value: SpawnIdempotencyValue::HarnessMcp,
            expires_at: now + Duration::from_millis(SPAWN_IDEMPOTENCY_TTL_MS),
        });
        Ok(())
    }

    fn replay_managed_spawn(
        &self,
        request: &ManagedWorktreeSpawnRequest,
        now: Instant,
    ) -> Result<Option<Result<ManagedWorktreeSpawnReceipt, NodeFailure>>, NodeFailure> {
        let mut cache = self.spawn_idempotency.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.entries.retain(|_, entry| entry.expires_at > now);
        if let Some(entry) = cache.entries.get(&request.spawn_spec.idempotency_key) {
            let SpawnIdempotencyValue::Managed { request: existing, result } = &entry.value else {
                return Err(failure(
                    NodeFailureCode::SpawnIdempotencyConflict,
                    "spawn idempotency key was reused across spawn kinds",
                ));
            };
            if existing != request {
                return Err(failure(
                    NodeFailureCode::SpawnIdempotencyConflict,
                    "spawn idempotency key was reused with a different managed specification",
                ));
            }
            return Ok(Some(result.clone()));
        }
        if cache.entries.len() >= SPAWN_IDEMPOTENCY_MAX_ENTRIES {
            return Err(failure(
                NodeFailureCode::SpawnIdempotencyCapacity,
                "spawn idempotency cache reached its bounded capacity",
            ));
        }
        Ok(None)
    }

    fn remember_managed_spawn(
        &self,
        request: ManagedWorktreeSpawnRequest,
        result: Result<ManagedWorktreeSpawnReceipt, NodeFailure>,
        accepted_at: Instant,
    ) {
        self.spawn_idempotency.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .insert(
                request.spawn_spec.idempotency_key.clone(),
                SpawnIdempotencyEntry {
                    value: SpawnIdempotencyValue::Managed { request, result },
                    expires_at: accepted_at + Duration::from_millis(SPAWN_IDEMPOTENCY_TTL_MS),
                },
            );
    }

    fn remember_managed_spawn_attempt(
        &self,
        durable_v2_key: Option<&SpawnIdempotencyKey>,
        request: ManagedWorktreeSpawnRequest,
        result: Result<ManagedWorktreeSpawnReceipt, NodeFailure>,
        accepted_at: Instant,
    ) {
        if durable_v2_key.is_none() {
            self.remember_managed_spawn(request, result, accepted_at);
        }
    }

    fn acquire_durable_managed_spawn_v2(
        &self,
        request: &ManagedWorktreeSpawnRequestV2,
        request_digest: String,
    ) -> Result<DurableManagedSpawnReplayDecision, NodeFailure> {
        if self.state_path.is_none() {
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "managed worktree V2 requires durable node state",
            ));
        }
        let _transaction = self.state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = request.spawn_spec.idempotency_key.clone();
        let mut previous = None;
        {
            let mut replays = self.managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(record) = replays.get_mut(&key) {
                if record.request_digest != request_digest {
                    return Err(failure(
                        NodeFailureCode::SpawnIdempotencyConflict,
                        "managed worktree V2 idempotency key was reused with a different request",
                    ));
                }
                match &record.state {
                    ManagedWorktreeSpawnReplayStateV10::Pending {
                        owner_incarnation_id,
                        lease_id: None,
                    } if owner_incarnation_id != &self.incarnation_id => {
                        previous = Some(record.clone());
                        record.state = ManagedWorktreeSpawnReplayStateV10::Pending {
                            owner_incarnation_id: self.incarnation_id,
                            lease_id: None,
                        };
                        record.updated_at_unix_ms = unix_time_ms();
                    }
                    ManagedWorktreeSpawnReplayStateV10::Pending {
                        lease_id: None,
                        ..
                    } => {
                        return Err(failure(
                            NodeFailureCode::BackendBusy,
                            "managed worktree V2 reservation is already active",
                        ));
                    }
                    ManagedWorktreeSpawnReplayStateV10::Pending {
                        lease_id: Some(_),
                        ..
                    } => {
                        return Ok(DurableManagedSpawnReplayDecision::PendingLinked);
                    }
                    ManagedWorktreeSpawnReplayStateV10::Committed { receipt } => {
                        return Ok(DurableManagedSpawnReplayDecision::Committed(
                            receipt.clone(),
                        ));
                    }
                }
            } else if replays.len() >= MAX_MANAGED_WORKTREE_SPAWN_REPLAYS {
                return Err(failure(
                    NodeFailureCode::SpawnIdempotencyCapacity,
                    "durable managed worktree replay ledger reached its bounded capacity",
                ));
            } else {
                let now = unix_time_ms();
                replays.insert(key.clone(), ManagedWorktreeSpawnReplayRecordV10 {
                    idempotency_key: key.clone(),
                    request_digest,
                    source_workspace_id: request.spawn_spec.target.workspace_id.clone(),
                    profile_id: request.worktree_profile_id.clone(),
                    expected_profile_revision: request.expected_profile_revision.clone(),
                    state: ManagedWorktreeSpawnReplayStateV10::Pending {
                        owner_incarnation_id: self.incarnation_id,
                        lease_id: None,
                    },
                    created_at_unix_ms: now,
                    updated_at_unix_ms: now,
                });
            }
        }
        if let Err(error) = self.persist_state_locked() {
            let mut replays = self.managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match previous {
                Some(previous) => {
                    replays.insert(key, previous);
                }
                None => {
                    replays.remove(&key);
                }
            }
            return Err(persistence_failure(error));
        }
        Ok(DurableManagedSpawnReplayDecision::Reserved)
    }

    fn link_durable_managed_spawn_v2_lease_locked(
        &self,
        key: &SpawnIdempotencyKey,
        lease_id: &ManagedWorktreeLeaseId,
    ) -> Result<ManagedWorktreeSpawnReplayRecordV10, NodeFailure> {
        let mut replays = self.managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = replays.get_mut(key).ok_or_else(|| {
            failure(
                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                "managed worktree V2 reservation disappeared before allocation",
            )
        })?;
        let previous = record.clone();
        match &mut record.state {
            ManagedWorktreeSpawnReplayStateV10::Pending {
                lease_id: current,
                ..
            }
                if current.is_none() =>
            {
                *current = Some(lease_id.clone());
                record.updated_at_unix_ms = unix_time_ms();
                Ok(previous)
            }
            _ => Err(failure(
                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                "managed worktree V2 reservation is not allocation-ready",
            )),
        }
    }

    fn restore_durable_managed_spawn_v2_locked(
        &self,
        key: &SpawnIdempotencyKey,
        previous: ManagedWorktreeSpawnReplayRecordV10,
    ) {
        self.managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone(), previous);
    }

    fn commit_durable_managed_spawn_v2(
        &self,
        request: &ManagedWorktreeSpawnRequestV2,
        receipt: ManagedWorktreeSpawnReceipt,
    ) -> Result<(), NodeFailure> {
        let _transaction = self.state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = &request.spawn_spec.idempotency_key;
        let previous = {
            let mut replays = self.managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let record = replays.get_mut(key).ok_or_else(|| {
                failure(
                    NodeFailureCode::ManagedWorktreeRecoveryRequired,
                    "managed worktree V2 reservation disappeared before commit",
                )
            })?;
            let previous = record.clone();
            match &record.state {
                ManagedWorktreeSpawnReplayStateV10::Pending {
                    lease_id: Some(lease_id),
                    ..
                } if lease_id == &receipt.lease.lease_id => {}
                _ => {
                    return Err(failure(
                        NodeFailureCode::ManagedWorktreeRecoveryRequired,
                        "managed worktree V2 reservation is not linked to the spawned lease",
                    ));
                }
            }
            record.state = ManagedWorktreeSpawnReplayStateV10::Committed { receipt };
            record.updated_at_unix_ms = unix_time_ms();
            previous
        };
        if let Err(error) = self.persist_state_locked() {
            self.restore_durable_managed_spawn_v2_locked(key, previous);
            return Err(persistence_failure(error));
        }
        Ok(())
    }

    fn remove_unallocated_durable_managed_spawn_v2(
        &self,
        request: &ManagedWorktreeSpawnRequestV2,
    ) -> Result<(), NodeFailure> {
        let _transaction = self.state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = &request.spawn_spec.idempotency_key;
        let removed = {
            let mut replays = self.managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match replays.get(key).map(|record| &record.state) {
                Some(ManagedWorktreeSpawnReplayStateV10::Pending {
                    lease_id: None,
                    ..
                }) => {
                    replays.remove(key)
                }
                _ => None,
            }
        };
        let Some(removed) = removed else {
            return Ok(());
        };
        if let Err(error) = self.persist_state_locked() {
            self.managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key.clone(), removed);
            return Err(persistence_failure(error));
        }
        Ok(())
    }

    fn resolve_spawn_spec(&self, spec: &SpawnSpec) -> Result<ResolvedSpawnSpec, NodeFailure> {
        if spec.target.node_id != self.node_id {
            return Err(failure(
                NodeFailureCode::SpawnTargetMismatch,
                "spawn target does not match this node",
            ));
        }
        let defaults = self.spawn_profiles.get(&spec.profile_id).ok_or_else(|| {
            failure(
                NodeFailureCode::UnknownSpawnProfile,
                "spawn profile is unavailable on this node",
            )
        })?;
        let resolved = spec.resolve(defaults).map_err(|error| match error {
            SpawnSpecResolveError::ProfileRevisionMismatch { .. } => failure(
                NodeFailureCode::SpawnProfileRevisionMismatch,
                "spawn profile revision does not match the loaded profile",
            ),
            _ => failure(
                NodeFailureCode::InvalidRequest,
                "spawn specification could not be resolved",
            ),
        })?;
        if resolved.target.worktree_id.is_some() {
            self.require_worktree_service(&resolved.target.workspace_id)?;
        }
        let environment_profile = self.resolve_environment_profile(&resolved)?;
        self.resolve_bundle(&resolved, environment_profile.as_ref())?;
        self.resolve_context(&resolved)?;
        Ok(resolved)
    }

    async fn resolve_spawn_workspace(
        &self,
        resolved: &ResolvedSpawnSpec,
        deadline: Instant,
    ) -> Result<WorkspaceId, NodeFailure> {
        let Some(worktree_id) = resolved.target.worktree_id.as_ref() else {
            return Ok(resolved.target.workspace_id.clone());
        };
        if self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lease_for_workspace(worktree_id)
            .is_some()
        {
            return Err(failure(
                NodeFailureCode::ManagedWorktreeOwnershipConflict,
                "managed worktree targets may only be spawned through their lease request",
            ));
        }
        self.require_worktree_service(&resolved.target.workspace_id)?;
        let source_root = self.workspace_root(&resolved.target.workspace_id)?;
        let selected_root = self.workspace_root(worktree_id)?;
        let remaining = spawn_deadline_remaining(deadline)?;
        let worktrees = timeout(remaining, list_git_worktrees(&source_root))
            .await
            .map_err(|_| {
                failure(
                    NodeFailureCode::SpawnDeadlineExceeded,
                    "Git worktree selection exceeded the spawn deadline",
                )
            })?
            .map_err(git_worktree_failure)?;
        validate_selected_worktree(&source_root, &selected_root, &worktrees)?;
        Ok(worktree_id.clone())
    }

    async fn spawn_from_spec(
        &self,
        spec: SpawnSpec,
    ) -> Result<ResolvedSpawnReceipt, NodeFailure> {
        self.spawn_from_spec_with_proxy(spec, None).await
    }

    async fn spawn_from_spec_with_proxy(
        &self,
        spec: SpawnSpec,
        harness_mcp: Option<&PreparedHarnessMcpSpawn>,
    ) -> Result<ResolvedSpawnReceipt, NodeFailure> {
        let accepted_at = Instant::now();
        if harness_mcp.is_none() {
            if let Some(replayed) = self.replay_spawn_spec(&spec, accepted_at)? {
                return replayed;
            }
        }
        let resolved = self.resolve_spawn_spec(&spec)?;
        let environment_profile = self.resolve_environment_profile(&resolved)?;
        let bundle = self.resolve_bundle(&resolved, environment_profile.as_ref())?;
        let context = self.resolve_context(&resolved)?;
        let required_capabilities = spawn_runtime_capabilities(&resolved.required_capabilities)?;
        let deadline = accepted_at + Duration::from_millis(resolved.deadline_ms.get());
        let spawn_workspace_id = self.resolve_spawn_workspace(&resolved, deadline).await;
        let spawn_workspace_id = match spawn_workspace_id {
            Ok(workspace_id) => workspace_id,
            Err(error) => {
                let result = Err(error);
                if harness_mcp.is_none() {
                    self.remember_spawn_spec(spec, result.clone(), accepted_at);
                }
                return result;
            }
        };
        if self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lease_for_workspace(&spawn_workspace_id)
            .is_some()
        {
            let result = Err(failure(
                NodeFailureCode::ManagedWorktreeOwnershipConflict,
                "managed worktree targets may only be spawned through their lease request",
            ));
            if harness_mcp.is_none() {
                self.remember_spawn_spec(spec, result.clone(), accepted_at);
            }
            return result;
        }
        let result = self
            .spawn_session_with_deadline(
                spawn_workspace_id,
                resolved.provider.clone(),
                resolved.mode,
                resolved.terminal_size,
                resolved.prompt.as_ref().map(|prompt| prompt.as_str().to_owned()),
                environment_profile.clone(),
                bundle.clone(),
                context.clone(),
                None,
                None,
                SpawnRecordPolicy::Always,
                Some(deadline),
                &required_capabilities,
                harness_mcp,
            )
            .await
            .map(|(session, _runtime_policy)| {
                resolved.receipt_with_materialization(
                    self.incarnation_id,
                    session,
                    environment_profile,
                    bundle,
                    context,
                )
            });
        if harness_mcp.is_none() {
            self.remember_spawn_spec(spec, result.clone(), accepted_at);
        }
        result
    }

    async fn spawn_from_spec_with_harness_mcp(
        &self,
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
        spec: SpawnSpec,
        deadline_unix_ms: u64,
    ) -> Result<ResolvedSpawnReceipt, NodeFailure> {
        let registry = self.harness_mcp_registry.as_ref().ok_or_else(|| {
            failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP proxy is not configured")
        })?;
        if let Some(receipt) = registry
            .replay_spawn(&reservation_id, &activation_digest, &spec)
            .map_err(harness_mcp_failure)?
        {
            return Ok(receipt);
        }
        let resolved = self.resolve_spawn_spec(&spec)?;
        let prepared = registry
            .prepare_spawn(
                &reservation_id,
                &activation_digest,
                &spec,
                deadline_unix_ms,
                resolved.provider.clone(),
            )
            .map_err(harness_mcp_failure)?;
        self.claim_harness_mcp_spawn(
            &reservation_id,
            &activation_digest,
            &spec,
            Instant::now(),
        )?;
        let spawn = self.spawn_from_spec_with_proxy(spec, Some(&prepared)).await;
        let mut receipt = match spawn {
            Ok(receipt) => receipt,
            Err(error) => {
                let _ = registry.abort(&reservation_id, &activation_digest);
                return Err(error);
            }
        };
        let record_id = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&receipt.session.session.instance_id)
            .and_then(|binding| binding.record_id.clone());
        let record_id = match record_id {
            Some(record_id) => record_id,
            None => {
                let rollback = self.remove_session(&receipt.session).await;
                let _ = registry.abort(&reservation_id, &activation_digest);
                if let Err(rollback_error) = rollback {
                    return Err(rollback_error);
                }
                return Err(failure(
                    NodeFailureCode::BindingMismatch,
                    "harness MCP spawn did not retain its exact managed record",
                ));
            }
        };
        receipt.harness_mcp_proxy = Some(ResolvedHarnessMcpProxyReceiptV1 {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
        });
        if let Err(error) = registry.mark_spawned(
            &prepared,
            receipt.session.clone(),
            record_id,
            receipt.clone(),
        ) {
            let rollback = self.remove_session(&receipt.session).await;
            let _ = registry.abort(&reservation_id, &activation_digest);
            if let Err(rollback_error) = rollback {
                return Err(rollback_error);
            }
            return Err(harness_mcp_failure(error));
        }
        Ok(receipt)
    }

    async fn spawn_managed_worktree(
        &self,
        request: ManagedWorktreeSpawnRequest,
    ) -> Result<ManagedWorktreeSpawnReceipt, NodeFailure> {
        self.spawn_managed_worktree_inner(request, None).await
    }

    async fn spawn_managed_worktree_inner(
        &self,
        request: ManagedWorktreeSpawnRequest,
        durable_v2_key: Option<&SpawnIdempotencyKey>,
    ) -> Result<ManagedWorktreeSpawnReceipt, NodeFailure> {
        let accepted_at = Instant::now();
        if durable_v2_key.is_none() {
            if let Some(replayed) = self.replay_managed_spawn(&request, accepted_at)? {
                return replayed;
            }
        }
        let resolved = self.resolve_spawn_spec(&request.spawn_spec)?;
        if request.spawn_spec.target.worktree_id.is_some() {
            let result = Err(failure(
                NodeFailureCode::InvalidRequest,
                "managed worktree spawn must not select a caller-provided worktree",
            ));
            self.remember_managed_spawn_attempt(
                durable_v2_key,
                request,
                result.clone(),
                accepted_at,
            );
            return result;
        }
        let environment_profile = self.resolve_environment_profile(&resolved)?;
        let bundle = self.resolve_bundle(&resolved, environment_profile.as_ref())?;
        let context = self.resolve_context(&resolved)?;
        let source_workspace_id = resolved.target.workspace_id.clone();
        let profile = self.managed_profile(&source_workspace_id, &request.worktree_profile_id)?;
        let deadline = accepted_at + Duration::from_millis(resolved.deadline_ms.get());
        let required_capabilities = spawn_runtime_capabilities(&resolved.required_capabilities)?;
        let runtime_requirement = match (resolved.mode, resolved.prompt.is_some()) {
            (SessionMode::Pty, false) => ProviderRuntimeRequirement::RawPty,
            (SessionMode::Pty, true) => ProviderRuntimeRequirement::SemanticPrompt,
            (SessionMode::Inline, _) => ProviderRuntimeRequirement::Inline,
        };
        let runtime_policy = timeout(
            spawn_deadline_remaining(deadline)?,
            self.admit_provider_runtime(&resolved.provider, runtime_requirement),
        ).await.map_err(|_| failure(
            NodeFailureCode::SpawnDeadlineExceeded,
            "provider admission exceeded the managed spawn deadline",
        ))??;
        if required_capabilities.iter().any(|capability| !runtime_policy.admits(*capability)) {
            return Err(failure(
                NodeFailureCode::UnsupportedSpawnCapability,
                "provider runtime does not admit a required spawn capability",
            ));
        }
        self.ensure_binding_capacity()?;
        let source_root = self.workspace_root(&source_workspace_id)?;
        profile.validate_for_workspace(&source_root)
            .map_err(|message| failure(NodeFailureCode::ManagedWorktreeOwnershipConflict, &message))?;
        let base_timeout_ms = u64::try_from(
            spawn_deadline_remaining(deadline)?.as_millis(),
        ).unwrap_or(u64::MAX).max(1);
        let base_commit = resolve_base_commit_with_timeout(
            &source_root,
            profile.base(),
            base_timeout_ms,
        ).await.map_err(|error| {
            if Instant::now() >= deadline || error.message.contains("timed out") {
                failure(
                    NodeFailureCode::SpawnDeadlineExceeded,
                    "managed worktree base resolution exceeded the spawn deadline",
                )
            } else {
                managed_git_worktree_failure(error)
            }
        })?;

        let lease = {
            let _transaction = self.state_transaction.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            let lease = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .allocate(source_workspace_id.clone(), &profile, base_commit.clone(), unix_time_ms())
                .map_err(|message| failure(NodeFailureCode::BackendBusy, &message))?;
            let previous_replay = match durable_v2_key
                .map(|key| self.link_durable_managed_spawn_v2_lease_locked(key, &lease.lease_id))
                .transpose()
            {
                Ok(previous) => previous,
                Err(error) => {
                    *self.managed_worktrees.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous;
                    return Err(error);
                }
            };
            if let Err(error) = self.persist_state_locked() {
                *self.managed_worktrees.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous;
                if let (Some(key), Some(previous_replay)) =
                    (durable_v2_key, previous_replay)
                {
                    self.restore_durable_managed_spawn_v2_locked(key, previous_replay);
                }
                return Err(persistence_failure(error));
            }
            lease
        };
        self.publish(NodeEvent::ManagedWorktreeUpserted { lease: lease.snapshot() });
        if let Err(message) = profile.validate_target_authority(&lease.lease_id, &lease.target_root) {
            self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove_unmutated(&lease.lease_id);
            if self.persist_state().is_err() {
                self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
            }
            let result = Err(failure(NodeFailureCode::ManagedWorktreeOwnershipConflict, &message));
            self.remember_managed_spawn_attempt(
                durable_v2_key,
                request,
                result.clone(),
                accepted_at,
            );
            return result;
        }
        let mutation_timeout_ms = match spawn_deadline_remaining(deadline) {
            Ok(remaining) => u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX).max(1),
            Err(error) => {
                self.reconcile_failed_allocation(&lease, &source_root).await;
                let result = Err(error);
                self.remember_managed_spawn_attempt(
                    durable_v2_key,
                    request,
                    result.clone(),
                    accepted_at,
                );
                return result;
            }
        };
        let created = match create_git_worktree_with_timeout(
                &source_root,
                &lease.target_root,
                &lease.branch,
                Some(&base_commit),
                mutation_timeout_ms,
            ).await {
            Ok(created) => created,
            Err(error) => {
                self.reconcile_failed_allocation(&lease, &source_root).await;
                let result = Err(if Instant::now() >= deadline
                    || error.message.contains("timed out")
                    || error.message.contains("deadline")
                {
                    failure(
                        NodeFailureCode::SpawnDeadlineExceeded,
                        "managed worktree creation exceeded the spawn deadline",
                    )
                } else {
                    managed_git_worktree_failure(error)
                });
                self.remember_managed_spawn_attempt(
                    durable_v2_key,
                    request,
                    result.clone(),
                    accepted_at,
                );
                return result;
            }
        };
        if !exact_created_worktree(&lease, &created.path, created.branch.as_deref(), &created.head) {
            self.mark_managed_recovery_required(&lease.lease_id, ManagedWorktreeCleanupFailure::OwnershipConflict);
            let result = Err(failure(
                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                "created worktree identity did not match its durable lease",
            ));
            self.remember_managed_spawn_attempt(
                durable_v2_key,
                request,
                result.clone(),
                accepted_at,
            );
            return result;
        }
        {
            let mut registry = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current = registry.get_mut(&lease.lease_id)
                .expect("allocated managed worktree lease remains present");
            current.expected_head = Some(created.head.clone());
            current.state = ManagedWorktreeLeaseState::Ready;
            current.updated_at_unix_ms = unix_time_ms();
        }
        if let Err(error) = self.persist_state() {
            self.mark_managed_recovery_required(
                &lease.lease_id,
                ManagedWorktreeCleanupFailure::Backend,
            );
            let result = Err(persistence_failure(error));
            self.remember_managed_spawn_attempt(
                durable_v2_key,
                request,
                result.clone(),
                accepted_at,
            );
            return result;
        }
        let ready = self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&lease.lease_id)
            .expect("ready managed worktree lease remains present")
            .snapshot();
        self.publish(NodeEvent::ManagedWorktreeUpserted { lease: ready });
        if let Err(error) = self.register_workspace(lease.workspace_id.clone(), created.path.clone()).await {
            let _ = self.cleanup_managed_worktree(&lease.lease_id, true).await;
            let result = Err(error);
            self.remember_managed_spawn_attempt(
                durable_v2_key,
                request,
                result.clone(),
                accepted_at,
            );
            return result;
        }
        let spawn = self.spawn_session_with_deadline(
            lease.workspace_id.clone(),
            resolved.provider.clone(),
            resolved.mode,
            resolved.terminal_size,
            resolved.prompt.as_ref().map(|prompt| prompt.as_str().to_owned()),
            environment_profile.clone(),
            bundle.clone(),
            context.clone(),
            Some(lease.lease_id.clone()),
            Some(runtime_policy),
            SpawnRecordPolicy::Always,
            Some(deadline),
            &required_capabilities,
            None,
        ).await;
        let (session, effective_runtime_policy) = match spawn {
            Ok(spawned) => spawned,
            Err(error) => {
                if self
                    .cleanup_managed_worktree(&lease.lease_id, true)
                    .await
                    .is_err()
                {
                    self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::Backend,
                    );
                }
                let result = Err(error);
                self.remember_managed_spawn_attempt(
                    durable_v2_key,
                    request,
                    result.clone(),
                    accepted_at,
                );
                return result;
            }
        };
        let (record_id, raw_inventory_record_id) = {
            let mut bindings = self.session_bindings.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let binding = bindings.get_mut(&session.session.instance_id)
                .expect("successful spawn retains its session binding");
            binding.managed_worktree_lease_id = Some(lease.lease_id.clone());
            managed_spawn_record_ownership(binding, effective_runtime_policy)
        };
        let bound = self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bind_session(
                &lease.lease_id,
                ManagedWorktreeSessionHolder {
                    incarnation_id: self.incarnation_id,
                    instance_id: session.session.instance_id,
                    generation: session.session.generation,
                },
                record_id,
                unix_time_ms(),
            )
            .map_err(|message| failure(NodeFailureCode::ManagedWorktreeRecoveryRequired, &message));
        let snapshot = match bound {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if raw_inventory_record_id
                    .as_ref()
                    .is_some_and(|record_id| self.discard_record(record_id).is_err())
                {
                    self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::Backend,
                    );
                }
                let _ = self.remove_session(&session).await;
                self.mark_managed_recovery_required(
                    &lease.lease_id,
                    ManagedWorktreeCleanupFailure::Backend,
                );
                let result = Err(error);
                self.remember_managed_spawn_attempt(
                    durable_v2_key,
                    request,
                    result.clone(),
                    accepted_at,
                );
                return result;
            }
        };
        if let Err(error) = self.persist_state() {
            if raw_inventory_record_id
                .as_ref()
                .is_some_and(|record_id| self.discard_record(record_id).is_err())
            {
                self.mark_managed_recovery_required(
                    &lease.lease_id,
                    ManagedWorktreeCleanupFailure::Backend,
                );
            }
            let _ = self.remove_session(&session).await;
            if self
                .cleanup_managed_worktree(&lease.lease_id, true)
                .await
                .is_err()
            {
                self.mark_managed_recovery_required(
                    &lease.lease_id,
                    ManagedWorktreeCleanupFailure::Backend,
                );
            }
            let result = Err(persistence_failure(error));
            self.remember_managed_spawn_attempt(
                durable_v2_key,
                request,
                result.clone(),
                accepted_at,
            );
            return result;
        }
        self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot.clone() });
        let receipt = managed_spawn_receipt(
            &resolved,
            self.incarnation_id,
            session,
            snapshot,
            environment_profile,
            bundle,
            context,
        );
        let result = Ok(receipt);
        self.remember_managed_spawn_attempt(
            durable_v2_key,
            request,
            result.clone(),
            accepted_at,
        );
        result
    }

    async fn spawn_managed_worktree_v2(
        &self,
        request: ManagedWorktreeSpawnRequestV2,
    ) -> Result<ManagedWorktreeSpawnReceipt, NodeFailure> {
        if request.spawn_spec.target.node_id != self.node_id {
            return Err(failure(
                NodeFailureCode::SpawnTargetMismatch,
                "spawn target does not match this node",
            ));
        }
        if request.spawn_spec.target.worktree_id.is_some() {
            return Err(failure(
                NodeFailureCode::InvalidRequest,
                "managed worktree spawn must not select a caller-provided worktree",
            ));
        }
        let request_digest = managed_spawn_request_digest_v2(&request)?;
        let replay = self.acquire_durable_managed_spawn_v2(&request, request_digest)?;
        match replay {
            DurableManagedSpawnReplayDecision::Committed(receipt) => return Ok(receipt),
            DurableManagedSpawnReplayDecision::PendingLinked => {
                return Err(failure(
                    NodeFailureCode::ManagedWorktreeRecoveryRequired,
                    "managed worktree V2 reservation has an incomplete durable allocation",
                ));
            }
            DurableManagedSpawnReplayDecision::Reserved => {}
        }
        let validated = (|| {
            let resolved = self.resolve_spawn_spec(&request.spawn_spec)?;
            let profile = self.managed_profile(
                &resolved.target.workspace_id,
                &request.worktree_profile_id,
            )?;
            if profile.revision() != &request.expected_profile_revision {
                return Err(failure(
                    NodeFailureCode::ManagedWorktreeProfileRevisionMismatch,
                    "managed worktree profile revision does not match the loaded profile",
                ));
            }
            Ok(())
        })();
        match validated {
            Ok(()) => {}
            Err(error) => {
                self.remove_unallocated_durable_managed_spawn_v2(&request)?;
                return Err(error);
            }
        }
        let legacy = ManagedWorktreeSpawnRequest {
            spawn_spec: request.spawn_spec.clone(),
            worktree_profile_id: request.worktree_profile_id.clone(),
        };
        match self
            .spawn_managed_worktree_inner(
                legacy,
                Some(&request.spawn_spec.idempotency_key),
            )
            .await
        {
            Ok(receipt) => {
                self.commit_durable_managed_spawn_v2(&request, receipt.clone())?;
                Ok(receipt)
            }
            Err(error) => {
                self.remove_unallocated_durable_managed_spawn_v2(&request)?;
                Err(error)
            }
        }
    }

    fn managed_profile(
        &self,
        workspace_id: &WorkspaceId,
        profile_id: &WorktreeProfileId,
    ) -> Result<ManagedWorktreeProfile, NodeFailure> {
        if self.worktree_service_modes.get(workspace_id).copied()
            != Some(WorktreeServiceMode::Managed)
        {
            return Err(failure(
                NodeFailureCode::UnsupportedSpawnCapability,
                "workspace does not enable managed worktrees",
            ));
        }
        self.managed_worktree_profiles.get(workspace_id)
            .and_then(|profiles| profiles.get(profile_id))
            .cloned()
            .ok_or_else(|| failure(
                NodeFailureCode::InvalidRequest,
                "managed worktree profile is unavailable for this workspace",
            ))
    }

    async fn reconcile_failed_allocation(
        &self,
        lease: &ManagedWorktreeLeaseRecord,
        source_root: &str,
    ) {
        match list_git_worktrees(source_root).await {
            Ok(worktrees) => {
                match worktrees.iter().find(|item| worktree_paths_equal(&item.path, &lease.target_root)) {
                    None => {
                        match std::fs::symlink_metadata(&lease.target_root) {
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                self.managed_worktrees.lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .remove_unmutated(&lease.lease_id);
                                let _ = self.persist_state();
                            }
                            _ => self.mark_managed_recovery_required(
                                &lease.lease_id,
                                ManagedWorktreeCleanupFailure::OwnershipConflict,
                            ),
                        }
                    }
                    Some(item) if exact_owned_worktree(
                        lease,
                        &item.path,
                        item.branch.as_deref(),
                        &item.head,
                    ) => {
                        let _ = self.cleanup_managed_worktree(&lease.lease_id, true).await;
                    }
                    Some(_) => self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::OwnershipConflict,
                    ),
                }
            }
            Err(_) => self.mark_managed_recovery_required(
                &lease.lease_id,
                ManagedWorktreeCleanupFailure::Backend,
            ),
        }
    }

    async fn reconcile_managed_worktrees(&self) {
        let leases = self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_records()
            .cloned()
            .collect::<Vec<_>>();
        for lease in leases {
            let profile = match self.managed_profile(&lease.source_workspace_id, &lease.profile_id) {
                Ok(profile) if profile.revision() == &lease.profile_revision => profile,
                _ => {
                    self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::OwnershipConflict,
                    );
                    continue;
                }
            };
            if profile.validate_target_authority(&lease.lease_id, &lease.target_root).is_err() {
                self.mark_managed_recovery_required(
                    &lease.lease_id,
                    ManagedWorktreeCleanupFailure::OwnershipConflict,
                );
                continue;
            }
            let source_root = match self.workspace_root(&lease.source_workspace_id) {
                Ok(root) => root,
                Err(_) => {
                    self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::OwnershipConflict,
                    );
                    continue;
                }
            };
            let worktrees = match list_git_worktrees(&source_root).await {
                Ok(worktrees) => worktrees,
                Err(_) => {
                    self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::Backend,
                    );
                    continue;
                }
            };
            let listed = worktrees.iter()
                .find(|item| worktree_paths_equal(&item.path, &lease.target_root));
            let Some(listed) = listed else {
                match std::fs::symlink_metadata(&lease.target_root) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound
                        && lease.state == ManagedWorktreeLeaseState::Allocating
                        && !lease.has_holders() => {
                            let removed = self.managed_worktrees.lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .tombstone(&lease.lease_id, unix_time_ms());
                            if self.persist_state().is_ok() && removed.is_some() {
                                self.publish(NodeEvent::ManagedWorktreeRemoved {
                                    lease_id: lease.lease_id.clone(),
                                });
                            }
                        }
                    _ => self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::OwnershipConflict,
                    ),
                }
                continue;
            };
            if listed.is_main || listed.is_bare || !exact_owned_worktree(
                &lease,
                &listed.path,
                listed.branch.as_deref(),
                &listed.head,
            ) {
                self.mark_managed_recovery_required(
                    &lease.lease_id,
                    ManagedWorktreeCleanupFailure::OwnershipConflict,
                );
                continue;
            }
            if !self.workspaces.read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&lease.workspace_id)
            {
                if self.register_workspace(lease.workspace_id.clone(), listed.path.clone()).await.is_err() {
                    self.mark_managed_recovery_required(
                        &lease.lease_id,
                        ManagedWorktreeCleanupFailure::Backend,
                    );
                    continue;
                }
            }
            let snapshot = {
                let mut registry = self.managed_worktrees.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let current = registry.get_mut(&lease.lease_id)
                    .expect("reconciled managed lease remains present");
                current.expected_head = Some(listed.head.clone());
                current.cleanup_failure = None;
                current.state = if current.has_holders() {
                    ManagedWorktreeLeaseState::InUse
                } else if current.retention == ManagedWorktreeRetention::Retain {
                    ManagedWorktreeLeaseState::Retained
                } else {
                    ManagedWorktreeLeaseState::Ready
                };
                current.updated_at_unix_ms = unix_time_ms();
                current.snapshot()
            };
            if self.persist_state().is_err() {
                self.mark_managed_recovery_required(
                    &lease.lease_id,
                    ManagedWorktreeCleanupFailure::Backend,
                );
                continue;
            }
            self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot.clone() });
            if snapshot.state == ManagedWorktreeLeaseState::Ready {
                let _ = self.cleanup_managed_worktree(&lease.lease_id, false).await;
            }
        }
    }

    /// Crash-recovery startup sweep: durably capture the pack for every
    /// `Live` managed record whose most recent session has not yet produced
    /// one (recovers a session whose `Exited{0}` event was observed but the
    /// Node crashed before the export finished). The reactive fast path for
    /// a single just-exited record does not go through this — see
    /// `drive_runtime_until_shutdown`, which resolves and spawns directly so
    /// it can capture the exact `SessionAddress` before `publish_control`
    /// clears it, rather than re-deriving it from (by then likely already
    /// `Dormant`) record state the way a rescan naturally would.
    ///
    /// Returns as soon as every eligible record's export has been *spawned*,
    /// not completed — each one runs on its own detached task (see
    /// `reconcile_context_pack_export_for_record`'s own doc comment for why:
    /// `NativeRuntime::tick()` has not even started its own loop yet at the
    /// point this is called from `NodeServer::run()`, so nothing could ever
    /// drain the history round trip if this awaited it inline here either).
    /// Best-effort throughout: never panics, no unwrap/expect; every failure
    /// is a silent skip for THIS record — an unusable source or a racing
    /// record is not surfaced to any caller.
    async fn reconcile_context_pack_exports(self: &Arc<Self>) {
        let candidates: Vec<(SessionRecordId, SessionAddress)> = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .values()
            .filter(|record| {
                record.state == ManagedSessionState::Live
                    && record.exported_context.is_none()
                    && record.provider_session.is_some()
            })
            .filter_map(|record| {
                record
                    .active_session
                    .clone()
                    .map(|session| (record.record_id.clone(), session))
            })
            .collect();
        for (record_id, session) in candidates {
            let shared = Arc::clone(self);
            tokio::spawn(async move {
                shared
                    .reconcile_context_pack_export_for_record(&record_id, session)
                    .await;
            });
        }
    }

    /// Runs on its own detached task (`tokio::spawn`, see both call sites),
    /// never inline on `drive_runtime_until_shutdown`'s own task: the history
    /// round trip this performs (`discover_history`/`load_history`, via
    /// `export_context_pack_for_session_record`) can only ever settle once
    /// `NativeRuntime::tick()` runs again to drain the authority worker's
    /// completion — and `tick()` is exclusively owned by, and only ever
    /// called from, that same drive-loop task. Awaiting this inline there
    /// would starve `tick()` and this call could never observe its own
    /// command complete.
    ///
    /// Because this now genuinely races the drive loop's own
    /// `publish_control` (which downgrades this exact record from `Live` to
    /// `Dormant`, clearing `active_session`, for the very `Exited` event that
    /// triggered this reconcile), the record legitimately may already be
    /// `Dormant` by the time this reads it, or may transition mid-flight —
    /// `allow_clean_detachment: true` on the downstream export call is what
    /// tolerates exactly that transition (and only that transition; see
    /// `session_record_export_target_matches`).
    async fn reconcile_context_pack_export_for_record(
        &self,
        record_id: &SessionRecordId,
        session: SessionAddress,
    ) {
        let Ok(record) = self.record(record_id) else {
            return;
        };
        if !matches!(
            record.state,
            ManagedSessionState::Live | ManagedSessionState::Dormant
        ) || record.exported_context.is_some()
            || record.provider_session.is_none()
        {
            return;
        }
        // Pack bytes are already durably committed inside this call, via
        // commit_context_pack_for_session_record's own durable
        // write-through (context_pack_store) — no separate step here.
        // Unclean exits (non-zero/forced/no loadable history) are already
        // rejected by context_pack_source_status_is_usable inside
        // materialize_context_pack: this simply returns Err and is skipped.
        let Ok(receipt) = self
            .export_context_pack_for_session_record(&record.record_id, &session, true)
            .await
        else {
            return;
        };
        self.set_exported_context(&record.record_id, receipt);
    }

    /// Records the record's most recent clean-exit pack. No-op (not an
    /// error) if the record disappeared, or if it already carries an
    /// `exported_context` — first-write-wins, matching the harness-side
    /// idempotent reconciliation that consumes this field.
    fn set_exported_context(&self, record_id: &SessionRecordId, receipt: ResolvedContextPackReceipt) {
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (previous, updated) = {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(record) = records.records.get_mut(record_id) else {
                return;
            };
            if record.exported_context.is_some() {
                return;
            }
            let previous = record.clone();
            record.exported_context = Some(receipt);
            record.updated_at_unix_ms = unix_time_ms().max(record.updated_at_unix_ms);
            (previous, record.clone())
        };
        if self.persist_state_locked().is_err() {
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .insert(record_id.clone(), previous);
            return;
        }
        drop(transaction);
        self.publish_record(updated);
    }

    /// Resolves the managed record and its exact `SessionAddress` currently
    /// bound to `instance_id` at exactly `generation`, used by the reactive
    /// `Exited{0}` hook in `drive_runtime_until_shutdown` to target a single
    /// record without a full rescan. Mirrors the lookup already inlined in
    /// `publish_control`. Captured synchronously, before `publish_control`
    /// for the same event clears the record's own `active_session` — the
    /// caller passes the resolved address through to a detached
    /// reconciliation task rather than re-deriving it from the record later,
    /// after that race has necessarily already happened.
    fn managed_record_export_target_for_instance(
        &self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    ) -> Option<(SessionRecordId, SessionAddress)> {
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = bindings.get(&instance_id).filter(|binding| binding.generation == generation)?;
        let record_id = binding.record_id.clone()?;
        Some((
            record_id,
            SessionAddress {
                workspace_id: binding.workspace_id.clone(),
                session: SessionKey { instance_id, generation },
            },
        ))
    }

    fn mark_managed_recovery_required(
        &self,
        lease_id: &ManagedWorktreeLeaseId,
        failure_kind: ManagedWorktreeCleanupFailure,
    ) {
        let snapshot = {
            let mut registry = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(lease) = registry.get_mut(lease_id) else { return };
            lease.state = ManagedWorktreeLeaseState::RecoveryRequired;
            lease.cleanup_failure = Some(failure_kind);
            lease.updated_at_unix_ms = unix_time_ms();
            lease.snapshot()
        };
        let _ = self.persist_state();
        self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot });
    }

    fn set_persistence_error(&self, error: Option<String>) {
        *self
            .persistence_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = error
            .map(|message| session_registry::sanitized_persistence_summary(&message));
    }

    fn persist_state(&self) -> io::Result<()> {
        let _transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.persist_state_locked()
    }

    fn persist_state_locked(&self) -> io::Result<()> {
        let workspaces = self
            .workspaces
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let records = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut managed = self.managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        managed.reattach_record_holders(
            &managed_worktree_record_holders(&records),
            unix_time_ms(),
        );
        let materializations = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let managed_spawn_replays = self.managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let result = session_registry::save_v10(
            self.state_path.as_deref(),
            &self.node_id,
            &workspaces,
            &records,
            &managed.records(),
            &managed.tombstones(),
            &materializations,
            &managed_spawn_replays,
        );
        match &result {
            Ok(warning) => self.set_persistence_error(warning.clone()),
            Err(_) => self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned())),
        }
        result.map(|_| ())
    }

    async fn begin_shutdown(&self) -> Result<(), NodeFailure> {
        let _mutation_guard = self.mutation_gate.lock().await;
        self.begin_shutdown_locked().await
    }

    async fn begin_shutdown_locked(&self) -> Result<(), NodeFailure> {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(registry) = self.harness_mcp_registry.as_ref() {
            let instances = registry.shutdown();
            if let Some(control) = self.native_launch_profile_control.as_ref() {
                for instance_id in instances {
                    control.clear_native_harness_mcp_launch_overlay(instance_id);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS);
        let mut first_error = None;
        for session in &self.handle.snapshot().sessions {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                first_error.get_or_insert_with(|| {
                    failure(NodeFailureCode::BackendBusy, "stop-all dispatch exceeded its bounded deadline")
                });
                break;
            }
            if let Err(error) = self
                .dispatch_bounded(
                    ControlCommand::Stop {
                        instance_id: session.instance_id,
                        force: true,
                    },
                    remaining,
                )
                .await
            {
                first_error.get_or_insert(error);
            }
        }
        self.shutdown_notify.notify_waiters();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn ensure_binding_capacity(&self) -> Result<(), NodeFailure> {
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bindings.len() >= CONTROL_SESSIONS_MAX {
            Err(failure(
                NodeFailureCode::BackendBusy,
                "node session-binding capacity is full; remove an existing session first",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn bind_session(
        &self,
        instance_id: AgentInstanceId,
        workspace_id: WorkspaceId,
        generation: SessionGeneration,
    ) {
        self.bind_session_with_policy(
            &SessionAddress {
                workspace_id,
                session: SessionKey { instance_id, generation },
            },
            ProviderRuntimePolicy::raw_pty(),
            None,
        );
    }

    #[cfg(test)]
    fn bind_session_with_policy(
        &self,
        address: &SessionAddress,
        runtime_policy: ProviderRuntimePolicy,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    ) {
        self.bind_session_with_materialization(
            address,
            runtime_policy,
            environment_profile,
            None,
            None,
            None,
        );
    }

    fn bind_session_with_materialization(
        &self,
        address: &SessionAddress,
        runtime_policy: ProviderRuntimePolicy,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
        materialization_id: Option<MaterializationId>,
    ) {
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(bindings.len() < CONTROL_SESSIONS_MAX);
        debug_assert!(!bindings.contains_key(&address.session.instance_id));
        bindings.insert(address.session.instance_id, SessionBinding {
            workspace_id: address.workspace_id.clone(),
            generation: address.session.generation,
            runtime_policy,
            pending_resume: None,
            record_id: None,
            managed_worktree_lease_id: None,
            environment_profile,
            bundle,
            context,
            materialization_id,
        });
    }

    #[cfg(test)]
    fn bind_managed_session(
        &self,
        address: &SessionAddress,
        record_id: SessionRecordId,
        runtime_policy: ProviderRuntimePolicy,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    ) {
        self.bind_managed_session_with_materialization(
            address,
            record_id,
            runtime_policy,
            environment_profile,
            None,
            None,
            None,
        );
    }

    fn bind_managed_session_with_materialization(
        &self,
        address: &SessionAddress,
        record_id: SessionRecordId,
        runtime_policy: ProviderRuntimePolicy,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
        materialization_id: Option<MaterializationId>,
    ) {
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(bindings.len() < CONTROL_SESSIONS_MAX);
        debug_assert!(!bindings.contains_key(&address.session.instance_id));
        bindings.insert(
            address.session.instance_id,
            SessionBinding {
                workspace_id: address.workspace_id.clone(),
                generation: address.session.generation,
                runtime_policy,
                pending_resume: None,
                record_id: Some(record_id),
                managed_worktree_lease_id: None,
                environment_profile,
                bundle,
                context,
                materialization_id,
            },
        );
    }

    #[cfg(test)]
    fn bind_spawn_session(
        &self,
        address: &SessionAddress,
        provider: AgentId,
        mode: SessionMode,
        runtime_policy: ProviderRuntimePolicy,
        record_policy: SpawnRecordPolicy,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    ) -> Result<Option<SessionRecordId>, NodeFailure> {
        self.bind_spawn_session_with_materialization(
            address,
            provider,
            mode,
            runtime_policy,
            record_policy,
            environment_profile,
            None,
            None,
            None,
        )
    }

    fn bind_spawn_session_with_materialization(
        &self,
        address: &SessionAddress,
        provider: AgentId,
        mode: SessionMode,
        runtime_policy: ProviderRuntimePolicy,
        record_policy: SpawnRecordPolicy,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
        materialization_id: Option<MaterializationId>,
    ) -> Result<Option<SessionRecordId>, NodeFailure> {
        if record_policy == SpawnRecordPolicy::ProviderIdentityOnly
            && !runtime_policy.provider_session_identity
        {
            self.bind_session_with_materialization(
                address,
                runtime_policy,
                environment_profile,
                bundle,
                context,
                materialization_id,
            );
            return Ok(None);
        }
        let record_owns_lifecycle = runtime_policy.provider_session_identity;
        let record = self.new_record(
            address,
            provider,
            mode,
            active_record_state(runtime_policy.provider_session_identity, false),
            None,
            record_owns_lifecycle.then(|| environment_profile.clone()).flatten(),
            record_owns_lifecycle.then(|| bundle.clone()).flatten(),
            record_owns_lifecycle.then(|| context.clone()).flatten(),
        )?;
        let record_id = record.record_id.clone();
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.insert_record(record.clone())?;
        let previous_materialization = if record_owns_lifecycle {
            if let Some(materialization_id) = materialization_id.as_ref() {
                let mut materializations = self
                    .materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(ownership) = materializations.get_mut(materialization_id) else {
                    drop(materializations);
                    self.remove_record_memory(&record_id);
                    return Err(failure(
                        NodeFailureCode::BackendOperationFailed,
                        "session environment ownership disappeared before binding",
                    ));
                };
                let previous = ownership.clone();
                if ownership
                    .transfer_to_record(record_id.clone(), unix_time_ms())
                    .is_err()
                {
                    drop(materializations);
                    self.remove_record_memory(&record_id);
                    return Err(failure(
                        NodeFailureCode::BackendOperationFailed,
                        "session environment ownership transfer failed",
                    ));
                }
                Some(previous)
            } else {
                None
            }
        } else {
            None
        };
        self.bind_managed_session_with_materialization(
            address,
            record_id.clone(),
            runtime_policy,
            environment_profile,
            bundle,
            context,
            materialization_id.clone(),
        );
        if let Err(error) = self.persist_state_locked() {
            self.remove_binding(address);
            self.remove_record_memory(&record_id);
            if let (Some(materialization_id), Some(previous)) =
                (materialization_id.as_ref(), previous_materialization)
            {
                self.materializations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(materialization_id.clone(), previous);
            }
            return Err(persistence_failure(error));
        }
        drop(transaction);
        self.publish_record(record);
        Ok(Some(record_id))
    }

    fn allocate_record_id(&self) -> Result<SessionRecordId, NodeFailure> {
        for _ in 0..8 {
            let bytes = random_nonce().map_err(|error| {
                failure(
                    NodeFailureCode::BackendOperationFailed,
                    &format!("session record identity generation failed: {error}"),
                )
            })?;
            let mut value = String::with_capacity(27);
            value.push_str("sr-");
            for byte in &bytes[..12] {
                use std::fmt::Write as _;
                let _ = write!(&mut value, "{byte:02x}");
            }
            let record_id = SessionRecordId::new(value).map_err(|error| {
                failure(NodeFailureCode::BackendOperationFailed, &error.to_string())
            })?;
            let records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !records.records.contains_key(&record_id) {
                return Ok(record_id);
            }
        }
        Err(failure(
            NodeFailureCode::BackendBusy,
            "could not allocate a unique session record ID",
        ))
    }

    fn allocate_task_id(&self) -> Result<TaskId, NodeFailure> {
        for _ in 0..8 {
            let bytes = random_nonce().map_err(|error| {
                failure(
                    NodeFailureCode::BackendOperationFailed,
                    &format!("task identity generation failed: {error}"),
                )
            })?;
            let mut nonce = [0_u8; 12];
            nonce.copy_from_slice(&bytes[..12]);
            let task_id = TaskId::from_nonce(nonce);
            let records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !records.records.values().any(|record| {
                record.task_binding.as_ref().and_then(|binding| binding.task_id.as_ref())
                    == Some(&task_id)
            }) {
                return Ok(task_id);
            }
        }
        Err(failure(
            NodeFailureCode::BackendBusy,
            "could not allocate a unique task ID",
        ))
    }

    fn default_record_name(&self, provider: &AgentId) -> String {
        let records = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ordinal = records
            .records
            .values()
            .filter(|record| &record.provider == provider)
            .count()
            .saturating_add(1);
        format!("{} #{ordinal}", provider.as_str())
    }

    fn new_record(
        &self,
        address: &SessionAddress,
        provider: AgentId,
        mode: SessionMode,
        state: ManagedSessionState,
        provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        let canonical_root = self.workspace_root(&address.workspace_id)?;
        let now = unix_time_ms();
        Ok(ManagedSessionRecord {
            record_id: self.allocate_record_id()?,
            display_name: self.default_record_name(&provider),
            provider,
            mode,
            state,
            workspace_id: address.workspace_id.clone(),
            canonical_root: opaque_windows_path(canonical_root),
            provider_session,
            active_session: Some(address.clone()),
            environment_profile,
            bundle,
            context_id: context.as_ref().map(|receipt| receipt.id.clone()),
            context,
            exported_context: None,
            task_binding: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            last_error: None,
        })
    }

    fn insert_record(&self, record: ManagedSessionRecord) -> Result<(), NodeFailure> {
        let mut records = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if records.records.len() >= MAX_MANAGED_SESSION_RECORDS {
            return Err(failure(
                NodeFailureCode::BackendBusy,
                "managed session record capacity is full; forget an inactive record first",
            ));
        }
        if let Some(identity) = record.provider_session.as_ref() {
            if records.records.values().any(|candidate| {
                candidate.provider_session.as_ref().is_some_and(|candidate_identity| {
                    session_registry::same_provider_session(
                        &candidate.provider,
                        candidate_identity,
                        &record.provider,
                        identity,
                    )
                })
            }) {
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    "provider session is already represented by another managed record",
                ));
            }
        }
        match records.records.entry(record.record_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(record);
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    "session record ID collision",
                ));
            }
        }
        Ok(())
    }

    fn record(&self, record_id: &SessionRecordId) -> Result<ManagedSessionRecord, NodeFailure> {
        self.session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .get(record_id)
            .cloned()
            .ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownSessionRecord,
                    "managed session record does not exist",
                )
            })
    }

    fn revalidate_session_record_preview(
        &self,
        expected: &ManagedSessionRecord,
        identity: &ProviderSessionIdentity,
    ) -> Result<(), NodeFailure> {
        let current = self.record(&expected.record_id)?;
        let current_root = self.workspace_root(&current.workspace_id)?;
        if current.provider != expected.provider
            || current.workspace_id != expected.workspace_id
            || current.canonical_root != expected.canonical_root
            || current.provider_session.as_ref() != Some(identity)
            || !platform::roots_equal(
                &current_root,
                windows_path_text(&current.canonical_root),
            )
        {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session changed while its preview was loading",
            ));
        }
        Ok(())
    }

    fn revalidate_session_record_context_export(
        &self,
        expected: &ManagedSessionRecord,
        identity: &ProviderSessionIdentity,
        session: &SessionAddress,
        allow_clean_detachment: bool,
    ) -> Result<(), NodeFailure> {
        let current = self.record(&expected.record_id)?;
        let current_root = self.workspace_root(&current.workspace_id)?;
        let snapshot = self.internal_session_snapshot(session)?;
        let binding = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.session.instance_id)
            .cloned()
            .ok_or_else(|| {
                failure(
                    NodeFailureCode::SessionRecordConflict,
                    "managed session runtime binding disappeared during context export",
                )
            })?;
        if !session_record_context_export_binding_is_exact(
            expected,
            &current,
            identity,
            session,
            &binding,
            &snapshot.agent_id,
            &current_root,
            allow_clean_detachment,
        ) || !session_record_context_export_source_is_usable(
            &current,
            &snapshot.status,
            allow_clean_detachment,
        ) {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session binding changed during context export",
            ));
        }
        Ok(())
    }

    fn revalidate_loaded_history(
        &self,
        session: &SessionAddress,
        candidate_id: &str,
        loaded: &HistorySessionRecord,
    ) -> Result<(), NodeFailure> {
        let snapshot = self.internal_session_snapshot(session)?;
        if snapshot.history.pending.is_some()
            || snapshot.history.loaded_candidate_id.as_deref() != Some(candidate_id)
            || snapshot.history.loaded.as_ref() != Some(loaded)
        {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session history changed during context export",
            ));
        }
        Ok(())
    }

    fn remove_record_memory(&self, record_id: &SessionRecordId) -> Option<ManagedSessionRecord> {
        self.session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .remove(record_id)
    }

    fn discard_record(&self, record_id: &SessionRecordId) -> Result<(), NodeFailure> {
        if self.destroy_record_materialization(record_id)? {
            self.publish(NodeEvent::SessionRecordRemoved {
                record_id: record_id.clone(),
            });
            return Ok(());
        }
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(removed) = self.remove_record_memory(record_id) else {
            return Ok(());
        };
        if let Err(error) = self.persist_state_locked() {
            let _ = self.insert_record(removed);
            return Err(persistence_failure(error));
        }
        drop(transaction);
        self.publish(NodeEvent::SessionRecordRemoved {
            record_id: record_id.clone(),
        });
        Ok(())
    }

    fn publish_record(&self, record: ManagedSessionRecord) {
        self.publish(NodeEvent::SessionRecordUpserted { record });
    }

    fn mark_record_error(
        &self,
        record_id: &SessionRecordId,
        message: &str,
    ) -> Result<(), NodeFailure> {
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(record) = records.records.get_mut(record_id) else {
                return Ok(());
            };
            let previous = record.clone();
            record.state = ManagedSessionState::Unavailable;
            record.last_error = Some(session_registry::sanitized_record_error_summary(message));
            record.updated_at_unix_ms = unix_time_ms();
            previous
        };
        if let Err(error) = self.persist_state_locked() {
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .insert(record_id.clone(), previous);
            return Err(persistence_failure(error));
        }
        let record = self.record(record_id)?;
        drop(transaction);
        self.publish_record(record);
        Ok(())
    }

    fn rename_session_record(
        &self,
        record_id: &SessionRecordId,
        display_name: String,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        validate_display_name(&display_name)
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
        let _transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let record = records.records.get_mut(record_id).ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownSessionRecord,
                    "managed session record does not exist",
                )
            })?;
            let previous = record.clone();
            record.display_name = display_name;
            record.updated_at_unix_ms = unix_time_ms();
            previous
        };
        if let Err(error) = self.persist_state_locked() {
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .insert(record_id.clone(), previous);
            return Err(persistence_failure(error));
        }
        let updated = self.record(record_id)?;
        self.publish_record(updated.clone());
        Ok(updated)
    }

    fn set_session_task(
        &self,
        record_id: &SessionRecordId,
        expected_revision: u64,
        target: SessionTaskTargetV1,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.record(record_id)?;
        let current_task_id = current
            .task_binding
            .as_ref()
            .and_then(|binding| binding.task_id.as_ref());
        let current_revision = current
            .task_binding
            .as_ref()
            .map_or(0, |binding| binding.revision);
        if expected_revision != current_revision {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session task binding revision changed",
            ));
        }
        let exact = match &target {
            SessionTaskTargetV1::Existing { task_id } => current_task_id == Some(task_id),
            SessionTaskTargetV1::Clear => current_task_id.is_none(),
            SessionTaskTargetV1::New => false,
        };
        if exact {
            return Ok(current);
        }
        let revision = current_revision.checked_add(1).ok_or_else(|| {
            failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session task binding revision is exhausted",
            )
        })?;
        let task_id = match target {
            SessionTaskTargetV1::New => Some(self.allocate_task_id()?),
            SessionTaskTargetV1::Existing { task_id } => Some(task_id),
            SessionTaskTargetV1::Clear => None,
        };
        let changed_at_unix_ms = unix_time_ms()
            .max(current.updated_at_unix_ms.checked_add(1).ok_or_else(|| {
                failure(
                    NodeFailureCode::SessionRecordConflict,
                    "managed session record timestamp is exhausted",
                )
            })?);
        let updated = {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let record = records.records.get_mut(record_id).ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownSessionRecord,
                    "managed session record does not exist",
                )
            })?;
            record.task_binding = Some(SessionTaskBindingV1 {
                revision,
                task_id,
                changed_at_unix_ms,
            });
            record.updated_at_unix_ms = changed_at_unix_ms;
            record.clone()
        };
        if let Err(error) = self.persist_state_locked() {
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .insert(record_id.clone(), current);
            return Err(persistence_failure(error));
        }
        drop(transaction);
        self.publish_record(updated.clone());
        Ok(updated)
    }

    fn index_provider_session(
        &self,
        workspace_id: WorkspaceId,
        provider: AgentId,
        identity: ProviderSessionIdentity,
        display_name: String,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        self.index_provider_session_with_policy(
            workspace_id,
            provider,
            identity,
            display_name,
            false,
        )
    }

    fn index_provider_session_with_policy(
        &self,
        workspace_id: WorkspaceId,
        provider: AgentId,
        identity: ProviderSessionIdentity,
        display_name: String,
        allow_validated_transcript_path: bool,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        validate_display_name(&display_name)
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
        identity
            .validate()
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
        if identity.transcript_path.is_some() && !allow_validated_transcript_path {
            return Err(failure(
                NodeFailureCode::InvalidRequest,
                "provider session reference index v1 does not accept transcript paths",
            ));
        }
        let canonical_root = self.workspace_root(&workspace_id)?;
        if !self.enabled_providers.contains(&provider) {
            return Err(failure(
                NodeFailureCode::InvalidRequest,
                "provider is not enabled on this node",
            ));
        }

        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = records.records.values().find(|candidate| {
                candidate.provider_session.as_ref().is_some_and(|candidate_identity| {
                    candidate_identity.key == identity.key && candidate_identity.id == identity.id
                })
            }) {
                if existing.provider == provider && existing.workspace_id == workspace_id {
                    return Ok(existing.clone());
                }
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    "provider session reference conflicts with another provider or workspace",
                ));
            }
            if records.records.len() >= MAX_MANAGED_SESSION_RECORDS {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    "managed session record capacity is full; forget an inactive record first",
                ));
            }
        }

        let now = unix_time_ms();
        let record = ManagedSessionRecord {
            record_id: self.allocate_record_id()?,
            display_name,
            provider,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id,
            canonical_root: opaque_windows_path(canonical_root),
            provider_session: Some(identity),
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: None,
            task_binding: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            last_error: None,
        };
        self.insert_record(record.clone())?;
        if let Err(error) = self.persist_state_locked() {
            self.remove_record_memory(&record.record_id);
            return Err(persistence_failure(error));
        }
        drop(transaction);
        self.publish_record(record.clone());
        Ok(record)
    }

    fn bound_address_for_record(&self, record_id: &SessionRecordId) -> Option<SessionAddress> {
        self.session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find_map(|(instance_id, binding)| {
                (binding.record_id.as_ref() == Some(record_id)).then(|| SessionAddress {
                    workspace_id: binding.workspace_id.clone(),
                    session: SessionKey {
                        instance_id: *instance_id,
                        generation: binding.generation,
                    },
                })
            })
    }

    async fn forget_session_record(
        &self,
        record_id: &SessionRecordId,
    ) -> Result<(), NodeFailure> {
        let record = self.record(record_id)?;
        let managed_lease_id = self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lease_for_workspace(&record.workspace_id);
        if record.active_session.is_some()
            || matches!(
                record.state,
                ManagedSessionState::Live | ManagedSessionState::IdentityPending
            )
        {
            return Err(failure(
                NodeFailureCode::SessionRecordBusy,
                "stop the managed session before forgetting it",
            ));
        }
        if let Some(address) = self.bound_address_for_record(record_id) {
            self.remove_session(&address).await?;
        }
        if !self.destroy_record_materialization(record_id)? {
            let transaction = self
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let removed = self.remove_record_memory(record_id).ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownSessionRecord,
                    "managed session record does not exist",
                )
            })?;
            if let Err(error) = self.persist_state_locked() {
                let _ = self.insert_record(removed);
                return Err(persistence_failure(error));
            }
            drop(transaction);
        }
        self.publish(NodeEvent::SessionRecordRemoved {
            record_id: record_id.clone(),
        });
        if let Some(lease_id) = managed_lease_id {
            if let Some(snapshot) = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&lease_id)
                .map(ManagedWorktreeLeaseRecord::snapshot)
            {
                self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot });
            }
            let _ = self.cleanup_managed_worktree(&lease_id, false).await;
        }
        Ok(())
    }

    async fn resume_session_record(
        &self,
        record_id: &SessionRecordId,
        terminal_size: gate4agent_types::TerminalSize,
        initial_prompt: Option<String>,
    ) -> Result<(ManagedSessionRecord, SessionAddress), NodeFailure> {
        let record = self.record(record_id)?;
        if record.active_session.is_some()
            || matches!(
                record.state,
                ManagedSessionState::Live | ManagedSessionState::IdentityPending
            )
        {
            return Err(failure(
                NodeFailureCode::SessionRecordBusy,
                "managed session already has a live runtime binding",
            ));
        }
        let identity = record.provider_session.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::SessionRecordNotResumable,
                "managed session has no verified provider session identity",
            )
        })?;
        let working_directory = self.workspace_root(&record.workspace_id)?;
        if !platform::roots_equal(
            &working_directory,
            windows_path_text(&record.canonical_root),
        ) {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session workspace root changed; refusing to resume in another directory",
            ));
        }
        {
            let records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if records.records.values().any(|candidate| {
                candidate.record_id != *record_id
                    && candidate.provider_session.as_ref().is_some_and(|candidate_identity| {
                        session_registry::same_provider_session(
                            &candidate.provider,
                            candidate_identity,
                            &record.provider,
                            &identity,
                        )
                    })
                    && (candidate.active_session.is_some()
                        || matches!(
                            candidate.state,
                            ManagedSessionState::Live | ManagedSessionState::IdentityPending
                        ))
            }) {
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    "provider session is already active through another managed record",
                ));
            }
        }
        let runtime_requirement = if initial_prompt.is_some() {
            ProviderRuntimeRequirement::ResumeWithPrompt
        } else {
            ProviderRuntimeRequirement::Resume
        };
        let runtime_policy = self.admit_provider_runtime(
            &record.provider,
            runtime_requirement,
        )
        .await?;
        let request = ResumeLaunchRequest {
            working_directory,
            terminal_size,
            initial_prompt,
        };
        request
            .validate()
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
        self.ensure_binding_capacity()?;
        let resolved_materialization = match self.resolve_record_materialization(
            record_id,
            &record.provider,
            record.mode,
            record.environment_profile.as_ref(),
            record.bundle.as_ref(),
            record.context.as_ref(),
        ) {
            Ok(materialization) => materialization,
            Err(error) => {
                let label = match error.code {
                    NodeFailureCode::UnknownBundle => "bundle-unavailable",
                    NodeFailureCode::BundleBindingMismatch => "bundle-binding-mismatch",
                    NodeFailureCode::BundleMaterializationFailed => "bundle-materialization-failed",
                    _ => "environment-profile-unavailable",
                };
                self.mark_record_error(record_id, label)?;
                return Err(error);
            }
        };
        let (materialization_id, environment_overlay) = match resolved_materialization {
            Some((id, overlay)) => (Some(id), overlay),
            None => (None, None),
        };
        let instance_id = AgentInstanceId(self.next_instance_id.fetch_add(1, Ordering::AcqRel));
        let mut environment_selection = match self.select_environment_profile(
            instance_id,
            &record.provider,
            record.mode,
            record.environment_profile.as_ref(),
        ) {
            Ok(selection) => selection,
            Err(error) => {
                self.mark_record_error(record_id, "environment-profile-unavailable")?;
                return Err(error);
            }
        };
        let mut instance_overlay = environment_overlay
            .map(|overlay| self.install_prepared_launch_overlay(instance_id, overlay))
            .transpose()?
            .flatten();
        if let Some(stale_address) = self.bound_address_for_record(record_id) {
            self.remove_session(&stale_address).await?;
        }
        let address = SessionAddress {
            workspace_id: record.workspace_id.clone(),
            session: SessionKey {
                instance_id,
                generation: SessionGeneration(1),
            },
        };
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.record(record_id)?;
        if current.active_session.is_some()
            || matches!(
                current.state,
                ManagedSessionState::Live | ManagedSessionState::IdentityPending
            )
        {
            return Err(failure(
                NodeFailureCode::SessionRecordBusy,
                "managed session became active while resume was being prepared",
            ));
        }
        self.bind_managed_session_with_materialization(
            &address,
            record_id.clone(),
            runtime_policy,
            record.environment_profile.clone(),
            record.bundle.clone(),
            record.context.clone(),
            materialization_id,
        );
        if let Some(selection) = environment_selection.take() {
            selection.retain();
        }
        if let Some(overlay) = instance_overlay.take() {
            overlay.retain();
        }
        drop(transaction);

        let transport = match record.mode {
            SessionMode::Pty => TransportKind::Pty,
            SessionMode::Inline => TransportKind::Pipe,
        };
        let agent_id = record.provider.clone();
        let dispatch_timeout = Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS);
        if let Err(error) = self
            .dispatch_bounded(
                ControlCommand::Register {
                    instance_id,
                    agent_id,
                    transport,
                },
                dispatch_timeout,
            )
            .await
        {
            if let Some(binding) = self.remove_binding(&address) {
                self.cleanup_session_owned_materialization(&address, &binding)?;
            }
            return Err(error);
        }
        let command = self.prepare_command(ControlCommand::Resume {
            instance_id,
            target: ResumeTarget::ProviderSession { identity },
            runtime_policy,
            request,
        });
        let started_after = self.current_sequence();
        if let Err(error) = self.arm_resume(&address, command.id, runtime_policy) {
            self.rollback_spawn(&address, dispatch_timeout).await?;
            return Err(error);
        }
        if let Err(error) = self.dispatch_envelope(command) {
            self.clear_armed_resume(&address);
            self.rollback_spawn(&address, dispatch_timeout).await?;
            return Err(error);
        }
        match self
            .wait_for_record_resume(&address, started_after)
            .await
        {
            Ok(settled_address) => {
                let managed_lease_id = {
                    self.managed_worktrees.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .lease_for_workspace(&record.workspace_id)
                };
                if let Some(lease_id) = managed_lease_id {
                    {
                        let mut bindings = self.session_bindings.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let binding = bindings.get_mut(&settled_address.session.instance_id)
                            .ok_or_else(|| failure(
                                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                                "resumed managed session lost its exact binding",
                            ))?;
                        if binding.generation != settled_address.session.generation {
                            return Err(failure(
                                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                                "resumed managed session generation diverged",
                            ));
                        }
                        binding.managed_worktree_lease_id = Some(lease_id.clone());
                    }
                    let snapshot = self.managed_worktrees.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .bind_session(
                            &lease_id,
                            ManagedWorktreeSessionHolder {
                                incarnation_id: self.incarnation_id,
                                instance_id: settled_address.session.instance_id,
                                generation: settled_address.session.generation,
                            },
                            Some(record_id.clone()),
                            unix_time_ms(),
                        ).map_err(|message| failure(
                            NodeFailureCode::ManagedWorktreeRecoveryRequired,
                            &message,
                        ))?;
                    if let Err(error) = self.persist_state() {
                        let _ = self.remove_session(&settled_address).await;
                        return Err(persistence_failure(error));
                    }
                    self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot });
                }
                Ok((self.record(record_id)?, settled_address))
            }
            Err(error) => {
                if matches!(
                    error.code,
                    NodeFailureCode::SessionRecordNotResumable
                        | NodeFailureCode::BackendOperationFailed
                ) {
                    if let Some(bound) = self.bound_address_for_record(record_id) {
                        let _ = self.remove_session(&bound).await;
                    }
                }
                Err(error)
            }
        }
    }

    async fn wait_for_record_resume(
        &self,
        initial_address: &SessionAddress,
        after_sequence: u64,
    ) -> Result<SessionAddress, NodeFailure> {
        let deadline = Instant::now()
            + Duration::from_millis(MANAGED_RESUME_SETTLE_TIMEOUT_MS);
        let mut scan_after = after_sequence;
        loop {
            let events = self
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .events
                .iter()
                .filter(|event| event.sequence > scan_after)
                .cloned()
                .collect::<Vec<_>>();
            for envelope in events {
                scan_after = scan_after.max(envelope.sequence);
                let NodeEvent::Control { address, event } = envelope.event else {
                    continue;
                };
                if address.session.instance_id != initial_address.session.instance_id {
                    continue;
                }
                match event.event {
                    ControlEventKind::Resumed { .. } => return Ok(address),
                    ControlEventKind::ResumeDenied { reason } => {
                        return Err(failure(
                            NodeFailureCode::SessionRecordNotResumable,
                            &reason,
                        ));
                    }
                    ControlEventKind::ResumeFailed { message }
                    | ControlEventKind::CommandRejected { message } => {
                        return Err(failure(
                            NodeFailureCode::BackendOperationFailed,
                            &message,
                        ));
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    "managed resume did not settle before the bounded deadline",
                ));
            }
            sleep(Duration::from_millis(5)).await;
        }
    }

    fn workspace_root(&self, workspace_id: &WorkspaceId) -> Result<String, NodeFailure> {
        self.workspaces
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(workspace_id)
            .cloned()
            .ok_or_else(|| failure(NodeFailureCode::UnknownWorkspace, "workspace does not exist"))
    }

    fn workspace_roots(&self) -> Vec<PathBuf> {
        self.workspaces
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(PathBuf::from)
            .collect()
    }

    async fn inspect_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceInspection, NodeFailure> {
        let _permit = self
            .inspection_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                tracing::warn!(
                    workspace_id = %workspace_id,
                    "workspace inspection rejected: inspection capacity is busy",
                );
                failure(
                    NodeFailureCode::BackendBusy,
                    "workspace inspection capacity is busy",
                )
            })?;
        let canonical_root = self.workspace_root(&workspace_id).map_err(|error| {
            tracing::warn!(
                workspace_id = %workspace_id,
                code = ?error.code,
                "workspace inspection rejected: workspace is not registered",
            );
            error
        })?;
        let tree_root = canonical_root.clone();
        let started_at = Instant::now();
        let time_budget_ms = workspace_inspection_time_budget_ms();
        let entry_cap = workspace_inspection_entry_cap();
        let deadline = started_at + Duration::from_millis(time_budget_ms);
        let (entries, tree_truncated, walk_budget) = tokio::task::spawn_blocking(move || {
            collect_workspace_entries(Path::new(&tree_root), deadline, entry_cap)
        })
        .await
        .map_err(|error| {
            tracing::warn!(
                workspace_id = %workspace_id,
                error = %error,
                "workspace inspection rejected: walk task failed",
            );
            failure(
                NodeFailureCode::BackendOperationFailed,
                &format!("workspace inspection task failed: {error}"),
            )
        })?;
        let (mut git, git_time_budget_exceeded) =
            inspect_git_workspace(&canonical_root, deadline).await;
        if !git.worktrees.is_empty() {
            let registered = self
                .workspaces
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .map(|(workspace_id, root)| (workspace_id.clone(), root.clone()))
                .collect::<Vec<_>>();
            for worktree in &mut git.worktrees {
                worktree.workspace_id = registered
                    .iter()
                    .find(|(_, root)| {
                        worktree_paths_equal(root, windows_path_text(&worktree.path))
                    })
                    .map(|(workspace_id, _)| workspace_id.clone());
            }
        }
        if git.is_repository {
            git.managed_worktree = self
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .git_scope(&workspace_id, git.branch.as_deref());
        }
        let truncation = workspace_inspection_truncation(
            &walk_budget,
            git_time_budget_exceeded,
            started_at.elapsed(),
        );
        if let Some(truncation) = &truncation {
            tracing::info!(
                workspace_id = %workspace_id,
                walk_time_budget_exceeded = truncation.walk_time_budget_exceeded,
                walk_entry_cap_exceeded = truncation.walk_entry_cap_exceeded,
                git_time_budget_exceeded = truncation.git_time_budget_exceeded,
                entries_visited = truncation.entries_visited,
                elapsed_ms = truncation.elapsed_ms,
                "workspace inspection truncated by its inner budget",
            );
        }
        Ok(WorkspaceInspection {
            workspace_id,
            entries,
            tree_truncated,
            git,
            truncation,
        })
    }

    async fn browse_host_directories(
        &self,
        directory: Option<OpaqueHostPath>,
        after: Option<OpaqueHostPath>,
    ) -> Result<HostDirectoryListing, NodeFailure> {
        let permit = self
            .inspection_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "host directory browse capacity is busy",
                )
            })?;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            browse_host_directories(directory, after)
        });
        timeout(Duration::from_millis(HOST_DIRECTORY_BROWSE_TIMEOUT_MS), task)
            .await
            .map_err(|_| {
                failure(
                    NodeFailureCode::HostDirectoryReadTimedOut,
                    "host directory browse exceeded its bounded deadline",
                )
            })?
            .map_err(|_| {
                failure(
                    NodeFailureCode::HostDirectoryReadFailed,
                    "host directory browse task failed",
                )
            })?
            .map_err(host_directory_failure)
    }

    async fn read_workspace_file(
        &self,
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    ) -> Result<WorkspaceFileRead, NodeFailure> {
        let canonical_root = self.workspace_root(&workspace_id)?;
        let permit = self
            .inspection_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "workspace file read capacity is busy",
                )
            })?;
        let path_to_read = path.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_workspace_file_from_disk(Path::new(&canonical_root), &path_to_read)
        });
        let content = timeout(
            Duration::from_millis(WORKSPACE_FILE_READ_TIMEOUT_MS),
            task,
        )
        .await
        .map_err(|_| {
            failure(
                NodeFailureCode::RepositoryFileReadTimedOut,
                "repository file read exceeded its bounded deadline",
            )
        })?
        .map_err(|_| {
            failure(
                NodeFailureCode::RepositoryFileReadFailed,
                "repository file read task failed",
            )
        })?
        .map_err(workspace_file_failure)?;
        let content = match content {
            WorkspaceFileBytes::Utf8(text) => {
                let revision = Some(workspace_file_revision(text.as_bytes()));
                return Ok(WorkspaceFileRead {
                    workspace_id,
                    path,
                    content: WorkspaceFileContent::Utf8 {
                        byte_len: u32::try_from(text.len())
                            .expect("bounded workspace text length must fit u32"),
                        text,
                    },
                    revision,
                });
            }
            WorkspaceFileBytes::NonUtf8 { byte_length } => {
                WorkspaceFileContent::NonUtf8 {
                    byte_len: u32::try_from(byte_length)
                        .expect("bounded workspace file length must fit u32"),
                }
            }
            WorkspaceFileBytes::TooLarge => WorkspaceFileContent::TooLarge {
                limit_bytes: MAX_WORKSPACE_FILE_BYTES as u32,
            },
        };
        Ok(WorkspaceFileRead {
            workspace_id,
            path,
            content,
            revision: None,
        })
    }

    async fn write_workspace_file(
        &self,
        workspace_id: WorkspaceId,
        path: RepositoryPath,
        expected_revision: WorkspaceFileRevision,
        text: String,
    ) -> Result<WorkspaceFileRead, NodeFailure> {
        let canonical_root = self.workspace_root(&workspace_id)?;
        let permit = self.inspection_slots.clone().try_acquire_owned().map_err(|_| {
            failure(NodeFailureCode::BackendBusy, "workspace file write capacity is busy")
        })?;
        let path_to_write = path.clone();
        let expected = expected_revision.as_str().to_owned();
        let text_to_write = text.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            write_workspace_file_to_disk(
                Path::new(&canonical_root),
                &path_to_write,
                &expected,
                &text_to_write,
            )
        });
        let revision = timeout(Duration::from_millis(WORKSPACE_FILE_WRITE_TIMEOUT_MS), task)
            .await
            .map_err(|_| failure(NodeFailureCode::RepositoryFileWriteTimedOut, "repository file write exceeded its bounded deadline"))?
            .map_err(|_| failure(NodeFailureCode::RepositoryFileWriteFailed, "repository file write task failed"))?
            .map_err(workspace_file_write_failure)?;
        Ok(WorkspaceFileRead {
            workspace_id,
            path,
            content: WorkspaceFileContent::Utf8 {
                byte_len: u32::try_from(text.len()).expect("bounded workspace text length must fit u32"),
                text,
            },
            revision: Some(WorkspaceFileRevision::new(revision)
                .expect("workspace writer returns a valid SHA-256 revision")),
        })
    }

    async fn create_workspace_file(
        &self,
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    ) -> Result<WorkspaceFileRead, NodeFailure> {
        let canonical_root = self.workspace_root(&workspace_id)?;
        let permit = self.inspection_slots.clone().try_acquire_owned().map_err(|_| {
            failure(NodeFailureCode::BackendBusy, "workspace entry create capacity is busy")
        })?;
        let path_to_create = path.clone();
        let commit_state = Arc::new(AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING));
        let task_commit_state = Arc::clone(&commit_state);
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            create_workspace_file_on_disk(
                Path::new(&canonical_root),
                &path_to_create,
                &task_commit_state,
            )
        });
        let revision = settle_workspace_entry_create(
            task,
            commit_state,
            Duration::from_millis(WORKSPACE_ENTRY_CREATE_TIMEOUT_MS),
            "repository file create exceeded its bounded deadline",
            "repository file create task failed",
        )
        .await?;
        Ok(WorkspaceFileRead {
            workspace_id,
            path,
            content: WorkspaceFileContent::Utf8 {
                text: String::new(),
                byte_len: 0,
            },
            revision: Some(WorkspaceFileRevision::new(revision)
                .expect("workspace creator returns a valid SHA-256 revision")),
        })
    }

    async fn create_workspace_directory(
        &self,
        workspace_id: WorkspaceId,
        path: RepositoryPath,
    ) -> Result<WorkspaceEntry, NodeFailure> {
        let canonical_root = self.workspace_root(&workspace_id)?;
        let permit = self.inspection_slots.clone().try_acquire_owned().map_err(|_| {
            failure(NodeFailureCode::BackendBusy, "workspace entry create capacity is busy")
        })?;
        let path_to_create = path.clone();
        let commit_state = Arc::new(AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING));
        let task_commit_state = Arc::clone(&commit_state);
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            create_workspace_directory_on_disk(
                Path::new(&canonical_root),
                &path_to_create,
                &task_commit_state,
            )
        });
        settle_workspace_entry_create(
            task,
            commit_state,
            Duration::from_millis(WORKSPACE_ENTRY_CREATE_TIMEOUT_MS),
            "repository directory create exceeded its bounded deadline",
            "repository directory create task failed",
        )
        .await?;
        Ok(WorkspaceEntry {
            relative_path: path,
            kind: WorkspaceEntryKind::Directory,
        })
    }

    async fn read_git_history(
        &self,
        workspace_id: WorkspaceId,
        path: Option<RepositoryPath>,
        before: Option<GitObjectId>,
        limit: u16,
    ) -> Result<GitHistoryPage, NodeFailure> {
        let _permit = self.inspection_slots.clone().try_acquire_owned().map_err(|_| {
            failure(NodeFailureCode::BackendBusy, "git history capacity is busy")
        })?;
        let root = self.workspace_root(&workspace_id)?;
        read_git_history_bounded(&root, path.as_ref(), before.as_ref(), limit).await
    }

    async fn read_git_diff(
        &self,
        workspace_id: WorkspaceId,
        request: GitDiffRequest,
    ) -> Result<GitDiff, NodeFailure> {
        let _permit = self.inspection_slots.clone().try_acquire_owned().map_err(|_| {
            failure(NodeFailureCode::BackendBusy, "git diff capacity is busy")
        })?;
        let root = self.workspace_root(&workspace_id)?;
        read_git_diff_bounded(&root, request).await
    }

    fn native_session_route_root(
        &self,
        route: &NativeSessionCatalogRoute,
    ) -> Result<Option<String>, NodeFailure> {
        route
            .validate()
            .map_err(|message| failure(NodeFailureCode::InvalidRequest, message))?;
        match route.scope {
            gate4agent_types::NativeSessionCatalogScope::Workspace => route
                .workspace_id
                .as_ref()
                .map(|workspace_id| self.workspace_root(workspace_id))
                .transpose(),
            gate4agent_types::NativeSessionCatalogScope::Unregistered => Ok(None),
        }
    }

    fn project_native_session_entries(
        &self,
        route: &NativeSessionCatalogRoute,
        entries: Vec<ScopedNativeSessionCatalogEntry>,
    ) -> Vec<crate::protocol::NativeSessionCatalogEntry> {
        let current_root = route
            .workspace_id
            .as_ref()
            .and_then(|workspace_id| self.workspace_root(workspace_id).ok());
        let records = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut seen_records = std::collections::HashSet::new();
        entries
            .into_iter()
            .map(|entry| {
                let record_id = route.workspace_id.as_ref().and_then(|workspace_id| {
                    let matches = records
                        .records
                        .values()
                        .filter(|record| {
                            record.provider == route.provider
                                && record.workspace_id == *workspace_id
                                && current_root.as_deref().is_some_and(|root| {
                                    platform::roots_equal(
                                        root,
                                        windows_path_text(&record.canonical_root),
                                    )
                                })
                                && record.provider_session.as_ref().is_some_and(|identity| {
                                    session_registry::same_provider_session(
                                        &record.provider,
                                        identity,
                                        &route.provider,
                                        &entry.provider_session,
                                    )
                                })
                        })
                        .map(|record| record.record_id.clone())
                        .collect::<Vec<_>>();
                    (matches.len() == 1
                        && seen_records.insert(matches[0].clone()))
                        .then(|| matches[0].clone())
                });
                let metadata = entry.metadata;
                crate::protocol::NativeSessionCatalogEntry {
                    selection_id: metadata.selection_id,
                    title: metadata.title,
                    modified_at_unix_ms: metadata.modified_at_unix_ms,
                    model: metadata.model,
                    message_count: metadata.message_count,
                    completed_turn_count: metadata.completed_turn_count,
                    external_group: entry.external_group,
                    record_id,
                }
            })
            .collect()
    }

    async fn catalog_native_sessions(
        &self,
        route: NativeSessionCatalogRoute,
        limit: u16,
    ) -> Result<(
        Vec<crate::protocol::NativeSessionCatalogEntry>,
        crate::protocol::NativeSessionCatalogSummary,
    ), NodeFailure> {
        let canonical_root = self.native_session_route_root(&route)?;
        let registered_workspace_roots = self.workspace_roots();
        let provider = route.provider.clone();
        let scope = route.scope;
        let catalog = self.native_session_catalog.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::InvalidRequest,
                "native session catalog is not configured",
            )
        })?;
        let _permit = Arc::clone(&self.inspection_slots)
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "native session catalog capacity is busy",
                )
            })?;
        let page = run_native_session_catalog_operation(
            catalog,
            "native session catalog",
            move |catalog| {
                catalog.catalog_initial_for_scope(
                    &provider,
                    scope,
                    canonical_root.as_deref().map(Path::new),
                    &registered_workspace_roots,
                    limit,
                )
            },
        )
        .await?;
        let summary = crate::protocol::NativeSessionCatalogSummary {
            catalog_revision: page.revision,
            recent_cutoff_unix_ms: page.cutoff_unix_ms,
            recent_total_count: page.recent_total_count,
            older_total_count: page.older_total_count,
            recent_next_after_selection_id: page.next_after_selection_id,
            recent_has_more: page.remaining_count > 0,
        };
        Ok((self.project_native_session_entries(&route, page.entries), summary))
    }

    async fn page_native_sessions(
        &self,
        route: NativeSessionCatalogRoute,
        window: crate::protocol::NativeSessionCatalogWindow,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        after_selection_id: Option<String>,
        limit: u16,
    ) -> Result<crate::protocol::NativeSessionCatalogPage, NodeFailure> {
        let canonical_root = self.native_session_route_root(&route)?;
        let registered_workspace_roots = self.workspace_roots();
        let provider = route.provider.clone();
        let scope = route.scope;
        let catalog = self.native_session_catalog.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::InvalidRequest,
                "native session catalog is not configured",
            )
        })?;
        let _permit = Arc::clone(&self.inspection_slots)
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "native session catalog capacity is busy",
                )
            })?;
        let page = run_native_session_catalog_operation(
            catalog,
            "native session catalog page",
            move |catalog| {
                Ok::<_, std::convert::Infallible>(catalog.catalog_page_for_scope(
                    &provider,
                    scope,
                    canonical_root.as_deref().map(Path::new),
                    &registered_workspace_roots,
                    window,
                    catalog_revision,
                    recent_cutoff_unix_ms,
                    after_selection_id.as_deref(),
                    limit,
                ))
            },
        )
        .await?;
        let page = page.map_err(|error| match error {
            NativeSessionCatalogError::StaleCatalog => failure(
                NodeFailureCode::StaleNativeSessionCatalog,
                "native session catalog revision or cursor is stale",
            ),
            _ => failure(
                NodeFailureCode::BackendOperationFailed,
                "native session catalog page failed",
            ),
        })?;
        let entries = self.project_native_session_entries(&route, page.entries);
        Ok(crate::protocol::NativeSessionCatalogPage {
            window: page.window,
            revision: page.revision,
            entries,
            next_after_selection_id: page.next_after_selection_id,
            remaining_count: page.remaining_count,
            has_more: page.remaining_count > 0,
        })
    }

    async fn preview_native_session(
        &self,
        selection: NativeSessionSelection,
        message_limit: u16,
    ) -> Result<crate::protocol::NativeSessionPreview, NodeFailure> {
        selection
            .validate()
            .map_err(|message| failure(NodeFailureCode::InvalidRequest, message))?;
        let canonical_root = self.native_session_route_root(&selection.route)?;
        let registered_workspace_roots = self.workspace_roots();
        let provider = selection.route.provider.clone();
        let scope = selection.route.scope;
        let catalog_revision = selection.catalog_revision;
        let recent_cutoff_unix_ms = selection.recent_cutoff_unix_ms;
        let selection_id = selection.selection_id.clone();
        let catalog = self.native_session_catalog.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::InvalidRequest,
                "native session preview is not configured",
            )
        })?;
        let _permit = Arc::clone(&self.inspection_slots)
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "native session preview capacity is busy",
                )
            })?;
        let preview = run_native_session_catalog_operation(
            catalog,
            "native session preview",
            move |catalog| {
                Ok::<_, std::convert::Infallible>(catalog.preview_for_scope(
                    &provider,
                    scope,
                    canonical_root.as_deref().map(Path::new),
                    &registered_workspace_roots,
                    catalog_revision,
                    recent_cutoff_unix_ms,
                    &selection_id,
                    message_limit,
                ))
            },
        )
        .await?;
        preview
            .map(Into::into)
            .map_err(native_session_preview_failure)
    }

    async fn index_native_session(
        &self,
        selection: NativeSessionSelection,
        display_name: String,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        selection
            .validate()
            .map_err(|message| failure(NodeFailureCode::InvalidRequest, message))?;
        if selection.route.scope
            != gate4agent_types::NativeSessionCatalogScope::Workspace
        {
            return Err(failure(
                NodeFailureCode::WorkspaceRegistrationRequired,
                "register the external project as a workspace before indexing or resuming it",
            ));
        }
        let workspace_id = selection
            .route
            .workspace_id
            .clone()
            .ok_or_else(|| failure(NodeFailureCode::InvalidRequest, "workspace route is incomplete"))?;
        let canonical_root = self
            .native_session_route_root(&selection.route)?
            .ok_or_else(|| failure(NodeFailureCode::InvalidRequest, "workspace route is incomplete"))?;
        let registered_workspace_roots = self.workspace_roots();
        let provider = selection.route.provider.clone();
        let catalog_revision = selection.catalog_revision;
        let recent_cutoff_unix_ms = selection.recent_cutoff_unix_ms;
        let selection_id = selection.selection_id.clone();
        let catalog = self.native_session_catalog.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::InvalidRequest,
                "native session index is not configured",
            )
        })?;
        let _permit = Arc::clone(&self.inspection_slots)
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "native session index capacity is busy",
                )
            })?;
        let resolved = run_native_session_catalog_operation(
            catalog,
            "native session index resolution",
            move |catalog| {
                Ok::<_, std::convert::Infallible>(catalog.resolve_selection_for_scope(
                    &provider,
                    gate4agent_types::NativeSessionCatalogScope::Workspace,
                    Some(Path::new(&canonical_root)),
                    &registered_workspace_roots,
                    catalog_revision,
                    recent_cutoff_unix_ms,
                    &selection_id,
                ))
            },
        )
        .await?
        .map_err(native_session_preview_failure)?;
        self.index_provider_session_with_policy(
            workspace_id,
            selection.route.provider,
            resolved.identity,
            display_name,
            true,
        )
    }

    async fn preview_native_session_by_session_id(
        &self,
        workspace_id: WorkspaceId,
        provider: AgentId,
        session_id: String,
        message_limit: u16,
    ) -> Result<gate4agent_types::NativeSessionPreview, NodeFailure> {
        let canonical_root = self.workspace_root(&workspace_id)?;
        let registered_workspace_roots = self.workspace_roots();
        let catalog = self.native_session_catalog.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::InvalidRequest,
                "native session preview is not configured",
            )
        })?;
        let _permit = Arc::clone(&self.inspection_slots)
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "native session preview capacity is busy",
                )
            })?;
        run_native_session_catalog_operation(
            catalog,
            "native session preview",
            move |catalog| {
                catalog.preview_session_id_for_workspace(
                    &provider,
                    Path::new(&canonical_root),
                    &registered_workspace_roots,
                    &session_id,
                    message_limit,
                )
            },
        )
        .await
    }

    async fn register_workspace(
        &self,
        workspace_id: WorkspaceId,
        root: String,
    ) -> Result<WorkspaceSnapshot, NodeFailure> {
        validate_workspace_request_root(&root)?;
        let canonicalize_workspace_id = workspace_id.clone();
        let workspace = tokio::task::spawn_blocking(move || {
            WorkspaceConfig::new(canonicalize_workspace_id, root)
        })
        .await
        .map_err(|error| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                &format!("workspace canonicalization task failed: {error}"),
            )
        })?
        .map_err(|error| failure(NodeFailureCode::InvalidWorkspaceRoot, &error.to_string()))?;
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut workspaces = self
            .workspaces
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if workspaces.contains_key(&workspace_id) {
            return Err(failure(
                NodeFailureCode::DuplicateWorkspaceId,
                "workspace ID is already registered",
            ));
        }
        if let Some((existing_id, _)) = workspaces.iter().find(|(_, existing_root)| {
            platform::roots_equal(existing_root, workspace.canonical_root())
        }) {
            return Err(failure(
                NodeFailureCode::DuplicateWorkspaceRoot,
                &format!("workspace root is already registered as '{existing_id}'"),
            ));
        }
        {
            let records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(record) = records.records.values().find(|record| {
                record.workspace_id == workspace_id
                    && !record
                        .canonical_root
                        .as_utf8()
                        .is_some_and(|root| platform::roots_equal(root, workspace.canonical_root()))
            }) {
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    &format!(
                        "workspace ID is retained by session record '{}' at another root",
                        record.record_id,
                    ),
                ));
            }
        }
        let snapshot = WorkspaceSnapshot {
            workspace_id: workspace_id.clone(),
            canonical_root: opaque_windows_path(workspace.canonical_root().to_owned()),
            sessions: Vec::new(),
            worktree_service_mode: Some(WorktreeServiceMode::Manual),
            managed_worktree_profiles: Some(WorktreeProfileInventory {
                profiles: Vec::new(),
            }),
        };
        workspaces.insert(workspace_id.clone(), workspace.canonical_root().to_owned());
        drop(workspaces);
        let previous_records = {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = records.records.clone();
            for record in records.records.values_mut().filter(|record| {
                record.workspace_id == workspace_id
                    && record
                        .canonical_root
                        .as_utf8()
                        .is_some_and(|root| platform::roots_equal(root, workspace.canonical_root()))
            }) {
                if record.active_session.is_none() {
                    record.state = detached_record_state(record);
                }
                if record.last_error.as_deref() == Some(WORKSPACE_UNAVAILABLE_ERROR) {
                    record.last_error = None;
                }
                record.updated_at_unix_ms = unix_time_ms();
            }
            previous
        };
        if let Err(error) = self.persist_state_locked() {
            self.workspaces
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&workspace_id);
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records = previous_records;
            return Err(persistence_failure(error));
        }
        let restored = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .values()
            .filter(|record| record.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        drop(transaction);
        self.publish(NodeEvent::WorkspaceAdded {
            workspace: snapshot.clone(),
        });
        for record in restored {
            self.publish_record(record);
        }
        Ok(snapshot)
    }

    async fn create_standalone_workspace(
        &self,
        workspace_id: WorkspaceId,
        root: String,
        initial_branch: Option<String>,
    ) -> Result<WorkspaceSnapshot, NodeFailure> {
        validate_workspace_request_root(&root)?;
        validate_git_revision("initial branch", initial_branch.as_deref())?;
        let preflight_workspace_id = workspace_id.clone();
        let preflight_root = root.clone();
        let canonical_candidate = tokio::task::spawn_blocking(move || {
            canonical_standalone_workspace_candidate(&preflight_workspace_id, &preflight_root)
        })
        .await
        .map_err(|_| failure(
            NodeFailureCode::BackendOperationFailed,
            "standalone workspace root preflight task failed",
        ))?
        .map_err(|error| failure(NodeFailureCode::InvalidWorkspaceRoot, &error.to_string()))?;
        {
            let workspaces = self
                .workspaces
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if workspaces.contains_key(&workspace_id) {
                return Err(failure(
                    NodeFailureCode::DuplicateWorkspaceId,
                    "workspace ID is already registered",
                ));
            }
            if let Some((existing_id, _)) = workspaces.iter().find(|(_, existing_root)| {
                platform::roots_equal(existing_root, &canonical_candidate)
            }) {
                return Err(failure(
                    NodeFailureCode::DuplicateWorkspaceRoot,
                    &format!("workspace root is already registered as '{existing_id}'"),
                ));
            }
        }
        let prepared = prepare_standalone_workspace(root, initial_branch.as_deref())
            .await
            .map_err(standalone_workspace_failure)?;
        prepared
            .verify_for_registration()
            .await
            .map_err(standalone_workspace_failure)?;
        match self
            .register_workspace(workspace_id, prepared.root().to_owned())
            .await
        {
            Ok(workspace) => Ok(workspace),
            Err(registration_error) => {
                if prepared.compensate_registration_failure().await {
                    Err(registration_error)
                } else {
                    Err(failure(
                        NodeFailureCode::StandaloneWorkspaceRecoveryRequired,
                        "standalone workspace initialization succeeded but registration failed; recovery is required",
                    ))
                }
            }
        }
    }

    fn unregister_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), NodeFailure> {
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bindings.values().any(|binding| &binding.workspace_id == workspace_id) {
            return Err(failure(
                NodeFailureCode::WorkspaceBusy,
                "workspace retains a bound session; remove every session before unregistering it",
            ));
        }
        drop(bindings);
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut workspaces = self
            .workspaces
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !workspaces.contains_key(workspace_id) {
            return Err(failure(NodeFailureCode::UnknownWorkspace, "workspace does not exist"));
        }
        if workspaces.len() == 1 {
            return Err(failure(
                NodeFailureCode::LastWorkspace,
                "node must retain at least one workspace",
            ));
        }
        let removed_root = workspaces
            .remove(workspace_id)
            .expect("workspace presence was validated");
        drop(workspaces);
        let previous_records = {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = records.records.clone();
            for record in records
                .records
                .values_mut()
                .filter(|record| &record.workspace_id == workspace_id)
            {
                record.active_session = None;
                record.state = ManagedSessionState::Unavailable;
                record.last_error = Some(WORKSPACE_UNAVAILABLE_ERROR.to_owned());
                record.updated_at_unix_ms = unix_time_ms();
            }
            previous
        };
        if let Err(error) = self.persist_state_locked() {
            self.workspaces
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(workspace_id.clone(), removed_root);
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records = previous_records;
            return Err(persistence_failure(error));
        }
        let unavailable = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .values()
            .filter(|record| &record.workspace_id == workspace_id)
            .cloned()
            .collect::<Vec<_>>();
        drop(transaction);
        self.publish(NodeEvent::WorkspaceRemoved {
            workspace_id: workspace_id.clone(),
        });
        for record in unavailable {
            self.publish_record(record);
        }
        Ok(())
    }

    async fn create_worktree(
        &self,
        source_workspace_id: WorkspaceId,
        workspace_id: WorkspaceId,
        target_root: String,
        branch: String,
        base: Option<String>,
    ) -> Result<(GitWorktreeSnapshot, WorkspaceSnapshot), NodeFailure> {
        self.require_worktree_service(&source_workspace_id)?;
        validate_workspace_request_root(&target_root)?;
        validate_git_revision("worktree base", base.as_deref())?;
        let source_root = self.workspace_root(&source_workspace_id)?;
        {
            let workspaces = self
                .workspaces
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if workspaces.contains_key(&workspace_id) {
                return Err(failure(
                    NodeFailureCode::DuplicateWorkspaceId,
                    "workspace ID is already registered",
                ));
            }
            if let Some((existing_id, _)) = workspaces
                .iter()
                .find(|(_, root)| worktree_paths_equal(root, &target_root))
            {
                return Err(failure(
                    NodeFailureCode::DuplicateWorkspaceRoot,
                    &format!("workspace root is already registered as '{existing_id}'"),
                ));
            }
        }
        let mut worktree = create_git_worktree(
            &source_root,
            &target_root,
            &branch,
            base.as_deref(),
        )
        .await
        .map_err(git_worktree_failure)?;
        let workspace = match self
            .register_workspace(workspace_id, worktree.path.clone())
            .await
        {
            Ok(workspace) => workspace,
            Err(registration_error) => {
                if remove_git_worktree(&source_root, &worktree.path).await.is_err() {
                    return Err(failure(
                        NodeFailureCode::ManagedWorktreeRecoveryRequired,
                        "worktree registration failed and compensation requires recovery",
                    ));
                }
                return Err(failure(
                    registration_error.code,
                    &registration_error.message,
                ));
            }
        };
        worktree.workspace_id = Some(workspace.workspace_id.clone());
        Ok((protocol_worktree(worktree), workspace))
    }

    async fn remove_worktree(
        &self,
        source_workspace_id: WorkspaceId,
        target_root: String,
    ) -> Result<Option<WorkspaceId>, NodeFailure> {
        self.require_worktree_service(&source_workspace_id)?;
        validate_workspace_request_root(&target_root)?;
        let source_root = self.workspace_root(&source_workspace_id)?;
        let target_root = normalize_worktree_removal_target(&target_root)
            .map_err(git_worktree_failure)?;
        let target = list_git_worktrees(&source_root)
            .await
            .map_err(git_worktree_failure)?
            .into_iter()
            .find(|worktree| worktree_paths_equal(&worktree.path, &target_root))
            .ok_or_else(|| {
                failure(
                    NodeFailureCode::WorktreeProtected,
                    "refusing to remove a path that is not in Git's worktree listing",
                )
            })?;
        let registered_workspace_id = {
            let workspaces = self
                .workspaces
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let matched = workspaces
                .iter()
                .find(|(_, root)| worktree_paths_equal(root, &target.path))
                .map(|(workspace_id, _)| workspace_id.clone());
            if matched.is_some() && workspaces.len() == 1 {
                return Err(failure(
                    NodeFailureCode::LastWorkspace,
                    "node must retain at least one workspace",
                ));
            }
            matched
        };
        if let Some(workspace_id) = registered_workspace_id.as_ref() {
            let bindings = self
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if bindings
                .values()
                .any(|binding| &binding.workspace_id == workspace_id)
            {
                return Err(failure(
                    NodeFailureCode::WorkspaceBusy,
                    "worktree workspace retains a bound session; remove every session first",
                ));
            }
        }
        remove_git_worktree(&source_root, &target.path)
            .await
            .map_err(git_worktree_failure)?;
        if let Some(workspace_id) = registered_workspace_id.as_ref() {
            self.unregister_workspace(workspace_id)
                .map_err(|error| failure(error.code, &error.message))?;
        }
        Ok(registered_workspace_id)
    }

    async fn cleanup_managed_worktree(
        &self,
        lease_id: &ManagedWorktreeLeaseId,
        explicit: bool,
    ) -> Result<ManagedWorktreeLeaseSnapshot, NodeFailure> {
        let lease = self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(lease_id)
            .cloned()
            .ok_or_else(|| failure(
                NodeFailureCode::UnknownManagedWorktreeLease,
                "managed worktree lease does not exist",
            ))?;
        if lease.state == ManagedWorktreeLeaseState::Removed {
            return Ok(lease.snapshot());
        }
        if lease.has_holders() {
            return Err(failure(
                NodeFailureCode::ManagedWorktreeBusy,
                "managed worktree lease still has a session or durable-record holder",
            ));
        }
        if self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|record| record.managed_lease_id() == Some(lease_id))
        {
            return Err(failure(
                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                "managed worktree cleanup is blocked by session environment ownership",
            ));
        }
        if !explicit && lease.retention == ManagedWorktreeRetention::Retain {
            let snapshot = {
                let mut registry = self.managed_worktrees.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let current = registry.get_mut(lease_id)
                    .expect("managed worktree lease remains present");
                current.state = ManagedWorktreeLeaseState::Retained;
                current.cleanup_failure = None;
                current.updated_at_unix_ms = unix_time_ms();
                current.snapshot()
            };
            if self.persist_state().is_err() {
                self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
            }
            self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot.clone() });
            return Ok(snapshot);
        }
        let profile = self.managed_profile(&lease.source_workspace_id, &lease.profile_id)?;
        if profile.revision() != &lease.profile_revision {
            self.mark_managed_recovery_required(
                lease_id,
                ManagedWorktreeCleanupFailure::OwnershipConflict,
            );
            return Err(failure(
                NodeFailureCode::ManagedWorktreeRecoveryRequired,
                "managed worktree profile revision changed",
            ));
        }
        profile.validate_target_authority(&lease.lease_id, &lease.target_root).map_err(|message| {
            self.mark_managed_recovery_required(
                lease_id,
                ManagedWorktreeCleanupFailure::OwnershipConflict,
            );
            failure(NodeFailureCode::ManagedWorktreeOwnershipConflict, &message)
        })?;
        let source_root = self.workspace_root(&lease.source_workspace_id)?;
        let listed = list_git_worktrees(&source_root).await.map_err(|error| {
            self.mark_managed_cleanup_blocked(lease_id, ManagedWorktreeCleanupFailure::Backend);
            managed_git_worktree_failure(error)
        })?;
        let target = listed.iter()
            .find(|item| worktree_paths_equal(&item.path, &lease.target_root));
        if let Some(target) = target {
            if target.is_main || target.is_bare || !exact_owned_worktree(
                &lease,
                &target.path,
                target.branch.as_deref(),
                &target.head,
            ) {
                self.mark_managed_recovery_required(
                    lease_id,
                    ManagedWorktreeCleanupFailure::OwnershipConflict,
                );
                return Err(failure(
                    NodeFailureCode::ManagedWorktreeOwnershipConflict,
                    "Git worktree identity no longer matches the managed lease",
                ));
            }
            if target.prunable {
                self.mark_managed_cleanup_blocked(
                    lease_id,
                    ManagedWorktreeCleanupFailure::Prunable,
                );
                return Err(failure(
                    NodeFailureCode::ManagedWorktreeRecoveryRequired,
                    "managed worktree is prunable and requires recovery",
                ));
            }
            if let Err(error) = remove_git_worktree(&source_root, &target.path).await {
                let failure_kind = match error.kind {
                    GitWorktreeErrorKind::Dirty => ManagedWorktreeCleanupFailure::Dirty,
                    GitWorktreeErrorKind::Locked => ManagedWorktreeCleanupFailure::Locked,
                    GitWorktreeErrorKind::Protected => ManagedWorktreeCleanupFailure::OwnershipConflict,
                    GitWorktreeErrorKind::Conflict => ManagedWorktreeCleanupFailure::Busy,
                    GitWorktreeErrorKind::Invalid | GitWorktreeErrorKind::NotRepository
                    | GitWorktreeErrorKind::Failed => ManagedWorktreeCleanupFailure::Backend,
                };
                if failure_kind == ManagedWorktreeCleanupFailure::OwnershipConflict {
                    self.mark_managed_recovery_required(lease_id, failure_kind);
                } else {
                    self.mark_managed_cleanup_blocked(lease_id, failure_kind);
                }
                return Err(managed_git_worktree_failure(error));
            }
        } else {
            match std::fs::symlink_metadata(&lease.target_root) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    self.mark_managed_recovery_required(
                        lease_id,
                        ManagedWorktreeCleanupFailure::OwnershipConflict,
                    );
                    return Err(failure(
                        NodeFailureCode::ManagedWorktreeOwnershipConflict,
                        "managed target exists but is not owned by Git",
                    ));
                }
                Err(_) => {
                    self.mark_managed_cleanup_blocked(
                        lease_id,
                        ManagedWorktreeCleanupFailure::Backend,
                    );
                    return Err(failure(
                        NodeFailureCode::BackendOperationFailed,
                        "managed target identity could not be inspected",
                    ));
                }
            }
        }
        let registered = self.workspaces.read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&lease.workspace_id);
        if registered {
            self.unregister_workspace(&lease.workspace_id)?;
        }
        let removed = {
            self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .tombstone(lease_id, unix_time_ms())
                .expect("managed worktree lease remains present during cleanup")
        };
        self.persist_state().map_err(persistence_failure)?;
        self.publish(NodeEvent::ManagedWorktreeRemoved { lease_id: lease_id.clone() });
        Ok(removed.snapshot())
    }

    fn mark_managed_cleanup_blocked(
        &self,
        lease_id: &ManagedWorktreeLeaseId,
        failure_kind: ManagedWorktreeCleanupFailure,
    ) {
        let snapshot = {
            let mut registry = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(lease) = registry.get_mut(lease_id) else { return };
            lease.state = ManagedWorktreeLeaseState::CleanupBlocked;
            lease.cleanup_failure = Some(failure_kind);
            lease.updated_at_unix_ms = unix_time_ms();
            lease.snapshot()
        };
        let _ = self.persist_state();
        self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot });
    }

    fn require_worktree_service(
        &self,
        source_workspace_id: &WorkspaceId,
    ) -> Result<(), NodeFailure> {
        if self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lease_for_workspace(source_workspace_id)
            .is_some()
        {
            return Err(failure(
                NodeFailureCode::ManagedWorktreeOwnershipConflict,
                "managed worktree targets are internal-only workspaces",
            ));
        }
        match self
            .worktree_service_modes
            .get(source_workspace_id)
            .copied()
            .unwrap_or(WorktreeServiceMode::Manual)
        {
            WorktreeServiceMode::Manual => Ok(()),
            WorktreeServiceMode::Managed | WorktreeServiceMode::Off => Err(failure(
                NodeFailureCode::UnsupportedSpawnCapability,
                "Git worktree service mode is unavailable in this node build",
            )),
        }
    }

    fn managed_reservation_conflict(
        &self,
        workspace_id: Option<&WorkspaceId>,
        root: Option<&str>,
    ) -> bool {
        self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_records()
            .any(|lease| {
                workspace_id.is_some_and(|workspace_id| {
                    workspace_id == &lease.source_workspace_id
                        || workspace_id == &lease.workspace_id
                }) || root.is_some_and(|root| worktree_paths_equal(root, &lease.target_root))
            })
    }

    fn reject_managed_reservation(
        &self,
        workspace_id: Option<&WorkspaceId>,
        root: Option<&str>,
    ) -> Result<(), NodeFailure> {
        if self.managed_reservation_conflict(workspace_id, root) {
            Err(failure(
                NodeFailureCode::ManagedWorktreeOwnershipConflict,
                "managed worktree ownership may only be mutated through its lease cleanup",
            ))
        } else {
            Ok(())
        }
    }

    fn bound_workspace_root(&self, address: &SessionAddress) -> Result<String, NodeFailure> {
        self.validate_address(address)?;
        self.workspace_root(&address.workspace_id)
    }

    fn require_session_runtime_policy(
        &self,
        address: &SessionAddress,
        requirement: ProviderRuntimeRequirement,
    ) -> Result<ProviderRuntimePolicy, NodeFailure> {
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = bindings
            .get(&address.session.instance_id)
            .ok_or_else(|| failure(NodeFailureCode::UnknownSession, "session instance does not exist"))?;
        if binding.workspace_id != address.workspace_id {
            return Err(failure(
                NodeFailureCode::SessionWorkspaceMismatch,
                "session belongs to another workspace",
            ));
        }
        if binding.generation != address.session.generation {
            return Err(failure(NodeFailureCode::StaleGeneration, "session generation is stale"));
        }
        require_policy(binding.runtime_policy, requirement)
            .map(|()| binding.runtime_policy)
            .map_err(|_| failure(
                NodeFailureCode::UnsupportedCapability,
                "session runtime policy does not admit this semantic operation",
            ))
    }

    fn remove_binding(&self, address: &SessionAddress) -> Option<SessionBinding> {
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bindings.get(&address.session.instance_id).is_some_and(|binding| {
            binding.workspace_id == address.workspace_id
                && binding.generation == address.session.generation
        }) {
            let removed = bindings.remove(&address.session.instance_id);
            self.clear_terminal_frame_watermark(address);
            if let Some(control) = self.native_launch_profile_control.as_ref() {
                control.clear_native_harness_mcp_launch_overlay(
                    address.session.instance_id,
                );
            }
            if let Some(registry) = self.harness_mcp_registry.as_ref() {
                registry.revoke_session(address);
            }
            if removed
                .as_ref()
                .is_some_and(|binding| {
                    binding.environment_profile.is_some()
                        || binding.bundle.is_some()
                        || binding.context.is_some()
                })
            {
                if let Some(control) = self.native_launch_profile_control.as_ref() {
                    control.clear_native_launch_profile_selection(
                        address.session.instance_id,
                    );
                    control.clear_native_instance_launch_overlay(
                        address.session.instance_id,
                    );
                }
            }
            return removed;
        }
        None
    }

    fn cleanup_session_owned_materialization(
        &self,
        address: &SessionAddress,
        binding: &SessionBinding,
    ) -> Result<(), NodeFailure> {
        let Some(id) = binding.materialization_id.as_ref() else {
            return Ok(());
        };
        let owner = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .map(|record| record.owner().clone());
        match owner {
            None | Some(MaterializationOwner::Record { .. }) => Ok(()),
            Some(MaterializationOwner::Session {
                incarnation_id,
                instance_id,
                generation,
            }) if incarnation_id == self.incarnation_id
                && instance_id == address.session.instance_id
                && generation == address.session.generation =>
            {
                self.cleanup_materialization(id)
            }
            Some(MaterializationOwner::Session { .. }) => Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "session environment ownership requires recovery",
            )),
        }
    }

    fn clear_terminal_frame_watermark(&self, address: &SessionAddress) {
        let mut watermarks = self
            .terminal_frame_watermarks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if watermarks
            .get(&address.session.instance_id)
            .is_some_and(|(current, _)| current == address)
        {
            watermarks.remove(&address.session.instance_id);
        }
    }

    #[cfg(test)]
    fn address_for(
        &self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    ) -> Option<SessionAddress> {
        let binding = self.session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&instance_id)
            .cloned()?;
        (binding.generation == generation).then_some(SessionAddress {
            workspace_id: binding.workspace_id,
            session: SessionKey { instance_id, generation },
        })
    }

    fn arm_resume(
        &self,
        address: &SessionAddress,
        command_id: CommandId,
        runtime_policy: ProviderRuntimePolicy,
    ) -> Result<(), NodeFailure> {
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = bindings
            .get_mut(&address.session.instance_id)
            .ok_or_else(|| failure(NodeFailureCode::UnknownSession, "session instance does not exist"))?;
        if binding.workspace_id != address.workspace_id {
            return Err(failure(
                NodeFailureCode::SessionWorkspaceMismatch,
                "session belongs to another workspace",
            ));
        }
        if binding.generation != address.session.generation {
            return Err(failure(NodeFailureCode::StaleGeneration, "session generation is stale"));
        }
        binding.pending_resume = Some((binding.generation, command_id, runtime_policy));
        Ok(())
    }

    fn clear_armed_resume(&self, address: &SessionAddress) {
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(binding) = bindings.get_mut(&address.session.instance_id) {
            if binding.workspace_id == address.workspace_id
                && binding.generation == address.session.generation
            {
                binding.pending_resume = None;
            }
        }
    }

    fn publish_control(&self, event: ControlEvent) {
        let revokes_harness_mcp = matches!(
            &event.event,
            ControlEventKind::Exited { .. }
                | ControlEventKind::Failed { .. }
                | ControlEventKind::Removed
        );
        let (address, previous_address, record_id, identity_admitted, resume_command_rejected) = {
            let mut bindings = self
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(binding) = bindings.get_mut(&event.instance_id) else {
                return;
            };
            let previous_address = SessionAddress {
                workspace_id: binding.workspace_id.clone(),
                session: SessionKey {
                    instance_id: event.instance_id,
                    generation: binding.generation,
                },
            };
            let mut resume_command_rejected = false;
            if binding.generation == event.generation {
                if let Some((pending_generation, pending_command_id, _)) = binding.pending_resume {
                    let rejected_resume = matches!(
                        &event.event,
                        ControlEventKind::CommandRejected { .. }
                    ) && event.command_id == Some(pending_command_id);
                    let resume_failed = matches!(
                        &event.event,
                        ControlEventKind::ResumeDenied { .. }
                            | ControlEventKind::ResumeFailed { .. }
                    );
                    if pending_generation == binding.generation
                        && (rejected_resume || resume_failed)
                    {
                        resume_command_rejected = rejected_resume;
                        binding.pending_resume = None;
                    }
                }
            } else {
                let expected = binding.generation.0.checked_add(1);
                let authorized = matches!(&event.event, ControlEventKind::ResumeAuthorized { .. });
                if !binding.pending_resume.is_some_and(|(generation, _, _)| {
                    generation == binding.generation
                })
                    || expected != Some(event.generation.0)
                    || !authorized
                {
                    return;
                }
                let (_, _, runtime_policy) = binding
                    .pending_resume
                    .expect("authorized resume retains its pending runtime policy");
                binding.generation = event.generation;
                binding.runtime_policy = runtime_policy;
                binding.pending_resume = None;
            }
            (
                SessionAddress {
                    workspace_id: binding.workspace_id.clone(),
                    session: SessionKey {
                        instance_id: event.instance_id,
                        generation: event.generation,
                    },
                },
                (previous_address.session.generation != event.generation)
                    .then_some(previous_address),
                binding.record_id.clone(),
                binding.runtime_policy.provider_session_identity,
                resume_command_rejected,
            )
        };
        if let Some(previous_address) = previous_address.as_ref() {
            self.clear_terminal_frame_watermark(previous_address);
        }
        if let Some(record_id) = record_id {
            self.reconcile_managed_record(
                &record_id,
                &address,
                &event,
                identity_admitted,
                resume_command_rejected,
            );
        }
        let managed_record_id = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .filter(|binding| {
                binding.workspace_id == address.workspace_id
                    && binding.generation == address.session.generation
            })
            .and_then(|binding| binding.record_id.clone());
        let observations = provider_observations(&event);
        self.publish(NodeEvent::Control {
            address: address.clone(),
            event,
        });
        for observation in observations {
            self.publish(NodeEvent::Observation {
                address: address.clone(),
                observation: observation.clone(),
            });
            if let Some(record_id) = managed_record_id.as_ref() {
                self.publish(NodeEvent::ManagedObservation {
                    record_id: record_id.clone(),
                    observation,
                });
            }
        }
        if revokes_harness_mcp {
            if let Some(registry) = self.harness_mcp_registry.as_ref() {
                registry.revoke_session(&address);
            }
            if let Some(control) = self.native_launch_profile_control.as_ref() {
                control.clear_native_harness_mcp_launch_overlay(
                    address.session.instance_id,
                );
            }
        }
    }

    fn reconcile_managed_record(
        &self,
        record_id: &SessionRecordId,
        address: &SessionAddress,
        event: &ControlEvent,
        identity_admitted: bool,
        resume_command_rejected: bool,
    ) {
        let _transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let observed_identity = match &event.event {
            ControlEventKind::ProviderEvent {
                event: gate4agent_types::ProviderEvent::SessionIdentityObserved { identity },
                ..
            } if identity_admitted => Some(identity.clone()),
            _ => None,
        };
        let identity_was_observed = observed_identity.is_some();
        let replacement_id = observed_identity
            .as_ref()
            .and_then(|_| self.allocate_record_id().ok());
        let mut upserts = Vec::new();
        let mut removals = Vec::new();
        let mut rebind = None;
        let original_records;
        let original_materializations = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        {
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            original_records = records.records.clone();
            let Some(current_snapshot) = records.records.get(record_id).cloned() else {
                return;
            };
            if let Some(identity) = observed_identity {
                let existing_id = records
                    .records
                    .values()
                    .find(|record| {
                        record.record_id != *record_id
                            && record.provider_session.as_ref().is_some_and(|record_identity| {
                                session_registry::same_provider_session(
                                    &record.provider,
                                    record_identity,
                                    &current_snapshot.provider,
                                    &identity,
                                )
                            })
                    })
                    .map(|record| record.record_id.clone());
                if let Some(existing_id) = existing_id {
                    let scope_matches = records.records.get(&existing_id).is_some_and(|record| {
                        record.mode == current_snapshot.mode
                            && record.workspace_id == current_snapshot.workspace_id
                            && record.environment_profile == current_snapshot.environment_profile
                            && record.bundle == current_snapshot.bundle
                            && record.context_id == current_snapshot.context_id
                            && record.context == current_snapshot.context
                            && record
                                .canonical_root
                                .as_utf8()
                                .zip(current_snapshot.canonical_root.as_utf8())
                                .is_some_and(|(left, right)| platform::roots_equal(left, right))
                    });
                    if !scope_matches {
                        if let Some(current) = records.records.get_mut(record_id) {
                            current.state = ManagedSessionState::Unavailable;
                            current.last_error =
                                Some(PROVIDER_SESSION_SCOPE_CONFLICT_ERROR.to_owned());
                            current.updated_at_unix_ms = unix_time_ms();
                            upserts.push(current.clone());
                        }
                    } else {
                        let existing_is_active = records
                            .records
                            .get(&existing_id)
                            .is_some_and(|record| record.active_session.is_some());
                        if existing_is_active {
                            if let Some(current) = records.records.get_mut(record_id) {
                                current.state = ManagedSessionState::Unavailable;
                                current.last_error =
                                    Some(PROVIDER_SESSION_LIVE_CONFLICT_ERROR.to_owned());
                                current.updated_at_unix_ms = unix_time_ms();
                                upserts.push(current.clone());
                            }
                        } else {
                            if current_snapshot.provider_session.is_some() {
                                if let Some(current) = records.records.get_mut(record_id) {
                                    current.active_session = None;
                                    current.state = ManagedSessionState::Dormant;
                                    current.updated_at_unix_ms = unix_time_ms();
                                    upserts.push(current.clone());
                                }
                            } else {
                                records.records.remove(record_id);
                                removals.push(record_id.clone());
                            }
                            if let Some(existing) = records.records.get_mut(&existing_id) {
                                existing.provider_session = Some(identity.clone());
                                existing.active_session = Some(address.clone());
                                existing.state = ManagedSessionState::Live;
                                existing.updated_at_unix_ms = unix_time_ms();
                                existing.last_error = None;
                                upserts.push(existing.clone());
                            }
                            rebind = Some(existing_id);
                        }
                    }
                } else if current_snapshot
                    .provider_session
                    .as_ref()
                    .is_some_and(|current| {
                        !session_registry::same_provider_session(
                            &current_snapshot.provider,
                            current,
                            &current_snapshot.provider,
                            &identity,
                        )
                    })
                {
                    if let Some(current) = records.records.get_mut(record_id) {
                        current.active_session = None;
                        if current.bundle.is_some() || current.context.is_some() {
                            let had_bundle = current.bundle.is_some();
                            current.provider_session = None;
                            current.bundle = None;
                            current.context_id = None;
                            current.context = None;
                            current.state = ManagedSessionState::Unavailable;
                            current.last_error = Some(if had_bundle {
                                "bundle-unavailable".to_owned()
                            } else {
                                "context-unavailable".to_owned()
                            });
                        } else {
                            current.state = ManagedSessionState::Dormant;
                        }
                        current.updated_at_unix_ms = unix_time_ms();
                        upserts.push(current.clone());
                    }
                    if records.records.len() >= MAX_MANAGED_SESSION_RECORDS {
                        if let Some(current) = records.records.get_mut(record_id) {
                            current.state = ManagedSessionState::Unavailable;
                            current.last_error =
                                Some(MANAGED_SESSION_CAPACITY_ERROR.to_owned());
                            current.updated_at_unix_ms = unix_time_ms();
                            upserts.push(current.clone());
                        }
                    } else if let Some(new_record_id) = replacement_id {
                        let now = unix_time_ms();
                        let replacement = ManagedSessionRecord {
                            record_id: new_record_id.clone(),
                            display_name: format!(
                                "{} #{}",
                                current_snapshot.provider.as_str(),
                                records
                                    .records
                                    .values()
                                    .filter(|record| {
                                        record.provider == current_snapshot.provider
                                    })
                                    .count()
                                    .saturating_add(1),
                            ),
                            provider: current_snapshot.provider.clone(),
                            mode: current_snapshot.mode,
                            state: ManagedSessionState::Live,
                            workspace_id: current_snapshot.workspace_id,
                            canonical_root: current_snapshot.canonical_root,
                            provider_session: Some(identity),
                            active_session: Some(address.clone()),
                            environment_profile: current_snapshot.environment_profile,
                            bundle: current_snapshot.bundle,
                            context_id: current_snapshot.context_id,
                            context: current_snapshot.context,
                            // This is a fresh record_id representing the session under its
                            // newly-observed identity; it has not yet produced any clean
                            // exit of its own, so it starts with no exported pack (the old
                            // record, still addressable under its own record_id, keeps
                            // whatever exported_context it already had).
                            exported_context: None,
                            task_binding: current_snapshot.task_binding,
                            created_at_unix_ms: now,
                            updated_at_unix_ms: now,
                            last_error: None,
                        };
                        records
                            .records
                            .insert(new_record_id.clone(), replacement.clone());
                        upserts.push(replacement);
                        rebind = Some(new_record_id);
                    } else if let Some(current) = records.records.get_mut(record_id) {
                        current.state = ManagedSessionState::Unavailable;
                        current.last_error =
                            Some(PROVIDER_IDENTITY_ALLOCATION_ERROR.to_owned());
                        upserts.push(current.clone());
                    }
                } else if let Some(current) = records.records.get_mut(record_id) {
                    current.provider_session = Some(identity);
                    current.active_session = Some(address.clone());
                    current.state = ManagedSessionState::Live;
                    current.updated_at_unix_ms = unix_time_ms();
                    current.last_error = None;
                    upserts.push(current.clone());
                }
            } else if let Some(current) = records.records.get_mut(record_id) {
                if resume_command_rejected {
                    current.active_session = None;
                    current.state = detached_record_state(current);
                    current.updated_at_unix_ms = unix_time_ms();
                    current.last_error =
                        Some(PROVIDER_RESUME_REJECTED_ERROR.to_owned());
                    upserts.push(current.clone());
                } else {
                    match &event.event {
                    ControlEventKind::Running { .. }
                    | ControlEventKind::ResumeAuthorized { .. }
                    | ControlEventKind::Resumed { .. } => {
                        current.active_session = Some(address.clone());
                        current.state = active_record_state(
                            identity_admitted,
                            current.provider_session.is_some(),
                        );
                        current.updated_at_unix_ms = unix_time_ms();
                        current.last_error = None;
                        upserts.push(current.clone());
                    }
                    ControlEventKind::Exited { .. } | ControlEventKind::Removed => {
                        current.active_session = None;
                        current.state = detached_record_state(current);
                        current.updated_at_unix_ms = unix_time_ms();
                        upserts.push(current.clone());
                    }
                    ControlEventKind::Failed { message }
                    | ControlEventKind::ResumeFailed { message } => {
                        current.active_session = None;
                        current.state = detached_record_state(current);
                        current.updated_at_unix_ms = unix_time_ms();
                        current.last_error = Some(
                            session_registry::sanitized_record_error_summary(message),
                        );
                        upserts.push(current.clone());
                    }
                    ControlEventKind::ResumeDenied { reason } => {
                        current.active_session = None;
                        current.state = detached_record_state(current);
                        current.updated_at_unix_ms = unix_time_ms();
                        current.last_error = Some(
                            session_registry::sanitized_record_error_summary(reason),
                        );
                        upserts.push(current.clone());
                    }
                        _ => {}
                    }
                }
            }
        }
        let materialization_owner_record = identity_was_observed.then(|| {
            rebind.clone().unwrap_or_else(|| record_id.clone())
        }).and_then(|candidate| {
            self.session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .get(&candidate)
                .filter(|record| {
                    record.provider_session.is_some()
                        && record.active_session.as_ref() == Some(address)
                })
                .map(|_| candidate)
        });
        if let Some(owner_record_id) = materialization_owner_record {
            let materialization_id = self
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&address.session.instance_id)
                .and_then(|binding| binding.materialization_id.clone());
            if let Some(materialization_id) = materialization_id {
                let mut transfer_failed = false;
                {
                    let mut materializations = self
                        .materializations
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(ownership) = materializations.get_mut(&materialization_id) {
                        match ownership.owner() {
                            MaterializationOwner::Session {
                                incarnation_id,
                                instance_id,
                                generation,
                            } if *incarnation_id == self.incarnation_id
                                && *instance_id == address.session.instance_id
                                && *generation == address.session.generation =>
                            {
                                transfer_failed = ownership
                                    .transfer_to_record(owner_record_id.clone(), unix_time_ms())
                                    .is_err();
                            }
                            MaterializationOwner::Record { record_id }
                                if record_id == &owner_record_id => {}
                            MaterializationOwner::Record { record_id } => {
                                let expected = record_id.clone();
                                transfer_failed = ownership
                                    .transfer_record_owner(
                                        &expected,
                                        owner_record_id.clone(),
                                        unix_time_ms(),
                                    )
                                    .is_err();
                            }
                            _ => transfer_failed = true,
                        }
                        if transfer_failed {
                            let _ = ownership.mark_recovery_required(unix_time_ms());
                        }
                    } else {
                        transfer_failed = true;
                    }
                }
                if transfer_failed {
                    if let Some(record) = self
                        .session_records
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .records
                        .get_mut(&owner_record_id)
                    {
                        record.state = ManagedSessionState::Unavailable;
                        record.last_error = Some("environment-profile-unavailable".to_owned());
                        record.updated_at_unix_ms = unix_time_ms();
                        upserts.push(record.clone());
                    }
                }
            }
        }
        if let Some(new_record_id) = rebind.clone() {
            if let Some(binding) = self
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_mut(&address.session.instance_id)
            {
                binding.record_id = Some(new_record_id);
            }
        }
        if upserts.is_empty() && removals.is_empty() {
            return;
        }
        if self.persist_state_locked().is_err() {
            self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
            let mut records = self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            records.records = original_records;
            *self
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = original_materializations;
            if let Some(current) = records.records.get_mut(record_id) {
                current.state = ManagedSessionState::Unavailable;
                current.last_error = Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned());
                current.updated_at_unix_ms = unix_time_ms();
                upserts = vec![current.clone()];
            } else {
                upserts.clear();
            }
            removals.clear();
            drop(records);
            if let Some(binding) = self
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_mut(&address.session.instance_id)
            {
                binding.record_id = Some(record_id.clone());
            }
            eprintln!("gate4agent-node: {DURABLE_STATE_COMMIT_FAILED_ERROR}");
        }
        for record in upserts {
            self.publish_record(record);
        }
        for record_id in removals {
            self.publish(NodeEvent::SessionRecordRemoved { record_id });
        }
    }

    fn snapshot(&self) -> NodeSnapshot {
        let control = self.handle.snapshot();
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let workspace_roots = self
            .workspaces
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let environment_profiles = self
            .environment_profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut workspaces = workspace_roots
            .iter()
            .map(|(workspace_id, canonical_root)| {
                (
                    workspace_id.clone(),
                    WorkspaceSnapshot {
                        workspace_id: workspace_id.clone(),
                        canonical_root: opaque_windows_path(canonical_root.clone()),
                        sessions: Vec::new(),
                        worktree_service_mode: Some(
                            self.worktree_service_modes
                                .get(workspace_id)
                                .copied()
                                .unwrap_or(WorktreeServiceMode::Manual),
                        ),
                        managed_worktree_profiles: Some(WorktreeProfileInventory {
                            profiles: self
                                .managed_worktree_profiles
                                .get(workspace_id)
                                .into_iter()
                                .flat_map(BTreeMap::values)
                                .map(|profile| ManagedWorktreeProfileSummary {
                                    id: profile.profile_id().clone(),
                                    revision: profile.revision().clone(),
                                    retention: profile.retention(),
                                })
                                .collect(),
                        }),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut agent_progress = Vec::new();
        for session in &control.sessions {
            let Some(binding) = bindings.get(&session.instance_id) else {
                continue;
            };
            if binding.generation != session.generation {
                continue;
            }
            if let Some(workspace) = workspaces.get_mut(&binding.workspace_id) {
                let address = SessionAddress {
                    workspace_id: binding.workspace_id.clone(),
                    session: SessionKey {
                        instance_id: session.instance_id,
                        generation: session.generation,
                    },
                };
                if let Some(progress) =
                    agent_progress_from_provider_snapshot(address, &session.provider)
                {
                    agent_progress.push(progress);
                }
                workspace.sessions.push(session.clone());
            }
        }
        NodeSnapshot {
            node_id: self.node_id.clone(),
            enabled_providers: self.enabled_providers.clone(),
            provider_runtime_statuses: {
                let updates = self
                    .provider_runtime_status_updates
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                ProviderRuntimeStatuses::new(
                    self.provider_runtime_statuses
                        .iter()
                        .filter(|status| !updates.contains_key(status.provider()))
                        .cloned()
                        .chain(updates.values().cloned()),
                )
                .expect("refreshed provider runtime inventory remains bounded and unique")
            },
            workspaces: workspaces.into_values().collect(),
            session_records: self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .values()
                .cloned()
                .collect(),
            managed_worktrees: self.managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshots(),
            launch_inventory: Some(LaunchInventory {
                spawn_profiles: Some(
                    self.spawn_profiles
                        .iter()
                        .filter_map(|profile| {
                            resolved_spawn_profile_summary(profile, &environment_profiles)
                        })
                        .collect(),
                ),
                bundles: Some(
                    self.bundle_catalog
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .iter()
                        .map(NodeBundle::receipt)
                        .collect(),
                ),
            }),
            agent_progress,
        }
    }

    fn publish(&self, event: NodeEvent) -> NodeEventEnvelope {
        assert!(
            !matches!(&event, NodeEvent::TerminalFrame { .. }),
            "terminal frame events must use the replaceable live channel",
        );
        let mut history = self.history.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = history
            .last_sequence
            .checked_add(1)
            .expect("node event sequence exhausted");
        if history.events.len() == NODE_EVENT_HISTORY_MAX {
            if let Some(evicted) = history.events.pop_front() {
                history.replay_floor_sequence = evicted
                    .sequence
                    .checked_add(1)
                    .expect("node replay floor sequence exhausted");
                history.removed_record_providers.remove(&evicted.sequence);
            }
        }
        match &event {
            NodeEvent::HarnessMcpReadCall { .. } => {
                panic!("transient harness MCP calls must not enter durable event history")
            }
            NodeEvent::SessionRecordUpserted { record } => {
                history
                    .record_providers
                    .insert(record.record_id.clone(), record.provider.clone());
            }
            NodeEvent::SessionRecordRemoved { record_id } => {
                if let Some(provider) = history.record_providers.remove(record_id) {
                    history.removed_record_providers.insert(sequence, provider);
                }
            }
            NodeEvent::ManagedWorktreeUpserted { .. }
            | NodeEvent::ManagedWorktreeRemoved { .. }
            | NodeEvent::Control { .. }
            | NodeEvent::Observation { .. }
            | NodeEvent::ManagedObservation { .. }
            | NodeEvent::ControllerChanged { .. }
            | NodeEvent::WorkspaceAdded { .. }
            | NodeEvent::WorkspaceRemoved { .. }
            | NodeEvent::ResyncRequired { .. }
            | NodeEvent::TerminalFrame { .. } => {}
        }
        let envelope = NodeEventEnvelope { sequence, event };
        history.events.push_back(envelope.clone());
        history.last_sequence = sequence;
        let _ = self.event_tx.send(envelope.clone());
        drop(history);
        envelope
    }

    fn publish_transient(&self, event: NodeEvent) {
        assert!(event.requires_harness_mcp_proxy_capability());
        assert!(event.harness_mcp_contract_is_valid_at(unix_time_ms()));
        let _ = self.event_tx.send(NodeEventEnvelope { sequence: 0, event });
    }

    fn publish_terminal_frames(&self) {
        let snapshot = self.handle.snapshot();
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidates = snapshot
            .sessions
            .iter()
            .filter_map(|session| {
                let binding = bindings.get(&session.instance_id)?;
                if binding.generation != session.generation {
                    return None;
                }
                let frame = session.terminal_frame.clone()?;
                Some((
                    SessionAddress {
                        workspace_id: binding.workspace_id.clone(),
                        session: SessionKey {
                            instance_id: session.instance_id,
                            generation: session.generation,
                        },
                    },
                    frame,
                ))
            })
            .collect();
        drop(bindings);
        self.publish_terminal_frame_candidates(candidates);
    }

    fn publish_terminal_frame_candidates(
        &self,
        candidates: Vec<(SessionAddress, TerminalFrame)>,
    ) {
        let mut watermarks = self
            .terminal_frame_watermarks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut events = Vec::with_capacity(candidates.len());
        for (address, frame) in candidates {
            let advanced = watermarks
                .get(&address.session.instance_id)
                .map_or(true, |(current_address, sequence)| {
                    current_address != &address || frame.sequence > *sequence
                });
            if !advanced {
                continue;
            }
            watermarks.insert(
                address.session.instance_id,
                (address.clone(), frame.sequence),
            );
            events.push(NodeEvent::TerminalFrame { address, frame });
        }
        drop(watermarks);
        if events.is_empty() {
            return;
        }
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut envelopes = Vec::with_capacity(events.len());
        for event in events {
            let sequence = history
                .last_sequence
                .checked_add(1)
                .expect("node event sequence exhausted");
            history.last_sequence = sequence;
            envelopes.push(NodeEventEnvelope { sequence, event });
        }
        let _ = self.terminal_event_tx.send(Arc::new(envelopes));
        drop(history);
    }

    fn current_sequence(&self) -> u64 {
        self.history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_sequence
    }

    fn controller_state(&self) -> Option<ControllerState> {
        let mut controller = self.controller.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let lease = controller.as_ref().copied()?;
        let remaining = lease.expires_at.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            *controller = None;
            return None;
        }
        Some(ControllerState {
            connection_id: lease.connection_id,
            lease_remaining_ms: remaining.as_millis().min(u64::MAX as u128) as u64,
        })
    }

    fn acquire_controller(&self, connection_id: u64, role: ClientRole, lease_ms: u64) -> Result<ControllerState, NodeFailure> {
        if role != ClientRole::Operator {
            return Err(failure(NodeFailureCode::ObserverReadOnly, "observer connections cannot acquire control"));
        }
        let lease_ms = if lease_ms == 0 {
            DEFAULT_CONTROLLER_LEASE_MS
        } else {
            lease_ms.clamp(MIN_CONTROLLER_LEASE_MS, MAX_CONTROLLER_LEASE_MS)
        };
        let now = Instant::now();
        let mut controller = self.controller.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(active) = controller.as_ref() {
            if active.connection_id != connection_id && active.expires_at > now {
                return Err(failure(NodeFailureCode::ControllerBusy, "another operator holds the controller lease"));
            }
        }
        *controller = Some(ControllerLease {
            connection_id,
            expires_at: now + Duration::from_millis(lease_ms),
        });
        drop(controller);
        let state = ControllerState { connection_id, lease_remaining_ms: lease_ms };
        self.publish(NodeEvent::ControllerChanged { controller: Some(state.clone()) });
        Ok(state)
    }

    fn release_controller(&self, connection_id: u64) -> bool {
        let mut controller = self.controller.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let released = controller.as_ref().is_some_and(|active| active.connection_id == connection_id);
        if released {
            *controller = None;
        }
        drop(controller);
        if released {
            self.publish(NodeEvent::ControllerChanged { controller: None });
        }
        released
    }

    fn require_controller(&self, connection_id: u64, role: ClientRole) -> Result<(), NodeFailure> {
        if role != ClientRole::Operator {
            return Err(failure(NodeFailureCode::ObserverReadOnly, "observer connections are read-only"));
        }
        let mut controller = self.controller.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        match controller.as_ref().copied() {
            Some(active) if active.connection_id == connection_id && active.expires_at > now => Ok(()),
            Some(active) if active.expires_at <= now => {
                *controller = None;
                Err(failure(NodeFailureCode::ControllerRequired, "controller lease expired"))
            }
            _ => Err(failure(NodeFailureCode::ControllerRequired, "controller lease is required")),
        }
    }

    fn validate_address(&self, address: &SessionAddress) -> Result<AgentId, NodeFailure> {
        if !self
            .workspaces
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&address.workspace_id)
        {
            return Err(failure(NodeFailureCode::UnknownWorkspace, "workspace does not exist"));
        }
        let binding = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .cloned()
            .ok_or_else(|| failure(NodeFailureCode::UnknownSession, "session instance does not exist"))?;
        if binding.workspace_id != address.workspace_id {
            return Err(failure(
                NodeFailureCode::SessionWorkspaceMismatch,
                "session belongs to another workspace",
            ));
        }
        if binding.generation != address.session.generation {
            return Err(failure(
                NodeFailureCode::StaleGeneration,
                &format!(
                    "stale session generation: expected {}, received {}",
                    binding.generation.0,
                    address.session.generation.0,
                ),
            ));
        }
        let snapshot = self.handle.snapshot();
        let Some(current) = snapshot
            .sessions
            .iter()
            .find(|item| item.instance_id == address.session.instance_id)
        else {
            return Err(failure(NodeFailureCode::UnknownSession, "session instance does not exist"));
        };
        if current.generation != address.session.generation {
            return Err(failure(
                NodeFailureCode::StaleGeneration,
                &format!(
                    "stale session generation: expected {}, received {}",
                    current.generation.0,
                    address.session.generation.0,
                ),
            ));
        }
        Ok(current.agent_id.clone())
    }

    fn dispatch(&self, command: ControlCommand) -> Result<CommandId, NodeFailure> {
        self.dispatch_envelope(self.prepare_command(command))
    }

    fn prepare_command(&self, command: ControlCommand) -> CommandEnvelope {
        let id = self.next_command_id.fetch_add(1, Ordering::AcqRel);
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command,
        }
    }

    fn dispatch_envelope(&self, envelope: CommandEnvelope) -> Result<CommandId, NodeFailure> {
        let command_id = envelope.id;
        self.handle
            .dispatch(envelope)
            .map_err(|error| match error {
                PortDispatchError::Full => failure(NodeFailureCode::BackendBusy, "backend command ingress is full"),
                PortDispatchError::Disconnected => failure(NodeFailureCode::BackendDisconnected, "backend command ingress is disconnected"),
            })?;
        Ok(command_id)
    }

    async fn dispatch_bounded(
        &self,
        command: ControlCommand,
        timeout_duration: Duration,
    ) -> Result<CommandId, NodeFailure> {
        let deadline = Instant::now() + timeout_duration;
        loop {
            match self.dispatch(command.clone()) {
                Ok(command_id) => return Ok(command_id),
                Err(error) if error.code == NodeFailureCode::BackendBusy && Instant::now() < deadline => {
                    sleep(Duration::from_millis(2)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn internal_session_snapshot(
        &self,
        address: &SessionAddress,
    ) -> Result<gate4agent_types::SessionSnapshot, NodeFailure> {
        self.validate_address(address)?;
        self.handle
            .snapshot()
            .sessions
            .clone()
            .into_iter()
            .find(|session| {
                session.instance_id == address.session.instance_id
                    && session.generation == address.session.generation
            })
            .ok_or_else(|| {
                failure(
                    NodeFailureCode::UnknownSession,
                    "session snapshot is unavailable",
                )
            })
    }

    async fn dispatch_history_bounded(
        &self,
        address: &SessionAddress,
        command: ControlCommand,
        operation: HistoryOperation,
    ) -> Result<gate4agent_types::SessionSnapshot, NodeFailure> {
        self.validate_address(address)?;
        let started_after = self.current_sequence();
        let command_id = self
            .dispatch_bounded(
                command,
                Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
            )
            .await?;
        let deadline = Instant::now() + Duration::from_millis(MUTATION_SETTLE_TIMEOUT_MS);
        let mut scan_after = started_after;
        let mut accepted_after = None;
        loop {
            let events = self
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .events
                .iter()
                .filter(|envelope| envelope.sequence > scan_after)
                .cloned()
                .collect::<Vec<_>>();
            for envelope in events {
                scan_after = scan_after.max(envelope.sequence);
                let NodeEvent::Control {
                    address: event_address,
                    event,
                } = envelope.event
                else {
                    continue;
                };
                if event_address != *address {
                    continue;
                }
                if event.command_id == Some(command_id) {
                    match &event.event {
                        ControlEventKind::CommandRejected { .. } => {
                            return Err(failure(
                                NodeFailureCode::InvalidRequest,
                                "history request was rejected",
                            ));
                        }
                        ControlEventKind::HistoryRequested {
                            operation: requested,
                            ..
                        } if requested == &operation => {
                            accepted_after = Some(envelope.sequence);
                            continue;
                        }
                        _ => {}
                    }
                }
                if accepted_after.is_some_and(|sequence| envelope.sequence > sequence) {
                    match (&operation, &event.event) {
                        (
                            HistoryOperation::Discover { .. },
                            ControlEventKind::HistoryDiscovered { .. },
                        )
                        | (
                            HistoryOperation::Load { .. },
                            ControlEventKind::HistoryLoaded { .. },
                        ) => return self.internal_session_snapshot(address),
                        (_, ControlEventKind::HistoryFailed { .. }) => {
                            return Err(failure(
                                NodeFailureCode::BackendOperationFailed,
                                "history operation failed",
                            ));
                        }
                        _ => {}
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    if accepted_after.is_some() {
                        "history operation did not settle before the bounded deadline"
                    } else {
                        "history operation was not accepted before the bounded deadline"
                    },
                ));
            }
            sleep(Duration::from_millis(2)).await;
        }
    }

    async fn discover_history(
        &self,
        address: &SessionAddress,
        limit: u16,
    ) -> Result<Vec<HistoryCandidateSummary>, NodeFailure> {
        let query = HistoryQuery {
            working_directory: Some(self.workspace_root(&address.workspace_id)?),
            limit,
        };
        query
            .validate()
            .map_err(|_| failure(NodeFailureCode::InvalidRequest, "invalid history query"))?;
        let snapshot = self
            .dispatch_history_bounded(
                address,
                ControlCommand::DiscoverHistory {
                    instance_id: address.session.instance_id,
                    query: query.clone(),
                },
                HistoryOperation::Discover { query },
            )
            .await?;
        if snapshot.history.pending.is_some() || snapshot.history.loaded.is_some() {
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "history discovery settled into an invalid state",
            ));
        }
        Ok(snapshot.history.candidates)
    }

    async fn load_history(
        &self,
        address: &SessionAddress,
        candidate_id: String,
    ) -> Result<HistorySessionRecord, NodeFailure> {
        validate_candidate_id(&candidate_id)
            .map_err(|_| failure(NodeFailureCode::InvalidRequest, "invalid history candidate"))?;
        let snapshot = self
            .dispatch_history_bounded(
                address,
                ControlCommand::LoadHistory {
                    instance_id: address.session.instance_id,
                    candidate_id: candidate_id.clone(),
                },
                HistoryOperation::Load {
                    candidate_id: candidate_id.clone(),
                },
            )
            .await?;
        if snapshot.history.pending.is_some()
            || snapshot.history.loaded_candidate_id.as_ref() != Some(&candidate_id)
        {
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "history load settled into an invalid state",
            ));
        }
        snapshot.history.loaded.ok_or_else(|| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                "history load produced no bounded session",
            )
        })
    }

    /// `allow_clean_detachment`: `false` for the on-demand
    /// `NodeRequest::ExportContextPackForSessionRecord` API path (the
    /// managed record must stay untouched, still live, for the whole call);
    /// `true` only for the reactive auto-export-at-exit path (`§2.1`), which
    /// reacts to the very clean-exit event that detaches the record out from
    /// under this same call — see `session_record_export_target_matches`.
    async fn export_context_pack_for_session_record(
        &self,
        record_id: &SessionRecordId,
        session: &SessionAddress,
        allow_clean_detachment: bool,
    ) -> Result<ResolvedContextPackReceipt, NodeFailure> {
        let record = self.record(record_id)?;
        let identity = record.provider_session.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                "managed session has no verified provider session identity",
            )
        })?;
        if identity.key != ProviderSessionKey::SessionId {
            return Err(failure(
                NodeFailureCode::BackendOperationFailed,
                "managed session provider identity cannot bind history exactly",
            ));
        }
        self.revalidate_session_record_context_export(
            &record,
            &identity,
            session,
            allow_clean_detachment,
        )?;
        let candidates = self
            .discover_history(session, HISTORY_DISCOVERY_LIMIT_MAX)
            .await?;
        let candidate_id = unique_history_candidate_for_provider_session(
            &candidates,
            &identity.id,
        )?;
        let loaded = self.load_history(session, candidate_id.clone()).await?;
        if loaded.session_id != identity.id {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "loaded history identity does not match the managed session record",
            ));
        }
        self.revalidate_session_record_context_export(
            &record,
            &identity,
            session,
            allow_clean_detachment,
        )?;
        self.revalidate_loaded_history(session, &candidate_id, &loaded)?;
        let pack = self.materialize_context_pack(session).await?;
        self.commit_context_pack_for_session_record(
            &record,
            &identity,
            session,
            &candidate_id,
            &loaded,
            pack,
            allow_clean_detachment,
        )
    }

    async fn export_context_pack(
        &self,
        address: &SessionAddress,
    ) -> Result<ResolvedContextPackReceipt, NodeFailure> {
        let pack = self.materialize_context_pack(address).await?;
        self.commit_context_pack(pack)
    }

    async fn materialize_context_pack(
        &self,
        address: &SessionAddress,
    ) -> Result<NodeContextPack, NodeFailure> {
        let snapshot = self.internal_session_snapshot(address)?;
        if !context_pack_source_status_is_usable(&snapshot.status) {
            return Err(failure(
                NodeFailureCode::InvalidRequest,
                "context export requires a running or successfully exited source session",
            ));
        }
        if snapshot.history.pending.is_some()
            || snapshot.history.loaded_candidate_id.is_none()
        {
            return Err(failure(
                NodeFailureCode::InvalidRequest,
                "context export requires settled loaded history",
            ));
        }
        let loaded = snapshot.history.loaded.clone().ok_or_else(|| {
            failure(
                NodeFailureCode::InvalidRequest,
                "context export requires settled loaded history",
            )
        })?;
        let source_provider = snapshot.agent_id.clone();
        let loaded_candidate_id = snapshot.history.loaded_candidate_id.clone();
        let repository = self
            .context_pack_repository(&address.workspace_id)
            .await?;
        let current = self.internal_session_snapshot(address)?;
        if !context_pack_source_status_is_usable(&current.status)
            || current.agent_id != source_provider
            || current.history.pending.is_some()
            || current.history.loaded_candidate_id != loaded_candidate_id
            || current.history.loaded.as_ref() != Some(&loaded)
        {
            return Err(failure(
                NodeFailureCode::ContextPackMaterializationFailed,
                "context source changed during bounded repository capture",
            ));
        }
        NodeContextPack::export_with_repository(
            ContextPackLineageReceipt {
                source_node_id: self.node_id.clone(),
                source_session: address.clone(),
                source_provider,
            },
            &loaded,
            Some(repository),
        )
        .map_err(|_| {
            failure(
                NodeFailureCode::ContextPackMaterializationFailed,
                "bounded context pack export failed",
            )
        })
    }

    /// Write-through to the durable [`ContextPackStore`], if one is
    /// configured (`state_path` is `Some`). A node with no durable state
    /// path — fixtures, tests, ephemeral runs — has no `ContextPackStore`
    /// open at all and this is a no-op, matching `persist_state`'s own
    /// "durability optional, degrade gracefully when unconfigured" idiom.
    fn commit_context_pack_durably(&self, pack: &NodeContextPack) -> Result<(), NodeFailure> {
        let mut store = self
            .context_pack_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(store) = store.as_mut() else {
            return Ok(());
        };
        store.commit(pack).map_err(|_| {
            failure(
                NodeFailureCode::ContextPackMaterializationFailed,
                "durable context pack commit failed",
            )
        })
    }

    fn commit_context_pack(
        &self,
        pack: NodeContextPack,
    ) -> Result<ResolvedContextPackReceipt, NodeFailure> {
        self.commit_context_pack_durably(&pack)?;
        self.context_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pack)
            .map_err(|_| {
                failure(
                    NodeFailureCode::ContextPackMaterializationFailed,
                    "context pack catalog is full",
                )
            })
    }

    fn commit_context_pack_for_session_record(
        &self,
        expected: &ManagedSessionRecord,
        identity: &ProviderSessionIdentity,
        session: &SessionAddress,
        candidate_id: &str,
        loaded: &HistorySessionRecord,
        pack: NodeContextPack,
        allow_clean_detachment: bool,
    ) -> Result<ResolvedContextPackReceipt, NodeFailure> {
        let current_root = self.workspace_root(&expected.workspace_id)?;
        let _transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let records = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = records.records.get(&expected.record_id).ok_or_else(|| {
            failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session record disappeared during context export",
            )
        })?;
        if !session_record_export_target_matches(expected, current, session, allow_clean_detachment) {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session record changed during context export",
            ));
        }
        let bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = bindings.get(&session.session.instance_id).ok_or_else(|| {
            failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session runtime binding disappeared during context export",
            )
        })?;
        let runtime = self
            .handle
            .snapshot()
            .sessions
            .iter()
            .find(|snapshot| {
                snapshot.instance_id == session.session.instance_id
                    && snapshot.generation == session.session.generation
            })
            .cloned()
            .ok_or_else(|| {
                failure(
                    NodeFailureCode::SessionRecordConflict,
                    "managed session runtime disappeared during context export",
                )
            })?;
        let receipt = pack.receipt();
        if !session_record_context_export_binding_is_exact(
            expected,
            current,
            identity,
            session,
            binding,
            &runtime.agent_id,
            &current_root,
            allow_clean_detachment,
        ) || !session_record_context_export_source_is_usable(
            current,
            &runtime.status,
            allow_clean_detachment,
        ) || runtime.history.pending.is_some()
            || runtime.history.loaded_candidate_id.as_deref() != Some(candidate_id)
            || runtime.history.loaded.as_ref() != Some(loaded)
            || receipt.lineage.source_node_id != self.node_id
            || receipt.lineage.source_session != *session
            || receipt.lineage.source_provider != current.provider
            || receipt.source_message_count != loaded.message_count
        {
            return Err(failure(
                NodeFailureCode::SessionRecordConflict,
                "managed session source changed before context catalog commit",
            ));
        }
        self.commit_context_pack_durably(&pack)?;
        self.context_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pack)
            .map_err(|_| {
                failure(
                    NodeFailureCode::ContextPackMaterializationFailed,
                    "context pack catalog is full",
                )
            })
    }

    async fn context_pack_repository(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<ContextPackRepository, NodeFailure> {
        let first = self
            .context_pack_repository_observation(workspace_id)
            .await?;
        let second = self
            .context_pack_repository_observation(workspace_id)
            .await?;
        stable_context_pack_repository(first, second)
    }

    async fn context_pack_repository_observation(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<ContextPackRepositoryObservation, NodeFailure> {
        let git = self.inspect_workspace(workspace_id.clone()).await?.git;
        let head = if git.is_repository {
            let root = self.workspace_root(workspace_id)?;
            match resolve_base_commit_with_timeout(
                &root,
                "HEAD",
                GIT_COMMAND_TIMEOUT_MS,
            )
            .await
            {
                Ok(commit) => ContextPackRepositoryHead::Commit(commit),
                Err(error) if error.kind == GitWorktreeErrorKind::Conflict => {
                    ContextPackRepositoryHead::Unborn
                }
                Err(_) => {
                    return Err(failure(
                        NodeFailureCode::ContextPackMaterializationFailed,
                        "context pack HEAD observation failed",
                    ));
                }
            }
        } else {
            ContextPackRepositoryHead::NotRepository
        };
        let mut files = Vec::with_capacity(CONTEXT_PACK_SELECTED_FILES.len());
        for selected_path in CONTEXT_PACK_SELECTED_FILES {
            let path = RepositoryPath::utf8(selected_path.to_owned()).map_err(|_| {
                failure(
                    NodeFailureCode::InvalidRepositoryPath,
                    "context pack selected file path is invalid",
                )
            })?;
            match self.read_workspace_file(workspace_id.clone(), path).await {
                Ok(WorkspaceFileRead {
                    content: WorkspaceFileContent::Utf8 { text, byte_len },
                    ..
                }) => files.push(ContextPackRepositoryFileSource::utf8(
                    selected_path,
                    text,
                    byte_len,
                )),
                Ok(WorkspaceFileRead {
                    content: WorkspaceFileContent::NonUtf8 { byte_len },
                    ..
                }) => files.push(ContextPackRepositoryFileSource::skipped(
                    selected_path,
                    ContextPackSelectedFileSkipReason::NonUtf8,
                    Some(byte_len),
                )),
                Ok(WorkspaceFileRead {
                    content: WorkspaceFileContent::TooLarge { .. },
                    ..
                }) => files.push(ContextPackRepositoryFileSource::skipped(
                    selected_path,
                    ContextPackSelectedFileSkipReason::TooLarge,
                    None,
                )),
                Err(error) => {
                    let reason = if matches!(
                        error.code,
                        NodeFailureCode::InvalidRepositoryPath
                            | NodeFailureCode::RepositoryFileNotRegular
                            | NodeFailureCode::RepositoryPathUnsafe
                    ) {
                        ContextPackSelectedFileSkipReason::Unsafe
                    } else {
                        ContextPackSelectedFileSkipReason::Unavailable
                    };
                    files.push(ContextPackRepositoryFileSource::skipped(
                        selected_path,
                        reason,
                        None,
                    ));
                }
            }
        }
        Ok(ContextPackRepositoryObservation { head, git, files })
    }

    fn forget_context_pack(&self, context_id: &SpawnContextId) -> Result<(), NodeFailure> {
        let referenced_by_binding = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|binding| {
                binding.context.as_ref().map(|receipt| &receipt.id) == Some(context_id)
            });
        let referenced_by_record = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .values()
            .any(|record| record.context_id.as_ref() == Some(context_id));
        let referenced_by_materialization = self
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .any(|ownership| {
                ownership.context().map(|receipt| &receipt.id) == Some(context_id)
            });
        if referenced_by_binding
            || referenced_by_record
            || referenced_by_materialization
        {
            return Err(failure(
                NodeFailureCode::ContextPackBusy,
                "context pack is still referenced",
            ));
        }
        let mut catalog = self.context_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if catalog.get(context_id).is_none() {
            return Err(failure(
                NodeFailureCode::UnknownContextPack,
                "context pack is unavailable",
            ));
        }
        self.spawn_idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .retain(|_, entry| !entry.value.references_context(context_id));
        let removed = catalog.remove(context_id);
        debug_assert!(removed.is_some(), "validated context pack remains present");
        Ok(())
    }

    async fn dispatch_input_bounded(
        &self,
        address: &SessionAddress,
        action: InputAction,
    ) -> Result<(), NodeFailure> {
        let started_after = self.current_sequence();
        let command_id = self
            .dispatch_bounded(
                ControlCommand::SendInput {
                    instance_id: address.session.instance_id,
                    action,
                },
                Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
            )
            .await?;
        // A semantic prompt may spend the provider's full readiness budget before
        // the runtime emits InputCompleted. Keep the node request deadline bounded,
        // but strictly beyond every readiness policy in this node's catalog.
        let deadline = Instant::now() + Duration::from_millis(self.input_settle_timeout_ms);
        let mut scan_after = started_after;
        let mut accepted_after = None;
        loop {
            let events = self
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .events
                .iter()
                .filter(|envelope| envelope.sequence > scan_after)
                .cloned()
                .collect::<Vec<_>>();
            for envelope in events {
                scan_after = scan_after.max(envelope.sequence);
                let NodeEvent::Control {
                    address: event_address,
                    event,
                } = envelope.event
                else {
                    continue;
                };
                if event_address != *address {
                    continue;
                }
                if event.command_id == Some(command_id) {
                    match &event.event {
                        ControlEventKind::CommandRejected { message } => {
                            return Err(failure(NodeFailureCode::InvalidRequest, message));
                        }
                        ControlEventKind::InputRequested { .. } => {
                            accepted_after = Some(envelope.sequence);
                            continue;
                        }
                        _ => {}
                    }
                }
                if accepted_after.is_some_and(|sequence| envelope.sequence > sequence) {
                    match &event.event {
                        ControlEventKind::InputCompleted { .. } => return Ok(()),
                        ControlEventKind::InputFailed { message, .. } => {
                            return Err(failure(
                                NodeFailureCode::BackendOperationFailed,
                                message,
                            ));
                        }
                        _ => {}
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    if accepted_after.is_some() {
                        "input was accepted but did not settle before the bounded deadline"
                    } else {
                        "input was not accepted before the bounded deadline"
                    },
                ));
            }
            sleep(Duration::from_millis(2)).await;
        }
    }

    async fn dispatch_resize_bounded(
        &self,
        address: &SessionAddress,
        size: gate4agent_types::TerminalSize,
    ) -> Result<(), NodeFailure> {
        let started_after = self.current_sequence();
        let command_id = self
            .dispatch_bounded(
                ControlCommand::Resize {
                    instance_id: address.session.instance_id,
                    size,
                },
                Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
            )
            .await?;
        let deadline = Instant::now() + Duration::from_millis(MUTATION_SETTLE_TIMEOUT_MS);
        let mut scan_after = started_after;
        let mut accepted_after = None;
        loop {
            let events = self
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .events
                .iter()
                .filter(|envelope| envelope.sequence > scan_after)
                .cloned()
                .collect::<Vec<_>>();
            for envelope in events {
                scan_after = scan_after.max(envelope.sequence);
                let NodeEvent::Control {
                    address: event_address,
                    event,
                } = envelope.event
                else {
                    continue;
                };
                if event_address != *address {
                    continue;
                }
                if event.command_id == Some(command_id) {
                    match &event.event {
                        ControlEventKind::CommandRejected { message } => {
                            return Err(failure(NodeFailureCode::InvalidRequest, message));
                        }
                        ControlEventKind::ResizeRequested {
                            size: requested_size,
                            ..
                        } if *requested_size == size => {
                            accepted_after = Some(envelope.sequence);
                            continue;
                        }
                        _ => {}
                    }
                }
                if accepted_after.is_some_and(|sequence| envelope.sequence > sequence) {
                    match &event.event {
                        ControlEventKind::Resized { size: settled_size }
                            if *settled_size == size =>
                        {
                            return Ok(());
                        }
                        ControlEventKind::ResizeFailed { message } => {
                            return Err(failure(
                                NodeFailureCode::BackendOperationFailed,
                                message,
                            ));
                        }
                        _ => {}
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    if accepted_after.is_some() {
                        "resize was accepted but did not settle before the bounded deadline"
                    } else {
                        "resize was not accepted before the bounded deadline"
                    },
                ));
            }
            sleep(Duration::from_millis(2)).await;
        }
    }

    async fn spawn_session(
        &self,
        workspace_id: WorkspaceId,
        provider: AgentId,
        mode: SessionMode,
        terminal_size: gate4agent_types::TerminalSize,
        initial_prompt: Option<String>,
    ) -> Result<SessionAddress, NodeFailure> {
        if self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lease_for_workspace(&workspace_id)
            .is_some()
        {
            return Err(failure(
                NodeFailureCode::ManagedWorktreeOwnershipConflict,
                "managed worktree targets may only be spawned through their lease request",
            ));
        }
        self
            .spawn_session_with_deadline(
                workspace_id,
                provider,
                mode,
                terminal_size,
                initial_prompt,
                None,
                None,
                None,
                None,
                None,
                SpawnRecordPolicy::ProviderIdentityOnly,
                None,
                &[],
                None,
            )
            .await
            .map(|(session, _runtime_policy)| session)
    }

    async fn spawn_session_with_deadline(
        &self,
        workspace_id: WorkspaceId,
        provider: AgentId,
        mode: SessionMode,
        terminal_size: gate4agent_types::TerminalSize,
        initial_prompt: Option<String>,
        environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
        bundle: Option<ResolvedBundleReceipt>,
        context: Option<ResolvedContextPackReceipt>,
        managed_authority: Option<ManagedWorktreeLeaseId>,
        admitted_runtime_policy: Option<ProviderRuntimePolicy>,
        record_policy: SpawnRecordPolicy,
        deadline: Option<Instant>,
        required_capabilities: &[ProviderRuntimeCapability],
        harness_mcp: Option<&PreparedHarnessMcpSpawn>,
    ) -> Result<(SessionAddress, ProviderRuntimePolicy), NodeFailure> {
        let reserved_lease = self.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lease_for_workspace(&workspace_id);
        match (reserved_lease.as_ref(), managed_authority.as_ref()) {
            (Some(expected), Some(provided)) if expected == provided => {}
            (None, None) => {}
            _ => {
                return Err(failure(
                    NodeFailureCode::ManagedWorktreeOwnershipConflict,
                    "spawn lacks exact managed worktree lease authority",
                ));
            }
        }
        let runtime_requirement = match (mode, initial_prompt.is_some()) {
            (SessionMode::Pty, false) => ProviderRuntimeRequirement::RawPty,
            (SessionMode::Pty, true) => ProviderRuntimeRequirement::SemanticPrompt,
            (SessionMode::Inline, _) => ProviderRuntimeRequirement::Inline,
        };
        let runtime_policy = if let Some(runtime_policy) = admitted_runtime_policy {
            require_policy(runtime_policy, runtime_requirement).map_err(|_| failure(
                NodeFailureCode::UnsupportedSpawnCapability,
                "pre-admitted provider runtime no longer satisfies spawn requirements",
            ))?;
            runtime_policy
        } else if let Some(deadline) = deadline {
            let remaining = spawn_deadline_remaining(deadline)?;
            timeout(
                remaining,
                self.admit_provider_runtime(&provider, runtime_requirement),
            )
            .await
            .map_err(|_| {
                failure(
                    NodeFailureCode::SpawnDeadlineExceeded,
                    "provider admission exceeded the spawn deadline",
                )
            })??
        } else {
            self.admit_provider_runtime(&provider, runtime_requirement).await?
        };
        let runtime_policy = self.effective_spawn_runtime_policy(
            &provider,
            mode,
            runtime_policy,
        );
        if required_capabilities
            .iter()
            .any(|capability| !runtime_policy.admits(*capability))
        {
            return Err(failure(
                NodeFailureCode::UnsupportedSpawnCapability,
                "provider runtime does not admit a required spawn capability",
            ));
        }
        if let Some(deadline) = deadline {
            spawn_deadline_remaining(deadline)?;
        }
        self.ensure_binding_capacity()?;
        let working_directory = self.workspace_root(&workspace_id)?;
        let instance_id = AgentInstanceId(self.next_instance_id.fetch_add(1, Ordering::AcqRel));
        let session = SessionKey {
            instance_id,
            generation: SessionGeneration(1),
        };
        let address = SessionAddress {
            workspace_id: workspace_id.clone(),
            session,
        };
        let mut harness_mcp_overlay = if let Some(prepared) = harness_mcp {
            if mode != SessionMode::Pty || prepared.provider != provider {
                return Err(failure(
                    NodeFailureCode::BindingMismatch,
                    "harness MCP reservation does not match the exact PTY provider",
                ));
            }
            prepared.verify_helper_program().map_err(harness_mcp_failure)?;
            let control = self.native_launch_profile_control.clone().ok_or_else(|| {
                failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP launch control is unavailable")
            })?;
            let overlay = NativeHarnessMcpLaunchOverlay::new(
                provider.clone(),
                prepared.endpoint().as_os_str().to_os_string(),
                prepared.token().expose().into(),
                prepared.helper_program().as_os_str().to_os_string(),
            ).map_err(|_| failure(
                NodeFailureCode::HarnessMcpUnavailable,
                "harness MCP launch overlay is unavailable",
            ))?;
            control
                .install_native_harness_mcp_launch_overlay(instance_id, overlay)
                .map_err(|_| failure(
                    NodeFailureCode::HarnessMcpUnavailable,
                    "harness MCP launch overlay could not be installed",
                ))?;
            Some(NativeHarnessMcpOverlayGuard { control, instance_id })
        } else {
            None
        };
        let prepared_materialization = self.prepare_session_materialization(
            &address,
            &provider,
            mode,
            environment_profile.as_ref(),
            bundle.as_ref(),
            context.as_ref(),
            managed_authority.clone(),
        )?;
        let (environment_overlay, mut materialization_guard) = match prepared_materialization {
            Some((overlay, guard)) => (Some(overlay), Some(guard)),
            None => (None, None),
        };
        let materialization_id = materialization_guard
            .as_ref()
            .and_then(SessionMaterializationGuard::id)
            .cloned();
        let mut environment_selection = self.select_environment_profile(
            instance_id,
            &provider,
            mode,
            environment_profile.as_ref(),
        )?;
        let mut instance_overlay = environment_overlay
            .flatten()
            .map(|overlay| self.install_prepared_launch_overlay(instance_id, overlay))
            .transpose()?
            .flatten();
        let dispatch_timeout = spawn_dispatch_timeout(deadline)?;
        let record_id = self.bind_spawn_session_with_materialization(
            &address,
            provider.clone(),
            mode,
            runtime_policy,
            record_policy,
            environment_profile,
            bundle,
            context,
            materialization_id,
        )?;
        if let Some(selection) = environment_selection.take() {
            selection.retain();
        }
        if let Some(overlay) = instance_overlay.take() {
            overlay.retain();
        }
        if let Some(guard) = materialization_guard.take() {
            guard.retain();
        }
        let transport = match mode {
            SessionMode::Pty => TransportKind::Pty,
            SessionMode::Inline => TransportKind::Pipe,
        };
        let agent_id = provider;
        if let Err(error) = self.dispatch_bounded(
            ControlCommand::Register {
                instance_id,
                agent_id,
                transport,
            },
            dispatch_timeout,
        )
        .await {
            if let Some(binding) = self.remove_binding(&address) {
                self.cleanup_session_owned_materialization(&address, &binding)?;
            }
            if let Some(record_id) = record_id.as_ref() {
                self.discard_record(record_id)?;
            }
            return Err(spawn_dispatch_error(error, deadline));
        }
        let start_timeout = match spawn_dispatch_timeout(deadline) {
            Ok(timeout) => timeout,
            Err(error) => {
                let recovery = self
                    .rollback_spawn(
                        &address,
                        Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
                    )
                    .await;
                return match recovery {
                    Ok(()) => {
                        if let Some(record_id) = record_id.as_ref() {
                            self.discard_record(record_id)?;
                        }
                        Err(error)
                    }
                    Err(recovery_error) => {
                        if let Some(record_id) = record_id.as_ref() {
                            self.mark_record_error(
                                record_id,
                                "spawn deadline elapsed and registration recovery failed",
                            )?;
                        }
                        Err(recovery_error)
                    }
                };
            }
        };
        if let Err(start_error) = self.verify_harness_mcp_before_start(harness_mcp) {
            if let Err(recovery_error) = self
                .rollback_spawn(&address, Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS))
                .await
            {
                return Err(recovery_error);
            }
            if let Some(record_id) = record_id.as_ref() {
                self.discard_record(record_id)?;
            }
            return Err(start_error);
        }
        let start_result = self
            .dispatch_bounded(
                ControlCommand::Start {
                    instance_id,
                    runtime_policy,
                    request: StartRequest {
                        working_directory,
                        terminal_size,
                        initial_prompt,
                        session_options: None,
                    },
                },
                start_timeout,
            )
            .await;
        if let Err(start_error) = start_result {
            let start_error = spawn_dispatch_error(start_error, deadline);
            let recovery = self
                .rollback_spawn(
                    &address,
                    Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
                )
                .await;
            return match recovery {
                Ok(()) => {
                    if let Some(record_id) = record_id.as_ref() {
                        self.discard_record(record_id)?;
                    }
                    Err(start_error)
                }
                Err(recovery_error) => {
                    let diagnostic = format!(
                        "start failed and registration recovery failed: {}",
                        recovery_error.message,
                    );
                    if let Some(record_id) = record_id.as_ref() {
                        self.mark_record_error(record_id, &diagnostic)?;
                    }
                    Err(failure(recovery_error.code, &diagnostic))
                }
            };
        }

        let commit_deadline = deadline.unwrap_or_else(|| Instant::now() + dispatch_timeout);
        loop {
            if let Some(current) = self
                .handle
                .snapshot()
                .sessions
                .iter()
                .find(|current| current.instance_id == instance_id)
            {
                if current.generation != session.generation {
                    let divergence = failure(
                        NodeFailureCode::StaleGeneration,
                        "spawn generation diverged",
                    );
                    return match self
                        .rollback_spawn(
                            &address,
                            Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
                        )
                        .await
                    {
                        Ok(()) => {
                            if let Some(record_id) = record_id.as_ref() {
                                self.discard_record(record_id)?;
                            }
                            Err(divergence)
                        }
                        Err(recovery_error) => {
                            if let Some(record_id) = record_id.as_ref() {
                                self.mark_record_error(
                                    record_id,
                                    "spawn generation diverged and recovery failed",
                                )?;
                            }
                            Err(recovery_error)
                        }
                    };
                }
                if !matches!(current.status, gate4agent_types::SessionStatus::Registered) {
                    if let Some(overlay) = harness_mcp_overlay.take() {
                        overlay.retain();
                    }
                    return Ok((address, runtime_policy));
                }
            }
            if Instant::now() >= commit_deadline {
                self.rollback_spawn(
                    &address,
                    Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS),
                ).await.map_err(|error| {
                        failure(
                            error.code,
                            &format!(
                                "start did not commit and registration recovery failed: {}",
                                error.message
                            ),
                        )
                    })?;
                if let Some(record_id) = record_id.as_ref() {
                    self.discard_record(record_id)?;
                }
                return Err(failure(
                    if deadline.is_some() {
                        NodeFailureCode::SpawnDeadlineExceeded
                    } else {
                        NodeFailureCode::BackendBusy
                    },
                    "start did not commit before the bounded deadline; registration was removed",
                ));
            }
            sleep(Duration::from_millis(2)).await;
        }
    }

    fn effective_spawn_runtime_policy(
        &self,
        provider: &AgentId,
        mode: SessionMode,
        runtime_policy: ProviderRuntimePolicy,
    ) -> ProviderRuntimePolicy {
        #[cfg(feature = "fixture")]
        let runtime_policy = if self.fixture_semantic_hook_policy {
            ProviderRuntimePolicy::new(true, true, true, true, false)
                .expect("monitoring Hook fixture policy is internally valid")
        } else {
            runtime_policy
        };
        self.admit_qwen_sidecar_observation_policy(provider, mode, runtime_policy)
    }

    fn verify_harness_mcp_before_start(
        &self,
        prepared: Option<&PreparedHarnessMcpSpawn>,
    ) -> Result<(), NodeFailure> {
        prepared
            .map(PreparedHarnessMcpSpawn::verify_helper_program)
            .transpose()
            .map(|_| ())
            .map_err(harness_mcp_failure)
    }

    async fn rollback_spawn(
        &self,
        address: &SessionAddress,
        commit_timeout: Duration,
    ) -> Result<(), NodeFailure> {
        self.dispatch_bounded(
            ControlCommand::Remove {
                instance_id: address.session.instance_id,
            },
            commit_timeout,
        )
        .await?;
        self.wait_until_removed(address, commit_timeout).await?;
        if let Some(binding) = self.remove_binding(address) {
            self.cleanup_session_owned_materialization(address, &binding)?;
        }
        Ok(())
    }

    async fn remove_session(&self, address: &SessionAddress) -> Result<(), NodeFailure> {
        let commit_timeout = Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS);
        self.dispatch_bounded(
            ControlCommand::Remove {
                instance_id: address.session.instance_id,
            },
            commit_timeout,
        )
            .await?;
        self.wait_until_removed(address, commit_timeout).await?;
        let removed = self.remove_binding(address);
        if let Some(binding) = removed.as_ref() {
            self.cleanup_session_owned_materialization(address, binding)?;
        }
        if let Some(lease_id) = removed.and_then(|binding| binding.managed_worktree_lease_id) {
            let snapshot = self.managed_worktrees.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .release_session(
                    &lease_id,
                    self.incarnation_id,
                    address.session.instance_id,
                    address.session.generation,
                    unix_time_ms(),
                );
            if self.persist_state().is_err() {
                self.set_persistence_error(Some(DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned()));
            }
            if let Some(snapshot) = snapshot {
                self.publish(NodeEvent::ManagedWorktreeUpserted { lease: snapshot });
            }
            let _ = self.cleanup_managed_worktree(&lease_id, false).await;
        }
        Ok(())
    }

    async fn wait_until_removed(
        &self,
        address: &SessionAddress,
        commit_timeout: Duration,
    ) -> Result<(), NodeFailure> {
        let deadline = Instant::now() + commit_timeout;
        loop {
            let still_present = self
                .handle
                .snapshot()
                .sessions
                .iter()
                .any(|session| session.instance_id == address.session.instance_id);
            if !still_present {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    "remove did not commit before the bounded deadline; session binding was retained",
                ));
            }
            sleep(Duration::from_millis(2)).await;
        }
    }

    fn resync(&self, after_sequence: u64) -> NodeResponse {
        let history = self.history.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let event_sequence = history.last_sequence;
        let oldest_available_sequence = history.replay_floor_sequence;
        let events = history
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect();
        drop(history);
        NodeResponse::Resync {
            event_sequence,
            oldest_available_sequence,
            snapshot: self.snapshot(),
            events,
        }
    }
}

async fn accept_connections(endpoint: &str, shared: Arc<NodeShared>) -> Result<(), NodeServerError> {
    let mut connections = JoinSet::new();
    let result = accept_connections_inner(endpoint, shared, &mut connections).await;
    let graceful_deadline = Instant::now() + Duration::from_millis(CONNECTION_SHUTDOWN_GRACE_MS);
    while !connections.is_empty() {
        let remaining = graceful_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            || timeout(remaining, connections.join_next()).await.is_err()
        {
            break;
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

async fn accept_connections_inner(
    endpoint: &str,
    shared: Arc<NodeShared>,
    connections: &mut JoinSet<Result<(), NodeServerError>>,
) -> Result<(), NodeServerError> {
    let mut listener = OwnerOnlyLocalListener::bind(endpoint).await?;
    loop {
        while connections.try_join_next().is_some() {}
        if shared.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let preauth_permit = tokio::select! {
            permit = Arc::clone(&shared.preauth_slots).acquire_owned() => {
                permit.map_err(|_| NodeServerError::PreauthClosed)?
            }
            _ = shared.shutdown_notify.notified() => return Ok(()),
        };
        let server = tokio::select! {
            result = listener.accept() => result?,
            _ = shared.shutdown_notify.notified() => return Ok(()),
        };
        let connection_shared = Arc::clone(&shared);
        connections.spawn(async move {
            serve_connection(server, connection_shared, preauth_permit).await
        });
        if shared.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
    }
}

async fn serve_connection(
    mut pipe: LocalServerStream,
    shared: Arc<NodeShared>,
    preauth_permit: OwnedSemaphorePermit,
) -> Result<(), NodeServerError> {
    let hello_frame = timeout(
        Duration::from_millis(AUTH_FRAME_TIMEOUT_MS),
        read_json_frame_limited_body_timeout(
            &mut pipe,
            MAX_NODE_HELLO_FRAME_BYTES,
            Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
        ),
    )
    .await
    .map_err(|_| NodeServerError::AuthenticationTimedOut)??;
    let ClientFrame::Hello(hello) = hello_frame else {
        return Err(NodeServerError::Handshake("first frame must be hello".to_owned()));
    };
    if hello.protocol_version != NODE_PROTOCOL_VERSION {
        return Err(NodeServerError::Handshake("node protocol version mismatch".to_owned()));
    }
    let compatibility = match hello.compatibility.as_ref() {
        Some(offer) => {
            let mut selected = node_compatibility_support(&shared)?
                .negotiate(NODE_PROTOCOL_VERSION, offer)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?;
            project_negotiated_provider_ids(&mut selected);
            Some(selected)
        }
        None => None,
    };
    let server_nonce = random_nonce().map_err(NodeServerError::Authentication)?;
    let server_proof = match (hello.compatibility.as_ref(), compatibility.as_ref()) {
        (Some(offer), Some(selected)) => negotiated_auth_proof(
            shared.access_token.as_bytes(),
            AuthDirection::Server,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
            offer,
            selected,
        ),
        (None, None) => auth_proof(
            shared.access_token.as_bytes(),
            AuthDirection::Server,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
        ),
        _ => Err("node compatibility authentication state is inconsistent".to_owned()),
    }
    .map_err(NodeServerError::Authentication)?;
    write_json_frame_limited(
        &mut pipe,
        &ServerFrame::Challenge(ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
            compatibility: compatibility.clone(),
        }),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await?;
    let authentication = timeout(
        Duration::from_millis(AUTH_FRAME_TIMEOUT_MS),
        read_json_frame_limited_body_timeout(
            &mut pipe,
            MAX_NODE_HELLO_FRAME_BYTES,
            Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
        ),
    )
    .await
    .map_err(|_| NodeServerError::AuthenticationTimedOut)??;
    let ClientFrame::Authenticate(authentication) = authentication else {
        return Err(NodeServerError::Handshake("second frame must authenticate the challenge".to_owned()));
    };
    let expected_client_proof = match (hello.compatibility.as_ref(), compatibility.as_ref()) {
        (Some(offer), Some(selected)) => negotiated_auth_proof(
            shared.access_token.as_bytes(),
            AuthDirection::Client,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
            offer,
            selected,
        ),
        (None, None) => auth_proof(
            shared.access_token.as_bytes(),
            AuthDirection::Client,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
        ),
        _ => Err("node compatibility authentication state is inconsistent".to_owned()),
    }
    .map_err(NodeServerError::Authentication)?;
    if !proofs_match(&authentication.client_proof, &expected_client_proof) {
        return Err(NodeServerError::Handshake("access denied".to_owned()));
    }
    let selected_capabilities = compatibility
        .as_ref()
        .map(|selected| selected.capabilities.clone())
        .unwrap_or_default();
    let include_provider_runtime_status = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY
    });
    let include_open_provider_ids = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_PROVIDER_ID_OPEN_CAPABILITY
    });
    let include_terminal_frame_events = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_TERMINAL_FRAME_EVENTS_CAPABILITY
    });
    let include_spawn_profiles = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
    });
    let include_managed_worktrees = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY
    }) && selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_WORKTREE_SELECTION_CAPABILITY
    });
    let include_child_environment_profiles = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY
    });
    let include_session_bundles = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY
    });
    let include_history_context_packs = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_HISTORY_CONTEXT_PACK_CAPABILITY
    });
    let include_agent_progress = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY
    });
    let include_session_task_correlation = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_SESSION_TASK_CORRELATION_CAPABILITY
    });
    let include_observation_events = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_OBSERVATION_EVENTS_CAPABILITY
    });
    let include_observation_managed_target = include_observation_events
        && selected_capabilities.iter().any(|capability| {
            capability.as_str() == NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY
        });
    let include_observation_workflow_detail = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY
    });
    let include_harness_mcp_proxy = selected_capabilities.iter().any(|capability| {
        capability.as_str() == NODE_HARNESS_MCP_READ_PROXY_CAPABILITY
    });
    let authenticated_permit = Arc::clone(&shared.authenticated_slots)
        .try_acquire_owned()
        .map_err(|_| NodeServerError::AuthenticatedConnectionLimit)?;
    let connection_id = shared.next_connection_id.fetch_add(1, Ordering::AcqRel);
    let _release_guard = ControllerReleaseGuard {
        shared: Arc::clone(&shared),
        connection_id,
    };
    drop(preauth_permit);
    let _authenticated_permit = authenticated_permit;
    let mut event_rx = shared.event_tx.subscribe();
    let mut terminal_event_rx = terminal_event_subscription(
        &shared,
        include_terminal_frame_events,
    );
    write_json_frame(
        &mut pipe,
        &ServerFrame::Hello(NodeHello {
            protocol_version: NODE_PROTOCOL_VERSION,
            incarnation_id: shared.incarnation_id,
            connection_id,
            role: hello.role,
            event_sequence: shared.current_sequence(),
            controller: shared.controller_state(),
            snapshot: snapshot_for_wire(
                &shared,
                include_provider_runtime_status,
                include_open_provider_ids,
                include_spawn_profiles,
                include_managed_worktrees,
                include_child_environment_profiles,
                include_session_bundles,
                include_history_context_packs,
                include_agent_progress,
                include_session_task_correlation,
            ),
            compatibility,
        }),
    )
    .await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (frame_tx, mut frame_rx) = mpsc::channel(16);
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = read_json_frame_limited_body_timeout::<_, ClientFrame>(
                &mut reader,
                MAX_NODE_CLIENT_FRAME_BYTES,
                Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
            )
            .await;
            let terminal = frame.is_err();
            if frame_tx.send(frame).await.is_err() || terminal {
                break;
            }
        }
    });
    let _reader_abort = AbortTaskOnDrop(reader_task.abort_handle());
    let mut pending_events = Vec::new();
    let mut resync_required = false;
    let mut discard_events_through = 0_u64;
    loop {
        let durable_drain_budget = NODE_CONNECTION_EVENT_BURST_MAX
            .saturating_sub(pending_events.len().min(NODE_CONNECTION_EVENT_BURST_MAX));
        for _ in 0..durable_drain_budget {
            match event_rx.try_recv() {
                Ok(event) => queue_connection_event(
                    &mut pending_events,
                    event,
                    discard_events_through,
                ),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    resync_required = true;
                    break;
                }
                Err(broadcast::error::TryRecvError::Closed) => return Ok(()),
            }
        }
        if let Some(receiver) = terminal_event_rx.as_mut() {
            match receiver.try_recv() {
                Ok(events) => queue_connection_event_batch(
                    &mut pending_events,
                    &events,
                    discard_events_through,
                ),
                Err(broadcast::error::TryRecvError::Empty) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    resync_required = true;
                }
                Err(broadcast::error::TryRecvError::Closed) => return Ok(()),
            }
        }
        if resync_required {
            let event = resync_required_event(&shared);
            apply_connection_resync_watermark(
                &mut pending_events,
                &mut discard_events_through,
                event.sequence,
            );
            write_json_frame(&mut writer, &ServerFrame::Event(event)).await?;
            resync_required = false;
            continue;
        }
        if !pending_events.is_empty() {
            pending_events.sort_unstable_by_key(|event| event.sequence);
            let burst_len = connection_event_burst_len(pending_events.len());
            for event in pending_events.drain(..burst_len) {
                let event = if include_managed_worktrees {
                    Some(event)
                } else {
                    project_event_without_managed_worktrees(event)
                };
                let event = event.and_then(|event| if include_open_provider_ids {
                    Some(event)
                } else {
                    project_event_legacy_provider_ids(&shared, event)
                });
                let event = event.map(|event| if include_child_environment_profiles {
                    event
                } else {
                    project_event_without_child_environment_profile(event)
                });
                let event = event.map(|event| if include_session_bundles {
                    event
                } else {
                    project_event_without_session_bundle(event)
                });
                let event = event.map(|event| {
                    project_event_history_for_wire(event, include_history_context_packs)
                });
                let event = event.map(|event| if include_history_context_packs {
                    event
                } else {
                    project_event_without_context_pack(event)
                });
                let event = event.map(|event| if include_session_task_correlation {
                    event
                } else {
                    project_event_without_session_task_binding(event)
                });
                let event = event.and_then(|event| if include_observation_events {
                    Some(event)
                } else {
                    project_event_without_observation(event)
                });
                let event = event.and_then(|event| if include_observation_managed_target {
                    Some(event)
                } else {
                    project_event_without_managed_observation(event)
                });
                let event = event.and_then(|event| if include_observation_workflow_detail {
                    Some(event)
                } else {
                    project_event_without_observation_workflow_detail(event)
                });
                let exact_harness_controller = hello.role == ClientRole::Operator
                    && shared.controller_state().is_some_and(|controller| {
                        controller.connection_id == connection_id
                    });
                let event = event.and_then(|event| {
                    if include_harness_mcp_proxy && exact_harness_controller {
                        Some(event)
                    } else {
                        project_event_without_harness_mcp_proxy(event)
                    }
                });
                if let Some(event) = event {
                    write_json_frame(&mut writer, &ServerFrame::Event(event)).await?;
                }
            }
        }
        tokio::select! {
            biased;
            frame = frame_rx.recv() => {
                let frame = match frame {
                    Some(frame) => frame,
                    None => break,
                };
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(FrameError::Io(error)) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof) => break,
                    Err(error) => return Err(error.into()),
                };
                let ClientFrame::Request(request) = frame else {
                    return Err(NodeServerError::Handshake("hello may only be sent once".to_owned()));
                };
                #[cfg(feature = "fixture")]
                let probe_spawn_managed_worktree_v2 = matches!(
                    &request.request,
                    NodeRequest::SpawnManagedWorktreeV2 { .. }
                );
                let requires_open_provider_ids =
                    request_requires_open_provider_ids(&shared, &request.request);
                let requires_child_environment_profile =
                    request_requires_child_environment_profile(&shared, &request.request);
                let requires_session_bundle =
                    request_requires_session_bundle(&shared, &request.request);
                let requires_history_context_pack =
                    request_requires_history_context_pack(&shared, &request.request);
                let mut reply = if requires_open_provider_ids && !include_open_provider_ids {
                    ResponseEnvelope {
                        request_id: request.request_id,
                        result: Err(failure(
                            NodeFailureCode::UnsupportedCapability,
                            "open provider IDs were not negotiated",
                        )),
                    }
                } else if requires_child_environment_profile
                    && !include_child_environment_profiles
                {
                    ResponseEnvelope {
                        request_id: request.request_id,
                        result: Err(failure(
                            NodeFailureCode::UnsupportedCapability,
                            "child environment profiles were not negotiated",
                        )),
                    }
                } else if requires_session_bundle && !include_session_bundles {
                    ResponseEnvelope {
                        request_id: request.request_id,
                        result: Err(failure(
                            NodeFailureCode::UnsupportedCapability,
                            "session bundles were not negotiated",
                        )),
                    }
                } else if requires_history_context_pack && !include_history_context_packs {
                    ResponseEnvelope {
                        request_id: request.request_id,
                        result: Err(failure(
                            NodeFailureCode::UnsupportedCapability,
                            "history context packs were not negotiated",
                        )),
                    }
                } else if request_uses_unnegotiated_capability(
                    &request.request,
                    &selected_capabilities,
                )
                {
                    ResponseEnvelope {
                        request_id: request.request_id,
                        result: Err(failure(
                            NodeFailureCode::UnsupportedCapability,
                            "request capability was not negotiated",
                        )),
                    }
                } else {
                    process_request(&shared, connection_id, hello.role, request).await
                };
                #[cfg(feature = "fixture")]
                if probe_spawn_managed_worktree_v2 {
                    shared
                        .fixture_spawn_managed_worktree_v2_failure_probe
                        .record(reply.result.as_ref().err().map(|failure| failure.code));
                }
                if !include_provider_runtime_status {
                    clear_response_provider_runtime_status(&mut reply);
                }
                if !include_managed_worktrees {
                    project_response_without_managed_worktrees(&mut reply);
                }
                if !include_open_provider_ids {
                    project_response_legacy_provider_ids(&shared, &mut reply);
                }
                if !include_child_environment_profiles {
                    project_response_without_child_environment_profile(&mut reply);
                }
                if !include_session_bundles {
                    project_response_without_session_bundle(&mut reply);
                }
                project_response_history_for_wire(
                    &mut reply,
                    include_history_context_packs,
                );
                if !include_history_context_packs {
                    project_response_without_context_pack(&mut reply);
                }
                if !include_agent_progress {
                    project_response_without_agent_progress(&mut reply);
                }
                if !include_session_task_correlation {
                    project_response_without_session_task_binding(&mut reply);
                }
                if !include_observation_events {
                    project_response_without_observations(&mut reply);
                }
                if !include_observation_managed_target {
                    project_response_without_managed_observations(&mut reply);
                }
                if !include_observation_workflow_detail {
                    project_response_without_observation_workflow_detail(&mut reply);
                }
                write_json_frame(&mut writer, &ServerFrame::Reply(reply)).await?;
            }
            _ = tokio::task::yield_now(), if !pending_events.is_empty() => {}
            event = event_rx.recv() => {
                match event {
                    Ok(event) => queue_connection_event(
                        &mut pending_events,
                        event,
                        discard_events_through,
                    ),
                    Err(broadcast::error::RecvError::Lagged(_)) => resync_required = true,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            events = receive_terminal_event_batch(&mut terminal_event_rx) => {
                match events {
                    Ok(events) => queue_connection_event_batch(
                        &mut pending_events,
                        &events,
                        discard_events_through,
                    ),
                    Err(broadcast::error::RecvError::Lagged(_)) => resync_required = true,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = shared.shutdown_notify.notified() => break,
        }
    }
    Ok(())
}

async fn receive_terminal_event_batch(
    receiver: &mut Option<broadcast::Receiver<Arc<Vec<NodeEventEnvelope>>>>,
) -> Result<Arc<Vec<NodeEventEnvelope>>, broadcast::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

fn terminal_event_subscription(
    shared: &NodeShared,
    enabled: bool,
) -> Option<broadcast::Receiver<Arc<Vec<NodeEventEnvelope>>>> {
    enabled.then(|| shared.terminal_event_tx.subscribe())
}

fn queue_connection_event(
    pending: &mut Vec<NodeEventEnvelope>,
    event: NodeEventEnvelope,
    discard_through: u64,
) {
    if event.sequence <= discard_through
        && !event.event.requires_harness_mcp_proxy_capability()
    {
        return;
    }
    if let NodeEvent::TerminalFrame { address, .. } = &event.event {
        if let Some(index) = pending.iter().position(|current| {
            matches!(
                &current.event,
                NodeEvent::TerminalFrame {
                    address: current_address,
                    ..
                } if current_address == address
            )
        }) {
            pending.remove(index);
        }
    }
    pending.push(event);
}

fn project_event_without_harness_mcp_proxy(
    envelope: NodeEventEnvelope,
) -> Option<NodeEventEnvelope> {
    (!envelope.event.requires_harness_mcp_proxy_capability()).then_some(envelope)
}

fn queue_connection_event_batch(
    pending: &mut Vec<NodeEventEnvelope>,
    events: &[NodeEventEnvelope],
    discard_through: u64,
) {
    for event in events {
        queue_connection_event(pending, event.clone(), discard_through);
    }
}

fn apply_connection_resync_watermark(
    pending: &mut Vec<NodeEventEnvelope>,
    discard_through: &mut u64,
    marker_sequence: u64,
) {
    *discard_through = (*discard_through).max(marker_sequence);
    pending.retain(|event| event.sequence > *discard_through);
}

fn connection_event_burst_len(pending_len: usize) -> usize {
    pending_len.min(NODE_CONNECTION_EVENT_BURST_MAX)
}

fn resync_required_event(shared: &NodeShared) -> NodeEventEnvelope {
    let history = shared
        .history
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sequence = history.last_sequence;
    let oldest_available_sequence = history.replay_floor_sequence;
    NodeEventEnvelope {
        sequence,
        event: NodeEvent::ResyncRequired {
            oldest_available_sequence,
        },
    }
}

fn node_compatibility_support(
    shared: &NodeShared,
) -> Result<NodeCompatibilitySupport, NodeServerError> {
    let mut support = node_compatibility_support_for_manifest(
        &shared.provider_contracts,
        &shared.provider_adapter_contracts,
    )?;
    if shared
        .delivery_store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some()
    {
        support.capabilities.push(
            CapabilityId::new(NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        );
    }
    if shared.harness_mcp_registry.is_some() {
        support.capabilities.push(
            CapabilityId::new(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        );
    }
    Ok(support)
}

fn node_compatibility_support_for_manifest(
    provider_contracts: &[ProviderContractSupport],
    provider_adapter_contracts: &[ProviderAdapterContractSupport],
) -> Result<NodeCompatibilitySupport, NodeServerError> {
    Ok(NodeCompatibilitySupport {
        protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION)
            .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        capabilities: baseline_capabilities()?,
        host: platform::host_descriptor().map_err(NodeServerError::Handshake)?,
        path_semantics: platform::path_semantics(),
        local_transport: platform::local_transport(),
        state_schema: StateSchemaSupport {
            versions: ProtocolRange::new(NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V10)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        },
        provider_contracts: provider_contracts.to_vec(),
        provider_adapter_contracts: provider_adapter_contracts.to_vec(),
    })
}

fn baseline_capabilities() -> Result<Vec<CapabilityId>, NodeServerError> {
    let capabilities = [
        NODE_COMPATIBILITY_METADATA_CAPABILITY,
        CAPABILITY_HOST_DIRECTORY_BROWSE_V1,
        NODE_REPOSITORY_PATH_CAPABILITY,
        NODE_WORKSPACE_FILE_READ_CAPABILITY,
        NODE_WORKSPACE_FILE_WRITE_CAPABILITY,
        NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY,
        NODE_GIT_READ_CAPABILITY,
        NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY,
        NODE_PROVIDER_ID_OPEN_CAPABILITY,
        NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY,
        NODE_PROVIDER_RUNTIME_STATUS_CAPABILITY,
        NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
        NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
        NODE_SPAWN_PROFILE_REVISION_CAPABILITY,
        NODE_WORKTREE_SELECTION_CAPABILITY,
        NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
        NODE_MANAGED_WORKTREE_SPAWN_V2_CAPABILITY,
        NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
        NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
        NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
        NODE_SESSION_RECORD_CONTEXT_EXPORT_CAPABILITY,
        NODE_NATIVE_SESSION_CATALOG_CAPABILITY,
        NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY,
        NODE_NATIVE_SESSION_INDEX_CAPABILITY,
        NODE_NATIVE_SESSION_PREVIEW_CAPABILITY,
        NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
        NODE_SESSION_TASK_CORRELATION_CAPABILITY,
        NODE_OBSERVATION_EVENTS_CAPABILITY,
        NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
        NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    ]
        .into_iter()
        .map(|capability| {
            CapabilityId::new(capability)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(windows)]
    let capabilities = {
        let mut capabilities = capabilities;
        capabilities.push(
            CapabilityId::new(NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        );
        capabilities
    };
    Ok(capabilities)
}

fn request_uses_unnegotiated_capability(
    request: &NodeRequest,
    selected_capabilities: &[CapabilityId],
) -> bool {
    let selected = |required: &str| {
        selected_capabilities
            .iter()
            .any(|capability| capability.as_str() == required)
    };
    request
        .required_capability()
        .is_some_and(|required| !selected(required))
        || (request.requires_worktree_selection_capability()
            && !selected(NODE_WORKTREE_SELECTION_CAPABILITY))
        || (request.requires_spawn_spec_defaults_overrides_capability()
            && !selected(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY))
        || (request.requires_spawn_profile_revision_capability()
            && !selected(NODE_SPAWN_PROFILE_REVISION_CAPABILITY))
        || (request.requires_child_environment_profile_capability()
            && !selected(NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY))
        || (request.requires_session_bundle_materialization_capability()
            && !selected(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY))
        || (request.requires_history_context_pack_capability()
            && !selected(NODE_HISTORY_CONTEXT_PACK_CAPABILITY))
}

fn snapshot_for_wire(
    shared: &NodeShared,
    include_provider_runtime_status: bool,
    include_open_provider_ids: bool,
    include_spawn_profiles: bool,
    include_managed_worktrees: bool,
    include_child_environment_profiles: bool,
    include_session_bundles: bool,
    include_history_context_packs: bool,
    include_agent_progress: bool,
    include_session_task_correlation: bool,
) -> NodeSnapshot {
    let mut snapshot = shared.snapshot();
    if !include_provider_runtime_status {
        snapshot.provider_runtime_statuses.clear();
    }
    if !include_open_provider_ids {
        project_snapshot_legacy_provider_ids(&mut snapshot);
    }
    if !include_managed_worktrees {
        snapshot.managed_worktrees.clear();
        for workspace in &mut snapshot.workspaces {
            workspace.managed_worktree_profiles = None;
        }
    }
    if let Some(inventory) = snapshot.launch_inventory.as_mut() {
        if !include_spawn_profiles {
            inventory.spawn_profiles = None;
        }
        if !include_session_bundles {
            inventory.bundles = None;
        }
        if inventory.spawn_profiles.is_none() && inventory.bundles.is_none() {
            snapshot.launch_inventory = None;
        }
    }
    if !include_child_environment_profiles {
        clear_snapshot_child_environment_profiles(&mut snapshot);
    }
    if !include_session_bundles {
        clear_snapshot_session_bundles(&mut snapshot);
    }
    project_snapshot_history_for_wire(&mut snapshot, include_history_context_packs);
    if !include_history_context_packs {
        clear_snapshot_context_packs(&mut snapshot);
    }
    if !include_agent_progress {
        snapshot.agent_progress.clear();
    }
    if !include_session_task_correlation {
        clear_snapshot_session_task_bindings(&mut snapshot);
    }
    snapshot
}

fn clear_snapshot_session_task_bindings(snapshot: &mut NodeSnapshot) {
    for record in &mut snapshot.session_records {
        record.task_binding = None;
    }
}

fn project_event_without_session_task_binding(
    mut envelope: NodeEventEnvelope,
) -> NodeEventEnvelope {
    if let NodeEvent::SessionRecordUpserted { record } = &mut envelope.event {
        record.task_binding = None;
    }
    envelope
}

fn project_event_without_observation(
    envelope: NodeEventEnvelope,
) -> Option<NodeEventEnvelope> {
    (!envelope.event.requires_observation_events_capability()).then_some(envelope)
}

fn project_event_without_managed_observation(
    envelope: NodeEventEnvelope,
) -> Option<NodeEventEnvelope> {
    (!envelope
        .event
        .requires_observation_managed_target_capability())
    .then_some(envelope)
}

fn project_event_without_observation_workflow_detail(
    envelope: NodeEventEnvelope,
) -> Option<NodeEventEnvelope> {
    (!envelope
        .event
        .requires_observation_workflow_detail_capability())
    .then_some(envelope)
}

fn project_response_without_observations(reply: &mut ResponseEnvelope) {
    let Ok(NodeResponse::Resync { events, .. }) = reply.result.as_mut() else {
        return;
    };
    strip_observation_events(events);
}

fn strip_observation_events(events: &mut Vec<NodeEventEnvelope>) {
    events.retain(|event| !event.event.requires_observation_events_capability());
}

fn project_response_without_managed_observations(reply: &mut ResponseEnvelope) {
    let Ok(NodeResponse::Resync { events, .. }) = reply.result.as_mut() else {
        return;
    };
    events.retain(|event| {
        !event
            .event
            .requires_observation_managed_target_capability()
    });
}

fn project_response_without_observation_workflow_detail(reply: &mut ResponseEnvelope) {
    let Ok(NodeResponse::Resync { events, .. }) = reply.result.as_mut() else {
        return;
    };
    events.retain(|event| {
        !event
            .event
            .requires_observation_workflow_detail_capability()
    });
}

fn project_response_without_session_task_binding(reply: &mut ResponseEnvelope) {
    let Ok(response) = reply.result.as_mut() else { return };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            clear_snapshot_session_task_bindings(snapshot);
        }
        NodeResponse::Resync { snapshot, events, .. } => {
            clear_snapshot_session_task_bindings(snapshot);
            for event in events {
                if let NodeEvent::SessionRecordUpserted { record } = &mut event.event {
                    record.task_binding = None;
                }
            }
        }
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::ProviderSessionIndexed { record }
        | NodeResponse::NativeSessionIndexed { record, .. }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            record.task_binding = None;
        }
        _ => {}
    }
}

fn project_response_without_agent_progress(reply: &mut ResponseEnvelope) {
    let Ok(response) = reply.result.as_mut() else { return };
    match response {
        NodeResponse::Snapshot { snapshot, .. } | NodeResponse::Resync { snapshot, .. } => {
            snapshot.agent_progress.clear();
        }
        _ => {}
    }
}

fn project_event_without_managed_worktrees(
    mut envelope: NodeEventEnvelope,
) -> Option<NodeEventEnvelope> {
    if matches!(
        envelope.event,
        NodeEvent::ManagedWorktreeUpserted { .. }
            | NodeEvent::ManagedWorktreeRemoved { .. }
    ) {
        return None;
    }
    if let NodeEvent::WorkspaceAdded { workspace } = &mut envelope.event {
        workspace.managed_worktree_profiles = None;
    }
    Some(envelope)
}

fn project_response_without_managed_worktrees(reply: &mut ResponseEnvelope) {
    if matches!(
        reply.result,
        Ok(NodeResponse::ManagedWorktreeSpawnAccepted { .. })
            | Ok(NodeResponse::ManagedWorktreeCleanup { .. })
    ) {
        reply.result = Err(failure(
            NodeFailureCode::UnsupportedCapability,
            "managed worktree capability was not negotiated",
        ));
        return;
    }
    let Ok(response) = reply.result.as_mut() else { return };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            snapshot.managed_worktrees.clear();
            for workspace in &mut snapshot.workspaces {
                workspace.managed_worktree_profiles = None;
            }
        }
        NodeResponse::Resync { snapshot, events, .. } => {
            snapshot.managed_worktrees.clear();
            for workspace in &mut snapshot.workspaces {
                workspace.managed_worktree_profiles = None;
            }
            *events = std::mem::take(events)
                .into_iter()
                .filter_map(project_event_without_managed_worktrees)
                .collect();
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::StandaloneWorkspaceCreated { workspace }
        | NodeResponse::WorktreeCreated { workspace, .. } => {
            workspace.managed_worktree_profiles = None;
        }
        NodeResponse::WorkspaceInspected { inspection } => {
            inspection.git.managed_worktree = None;
        }
        _ => {}
    }
}

fn project_negotiated_provider_ids(compatibility: &mut NegotiatedNodeCompatibility) {
    let includes_open_provider_ids = compatibility.capabilities.iter().any(|capability| {
        capability.as_str() == NODE_PROVIDER_ID_OPEN_CAPABILITY
    });
    if includes_open_provider_ids {
        return;
    }
    compatibility
        .provider_contracts
        .retain(|contract| provider_id_is_legacy(&contract.provider));
    compatibility
        .provider_adapter_contracts
        .retain(|contract| provider_id_is_legacy(&contract.provider));
}

fn request_requires_open_provider_ids(shared: &NodeShared, request: &NodeRequest) -> bool {
    if let Some(spec) = match request {
        NodeRequest::SpawnSpec { spec }
        | NodeRequest::SpawnSpecWithHarnessMcp { spec, .. }
        | NodeRequest::ArmHarnessMcpReservation { spawn_spec: spec, .. } => Some(spec),
        NodeRequest::SpawnManagedWorktree { request } => Some(&request.spawn_spec),
        NodeRequest::SpawnManagedWorktreeV2 { request } => Some(&request.spawn_spec),
        _ => None,
    } {
        return shared
            .resolve_spawn_spec(spec)
            .is_ok_and(|resolved| !provider_id_is_legacy(&resolved.provider));
    }
    request_requires_open_provider_ids_with(
        request,
        |session| shared.validate_address(session).ok(),
        |record_id| shared.record(record_id).ok().map(|record| record.provider),
    )
}

fn request_requires_child_environment_profile(
    shared: &NodeShared,
    request: &NodeRequest,
) -> bool {
    if let Some(spec) = match request {
        NodeRequest::SpawnSpec { spec }
        | NodeRequest::SpawnSpecWithHarnessMcp { spec, .. }
        | NodeRequest::ArmHarnessMcpReservation { spawn_spec: spec, .. } => Some(spec),
        NodeRequest::SpawnManagedWorktree { request } => Some(&request.spawn_spec),
        NodeRequest::SpawnManagedWorktreeV2 { request } => Some(&request.spawn_spec),
        _ => None,
    } {
        return shared
            .resolve_spawn_spec(spec)
            .is_ok_and(|resolved| resolved.environment_profile_id.is_some());
    }
    match request {
        NodeRequest::Resume { session, .. }
        | NodeRequest::Prompt { session, .. }
        | NodeRequest::Paste { session, .. }
        | NodeRequest::Input { session, .. }
        | NodeRequest::TerminalBytes { session, .. }
        | NodeRequest::TerminalControl { session, .. }
        | NodeRequest::Resize { session, .. }
        | NodeRequest::Interrupt { session }
        | NodeRequest::Stop { session, .. }
        | NodeRequest::Remove { session }
        | NodeRequest::DiscoverHistory { session, .. }
        | NodeRequest::LoadHistory { session, .. }
        | NodeRequest::ExportContextPackForSessionRecord { session, .. }
        | NodeRequest::ExportContextPack { session } => shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.session.instance_id)
            .filter(|binding| {
                binding.workspace_id == session.workspace_id
                    && binding.generation == session.session.generation
            })
            .map_or(true, |binding| binding.environment_profile.is_some()),
        NodeRequest::RenameSessionRecord { record_id, .. }
        | NodeRequest::SetSessionTask { record_id, .. }
        | NodeRequest::ResumeSessionRecord { record_id, .. }
        | NodeRequest::ForgetSessionRecord { record_id } => shared
            .record(record_id)
            .map_or(true, |record| record.environment_profile.is_some()),
        NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::BrowseHostDirectories { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::Spawn { .. }
        | NodeRequest::SpawnSpec { .. }
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::CreateStandaloneWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::SpawnManagedWorktreeV2 { .. }
        | NodeRequest::CleanupManagedWorktree { .. }
        | NodeRequest::IndexProviderSession { .. }
        | NodeRequest::IndexNativeSession { .. }
        | NodeRequest::CatalogNativeSessions { .. }
        | NodeRequest::PageNativeSessions { .. }
        | NodeRequest::PreviewNativeSession { .. }
        | NodeRequest::PreviewSessionRecord { .. }
        | NodeRequest::ForgetContextPack { .. }
        | NodeRequest::ResolveDurableContextPack { .. }
        | NodeRequest::Shutdown => false,
    }
}

fn request_requires_session_bundle(shared: &NodeShared, request: &NodeRequest) -> bool {
    if let Some(spec) = match request {
        NodeRequest::SpawnSpec { spec }
        | NodeRequest::SpawnSpecWithHarnessMcp { spec, .. }
        | NodeRequest::ArmHarnessMcpReservation { spawn_spec: spec, .. } => Some(spec),
        NodeRequest::SpawnManagedWorktree { request } => Some(&request.spawn_spec),
        NodeRequest::SpawnManagedWorktreeV2 { request } => Some(&request.spawn_spec),
        _ => None,
    } {
        return shared
            .resolve_spawn_spec(spec)
            .is_ok_and(|resolved| resolved.bundle_id.is_some());
    }
    match request {
        NodeRequest::Resume { session, .. }
        | NodeRequest::Prompt { session, .. }
        | NodeRequest::Paste { session, .. }
        | NodeRequest::Input { session, .. }
        | NodeRequest::TerminalBytes { session, .. }
        | NodeRequest::TerminalControl { session, .. }
        | NodeRequest::Resize { session, .. }
        | NodeRequest::Interrupt { session }
        | NodeRequest::Stop { session, .. }
        | NodeRequest::Remove { session }
        | NodeRequest::DiscoverHistory { session, .. }
        | NodeRequest::LoadHistory { session, .. }
        | NodeRequest::ExportContextPackForSessionRecord { session, .. }
        | NodeRequest::ExportContextPack { session } => shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.session.instance_id)
            .filter(|binding| {
                binding.workspace_id == session.workspace_id
                    && binding.generation == session.session.generation
            })
            .map_or(true, |binding| binding.bundle.is_some()),
        NodeRequest::RenameSessionRecord { record_id, .. }
        | NodeRequest::SetSessionTask { record_id, .. }
        | NodeRequest::ResumeSessionRecord { record_id, .. }
        | NodeRequest::ForgetSessionRecord { record_id } => shared
            .record(record_id)
            .map_or(true, |record| record.bundle.is_some()),
        NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::BrowseHostDirectories { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::Spawn { .. }
        | NodeRequest::SpawnSpec { .. }
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::CreateStandaloneWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::SpawnManagedWorktreeV2 { .. }
        | NodeRequest::CleanupManagedWorktree { .. }
        | NodeRequest::IndexProviderSession { .. }
        | NodeRequest::IndexNativeSession { .. }
        | NodeRequest::CatalogNativeSessions { .. }
        | NodeRequest::PageNativeSessions { .. }
        | NodeRequest::PreviewNativeSession { .. }
        | NodeRequest::PreviewSessionRecord { .. }
        | NodeRequest::ForgetContextPack { .. }
        | NodeRequest::ResolveDurableContextPack { .. }
        | NodeRequest::Shutdown => false,
    }
}

fn request_requires_history_context_pack(shared: &NodeShared, request: &NodeRequest) -> bool {
    if request.requires_history_context_pack_capability() {
        return true;
    }
    if let Some(spec) = match request {
        NodeRequest::SpawnSpec { spec }
        | NodeRequest::SpawnSpecWithHarnessMcp { spec, .. }
        | NodeRequest::ArmHarnessMcpReservation { spawn_spec: spec, .. } => Some(spec),
        NodeRequest::SpawnManagedWorktree { request } => Some(&request.spawn_spec),
        NodeRequest::SpawnManagedWorktreeV2 { request } => Some(&request.spawn_spec),
        _ => None,
    } {
        return shared
            .resolve_spawn_spec(spec)
            .map_or(true, |resolved| resolved.context_id.is_some());
    }
    match request {
        NodeRequest::Resume { session, .. }
        | NodeRequest::Prompt { session, .. }
        | NodeRequest::Paste { session, .. }
        | NodeRequest::Input { session, .. }
        | NodeRequest::TerminalBytes { session, .. }
        | NodeRequest::TerminalControl { session, .. }
        | NodeRequest::Resize { session, .. }
        | NodeRequest::Interrupt { session }
        | NodeRequest::Stop { session, .. }
        | NodeRequest::Remove { session } => shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session.session.instance_id)
            .filter(|binding| {
                binding.workspace_id == session.workspace_id
                    && binding.generation == session.session.generation
            })
            .map_or(true, |binding| binding.context.is_some()),
        NodeRequest::RenameSessionRecord { record_id, .. }
        | NodeRequest::SetSessionTask { record_id, .. }
        | NodeRequest::ResumeSessionRecord { record_id, .. }
        | NodeRequest::ForgetSessionRecord { record_id } => shared
            .record(record_id)
            .map_or(true, |record| record.context.is_some()),
        NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::BrowseHostDirectories { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::Spawn { .. }
        | NodeRequest::SpawnSpec { .. }
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::CreateStandaloneWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::SpawnManagedWorktreeV2 { .. }
        | NodeRequest::CleanupManagedWorktree { .. }
        | NodeRequest::IndexProviderSession { .. }
        | NodeRequest::IndexNativeSession { .. }
        | NodeRequest::CatalogNativeSessions { .. }
        | NodeRequest::PageNativeSessions { .. }
        | NodeRequest::PreviewNativeSession { .. }
        | NodeRequest::PreviewSessionRecord { .. }
        | NodeRequest::DiscoverHistory { .. }
        | NodeRequest::LoadHistory { .. }
        | NodeRequest::ExportContextPackForSessionRecord { .. }
        | NodeRequest::ExportContextPack { .. }
        | NodeRequest::ForgetContextPack { .. }
        | NodeRequest::ResolveDurableContextPack { .. }
        | NodeRequest::Shutdown => false,
    }
}

fn request_requires_open_provider_ids_with(
    request: &NodeRequest,
    provider_for_session: impl Fn(&SessionAddress) -> Option<AgentId>,
    provider_for_record: impl Fn(&SessionRecordId) -> Option<AgentId>,
) -> bool {
    match request {
        NodeRequest::Spawn { provider, .. } => !provider_id_is_legacy(provider),
        NodeRequest::IndexProviderSession { provider, .. } => !provider_id_is_legacy(provider),
        NodeRequest::CatalogNativeSessions { route, .. }
        | NodeRequest::PageNativeSessions { route, .. } => {
            !provider_id_is_legacy(&route.provider)
        }
        NodeRequest::PreviewNativeSession { selection, .. }
        | NodeRequest::IndexNativeSession { selection, .. } => {
            !provider_id_is_legacy(&selection.route.provider)
        }
        NodeRequest::SpawnSpec { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. } => false,
        NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::SpawnManagedWorktreeV2 { .. } => false,
        NodeRequest::Resume { session, .. }
        | NodeRequest::Prompt { session, .. }
        | NodeRequest::Paste { session, .. }
        | NodeRequest::Input { session, .. }
        | NodeRequest::TerminalBytes { session, .. }
        | NodeRequest::TerminalControl { session, .. }
        | NodeRequest::Resize { session, .. }
        | NodeRequest::Interrupt { session }
        | NodeRequest::Stop { session, .. }
        | NodeRequest::Remove { session }
        | NodeRequest::DiscoverHistory { session, .. }
        | NodeRequest::LoadHistory { session, .. }
        | NodeRequest::ExportContextPackForSessionRecord { session, .. }
        | NodeRequest::ExportContextPack { session } => provider_for_session(session)
            .map_or(true, |provider| !provider_id_is_legacy(&provider)),
        NodeRequest::RenameSessionRecord { record_id, .. }
        | NodeRequest::SetSessionTask { record_id, .. }
        | NodeRequest::ResumeSessionRecord { record_id, .. }
        | NodeRequest::ForgetSessionRecord { record_id } => provider_for_record(record_id)
            .map_or(true, |provider| !provider_id_is_legacy(&provider)),
        NodeRequest::PreviewSessionRecord { record_id, .. } => provider_for_record(record_id)
            .map_or(true, |provider| !provider_id_is_legacy(&provider)),
        NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::BrowseHostDirectories { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::CreateStandaloneWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::CleanupManagedWorktree { .. }
        | NodeRequest::Shutdown => false,
        NodeRequest::ForgetContextPack { .. } => true,
        // No session/record to cheaply resolve a provider from here either;
        // the resolved receipt can carry an open-id provider, so require the
        // same capability as ForgetContextPack unconditionally.
        NodeRequest::ResolveDurableContextPack { .. } => true,
    }
}

fn project_snapshot_legacy_provider_ids(snapshot: &mut NodeSnapshot) {
    snapshot.enabled_providers.retain(provider_id_is_legacy);
    snapshot.provider_runtime_statuses = ProviderRuntimeStatuses::new(
        snapshot
            .provider_runtime_statuses
            .as_slice()
            .iter()
            .filter(|status| provider_id_is_legacy(status.provider()))
            .cloned(),
    )
    .expect("a filtered provider runtime projection remains valid");
    snapshot
        .session_records
        .retain(|record| provider_id_is_legacy(&record.provider));
    for workspace in &mut snapshot.workspaces {
        workspace
            .sessions
            .retain(|session| provider_id_is_legacy(&session.agent_id));
    }
}

fn clear_snapshot_child_environment_profiles(snapshot: &mut NodeSnapshot) {
    for record in &mut snapshot.session_records {
        record.environment_profile = None;
    }
    if let Some(profiles) = snapshot
        .launch_inventory
        .as_mut()
        .and_then(|inventory| inventory.spawn_profiles.as_mut())
    {
        for profile in profiles {
            profile.environment_profile = None;
        }
    }
}

fn project_event_without_child_environment_profile(
    mut envelope: NodeEventEnvelope,
) -> NodeEventEnvelope {
    if let NodeEvent::SessionRecordUpserted { record } = &mut envelope.event {
        record.environment_profile = None;
    }
    envelope
}

fn project_response_without_child_environment_profile(reply: &mut ResponseEnvelope) {
    let contains_environment_profile = match reply.result.as_ref() {
        Ok(NodeResponse::SpawnSpecAccepted { receipt }) => {
            receipt.environment_profile.is_some()
        }
        Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => {
            receipt.spawn.environment_profile.is_some()
        }
        _ => false,
    };
    if contains_environment_profile {
        reply.result = Err(failure(
            NodeFailureCode::UnsupportedCapability,
            "child environment profiles were not negotiated",
        ));
        return;
    }
    let Ok(response) = reply.result.as_mut() else {
        return;
    };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            clear_snapshot_child_environment_profiles(snapshot);
        }
        NodeResponse::Resync { snapshot, events, .. } => {
            clear_snapshot_child_environment_profiles(snapshot);
            for event in events {
                if let NodeEvent::SessionRecordUpserted { record } = &mut event.event {
                    record.environment_profile = None;
                }
            }
        }
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::ProviderSessionIndexed { record }
        | NodeResponse::NativeSessionIndexed { record, .. }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            record.environment_profile = None;
        }
        NodeResponse::Armed { .. }
        | NodeResponse::Spawned { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::WorkspaceInspected { .. }
        | NodeResponse::HostDirectoriesBrowsed { .. }
        | NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::WorkspaceFileWritten { .. }
        | NodeResponse::WorkspaceFileCreated { .. }
        | NodeResponse::WorkspaceDirectoryCreated { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::GitDiffRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SpawnSpecAccepted { .. }
        | NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::NativeSessionsCataloged { .. }
        | NodeResponse::NativeSessionsPaged { .. }
        | NodeResponse::NativeSessionPreviewed { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackForSessionRecordExported { .. }
        | NodeResponse::ContextPackExported { .. }
        | NodeResponse::ContextPackForgotten { .. }
        | NodeResponse::DurableContextPackResolved { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceRegistered { .. }
        | NodeResponse::StandaloneWorkspaceCreated { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::WorktreeCreated { .. }
        | NodeResponse::WorktreeRemoved { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => {}
    }
}

fn clear_snapshot_session_bundles(snapshot: &mut NodeSnapshot) {
    for record in &mut snapshot.session_records {
        record.bundle = None;
    }
}

fn project_event_without_session_bundle(
    mut envelope: NodeEventEnvelope,
) -> NodeEventEnvelope {
    if let NodeEvent::SessionRecordUpserted { record } = &mut envelope.event {
        record.bundle = None;
    }
    envelope
}

fn project_response_without_session_bundle(reply: &mut ResponseEnvelope) {
    let contains_bundle = match reply.result.as_ref() {
        Ok(NodeResponse::SpawnSpecAccepted { receipt }) => receipt.bundle.is_some(),
        Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => {
            receipt.spawn.bundle.is_some()
        }
        _ => false,
    };
    if contains_bundle {
        reply.result = Err(failure(
            NodeFailureCode::UnsupportedCapability,
            "session bundles were not negotiated",
        ));
        return;
    }
    let Ok(response) = reply.result.as_mut() else {
        return;
    };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => clear_snapshot_session_bundles(snapshot),
        NodeResponse::Resync { snapshot, events, .. } => {
            clear_snapshot_session_bundles(snapshot);
            for event in events {
                if let NodeEvent::SessionRecordUpserted { record } = &mut event.event {
                    record.bundle = None;
                }
            }
        }
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::ProviderSessionIndexed { record }
        | NodeResponse::NativeSessionIndexed { record, .. }
        | NodeResponse::SessionRecordResumed { record, .. } => record.bundle = None,
        NodeResponse::Armed { .. }
        | NodeResponse::Spawned { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::WorkspaceInspected { .. }
        | NodeResponse::HostDirectoriesBrowsed { .. }
        | NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::WorkspaceFileWritten { .. }
        | NodeResponse::WorkspaceFileCreated { .. }
        | NodeResponse::WorkspaceDirectoryCreated { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::GitDiffRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SpawnSpecAccepted { .. }
        | NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::NativeSessionsCataloged { .. }
        | NodeResponse::NativeSessionsPaged { .. }
        | NodeResponse::NativeSessionPreviewed { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackForSessionRecordExported { .. }
        | NodeResponse::ContextPackExported { .. }
        | NodeResponse::ContextPackForgotten { .. }
        | NodeResponse::DurableContextPackResolved { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceRegistered { .. }
        | NodeResponse::StandaloneWorkspaceCreated { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::WorktreeCreated { .. }
        | NodeResponse::WorktreeRemoved { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => {}
    }
}

fn project_snapshot_history_for_wire(
    snapshot: &mut NodeSnapshot,
    include_history_metadata: bool,
) {
    for workspace in &mut snapshot.workspaces {
        for session in &mut workspace.sessions {
            if include_history_metadata {
                session.history.loaded = None;
            } else {
                session.history = Default::default();
            }
        }
    }
}

fn project_event_history_for_wire(
    mut envelope: NodeEventEnvelope,
    include_history_metadata: bool,
) -> NodeEventEnvelope {
    if let NodeEvent::WorkspaceAdded { workspace } = &mut envelope.event {
        for session in &mut workspace.sessions {
            if include_history_metadata {
                session.history.loaded = None;
            } else {
                session.history = Default::default();
            }
        }
    }
    envelope
}

fn project_response_history_for_wire(
    reply: &mut ResponseEnvelope,
    include_history_metadata: bool,
) {
    let Ok(response) = reply.result.as_mut() else {
        return;
    };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            project_snapshot_history_for_wire(snapshot, include_history_metadata);
        }
        NodeResponse::Resync {
            snapshot, events, ..
        } => {
            project_snapshot_history_for_wire(snapshot, include_history_metadata);
            for event in events {
                *event = project_event_history_for_wire(
                    event.clone(),
                    include_history_metadata,
                );
            }
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::StandaloneWorkspaceCreated { workspace }
        | NodeResponse::WorktreeCreated { workspace, .. } => {
            for session in &mut workspace.sessions {
                if include_history_metadata {
                    session.history.loaded = None;
                } else {
                    session.history = Default::default();
                }
            }
        }
        _ => {}
    }
}

fn clear_snapshot_context_packs(snapshot: &mut NodeSnapshot) {
    for record in &mut snapshot.session_records {
        record.context_id = None;
        record.context = None;
        record.exported_context = None;
    }
}

fn project_event_without_context_pack(
    mut envelope: NodeEventEnvelope,
) -> NodeEventEnvelope {
    if let NodeEvent::SessionRecordUpserted { record } = &mut envelope.event {
        record.context_id = None;
        record.context = None;
        record.exported_context = None;
    }
    envelope
}

fn project_response_without_context_pack(reply: &mut ResponseEnvelope) {
    let contains_context = match reply.result.as_ref() {
        Ok(NodeResponse::SpawnSpecAccepted { receipt }) => receipt.context.is_some(),
        Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => {
            receipt.spawn.context.is_some()
        }
        Ok(NodeResponse::HistoryDiscovered { .. })
        | Ok(NodeResponse::HistoryLoaded { .. })
        | Ok(NodeResponse::ContextPackForSessionRecordExported { .. })
        | Ok(NodeResponse::ContextPackExported { .. })
        | Ok(NodeResponse::ContextPackForgotten { .. })
        | Ok(NodeResponse::DurableContextPackResolved { .. }) => true,
        _ => false,
    };
    if contains_context {
        reply.result = Err(failure(
            NodeFailureCode::UnsupportedCapability,
            "history context packs were not negotiated",
        ));
        return;
    }
    let Ok(response) = reply.result.as_mut() else {
        return;
    };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => clear_snapshot_context_packs(snapshot),
        NodeResponse::Resync {
            snapshot, events, ..
        } => {
            clear_snapshot_context_packs(snapshot);
            for event in events {
                if let NodeEvent::SessionRecordUpserted { record } = &mut event.event {
                    record.context_id = None;
                    record.context = None;
                    record.exported_context = None;
                }
            }
        }
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::ProviderSessionIndexed { record }
        | NodeResponse::NativeSessionIndexed { record, .. }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            record.context_id = None;
            record.context = None;
            record.exported_context = None;
        }
        _ => {}
    }
}

fn project_response_legacy_provider_ids(shared: &NodeShared, reply: &mut ResponseEnvelope) {
    let contains_open_record = match reply.result.as_ref() {
        Ok(NodeResponse::SessionRecordUpdated { record })
        | Ok(NodeResponse::ProviderSessionIndexed { record })
        | Ok(NodeResponse::SessionRecordResumed { record, .. }) => {
            !provider_id_is_legacy(&record.provider)
        }
        Ok(NodeResponse::NativeSessionIndexed { selection, record }) => {
            !provider_id_is_legacy(&selection.route.provider)
                || !provider_id_is_legacy(&record.provider)
        }
        Ok(NodeResponse::SpawnSpecAccepted { receipt }) => {
            !provider_id_is_legacy(&receipt.provider)
        }
        Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => {
            !provider_id_is_legacy(&receipt.spawn.provider)
        }
        Ok(NodeResponse::NativeSessionsCataloged { route, .. })
        | Ok(NodeResponse::NativeSessionsPaged { route, .. }) => {
            !provider_id_is_legacy(&route.provider)
        }
        Ok(NodeResponse::NativeSessionPreviewed { selection, .. }) => {
            !provider_id_is_legacy(&selection.route.provider)
        }
        _ => false,
    };
    if contains_open_record {
        reply.result = Err(failure(
            NodeFailureCode::UnsupportedCapability,
            "open provider IDs were not negotiated",
        ));
        return;
    }
    let Ok(response) = reply.result.as_mut() else {
        return;
    };
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            project_snapshot_legacy_provider_ids(snapshot);
        }
        NodeResponse::Resync { snapshot, events, .. } => {
            project_snapshot_legacy_provider_ids(snapshot);
            *events = std::mem::take(events)
                .into_iter()
                .filter_map(|event| project_event_legacy_provider_ids(shared, event))
                .collect();
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::StandaloneWorkspaceCreated { workspace }
        | NodeResponse::WorktreeCreated { workspace, .. } => {
            workspace
                .sessions
                .retain(|session| provider_id_is_legacy(&session.agent_id));
        }
        NodeResponse::Armed { .. }
        | NodeResponse::Spawned { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::WorkspaceInspected { .. }
        | NodeResponse::HostDirectoriesBrowsed { .. }
        | NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::WorkspaceFileWritten { .. }
        | NodeResponse::WorkspaceFileCreated { .. }
        | NodeResponse::WorkspaceDirectoryCreated { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::GitDiffRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SpawnSpecAccepted { .. }
        | NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::NativeSessionsCataloged { .. }
        | NodeResponse::NativeSessionsPaged { .. }
        | NodeResponse::NativeSessionPreviewed { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackForSessionRecordExported { .. }
        | NodeResponse::ContextPackExported { .. }
        | NodeResponse::ContextPackForgotten { .. }
        | NodeResponse::DurableContextPackResolved { .. }
        | NodeResponse::SessionRecordUpdated { .. }
        | NodeResponse::ProviderSessionIndexed { .. }
        | NodeResponse::NativeSessionIndexed { .. }
        | NodeResponse::SessionRecordResumed { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::WorktreeRemoved { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => {}
    }
}

fn project_event_legacy_provider_ids(
    shared: &NodeShared,
    mut envelope: NodeEventEnvelope,
) -> Option<NodeEventEnvelope> {
    let include = match &mut envelope.event {
        NodeEvent::HarnessMcpReadCall { .. } => true,
        NodeEvent::Control { address, .. }
        | NodeEvent::Observation { address, .. }
        | NodeEvent::TerminalFrame { address, .. } => shared
            .validate_address(address)
            .is_ok_and(|provider| provider_id_is_legacy(&provider)),
        NodeEvent::ManagedObservation { .. } => false,
        NodeEvent::WorkspaceAdded { workspace } => {
            workspace
                .sessions
                .retain(|session| provider_id_is_legacy(&session.agent_id));
            true
        }
        NodeEvent::SessionRecordUpserted { record } => {
            provider_id_is_legacy(&record.provider)
        }
        NodeEvent::SessionRecordRemoved { .. } => shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .removed_record_providers
            .get(&envelope.sequence)
            .is_some_and(provider_id_is_legacy),
        NodeEvent::ControllerChanged { .. }
        | NodeEvent::ManagedWorktreeUpserted { .. }
        | NodeEvent::ManagedWorktreeRemoved { .. }
        | NodeEvent::WorkspaceRemoved { .. }
        | NodeEvent::ResyncRequired { .. } => true,
    };
    include.then_some(envelope)
}

fn clear_response_provider_runtime_status(reply: &mut ResponseEnvelope) {
    let Ok(response) = reply.result.as_mut() else {
        return;
    };
    match response {
        NodeResponse::Snapshot { snapshot, .. } | NodeResponse::Resync { snapshot, .. } => {
            snapshot.provider_runtime_statuses.clear();
        }
        NodeResponse::Armed { .. }
        | NodeResponse::Spawned { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::WorkspaceInspected { .. }
        | NodeResponse::HostDirectoriesBrowsed { .. }
        | NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::WorkspaceFileWritten { .. }
        | NodeResponse::WorkspaceFileCreated { .. }
        | NodeResponse::WorkspaceDirectoryCreated { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::GitDiffRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SpawnSpecAccepted { .. }
        | NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::NativeSessionsCataloged { .. }
        | NodeResponse::NativeSessionsPaged { .. }
        | NodeResponse::NativeSessionPreviewed { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackForSessionRecordExported { .. }
        | NodeResponse::ContextPackExported { .. }
        | NodeResponse::ContextPackForgotten { .. }
        | NodeResponse::DurableContextPackResolved { .. }
        | NodeResponse::SessionRecordUpdated { .. }
        | NodeResponse::ProviderSessionIndexed { .. }
        | NodeResponse::NativeSessionIndexed { .. }
        | NodeResponse::SessionRecordResumed { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceRegistered { .. }
        | NodeResponse::StandaloneWorkspaceCreated { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::WorktreeCreated { .. }
        | NodeResponse::WorktreeRemoved { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => {}
    }
}

struct ControllerReleaseGuard {
    shared: Arc<NodeShared>,
    connection_id: u64,
}

impl Drop for ControllerReleaseGuard {
    fn drop(&mut self) {
        self.shared.release_controller(self.connection_id);
    }
}

struct AbortTaskOnDrop(AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Reads an unsigned env-config override, following the pattern of every
/// other bounded knob in this module: missing/unparsable/zero falls back to
/// `default`, and the resolved value is always clamped to `max` so a bad
/// override cannot reopen a bound this budget exists to enforce.
fn env_config_bounded(name: &str, default: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .min(max)
}

fn workspace_inspection_time_budget_ms() -> u64 {
    env_config_bounded(
        "GATE4AGENT_NODE_WORKSPACE_INSPECTION_BUDGET_MS",
        WORKSPACE_INSPECTION_TIME_BUDGET_MS_DEFAULT,
        WORKSPACE_INSPECTION_TIME_BUDGET_MS_MAX,
    )
}

fn workspace_inspection_entry_cap() -> usize {
    env_config_bounded(
        "GATE4AGENT_NODE_WORKSPACE_INSPECTION_ENTRY_CAP",
        WORKSPACE_INSPECTION_ENTRY_CAP_DEFAULT as u64,
        WORKSPACE_INSPECTION_ENTRY_CAP_MAX as u64,
    ) as usize
}

/// Tracks the shared walk-side budget across the whole recursive walk: a
/// wall-clock deadline (shared with the git phase that runs after the walk)
/// and a cap on directory entries visited, whether or not they were pushed
/// to the response. Cheap to check per entry — one `Instant::now()` and one
/// integer compare.
struct WorkspaceWalkBudget {
    deadline: Instant,
    entry_cap: usize,
    entries_visited: u64,
    time_budget_exceeded: bool,
    entry_cap_exceeded: bool,
}

impl WorkspaceWalkBudget {
    fn new(deadline: Instant, entry_cap: usize) -> Self {
        Self {
            deadline,
            entry_cap,
            entries_visited: 0,
            time_budget_exceeded: false,
            entry_cap_exceeded: false,
        }
    }

    fn is_exhausted(&self) -> bool {
        self.time_budget_exceeded || self.entry_cap_exceeded
    }

    /// Records one visited directory entry (pushed to the response or not)
    /// and reports whether the walk must stop now.
    fn record_visit(&mut self) -> bool {
        self.entries_visited += 1;
        if self.entries_visited as usize >= self.entry_cap {
            self.entry_cap_exceeded = true;
        }
        if Instant::now() >= self.deadline {
            self.time_budget_exceeded = true;
        }
        self.is_exhausted()
    }
}

fn collect_workspace_entries(
    root: &Path,
    deadline: Instant,
    entry_cap: usize,
) -> (Vec<WorkspaceEntry>, bool, WorkspaceWalkBudget) {
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut budget = WorkspaceWalkBudget::new(deadline, entry_cap);
    walk_workspace_directory(root, None, 0, &mut entries, &mut truncated, &mut budget);
    // DFS pre-order emits `x/child` between prefix-named siblings `x` and
    // `x-y` ('-' sorts below '/'), while the bounded operator projections
    // require strict global byte order over relative paths.
    entries.sort_by(|left, right| left.relative_path.as_utf8().cmp(&right.relative_path.as_utf8()));
    (entries, truncated, budget)
}

/// Builds the additive `WorkspaceInspectionTruncationV1` for an
/// `inspect_workspace` response — `None` once the walk and git phase both
/// completed inside their shared budget.
fn workspace_inspection_truncation(
    walk_budget: &WorkspaceWalkBudget,
    git_time_budget_exceeded: bool,
    elapsed: Duration,
) -> Option<WorkspaceInspectionTruncationV1> {
    if !walk_budget.time_budget_exceeded
        && !walk_budget.entry_cap_exceeded
        && !git_time_budget_exceeded
    {
        return None;
    }
    Some(WorkspaceInspectionTruncationV1 {
        walk_time_budget_exceeded: walk_budget.time_budget_exceeded,
        walk_entry_cap_exceeded: walk_budget.entry_cap_exceeded,
        git_time_budget_exceeded,
        entries_visited: walk_budget.entries_visited,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    })
}

fn walk_workspace_directory(
    directory: &Path,
    relative_directory: Option<&RepositoryPath>,
    depth: usize,
    entries: &mut Vec<WorkspaceEntry>,
    truncated: &mut bool,
    budget: &mut WorkspaceWalkBudget,
) {
    if budget.is_exhausted() {
        *truncated = true;
        return;
    }
    let read_dir = match std::fs::read_dir(directory) {
        Ok(read_dir) => read_dir,
        Err(_) => {
            *truncated = true;
            return;
        }
    };
    let mut children = Vec::new();
    for child in read_dir {
        match child {
            Ok(child) => children.push(child),
            Err(_) => *truncated = true,
        }
    }
    children.sort_by_key(|child| child.file_name());
    for child in children {
        if budget.record_visit() {
            *truncated = true;
            return;
        }
        if entries.len() >= WORKSPACE_TREE_MAX_ENTRIES {
            *truncated = true;
            return;
        }
        let file_type = match child.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                *truncated = true;
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        let name = match child.file_name().into_string() {
            Ok(name) => name,
            Err(_) => {
                *truncated = true;
                continue;
            }
        };
        if file_type.is_dir() && is_skipped_workspace_directory(&name, &child.path()) {
            continue;
        }
        let relative_path = match relative_directory {
            None => windows_repository_path(name),
            Some(parent) => parent
                .as_utf8()
                .and_then(|parent| windows_repository_path(format!("{parent}/{name}"))),
        };
        let Some(relative_path) = relative_path else {
            *truncated = true;
            continue;
        };
        let kind = if file_type.is_dir() {
            WorkspaceEntryKind::Directory
        } else if file_type.is_file() {
            WorkspaceEntryKind::File
        } else {
            continue;
        };
        entries.push(WorkspaceEntry {
            relative_path: relative_path.clone(),
            kind,
        });
        if file_type.is_dir() {
            if depth + 1 < WORKSPACE_TREE_MAX_DEPTH {
                walk_workspace_directory(
                    &child.path(),
                    Some(&relative_path),
                    depth + 1,
                    entries,
                    truncated,
                    budget,
                );
            } else {
                *truncated = true;
            }
        }
    }
}

fn windows_repository_path(value: String) -> Option<RepositoryPath> {
    if value.contains('\\') {
        return None;
    }
    RepositoryPath::utf8(value).ok()
}

fn windows_repository_path_from_bytes(value: &[u8]) -> Option<RepositoryPath> {
    windows_repository_path(std::str::from_utf8(value).ok()?.to_owned())
}

/// Excludes a directory from the workspace tree walk. The three literal
/// names are the common convention-based cases; the `CACHEDIR.TAG` check
/// beneath them catches every build-output directory regardless of name
/// (cargo writes `CACHEDIR.TAG` into every `--target-dir` it creates, so
/// this covers `target-a2`, `target-a2-tui`, and any future renamed build
/// directory without maintaining a name heuristic list). The name check
/// runs first and returns early so a directory rejected by name never pays
/// for the extra `CACHEDIR.TAG` stat.
fn is_skipped_workspace_directory(name: &str, path: &Path) -> bool {
    if name.eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case("target")
        || name.eq_ignore_ascii_case("node_modules")
    {
        return true;
    }
    path.join("CACHEDIR.TAG").is_file()
}

fn context_pack_source_status_is_usable(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Running | SessionStatus::Exited { exit_code: Some(0) }
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ContextPackRepositoryHead {
    NotRepository,
    Unborn,
    Commit(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContextPackRepositoryObservation {
    head: ContextPackRepositoryHead,
    git: GitSnapshot,
    files: Vec<ContextPackRepositoryFileSource>,
}

fn stable_context_pack_repository(
    first: ContextPackRepositoryObservation,
    second: ContextPackRepositoryObservation,
) -> Result<ContextPackRepository, NodeFailure> {
    if first != second {
        return Err(failure(
            NodeFailureCode::ContextPackMaterializationFailed,
            "context repository changed during bounded capture",
        ));
    }
    Ok(ContextPackRepository::from_sources(
        &second.git,
        second.files,
    ))
}

/// Appends a "skipped: budget exhausted" diagnostic and returns `true` once
/// (and only once) the shared walk+git `deadline` has already passed. Every
/// remaining phase in `inspect_git_workspace` checks this immediately
/// before issuing its own git call, so a budget spent by the walk (or by an
/// earlier git call) skips the rest of the chain instead of paying for it
/// anyway.
fn git_budget_exhausted(snapshot: &mut GitSnapshot, deadline: Instant, stage: &str) -> bool {
    if Instant::now() < deadline {
        return false;
    }
    append_git_diagnostic(
        snapshot,
        &format!("{stage} skipped: workspace inspection time budget exhausted"),
    );
    true
}

/// Deadline-aware sibling of `run_git_bounded`, used only by
/// `inspect_git_workspace`'s six-call chain: caps the per-call timeout to
/// whatever remains of the shared `deadline` (never more than
/// `GIT_COMMAND_TIMEOUT_MS`), so a slow walk phase that already spent most
/// of the budget cannot still hand a full per-call timeout to every git
/// probe behind it. `inspect_git_workspace` already guards each call site
/// with `git_budget_exhausted`, so `deadline` is guaranteed not to have
/// passed yet here.
async fn run_git_bounded_with_deadline(
    root: &str,
    arguments: &[&str],
    output_limit: usize,
    deadline: Instant,
) -> io::Result<GitCommandOutput> {
    let remaining = deadline.checked_duration_since(Instant::now());
    let timeout_ms = remaining
        .and_then(|remaining| u64::try_from(remaining.as_millis()).ok())
        .unwrap_or(0)
        .clamp(1, GIT_COMMAND_TIMEOUT_MS);
    run_git_read_bounded(root, arguments, output_limit, timeout_ms).await
}

/// Runs the bounded six-call git probe chain used by `inspect_workspace`.
/// `deadline` is the same absolute instant the preceding directory walk was
/// bounded by, so time already spent walking counts against what remains
/// for git enrichment. Returns the resulting `GitSnapshot` plus whether the
/// shared budget (not an individual per-call timeout) is what cut the git
/// phase short.
async fn inspect_git_workspace(root: &str, deadline: Instant) -> (GitSnapshot, bool) {
    let mut snapshot = GitSnapshot {
        is_repository: false,
        branch: None,
        status: Vec::new(),
        recent_commits: Vec::new(),
        worktrees: Vec::new(),
        managed_worktree: None,
        truncated: false,
        diagnostic: None,
    };
    if git_budget_exhausted(&mut snapshot, deadline, "git repository probe") {
        return (snapshot, true);
    }
    let repository = match run_git_bounded_with_deadline(
        root,
        &["rev-parse", "--is-inside-work-tree"],
        4 * 1_024,
        deadline,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            append_git_diagnostic(
                &mut snapshot,
                &format!("git inspection unavailable: {error}"),
            );
            return (snapshot, false);
        }
    };
    snapshot.truncated |= repository.truncated;
    if repository.timed_out {
        append_git_diagnostic(&mut snapshot, "git repository probe timed out");
        return (snapshot, false);
    }
    if !repository.success
        || String::from_utf8_lossy(&repository.stdout).trim() != "true"
    {
        let stderr = String::from_utf8_lossy(&repository.stderr);
        if !stderr.to_ascii_lowercase().contains("not a git repository") {
            append_git_diagnostic(&mut snapshot, stderr.trim());
        }
        return (snapshot, false);
    }
    snapshot.is_repository = true;

    if git_budget_exhausted(&mut snapshot, deadline, "git branch query") {
        return (snapshot, true);
    }
    match run_git_bounded_with_deadline(root, &["branch", "--show-current"], 4 * 1_024, deadline).await {
        Ok(output) => {
            snapshot.truncated |= output.truncated;
            if output.timed_out {
                append_git_diagnostic(&mut snapshot, "git branch query timed out");
            } else if output.success {
                let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !branch.is_empty() {
                    snapshot.branch = Some(truncate_git_field(
                        &branch,
                        MAX_REPOSITORY_PATH_BYTES,
                        &mut snapshot.truncated,
                    ));
                }
            }
        }
        Err(error) => append_git_diagnostic(
            &mut snapshot,
            &format!("git branch query failed: {error}"),
        ),
    }
    if snapshot.branch.is_none() && !git_budget_exhausted(&mut snapshot, deadline, "git detached-HEAD probe") {
        if let Ok(output) = run_git_bounded_with_deadline(root, &["rev-parse", "--short", "HEAD"], 4 * 1_024, deadline).await {
            snapshot.truncated |= output.truncated;
            if output.success && !output.timed_out {
                let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !head.is_empty() {
                    snapshot.branch = Some(format!("detached:{head}"));
                }
            }
        }
    }

    if git_budget_exhausted(&mut snapshot, deadline, "git status") {
        return (snapshot, true);
    }
    match run_git_bounded_with_deadline(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--renames",
            "--untracked-files=normal",
            "--",
            ".",
        ],
        GIT_OUTPUT_MAX_BYTES,
        deadline,
    )
    .await
    {
        Ok(output) => {
            snapshot.truncated |= output.truncated;
            if output.timed_out {
                append_git_diagnostic(&mut snapshot, "git status timed out");
            } else if output.success {
                parse_git_status(&output.stdout, &mut snapshot);
            } else {
                append_git_diagnostic(
                    &mut snapshot,
                    String::from_utf8_lossy(&output.stderr).trim(),
                );
            }
        }
        Err(error) => append_git_diagnostic(
            &mut snapshot,
            &format!("git status failed: {error}"),
        ),
    }

    if git_budget_exhausted(&mut snapshot, deadline, "git log") {
        return (snapshot, true);
    }
    let log_limit = (GIT_COMMIT_MAX_ENTRIES + 1).to_string();
    match run_git_bounded_with_deadline(
        root,
        &["log", "-n", &log_limit, "--pretty=format:%H%x1f%s", "--", "."],
        16 * 1_024,
        deadline,
    )
    .await
    {
        Ok(output) => {
            snapshot.truncated |= output.truncated;
            if output.timed_out {
                append_git_diagnostic(&mut snapshot, "git log timed out");
            } else if output.success {
                parse_git_commits(&output.stdout, &mut snapshot);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.to_ascii_lowercase().contains("does not have any commits") {
                    append_git_diagnostic(&mut snapshot, stderr.trim());
                }
            }
        }
        Err(error) => append_git_diagnostic(
            &mut snapshot,
            &format!("git log failed: {error}"),
        ),
    }

    if git_budget_exhausted(&mut snapshot, deadline, "git worktree list") {
        return (snapshot, true);
    }
    match list_git_worktrees_with_deadline(root, deadline).await {
        Ok(worktrees) => {
            snapshot.worktrees = worktrees.into_iter().map(protocol_worktree).collect()
        }
        Err(error) => append_git_diagnostic(
            &mut snapshot,
            &format!("git worktree list failed: {}", error.message),
        ),
    }
    (snapshot, false)
}

fn parse_git_status(output: &[u8], snapshot: &mut GitSnapshot) {
    let mut remaining = output;
    while !remaining.is_empty() {
        if snapshot.status.len() >= GIT_STATUS_MAX_ENTRIES {
            snapshot.truncated = true;
            break;
        }
        let Some(record_end) = remaining.iter().position(|byte| *byte == 0) else {
            snapshot.truncated = true;
            break;
        };
        let record = &remaining[..record_end];
        remaining = &remaining[record_end + 1..];
        let has_previous_path = record
            .get(..2)
            .is_some_and(|status| status.iter().any(|byte| matches!(*byte, b'R' | b'C')));
        let previous_path_bytes = if has_previous_path {
            let Some(previous_end) = remaining.iter().position(|byte| *byte == 0) else {
                snapshot.truncated = true;
                break;
            };
            let previous = &remaining[..previous_end];
            remaining = &remaining[previous_end + 1..];
            Some(previous)
        } else {
            None
        };
        if record.len() < 4
            || record[2] != b' '
            || !is_git_status_code(record[0])
            || !is_git_status_code(record[1])
        {
            snapshot.truncated = true;
            continue;
        }
        let Some(path) = windows_repository_path_from_bytes(&record[3..]) else {
            snapshot.truncated = true;
            continue;
        };
        let previous_path = match previous_path_bytes {
            Some(previous) => {
                let Some(previous) = windows_repository_path_from_bytes(previous) else {
                    snapshot.truncated = true;
                    continue;
                };
                Some(previous)
            }
            None => None,
        };
        snapshot.status.push(GitStatusEntry {
            index_status: (record[0] as char).to_string(),
            worktree_status: (record[1] as char).to_string(),
            path,
            previous_path,
        });
    }
}

fn is_git_status_code(byte: u8) -> bool {
    matches!(byte, b' ' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!')
}

fn parse_git_commits(output: &[u8], snapshot: &mut GitSnapshot) {
    for line in String::from_utf8_lossy(output).lines() {
        let Some((id, summary)) = line.split_once('\u{1f}') else {
            continue;
        };
        if snapshot.recent_commits.len() >= GIT_COMMIT_MAX_ENTRIES {
            snapshot.truncated = true;
            break;
        }
        let id = truncate_git_field(id, 64, &mut snapshot.truncated);
        let summary = truncate_git_field(summary, 512, &mut snapshot.truncated);
        snapshot.recent_commits.push(GitCommitSummary { id, summary });
    }
}

fn truncate_git_field(value: &str, max_bytes: usize, truncated: &mut bool) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    *truncated = true;
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

fn append_git_diagnostic(snapshot: &mut GitSnapshot, message: &str) {
    if message.is_empty() {
        return;
    }
    let normalized = message
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect::<String>();
    let mut diagnostic = snapshot.diagnostic.take().unwrap_or_default();
    if !diagnostic.is_empty() {
        diagnostic.push_str("; ");
    }
    diagnostic.push_str(normalized.trim());
    snapshot.diagnostic = Some(truncate_git_field(
        &diagnostic,
        GIT_DIAGNOSTIC_MAX_BYTES,
        &mut snapshot.truncated,
    ));
}

async fn run_git_bounded(
    root: &str,
    arguments: &[&str],
    output_limit: usize,
) -> io::Result<GitCommandOutput> {
    run_git_read_bounded(root, arguments, output_limit, GIT_COMMAND_TIMEOUT_MS).await
}

async fn read_git_history_bounded(
    root: &str,
    path: Option<&RepositoryPath>,
    before: Option<&GitObjectId>,
    limit: u16,
) -> Result<GitHistoryPage, NodeFailure> {
    let path = path
        .map(|path| {
            path.as_utf8().ok_or_else(|| {
                failure(
                    NodeFailureCode::InvalidRepositoryPath,
                    "git history path must use UTF-8",
                )
            })
        })
        .transpose()?
        .unwrap_or(".");
    let count = (usize::from(limit) + 1).to_string();
    let start = before
        .map(|revision| format!("{}^", revision.as_str()))
        .unwrap_or_else(|| "HEAD".to_owned());
    let arguments = [
        "--no-pager",
        "log",
        "--no-decorate",
        "--date=iso-strict",
        "--pretty=format:%H%x1f%P%x1f%s%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1e",
        "-n",
        count.as_str(),
        start.as_str(),
        "--",
        path,
    ];
    let output = run_git_bounded(root, &arguments, 256 * 1_024)
        .await
        .map_err(|_| failure(NodeFailureCode::GitReadFailed, "git history command failed"))?;
    if output.timed_out {
        return Err(failure(NodeFailureCode::GitReadTimedOut, "git history exceeded its bounded deadline"));
    }
    if !output.success {
        return Err(failure(NodeFailureCode::GitReadFailed, "git history could not be read"));
    }
    let mut commits = parse_git_history(&output.stdout);
    let has_more = commits.len() > usize::from(limit) || output.truncated;
    commits.truncate(usize::from(limit));
    let next_before = has_more.then(|| commits.last().map(|commit| commit.id.clone())).flatten();
    Ok(GitHistoryPage { commits, next_before, truncated: output.truncated })
}

fn parse_git_history(output: &[u8]) -> Vec<GitCommitDetails> {
    String::from_utf8_lossy(output)
        .split('\u{1e}')
        .filter_map(|record| {
            let fields = record.trim_matches(['\r', '\n']).split('\u{1f}').collect::<Vec<_>>();
            if fields.len() != 9 {
                return None;
            }
            let id = GitObjectId::new(fields[0].to_owned()).ok()?;
            let parents = if fields[1].is_empty() {
                Vec::new()
            } else {
                fields[1]
                    .split(' ')
                    .map(|value| GitObjectId::new(value.to_owned()))
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?
            };
            Some(GitCommitDetails {
                id,
                parents,
                subject: bounded_git_text(fields[2], 512),
                author_name: bounded_git_text(fields[3], 256),
                author_email: bounded_git_text(fields[4], 320),
                authored_at: bounded_git_text(fields[5], 64),
                committer_name: bounded_git_text(fields[6], 256),
                committer_email: bounded_git_text(fields[7], 320),
                committed_at: bounded_git_text(fields[8], 64),
                signature_status: GitSignatureStatus::NoSignature,
                signer: None,
            })
        })
        .take(usize::from(MAX_GIT_HISTORY_COMMITS) + 1)
        .collect()
}

fn bounded_git_text(value: &str, max_bytes: usize) -> String {
    let normalized = value
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect::<String>();
    if normalized.len() <= max_bytes {
        return normalized;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !normalized.is_char_boundary(boundary) {
        boundary -= 1;
    }
    normalized[..boundary].to_owned()
}

async fn read_git_diff_bounded(root: &str, request: GitDiffRequest) -> Result<GitDiff, NodeFailure> {
    let path = request.path.as_ref().map(|path| {
        path.as_utf8()
            .ok_or_else(|| failure(NodeFailureCode::InvalidRepositoryPath, "git diff path must use UTF-8"))
    }).transpose()?;
    let mut arguments = match &request.mode {
        GitDiffMode::Working => vec!["--no-pager", "-c", "diff.external=", "diff", "--no-ext-diff", "--no-textconv"],
        GitDiffMode::Staged => vec!["--no-pager", "-c", "diff.external=", "diff", "--cached", "--no-ext-diff", "--no-textconv"],
        GitDiffMode::Commit { .. } => vec!["--no-pager", "-c", "diff.external=", "show", "--format=fuller", "--no-ext-diff", "--no-textconv"],
    };
    if let GitDiffMode::Commit { revision } = &request.mode {
        arguments.push(revision.as_str());
    }
    arguments.push("--");
    if let Some(path) = path {
        arguments.push(path);
    }
    let output = run_git_bounded(root, &arguments, MAX_GIT_DIFF_BYTES)
        .await
        .map_err(|_| failure(NodeFailureCode::GitReadFailed, "git diff command failed"))?;
    if output.timed_out {
        return Err(failure(NodeFailureCode::GitReadTimedOut, "git diff exceeded its bounded deadline"));
    }
    if !output.success {
        return Err(failure(NodeFailureCode::GitReadFailed, "git diff could not be read"));
    }
    Ok(GitDiff {
        mode: request.mode,
        path: request.path,
        text: String::from_utf8_lossy(&output.stdout).into_owned(),
        truncated: output.truncated,
    })
}

async fn process_request(shared: &NodeShared, connection_id: u64, role: ClientRole, envelope: RequestEnvelope) -> ResponseEnvelope {
    let result = process_request_inner(shared, connection_id, role, envelope.request).await;
    ResponseEnvelope { request_id: envelope.request_id, result }
}

async fn process_request_inner(shared: &NodeShared, connection_id: u64, role: ClientRole, request: NodeRequest) -> Result<NodeResponse, NodeFailure> {
    if !request.harness_mcp_contract_is_valid_at(unix_time_ms()) {
        return Err(failure(NodeFailureCode::InvalidRequest, "harness MCP request is invalid or stale"));
    }
    let read_only = matches!(
        &request,
        NodeRequest::Snapshot
            | NodeRequest::Resync { .. }
            | NodeRequest::BrowseHostDirectories { .. }
            | NodeRequest::InspectWorkspace { .. }
            | NodeRequest::ReadWorkspaceFile { .. }
            | NodeRequest::ReadGitHistory { .. }
            | NodeRequest::ReadGitDiff { .. }
            | NodeRequest::CatalogNativeSessions { .. }
            | NodeRequest::PageNativeSessions { .. }
            | NodeRequest::PreviewNativeSession { .. }
            | NodeRequest::PreviewSessionRecord { .. }
    );
    let _mutation_guard = if read_only {
        None
    } else {
        let guard = shared.mutation_gate.lock().await;
        if shared.shutdown.load(Ordering::Acquire) {
            return Err(failure(NodeFailureCode::ShuttingDown, "node shutdown has begun"));
        }
        Some(guard)
    };
    match request {
        NodeRequest::Snapshot => Ok(NodeResponse::Snapshot {
            event_sequence: shared.current_sequence(),
            controller: shared.controller_state(),
            snapshot: shared.snapshot(),
        }),
        NodeRequest::Resync { after_sequence } => Ok(shared.resync(after_sequence)),
        NodeRequest::ArmHarnessMcpReservation {
            reservation_id,
            activation_digest,
            spawn_spec,
            expires_at_unix_ms,
        } => {
            shared.require_controller(connection_id, role)?;
            let registry = shared.harness_mcp_registry.as_ref().ok_or_else(|| {
                failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP proxy is not configured")
            })?;
            let expires_at_unix_ms = registry
                .arm(reservation_id.clone(), activation_digest.clone(), spawn_spec, expires_at_unix_ms)
                .await
                .map_err(harness_mcp_failure)?;
            Ok(NodeResponse::Armed { reservation_id, activation_digest, expires_at_unix_ms })
        }
        NodeRequest::SpawnSpecWithHarnessMcp {
            reservation_id,
            activation_digest,
            spec,
            deadline_unix_ms,
        } => {
            shared.require_controller(connection_id, role)?;
            let receipt = shared
                .spawn_from_spec_with_harness_mcp(
                    reservation_id.clone(), activation_digest.clone(), spec, deadline_unix_ms,
                )
                .await?;
            Ok(NodeResponse::Spawned { reservation_id, activation_digest, receipt })
        }
        NodeRequest::ActivateHarnessMcpReservation {
            reservation_id,
            activation_digest,
            record_id,
            session,
        } => {
            shared.require_controller(connection_id, role)?;
            let provider_root_pid = shared.handle.snapshot().sessions.iter().find(|current| {
                current.instance_id == session.session.instance_id
                    && current.generation == session.session.generation
            }).and_then(|current| current.process_id).ok_or_else(|| {
                failure(NodeFailureCode::BindingMismatch, "provider root process is unavailable")
            })?;
            shared.harness_mcp_registry.as_ref().ok_or_else(|| {
                failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP proxy is not configured")
            })?.activate(
                &reservation_id, &activation_digest, &record_id, &session, provider_root_pid,
            ).map_err(harness_mcp_failure)?;
            Ok(NodeResponse::Activated { reservation_id, activation_digest, record_id, session })
        }
        NodeRequest::AbortHarnessMcpReservation { reservation_id, activation_digest } => {
            shared.require_controller(connection_id, role)?;
            let instance_id = shared.harness_mcp_registry.as_ref().ok_or_else(|| {
                failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP proxy is not configured")
            })?.abort(&reservation_id, &activation_digest).map_err(harness_mcp_failure)?;
            if let (Some(control), Some(instance_id)) =
                (shared.native_launch_profile_control.as_ref(), instance_id)
            {
                control.clear_native_harness_mcp_launch_overlay(instance_id);
            }
            Ok(NodeResponse::Aborted { reservation_id, activation_digest })
        }
        NodeRequest::PutHarnessMcpReplyChunk {
            reservation_id, activation_digest, record_id, session, call_id,
            offset, final_chunk, chunk_hex,
        } => {
            shared.require_controller(connection_id, role)?;
            let (next_offset, completed) = shared.harness_mcp_registry.as_ref().ok_or_else(|| {
                failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP proxy is not configured")
            })?.put_reply_chunk(
                &reservation_id, &activation_digest, &record_id, &session, &call_id,
                offset, final_chunk, &chunk_hex,
            ).map_err(harness_mcp_failure)?;
            Ok(NodeResponse::ReplyChunkAccepted {
                reservation_id, activation_digest, record_id, session, call_id,
                next_offset, completed,
            })
        }
        NodeRequest::RejectHarnessMcpCall {
            reservation_id, activation_digest, record_id, session, call_id, reason,
        } => {
            shared.require_controller(connection_id, role)?;
            shared.harness_mcp_registry.as_ref().ok_or_else(|| {
                failure(NodeFailureCode::HarnessMcpUnavailable, "harness MCP proxy is not configured")
            })?.reject_call(
                &reservation_id, &activation_digest, &record_id, &session, &call_id, reason,
            ).map_err(harness_mcp_failure)?;
            Ok(NodeResponse::CallRejected {
                reservation_id, activation_digest, record_id, session, call_id,
            })
        }
        NodeRequest::BeginDeliveryStage { manifest } => {
            shared.require_controller(connection_id, role)?;
            shared.begin_delivery_stage(manifest)
        }
        NodeRequest::PutDeliveryBlobChunk {
            stage_id,
            blob_digest,
            offset,
            chunk_hex,
        } => {
            shared.require_controller(connection_id, role)?;
            shared.put_delivery_blob_chunk(stage_id, blob_digest, offset, chunk_hex)
        }
        NodeRequest::CommitDeliveryStage { stage_id } => {
            shared.require_controller(connection_id, role)?;
            let receipt = shared.commit_delivery_stage(stage_id)?;
            Ok(NodeResponse::DeliveryCommitted { receipt })
        }
        NodeRequest::AbortDeliveryStage { stage_id } => {
            shared.require_controller(connection_id, role)?;
            shared.abort_delivery_stage(&stage_id)?;
            Ok(NodeResponse::DeliveryStageAborted { stage_id })
        }
        NodeRequest::BrowseHostDirectories { directory, after } => {
            let listing = shared.browse_host_directories(directory, after).await?;
            Ok(NodeResponse::HostDirectoriesBrowsed { listing })
        }
        NodeRequest::InspectWorkspace { workspace_id } => {
            let inspection = shared.inspect_workspace(workspace_id).await?;
            Ok(NodeResponse::WorkspaceInspected { inspection })
        }
        NodeRequest::ReadWorkspaceFile { workspace_id, path } => {
            let file = shared.read_workspace_file(workspace_id, path).await?;
            Ok(NodeResponse::WorkspaceFileRead { file })
        }
        NodeRequest::WriteWorkspaceFile { workspace_id, path, expected_revision, text } => {
            shared.require_controller(connection_id, role)?;
            let file = shared
                .write_workspace_file(workspace_id, path, expected_revision, text)
                .await?;
            Ok(NodeResponse::WorkspaceFileWritten { file })
        }
        NodeRequest::CreateWorkspaceFile { workspace_id, path } => {
            shared.require_controller(connection_id, role)?;
            let file = shared.create_workspace_file(workspace_id, path).await?;
            Ok(NodeResponse::WorkspaceFileCreated { file })
        }
        NodeRequest::CreateWorkspaceDirectory { workspace_id, path } => {
            shared.require_controller(connection_id, role)?;
            let entry = shared
                .create_workspace_directory(workspace_id.clone(), path)
                .await?;
            Ok(NodeResponse::WorkspaceDirectoryCreated { workspace_id, entry })
        }
        NodeRequest::ReadGitHistory { workspace_id, path, before, limit } => {
            if role != ClientRole::Operator {
                return Err(failure(NodeFailureCode::ObserverReadOnly, "git history requires an operator connection"));
            }
            if !(1..=MAX_GIT_HISTORY_COMMITS).contains(&limit) {
                return Err(failure(NodeFailureCode::InvalidRequest, "git history limit is invalid"));
            }
            let page = shared
                .read_git_history(workspace_id.clone(), path, before, limit)
                .await?;
            Ok(NodeResponse::GitHistoryRead { workspace_id, page })
        }
        NodeRequest::ReadGitDiff { workspace_id, request } => {
            if role != ClientRole::Operator {
                return Err(failure(NodeFailureCode::ObserverReadOnly, "git diff requires an operator connection"));
            }
            let diff = shared.read_git_diff(workspace_id.clone(), request).await?;
            Ok(NodeResponse::GitDiffRead { workspace_id, diff })
        }
        NodeRequest::CatalogNativeSessions { route, limit } => {
            if role != ClientRole::Operator {
                return Err(failure(
                    NodeFailureCode::ObserverReadOnly,
                    "native session catalog requires an operator connection",
                ));
            }
            if !(1..=gate4agent_types::NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(&limit) {
                return Err(failure(
                    NodeFailureCode::InvalidRequest,
                    "native session catalog limit is invalid",
                ));
            }
            let (entries, summary) = shared
                .catalog_native_sessions(route.clone(), limit)
                .await?;
            Ok(NodeResponse::NativeSessionsCataloged {
                route,
                entries,
                summary: Some(summary),
            })
        }
        NodeRequest::PageNativeSessions {
            route,
            window,
            catalog_revision,
            recent_cutoff_unix_ms,
            after_selection_id,
            limit,
        } => {
            if role != ClientRole::Operator {
                return Err(failure(
                    NodeFailureCode::ObserverReadOnly,
                    "native session catalog paging requires an operator connection",
                ));
            }
            if !(1..=gate4agent_types::NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(&limit)
                || after_selection_id
                    .as_deref()
                    .is_some_and(|cursor| validate_candidate_id(cursor).is_err())
            {
                return Err(failure(
                    NodeFailureCode::InvalidRequest,
                    "native session catalog page request is invalid",
                ));
            }
            let page = shared
                .page_native_sessions(
                    route.clone(),
                    window,
                    catalog_revision,
                    recent_cutoff_unix_ms,
                    after_selection_id,
                    limit,
                )
                .await?;
            Ok(NodeResponse::NativeSessionsPaged {
                route,
                page,
            })
        }
        NodeRequest::PreviewNativeSession {
            selection,
            message_limit,
        } => {
            if role != ClientRole::Operator {
                return Err(failure(
                    NodeFailureCode::ObserverReadOnly,
                    "native session preview requires an operator connection",
                ));
            }
            if !(1..=gate4agent_types::NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
                .contains(&message_limit)
                || selection.validate().is_err()
            {
                return Err(failure(
                    NodeFailureCode::InvalidRequest,
                    "native session preview request is invalid",
                ));
            }
            let preview = shared
                .preview_native_session(selection.clone(), message_limit)
                .await?;
            Ok(NodeResponse::NativeSessionPreviewed {
                selection,
                preview,
            })
        }
        NodeRequest::IndexNativeSession {
            selection,
            display_name,
        } => {
            shared.require_controller(connection_id, role)?;
            let record = shared
                .index_native_session(selection.clone(), display_name)
                .await?;
            Ok(NodeResponse::NativeSessionIndexed { selection, record })
        }
        NodeRequest::PreviewSessionRecord {
            record_id,
            message_limit,
        } => {
            if role != ClientRole::Operator {
                return Err(failure(
                    NodeFailureCode::ObserverReadOnly,
                    "native session preview requires an operator connection",
                ));
            }
            if !(1..=gate4agent_types::NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
                .contains(&message_limit)
            {
                return Err(failure(
                    NodeFailureCode::InvalidRequest,
                    "native session preview request is invalid",
                ));
            }
            let record = shared.record(&record_id)?;
            let working_directory = shared.workspace_root(&record.workspace_id)?;
            if !platform::roots_equal(
                &working_directory,
                windows_path_text(&record.canonical_root),
            ) {
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    "managed session workspace root changed; refusing to preview another directory",
                ));
            }
            let identity = record.provider_session.clone().ok_or_else(|| {
                failure(
                    NodeFailureCode::BackendOperationFailed,
                    "native session preview is unavailable for this session record",
                )
            })?;
            if identity.key != ProviderSessionKey::SessionId {
                return Err(failure(
                    NodeFailureCode::BackendOperationFailed,
                    "native session preview is unavailable for this session identity",
                ));
            }
            let preview = shared
                .preview_native_session_by_session_id(
                    record.workspace_id.clone(), record.provider.clone(), identity.id.clone(), message_limit,
                )
                .await?;
            if preview.session_id != identity.id {
                return Err(failure(
                    NodeFailureCode::SessionRecordConflict,
                    "native session preview identity changed while loading",
                ));
            }
            shared.revalidate_session_record_preview(&record, &identity)?;
            let preview: gate4agent_types::SessionRecordPreview = preview.into();
            for observation in history_summary_observations(&preview) {
                if observation.validate().is_ok() {
                    shared.publish(NodeEvent::ManagedObservation {
                        record_id: record_id.clone(),
                        observation,
                    });
                }
            }
            Ok(NodeResponse::SessionRecordPreviewed {
                record_id,
                preview,
            })
        }
        NodeRequest::AcquireController { lease_ms } => {
            let controller = shared.acquire_controller(connection_id, role, lease_ms)?;
            Ok(NodeResponse::Controller { controller: Some(controller) })
        }
        NodeRequest::ReleaseController => {
            shared.release_controller(connection_id);
            Ok(NodeResponse::Controller { controller: shared.controller_state() })
        }
        NodeRequest::RegisterWorkspace { workspace_id, root } => {
            shared.require_controller(connection_id, role)?;
            let root = require_windows_path(root)?;
            shared.reject_managed_reservation(Some(&workspace_id), Some(&root))?;
            let workspace = shared.register_workspace(workspace_id, root).await?;
            Ok(NodeResponse::WorkspaceRegistered { workspace })
        }
        NodeRequest::CreateStandaloneWorkspace {
            workspace_id,
            root,
            initial_branch,
        } => {
            shared.require_controller(connection_id, role)?;
            let root = require_windows_path(root)?;
            shared.reject_managed_reservation(Some(&workspace_id), Some(&root))?;
            let workspace = shared
                .create_standalone_workspace(workspace_id, root, initial_branch)
                .await?;
            Ok(NodeResponse::StandaloneWorkspaceCreated { workspace })
        }
        NodeRequest::UnregisterWorkspace { workspace_id } => {
            shared.require_controller(connection_id, role)?;
            shared.reject_managed_reservation(Some(&workspace_id), None)?;
            shared.unregister_workspace(&workspace_id)?;
            Ok(NodeResponse::WorkspaceUnregistered { workspace_id })
        }
        NodeRequest::CreateWorktree {
            source_workspace_id,
            workspace_id,
            target_root,
            branch,
            base,
        } => {
            shared.require_controller(connection_id, role)?;
            let target_root = require_windows_path(target_root)?;
            shared.reject_managed_reservation(Some(&workspace_id), Some(&target_root))?;
            let (worktree, workspace) = shared
                .create_worktree(
                    source_workspace_id,
                    workspace_id,
                    target_root,
                    branch,
                    base,
                )
                .await?;
            Ok(NodeResponse::WorktreeCreated { worktree, workspace })
        }
        NodeRequest::RemoveWorktree {
            source_workspace_id,
            target_root,
        } => {
            shared.require_controller(connection_id, role)?;
            let native_target_root = require_windows_path(target_root.clone())?;
            shared.reject_managed_reservation(None, Some(&native_target_root))?;
            let workspace_id = shared
                .remove_worktree(source_workspace_id, native_target_root)
                .await?;
            Ok(NodeResponse::WorktreeRemoved {
                target_root,
                workspace_id,
            })
        }
        NodeRequest::Spawn { workspace_id, provider, mode, terminal_size, initial_prompt } => {
            shared.require_controller(connection_id, role)?;
            if !terminal_size.is_valid() {
                return Err(failure(NodeFailureCode::InvalidRequest, "spawn requires a valid terminal size"));
            }
            if let Some(prompt) = initial_prompt.as_deref() {
                validate_node_text("spawn initial prompt", prompt)?;
            }
            let session = shared
                .spawn_session(workspace_id, provider, mode, terminal_size, initial_prompt)
                .await?;
            Ok(NodeResponse::SpawnAccepted { session })
        }
        NodeRequest::SpawnSpec { spec } => {
            shared.require_controller(connection_id, role)?;
            let receipt = shared.spawn_from_spec(spec).await?;
            Ok(NodeResponse::SpawnSpecAccepted { receipt })
        }
        NodeRequest::SpawnManagedWorktree { request } => {
            shared.require_controller(connection_id, role)?;
            let receipt = shared.spawn_managed_worktree(request).await?;
            Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt })
        }
        NodeRequest::SpawnManagedWorktreeV2 { request } => {
            shared.require_controller(connection_id, role)?;
            let receipt = shared.spawn_managed_worktree_v2(request).await?;
            Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt })
        }
        NodeRequest::CleanupManagedWorktree { lease_id } => {
            shared.require_controller(connection_id, role)?;
            let lease = shared.cleanup_managed_worktree(&lease_id, true).await?;
            Ok(NodeResponse::ManagedWorktreeCleanup { lease })
        }
        NodeRequest::DiscoverHistory { session, limit } => {
            controlled_session(shared, connection_id, role, &session)?;
            let candidates = shared.discover_history(&session, limit).await?;
            Ok(NodeResponse::HistoryDiscovered {
                session,
                candidates,
            })
        }
        NodeRequest::LoadHistory {
            session,
            candidate_id,
        } => {
            controlled_session(shared, connection_id, role, &session)?;
            let loaded = shared
                .load_history(&session, candidate_id)
                .await?;
            Ok(NodeResponse::HistoryLoaded {
                session,
                session_id: loaded.session_id,
                message_count: loaded.message_count,
                completed_turn_count: loaded.completed_turn_count,
            })
        }
        NodeRequest::ExportContextPackForSessionRecord { record_id, session } => {
            shared.require_controller(connection_id, role)?;
            let context = shared
                .export_context_pack_for_session_record(&record_id, &session, false)
                .await?;
            Ok(NodeResponse::ContextPackForSessionRecordExported {
                record_id,
                session,
                context,
            })
        }
        NodeRequest::ExportContextPack { session } => {
            controlled_session(shared, connection_id, role, &session)?;
            let context = shared.export_context_pack(&session).await?;
            Ok(NodeResponse::ContextPackExported { context })
        }
        NodeRequest::ForgetContextPack { context_id } => {
            shared.require_controller(connection_id, role)?;
            shared.forget_context_pack(&context_id)?;
            Ok(NodeResponse::ContextPackForgotten { context_id })
        }
        NodeRequest::ResolveDurableContextPack { context_id } => {
            // Read-only, idempotent, no controller lease required: this lets
            // a caller preflight "is this durably exported pack still here"
            // without holding the Node's mutation controller.
            let context = shared
                .context_catalog
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&context_id)
                .map(|pack| pack.receipt().clone())
                .ok_or_else(|| {
                    failure(
                        NodeFailureCode::UnknownContextPack,
                        "context pack is not present",
                    )
                })?;
            Ok(NodeResponse::DurableContextPackResolved { context })
        }
        NodeRequest::Resume { session, terminal_size, initial_prompt } => {
            let provider = controlled_session(shared, connection_id, role, &session)?;
            if let Some(prompt) = initial_prompt.as_deref() {
                validate_node_text("resume initial prompt", prompt)?;
            }
            let runtime_requirement = if initial_prompt.is_some() {
                ProviderRuntimeRequirement::ResumeWithPrompt
            } else {
                ProviderRuntimeRequirement::Resume
            };
            shared.require_session_runtime_policy(&session, runtime_requirement)?;
            let runtime_policy = shared
                .admit_provider_runtime(&provider, runtime_requirement)
                .await?;
            let working_directory = shared.bound_workspace_root(&session)?;
            let request = ResumeLaunchRequest {
                working_directory,
                terminal_size,
                initial_prompt,
            };
            request
                .validate()
                .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
            let command = shared.prepare_command(ControlCommand::Resume {
                instance_id: session.session.instance_id,
                target: ResumeTarget::CurrentProvider,
                runtime_policy,
                request,
            });
            shared.arm_resume(&session, command.id, runtime_policy)?;
            let dispatch = shared.dispatch_envelope(command);
            if let Err(error) = dispatch {
                shared.clear_armed_resume(&session);
                return Err(error);
            }
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::RenameSessionRecord {
            record_id,
            display_name,
        } => {
            shared.require_controller(connection_id, role)?;
            let record = shared.rename_session_record(&record_id, display_name)?;
            Ok(NodeResponse::SessionRecordUpdated { record })
        }
        NodeRequest::SetSessionTask {
            record_id,
            expected_revision,
            target,
        } => {
            shared.require_controller(connection_id, role)?;
            let record = shared.set_session_task(&record_id, expected_revision, target)?;
            Ok(NodeResponse::SessionRecordUpdated { record })
        }
        NodeRequest::IndexProviderSession {
            workspace_id,
            provider,
            identity,
            display_name,
        } => {
            shared.require_controller(connection_id, role)?;
            let record = shared.index_provider_session(
                workspace_id,
                provider,
                identity,
                display_name,
            )?;
            Ok(NodeResponse::ProviderSessionIndexed { record })
        }
        NodeRequest::ResumeSessionRecord {
            record_id,
            terminal_size,
            initial_prompt,
        } => {
            shared.require_controller(connection_id, role)?;
            if let Some(prompt) = initial_prompt.as_deref() {
                validate_node_text("managed resume initial prompt", prompt)?;
            }
            let (record, session) = shared
                .resume_session_record(
                    &record_id,
                    terminal_size,
                    initial_prompt,
                )
                .await?;
            Ok(NodeResponse::SessionRecordResumed { record, session })
        }
        NodeRequest::ForgetSessionRecord { record_id } => {
            shared.require_controller(connection_id, role)?;
            shared.forget_session_record(&record_id).await?;
            Ok(NodeResponse::SessionRecordForgotten { record_id })
        }
        NodeRequest::Prompt { session, text } => {
            let agent_id = controlled_session(shared, connection_id, role, &session)?;
            validate_node_text("prompt", &text)?;
            shared.require_session_runtime_policy(
                &session,
                ProviderRuntimeRequirement::SemanticPrompt,
            )?;
            let framing = prompt_framing(&agent_id);
            shared
                .dispatch_input_bounded(
                    &session,
                    InputAction::SubmitPrompt(PromptPayload { text, framing }),
                )
                .await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Paste { session, text } => {
            controlled_session(shared, connection_id, role, &session)?;
            validate_node_text("paste", &text)?;
            shared.require_session_runtime_policy(
                &session,
                ProviderRuntimeRequirement::SemanticPrompt,
            )?;
            shared
                .dispatch_input_bounded(
                    &session,
                    InputAction::InsertDraft(PromptPayload {
                        text,
                        framing: PromptFraming::BracketedPaste,
                    }),
                )
                .await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Input { session, text } => {
            controlled_session(shared, connection_id, role, &session)?;
            validate_node_text("terminal input", &text)?;
            shared
                .dispatch_input_bounded(
                    &session,
                    InputAction::TerminalText(TerminalText { text }),
                )
                .await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::TerminalBytes { session, bytes } => {
            controlled_session(shared, connection_id, role, &session)?;
            let action = terminal_bytes_action(bytes)?;
            shared
                .dispatch_input_bounded(&session, action)
                .await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::TerminalControl { session, control } => {
            controlled_session(shared, connection_id, role, &session)?;
            shared
                .dispatch_input_bounded(&session, InputAction::TerminalControl(control))
                .await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Resize { session, size } => {
            controlled_session(shared, connection_id, role, &session)?;
            if !size.is_valid() {
                return Err(failure(NodeFailureCode::InvalidRequest, "terminal size is invalid"));
            }
            shared.dispatch_resize_bounded(&session, size).await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Interrupt { session } => {
            controlled_session(shared, connection_id, role, &session)?;
            shared
                .dispatch_input_bounded(
                    &session,
                    InputAction::TerminalControl(TerminalControl::Interrupt),
                )
                .await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Stop { session, force } => {
            controlled_session(shared, connection_id, role, &session)?;
            shared.dispatch(ControlCommand::Stop { instance_id: session.session.instance_id, force })?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Remove { session } => {
            controlled_session(shared, connection_id, role, &session)?;
            shared.remove_session(&session).await?;
            Ok(NodeResponse::Accepted)
        }
        NodeRequest::Shutdown => {
            shared.require_controller(connection_id, role)?;
            shared.begin_shutdown_locked().await?;
            Ok(NodeResponse::ShuttingDown)
        }
    }
}

fn controlled_session(shared: &NodeShared, connection_id: u64, role: ClientRole, session: &SessionAddress) -> Result<AgentId, NodeFailure> {
    shared.require_controller(connection_id, role)?;
    shared.validate_address(session)
}

fn unique_history_candidate_for_provider_session(
    candidates: &[HistoryCandidateSummary],
    provider_session_id: &str,
) -> Result<String, NodeFailure> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate.session_id_hint == provider_session_id);
    let candidate = matches.next().ok_or_else(|| {
        failure(
            NodeFailureCode::SessionRecordConflict,
            "no history candidate matches the managed provider session",
        )
    })?;
    if matches.next().is_some() {
        return Err(failure(
            NodeFailureCode::SessionRecordConflict,
            "history identity is ambiguous for the managed provider session",
        ));
    }
    Ok(candidate.id.clone())
}

fn resolved_spawn_profile_summary(
    profile: &SpawnProfileDefaults,
    environment_profiles: &BTreeMap<SpawnEnvironmentProfileId, EnvironmentProfileBinding>,
) -> Option<SpawnProfileSummary> {
    let environment_profile = match profile.environment_profile_id.as_ref() {
        None => None,
        Some(profile_id) => {
            let binding = environment_profiles.get(profile_id)?;
            if binding.provider != profile.provider
                || binding.native_profile_id(profile.mode).is_none()
            {
                return None;
            }
            Some(ResolvedEnvironmentProfileReceipt {
                profile_id: binding.id.clone(),
                profile_revision: binding.revision.clone(),
            })
        }
    };
    Some(SpawnProfileSummary {
        id: profile.profile_id.clone(),
        revision: profile.revision.clone(),
        environment_profile,
    })
}

/// True if `current` is an acceptable version of `expected` at export
/// revalidation/commit time.
///
/// When `allow_clean_detachment` is `false` (the on-demand,
/// `NodeRequest::ExportContextPackForSessionRecord` API path), `current` must
/// be byte-identical to `expected` and still actively bound to `session` —
/// the original, unchanged invariant.
///
/// When `true` (the reactive auto-export-at-exit path, `§2.1`), `current` may
/// ALSO be the exact Live -> Dormant transition that the same clean exit this
/// export reacts to produces on its own: `state` Live -> Dormant,
/// `active_session` `Some(session)` -> `None`, `updated_at_unix_ms`
/// advancing. Every other field — including `provider_session`, `context`,
/// `bundle`, `task_binding` — must still match exactly. A record that changed
/// in any other way (rebind, a different active session, an identity
/// conflict, capacity eviction) is rejected either way; this widens nothing
/// about which sessions are eligible, only how the record's own expected
/// detachment is tolerated mid-export.
fn session_record_export_target_matches(
    expected: &ManagedSessionRecord,
    current: &ManagedSessionRecord,
    session: &SessionAddress,
    allow_clean_detachment: bool,
) -> bool {
    if current == expected && current.active_session.as_ref() == Some(session) {
        return current.state == ManagedSessionState::Live;
    }
    allow_clean_detachment
        && current.state == ManagedSessionState::Dormant
        && current.active_session.is_none()
        && current.record_id == expected.record_id
        && current.display_name == expected.display_name
        && current.provider == expected.provider
        && current.mode == expected.mode
        && current.workspace_id == expected.workspace_id
        && current.canonical_root == expected.canonical_root
        && current.provider_session == expected.provider_session
        && current.environment_profile == expected.environment_profile
        && current.bundle == expected.bundle
        && current.context_id == expected.context_id
        && current.context == expected.context
        && current.exported_context == expected.exported_context
        && current.task_binding == expected.task_binding
        && current.created_at_unix_ms == expected.created_at_unix_ms
        && current.last_error == expected.last_error
}

fn session_record_context_export_binding_is_exact(
    expected: &ManagedSessionRecord,
    current: &ManagedSessionRecord,
    identity: &ProviderSessionIdentity,
    session: &SessionAddress,
    binding: &SessionBinding,
    provider: &AgentId,
    current_root: &str,
    allow_clean_detachment: bool,
) -> bool {
    session_record_export_target_matches(expected, current, session, allow_clean_detachment)
        && current.provider_session.as_ref() == Some(identity)
        && current.workspace_id == session.workspace_id
        && provider == &current.provider
        && binding.workspace_id == session.workspace_id
        && binding.generation == session.session.generation
        && binding.record_id.as_ref() == Some(&current.record_id)
        && platform::roots_equal(
            current_root,
            windows_path_text(&current.canonical_root),
        )
}

fn session_record_context_export_source_is_usable(
    record: &ManagedSessionRecord,
    status: &SessionStatus,
    allow_clean_detachment: bool,
) -> bool {
    (record.state == ManagedSessionState::Live
        || (allow_clean_detachment && record.state == ManagedSessionState::Dormant))
        && context_pack_source_status_is_usable(status)
}

fn prompt_framing(_agent_id: &AgentId) -> PromptFraming {
    PromptFraming::BracketedPaste
}

fn validate_node_text(field: &str, text: &str) -> Result<(), NodeFailure> {
    if text.len() > MAX_NODE_TEXT_BYTES {
        return Err(failure(
            NodeFailureCode::InvalidRequest,
            &format!(
                "{field} length {} exceeds the {MAX_NODE_TEXT_BYTES}-byte node limit",
                text.len(),
            ),
        ));
    }
    Ok(())
}

fn validate_terminal_bytes(bytes: &[u8]) -> Result<(), NodeFailure> {
    if bytes.is_empty() || bytes.len() > MAX_NODE_TERMINAL_BYTES {
        return Err(failure(
            NodeFailureCode::InvalidRequest,
            &format!(
                "terminal byte sequence must contain 1..={MAX_NODE_TERMINAL_BYTES} bytes; received {}",
                bytes.len(),
            ),
        ));
    }
    Ok(())
}

fn terminal_bytes_action(bytes: Vec<u8>) -> Result<InputAction, NodeFailure> {
    validate_terminal_bytes(&bytes)?;
    Ok(InputAction::TerminalBytes(bytes))
}

fn validate_workspace_request_root(root: &str) -> Result<(), NodeFailure> {
    if root.is_empty()
        || root.len() > MAX_WORKSPACE_ROOT_BYTES
        || root.chars().any(char::is_control)
    {
        return Err(failure(
            NodeFailureCode::InvalidWorkspaceRoot,
            &format!(
                "workspace root must contain 1..={MAX_WORKSPACE_ROOT_BYTES} bytes and no control characters",
            ),
        ));
    }
    Ok(())
}

fn canonical_standalone_workspace_candidate(
    workspace_id: &WorkspaceId,
    root: &str,
) -> Result<String, NodeServerError> {
    let path = Path::new(root);
    match std::fs::symlink_metadata(path) {
        Ok(_) => WorkspaceConfig::new(workspace_id.clone(), path)
            .map(|workspace| workspace.canonical_root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !path.is_absolute() {
                return Err(NodeServerError::InvalidWorkspaceRoot {
                    workspace_id: workspace_id.clone(),
                    path: root.to_owned(),
                    message: "path is not absolute".to_owned(),
                });
            }
            let name = path.file_name().ok_or_else(|| NodeServerError::InvalidWorkspaceRoot {
                workspace_id: workspace_id.clone(),
                path: root.to_owned(),
                message: "path has no standalone directory name".to_owned(),
            })?;
            let parent = path.parent().ok_or_else(|| NodeServerError::InvalidWorkspaceRoot {
                workspace_id: workspace_id.clone(),
                path: root.to_owned(),
                message: "path has no parent directory".to_owned(),
            })?;
            let canonical = std::fs::canonicalize(parent)
                .map(|parent| parent.join(name))
                .map_err(|error| NodeServerError::InvalidWorkspaceRoot {
                    workspace_id: workspace_id.clone(),
                    path: root.to_owned(),
                    message: error.to_string(),
                })?;
            let canonical_root = canonical.into_os_string().into_string().map_err(|path| {
                NodeServerError::InvalidWorkspaceRoot {
                    workspace_id: workspace_id.clone(),
                    path: path.to_string_lossy().into_owned(),
                    message: "canonical path is not valid Unicode".to_owned(),
                }
            })?;
            let canonical_root = platform::normalize_canonical_root(canonical_root);
            validate_workspace_root(workspace_id, &canonical_root)?;
            Ok(canonical_root)
        }
        Err(error) => Err(NodeServerError::InvalidWorkspaceRoot {
            workspace_id: workspace_id.clone(),
            path: root.to_owned(),
            message: error.to_string(),
        }),
    }
}

fn validate_git_revision(field: &str, revision: Option<&str>) -> Result<(), NodeFailure> {
    let Some(revision) = revision else {
        return Ok(());
    };
    if revision.is_empty()
        || revision.len() > MAX_REPOSITORY_PATH_BYTES
        || revision.chars().any(char::is_control)
    {
        return Err(failure(
            NodeFailureCode::InvalidRequest,
            &format!(
                "{field} must contain 1..={MAX_REPOSITORY_PATH_BYTES} bytes and no control characters",
            ),
        ));
    }
    Ok(())
}

fn git_worktree_failure(error: GitWorktreeError) -> NodeFailure {
    let code = match error.kind {
        GitWorktreeErrorKind::Invalid => NodeFailureCode::InvalidRequest,
        GitWorktreeErrorKind::NotRepository => NodeFailureCode::NotGitRepository,
        GitWorktreeErrorKind::Conflict => NodeFailureCode::WorktreeConflict,
        GitWorktreeErrorKind::Protected => NodeFailureCode::WorktreeProtected,
        GitWorktreeErrorKind::Dirty => NodeFailureCode::WorktreeDirty,
        GitWorktreeErrorKind::Locked => NodeFailureCode::WorktreeLocked,
        GitWorktreeErrorKind::Failed => NodeFailureCode::BackendOperationFailed,
    };
    failure(code, &error.message)
}

fn standalone_workspace_failure(error: StandaloneWorkspaceError) -> NodeFailure {
    let code = match error.kind {
        StandaloneWorkspaceErrorKind::InvalidRoot => NodeFailureCode::InvalidWorkspaceRoot,
        StandaloneWorkspaceErrorKind::InvalidInitialBranch => NodeFailureCode::InvalidRequest,
        StandaloneWorkspaceErrorKind::GitInitializationFailed => {
            NodeFailureCode::BackendOperationFailed
        }
        StandaloneWorkspaceErrorKind::RecoveryRequired => {
            NodeFailureCode::StandaloneWorkspaceRecoveryRequired
        }
    };
    failure(code, error.message)
}

fn managed_git_worktree_failure(error: GitWorktreeError) -> NodeFailure {
    let (code, message) = match error.kind {
        GitWorktreeErrorKind::Invalid => (
            NodeFailureCode::InvalidRequest,
            "managed Git input is invalid",
        ),
        GitWorktreeErrorKind::NotRepository => (
            NodeFailureCode::NotGitRepository,
            "managed source is not a Git repository",
        ),
        GitWorktreeErrorKind::Conflict => (
            NodeFailureCode::WorktreeConflict,
            "managed Git identity conflicts with existing repository state",
        ),
        GitWorktreeErrorKind::Protected => (
            NodeFailureCode::ManagedWorktreeOwnershipConflict,
            "managed worktree ownership validation failed",
        ),
        GitWorktreeErrorKind::Dirty => (
            NodeFailureCode::WorktreeDirty,
            "managed worktree contains uncommitted changes",
        ),
        GitWorktreeErrorKind::Locked => (
            NodeFailureCode::WorktreeLocked,
            "managed worktree is locked",
        ),
        GitWorktreeErrorKind::Failed => (
            NodeFailureCode::BackendOperationFailed,
            "managed Git operation failed",
        ),
    };
    NodeFailure { code, message: message.to_owned() }
}

fn managed_spawn_receipt(
    resolved: &ResolvedSpawnSpec,
    incarnation_id: NodeIncarnationId,
    session: SessionAddress,
    lease: ManagedWorktreeLeaseSnapshot,
    environment_profile: Option<ResolvedEnvironmentProfileReceipt>,
    bundle: Option<ResolvedBundleReceipt>,
    context: Option<ResolvedContextPackReceipt>,
) -> ManagedWorktreeSpawnReceipt {
    let mut managed = resolved.clone();
    managed.target.worktree_id = Some(lease.workspace_id.clone());
    ManagedWorktreeSpawnReceipt {
        spawn: managed.receipt_with_materialization(
            incarnation_id,
            session,
            environment_profile,
            bundle,
            context,
        ),
        lease,
    }
}

fn validate_selected_worktree(
    source_root: &str,
    selected_root: &str,
    worktrees: &[NativeGitWorktreeSnapshot],
) -> Result<(), NodeFailure> {
    if !worktrees
        .iter()
        .any(|worktree| worktree_paths_equal(&worktree.path, source_root) && !worktree.is_bare)
    {
        return Err(failure(
            NodeFailureCode::WorktreeProtected,
            "source workspace is not an authoritative non-bare Git worktree root",
        ));
    }
    if !worktrees.iter().any(|worktree| {
        worktree_paths_equal(&worktree.path, selected_root)
            && !worktree.is_main
            && !worktree.is_bare
    }) {
        return Err(failure(
            NodeFailureCode::WorktreeProtected,
            "selected workspace is not an eligible Git worktree in the source repository",
        ));
    }
    Ok(())
}

fn workspace_file_failure(error: WorkspaceFileReadError) -> NodeFailure {
    let code = match error.kind() {
        WorkspaceFileReadErrorKind::InvalidPath => NodeFailureCode::InvalidRepositoryPath,
        WorkspaceFileReadErrorKind::UnsafePath | WorkspaceFileReadErrorKind::ReparsePoint => {
            NodeFailureCode::RepositoryPathUnsafe
        }
        WorkspaceFileReadErrorKind::NotFound => NodeFailureCode::RepositoryFileNotFound,
        WorkspaceFileReadErrorKind::NotRegularFile => {
            NodeFailureCode::RepositoryFileNotRegular
        }
        WorkspaceFileReadErrorKind::AccessDenied | WorkspaceFileReadErrorKind::Io => {
            NodeFailureCode::RepositoryFileReadFailed
        }
        WorkspaceFileReadErrorKind::RevisionConflict => {
            NodeFailureCode::RepositoryFileRevisionConflict
        }
        WorkspaceFileReadErrorKind::AlreadyExists
        | WorkspaceFileReadErrorKind::ParentNotFound
        | WorkspaceFileReadErrorKind::ParentNotDirectory
        | WorkspaceFileReadErrorKind::Canceled => {
            NodeFailureCode::RepositoryFileReadFailed
        }
    };
    failure(code, "repository file read failed")
}

fn workspace_file_write_failure(error: WorkspaceFileReadError) -> NodeFailure {
    let code = match error.kind() {
        WorkspaceFileReadErrorKind::InvalidPath => NodeFailureCode::InvalidRepositoryPath,
        WorkspaceFileReadErrorKind::UnsafePath | WorkspaceFileReadErrorKind::ReparsePoint => {
            NodeFailureCode::RepositoryPathUnsafe
        }
        WorkspaceFileReadErrorKind::NotFound => NodeFailureCode::RepositoryFileNotFound,
        WorkspaceFileReadErrorKind::NotRegularFile => {
            NodeFailureCode::RepositoryFileNotRegular
        }
        WorkspaceFileReadErrorKind::AccessDenied | WorkspaceFileReadErrorKind::Io => {
            NodeFailureCode::RepositoryFileWriteFailed
        }
        WorkspaceFileReadErrorKind::RevisionConflict => {
            NodeFailureCode::RepositoryFileRevisionConflict
        }
        WorkspaceFileReadErrorKind::AlreadyExists
        | WorkspaceFileReadErrorKind::ParentNotFound
        | WorkspaceFileReadErrorKind::ParentNotDirectory
        | WorkspaceFileReadErrorKind::Canceled => {
            NodeFailureCode::RepositoryFileWriteFailed
        }
    };
    failure(code, "repository file write failed")
}

async fn settle_workspace_entry_create<T>(
    mut task: tokio::task::JoinHandle<Result<T, WorkspaceFileReadError>>,
    commit_state: Arc<AtomicU8>,
    deadline: Duration,
    timeout_message: &'static str,
    task_failure_message: &'static str,
) -> Result<T, NodeFailure> {
    let joined = match timeout(deadline, &mut task).await {
        Ok(joined) => joined,
        Err(_) => {
            if commit_state
                .compare_exchange(
                    WORKSPACE_ENTRY_CREATE_PENDING,
                    WORKSPACE_ENTRY_CREATE_CANCELED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Err(failure(
                    NodeFailureCode::RepositoryEntryCreateTimedOut,
                    timeout_message,
                ));
            }
            debug_assert_eq!(
                commit_state.load(Ordering::Acquire),
                WORKSPACE_ENTRY_CREATE_COMMITTING,
            );
            task.await
        }
    };
    joined
        .map_err(|_| {
            failure(
                NodeFailureCode::RepositoryEntryCreateFailed,
                task_failure_message,
            )
        })?
        .map_err(workspace_entry_create_failure)
}

fn workspace_entry_create_failure(error: WorkspaceFileReadError) -> NodeFailure {
    let code = match error.kind() {
        WorkspaceFileReadErrorKind::InvalidPath => NodeFailureCode::InvalidRepositoryPath,
        WorkspaceFileReadErrorKind::UnsafePath | WorkspaceFileReadErrorKind::ReparsePoint => {
            NodeFailureCode::RepositoryPathUnsafe
        }
        WorkspaceFileReadErrorKind::AlreadyExists => {
            NodeFailureCode::RepositoryEntryAlreadyExists
        }
        WorkspaceFileReadErrorKind::ParentNotFound | WorkspaceFileReadErrorKind::NotFound => {
            NodeFailureCode::RepositoryParentNotFound
        }
        WorkspaceFileReadErrorKind::ParentNotDirectory
        | WorkspaceFileReadErrorKind::NotRegularFile => {
            NodeFailureCode::RepositoryParentNotDirectory
        }
        WorkspaceFileReadErrorKind::AccessDenied | WorkspaceFileReadErrorKind::Io => {
            NodeFailureCode::RepositoryEntryCreateFailed
        }
        WorkspaceFileReadErrorKind::RevisionConflict => {
            NodeFailureCode::RepositoryEntryCreateFailed
        }
        WorkspaceFileReadErrorKind::Canceled => {
            NodeFailureCode::RepositoryEntryCreateTimedOut
        }
    };
    failure(code, "repository entry create failed")
}

fn workspace_file_revision(bytes: &[u8]) -> WorkspaceFileRevision {
    let value = digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    WorkspaceFileRevision::new(value).expect("SHA-256 produces a valid workspace revision")
}

fn managed_spawn_request_digest_v2(
    request: &ManagedWorktreeSpawnRequestV2,
) -> Result<String, NodeFailure> {
    let encoded = serde_json::to_vec(request).map_err(|_| {
        failure(
            NodeFailureCode::InvalidRequest,
            "managed worktree V2 request could not be canonicalized",
        )
    })?;
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in digest(&SHA256, &encoded).as_ref() {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(value)
}

fn native_session_preview_failure(error: NativeSessionPreviewError) -> NodeFailure {
    let code = match error {
        NativeSessionPreviewError::InvalidLimit => NodeFailureCode::InvalidRequest,
        NativeSessionPreviewError::WorkspaceUnavailable => NodeFailureCode::UnknownWorkspace,
        NativeSessionPreviewError::StaleCatalog => NodeFailureCode::StaleNativeSessionCatalog,
        NativeSessionPreviewError::UnsupportedProvider
        | NativeSessionPreviewError::PreviewUnavailable
        | NativeSessionPreviewError::SessionNotFound
        | NativeSessionPreviewError::AmbiguousSession => {
            NodeFailureCode::BackendOperationFailed
        }
    };
    failure(code, "native session preview failed")
}

fn host_directory_failure(error: HostDirectoryBrowseError) -> NodeFailure {
    let code = match error.kind() {
        HostDirectoryBrowseErrorKind::Invalid => NodeFailureCode::HostDirectoryInvalid,
        HostDirectoryBrowseErrorKind::ReadFailed => NodeFailureCode::HostDirectoryReadFailed,
    };
    failure(code, "host directory browse failed")
}

fn delivery_failure(error: DeliveryStoreError) -> NodeFailure {
    let code = match error {
        DeliveryStoreError::InvalidManifest => NodeFailureCode::DeliveryManifestInvalid,
        DeliveryStoreError::UnknownStage => NodeFailureCode::UnknownDeliveryStage,
        DeliveryStoreError::Capacity | DeliveryStoreError::StageConflict => {
            NodeFailureCode::DeliveryStageConflict
        }
        DeliveryStoreError::UnexpectedBlob => NodeFailureCode::DeliveryBlobUnexpected,
        DeliveryStoreError::ChunkOutOfOrder | DeliveryStoreError::ChunkOverflow => {
            NodeFailureCode::DeliveryChunkOutOfOrder
        }
        DeliveryStoreError::BlobDigestMismatch => {
            NodeFailureCode::DeliveryBlobDigestMismatch
        }
        DeliveryStoreError::BundleDigestMismatch => {
            NodeFailureCode::DeliveryBundleDigestMismatch
        }
        DeliveryStoreError::StageIncomplete => NodeFailureCode::DeliveryStageIncomplete,
        DeliveryStoreError::Unavailable
        | DeliveryStoreError::Corrupt
        | DeliveryStoreError::Storage(_) => NodeFailureCode::DeliveryStageStorageFailed,
    };
    failure(code, "delivery operation failed")
}

fn validate_workspace_root(
    workspace_id: &WorkspaceId,
    canonical_root: &str,
) -> Result<(), NodeServerError> {
    #[cfg(windows)]
    if canonical_root.starts_with(r"\\") {
        return Err(NodeServerError::InvalidWorkspaceRoot {
            workspace_id: workspace_id.clone(),
            path: canonical_root.to_owned(),
            message: "UNC workspace roots are unsupported by Windows PTY providers".to_owned(),
        });
    }
    if !platform::workspace_root_supported(canonical_root)
        || canonical_root.len() > WORKING_DIRECTORY_MAX_BYTES
    {
        return Err(NodeServerError::InvalidWorkspaceRoot {
            workspace_id: workspace_id.clone(),
            path: canonical_root.to_owned(),
            message: "canonical path must be absolute, bounded, and free of control characters"
                .to_owned(),
        });
    }
    Ok(())
}

fn failure(code: NodeFailureCode, _message: &str) -> NodeFailure {
    NodeFailure {
        code,
        message: node_failure_category(code).to_owned(),
    }
}

fn harness_mcp_failure(error: HarnessMcpProxyError) -> NodeFailure {
    let code = match error {
        HarnessMcpProxyError::Unavailable => NodeFailureCode::HarnessMcpUnavailable,
        HarnessMcpProxyError::NotFound => NodeFailureCode::ReservationNotFound,
        HarnessMcpProxyError::Conflict => NodeFailureCode::ReservationConflict,
        HarnessMcpProxyError::Expired => NodeFailureCode::ReservationExpired,
        HarnessMcpProxyError::BindingMismatch => NodeFailureCode::BindingMismatch,
        HarnessMcpProxyError::NotActivated => NodeFailureCode::NotActivated,
        HarnessMcpProxyError::CallNotFound => NodeFailureCode::CallNotFound,
        HarnessMcpProxyError::ChunkOutOfOrder => NodeFailureCode::ChunkOutOfOrder,
        HarnessMcpProxyError::ResponseTooLarge => NodeFailureCode::ResponseTooLarge,
    };
    failure(code, "harness MCP proxy operation failed")
}

fn spawn_deadline_remaining(deadline: Instant) -> Result<Duration, NodeFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(failure(
            NodeFailureCode::SpawnDeadlineExceeded,
            "spawn deadline elapsed",
        ))
    } else {
        Ok(remaining)
    }
}

fn spawn_dispatch_timeout(deadline: Option<Instant>) -> Result<Duration, NodeFailure> {
    let bounded = Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS);
    match deadline {
        Some(deadline) => Ok(spawn_deadline_remaining(deadline)?.min(bounded)),
        None => Ok(bounded),
    }
}

fn spawn_dispatch_error(error: NodeFailure, deadline: Option<Instant>) -> NodeFailure {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        failure(
            NodeFailureCode::SpawnDeadlineExceeded,
            "spawn dispatch exceeded the spawn deadline",
        )
    } else {
        error
    }
}

fn spawn_runtime_capabilities(
    capabilities: &SpawnRequiredCapabilities,
) -> Result<Vec<ProviderRuntimeCapability>, NodeFailure> {
    capabilities
        .iter()
        .map(|capability| match capability.as_str() {
            SPAWN_RUNTIME_RAW_PTY_LIFECYCLE => Ok(ProviderRuntimeCapability::RawPtyLifecycle),
            SPAWN_RUNTIME_SEMANTIC_READINESS => Ok(ProviderRuntimeCapability::SemanticReadiness),
            SPAWN_RUNTIME_STRUCTURED_PROMPT => Ok(ProviderRuntimeCapability::StructuredPrompt),
            SPAWN_RUNTIME_PROVIDER_SESSION_IDENTITY => {
                Ok(ProviderRuntimeCapability::ProviderSessionIdentity)
            }
            SPAWN_RUNTIME_SEMANTIC_RESUME => Ok(ProviderRuntimeCapability::SemanticResume),
            _ => Err(failure(
                NodeFailureCode::UnsupportedSpawnCapability,
                "spawn required capability is unknown",
            )),
        })
        .collect()
}

fn persistence_failure(_error: io::Error) -> NodeFailure {
    NodeFailure {
        code: NodeFailureCode::BackendOperationFailed,
        message: DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned(),
    }
}

fn node_failure_category(code: NodeFailureCode) -> &'static str {
    match code {
        NodeFailureCode::InvalidRequest => "invalid-request",
        NodeFailureCode::UnsupportedCapability => "unsupported-capability",
        NodeFailureCode::HarnessMcpUnavailable => "harness-mcp-unavailable",
        NodeFailureCode::ReservationNotFound => "reservation-not-found",
        NodeFailureCode::ReservationConflict => "reservation-conflict",
        NodeFailureCode::ReservationExpired => "reservation-expired",
        NodeFailureCode::BindingMismatch => "binding-mismatch",
        NodeFailureCode::NotActivated => "not-activated",
        NodeFailureCode::CallNotFound => "call-not-found",
        NodeFailureCode::ChunkOutOfOrder => "chunk-out-of-order",
        NodeFailureCode::ResponseTooLarge => "response-too-large",
        NodeFailureCode::DeliveryManifestInvalid => "delivery-manifest-invalid",
        NodeFailureCode::UnknownDeliveryStage => "unknown-delivery-stage",
        NodeFailureCode::DeliveryStageConflict => "delivery-stage-conflict",
        NodeFailureCode::DeliveryBlobUnexpected => "delivery-blob-unexpected",
        NodeFailureCode::DeliveryChunkOutOfOrder => "delivery-chunk-out-of-order",
        NodeFailureCode::DeliveryBlobDigestMismatch => "delivery-blob-digest-mismatch",
        NodeFailureCode::DeliveryBundleDigestMismatch => "delivery-bundle-digest-mismatch",
        NodeFailureCode::DeliveryStageIncomplete => "delivery-stage-incomplete",
        NodeFailureCode::DeliveryStageStorageFailed => "delivery-stage-storage-failed",
        NodeFailureCode::Unauthorized => "unauthorized",
        NodeFailureCode::ObserverReadOnly => "observer-read-only",
        NodeFailureCode::ControllerBusy => "controller-busy",
        NodeFailureCode::ControllerRequired => "controller-required",
        NodeFailureCode::UnknownWorkspace => "unknown-workspace",
        NodeFailureCode::HostDirectoryInvalid => "host-directory-invalid",
        NodeFailureCode::HostDirectoryReadFailed => "host-directory-read-failed",
        NodeFailureCode::HostDirectoryReadTimedOut => "host-directory-read-timed-out",
        NodeFailureCode::InvalidRepositoryPath => "invalid-repository-path",
        NodeFailureCode::RepositoryFileNotFound => "repository-file-not-found",
        NodeFailureCode::RepositoryFileNotRegular => "repository-file-not-regular",
        NodeFailureCode::RepositoryPathUnsafe => "repository-path-unsafe",
        NodeFailureCode::RepositoryFileReadTimedOut => "repository-file-read-timed-out",
        NodeFailureCode::RepositoryFileReadFailed => "repository-file-read-failed",
        NodeFailureCode::RepositoryFileWriteTimedOut => "repository-file-write-timed-out",
        NodeFailureCode::RepositoryFileWriteFailed => "repository-file-write-failed",
        NodeFailureCode::RepositoryFileRevisionConflict => "repository-file-revision-conflict",
        NodeFailureCode::RepositoryEntryAlreadyExists => "repository-entry-already-exists",
        NodeFailureCode::RepositoryParentNotFound => "repository-parent-not-found",
        NodeFailureCode::RepositoryParentNotDirectory => "repository-parent-not-directory",
        NodeFailureCode::RepositoryEntryCreateTimedOut => "repository-entry-create-timed-out",
        NodeFailureCode::RepositoryEntryCreateFailed => "repository-entry-create-failed",
        NodeFailureCode::GitReadTimedOut => "git-read-timed-out",
        NodeFailureCode::GitReadFailed => "git-read-failed",
        NodeFailureCode::UnknownSpawnProfile => "unknown-spawn-profile",
        NodeFailureCode::SpawnProfileRevisionMismatch => "spawn-profile-revision-mismatch",
        NodeFailureCode::SpawnTargetMismatch => "spawn-target-mismatch",
        NodeFailureCode::SpawnIdempotencyConflict => "spawn-idempotency-conflict",
        NodeFailureCode::SpawnIdempotencyCapacity => "spawn-idempotency-capacity",
        NodeFailureCode::SpawnDeadlineExceeded => "spawn-deadline-exceeded",
        NodeFailureCode::UnsupportedSpawnCapability => "unsupported-spawn-capability",
        NodeFailureCode::UnknownEnvironmentProfile => "unknown-environment-profile",
        NodeFailureCode::EnvironmentProfileBindingMismatch => {
            "environment-profile-binding-mismatch"
        }
        NodeFailureCode::UnknownBundle => "unknown-bundle",
        NodeFailureCode::BundleBindingMismatch => "bundle-binding-mismatch",
        NodeFailureCode::BundleMaterializationFailed => "bundle-materialization-failed",
        NodeFailureCode::UnknownContextPack => "unknown-context-pack",
        NodeFailureCode::ContextPackBusy => "context-pack-busy",
        NodeFailureCode::ContextPackMaterializationFailed => {
            "context-pack-materialization-failed"
        }
        NodeFailureCode::InvalidWorkspaceRoot => "invalid-workspace-root",
        NodeFailureCode::DuplicateWorkspaceId => "duplicate-workspace-id",
        NodeFailureCode::DuplicateWorkspaceRoot => "duplicate-workspace-root",
        NodeFailureCode::WorkspaceBusy => "workspace-busy",
        NodeFailureCode::LastWorkspace => "last-workspace",
        NodeFailureCode::NotGitRepository => "not-git-repository",
        NodeFailureCode::WorktreeConflict => "worktree-conflict",
        NodeFailureCode::WorktreeProtected => "worktree-protected",
        NodeFailureCode::WorktreeDirty => "worktree-dirty",
        NodeFailureCode::WorktreeLocked => "worktree-locked",
        NodeFailureCode::UnknownManagedWorktreeLease => "unknown-managed-worktree-lease",
        NodeFailureCode::ManagedWorktreeBusy => "managed-worktree-busy",
        NodeFailureCode::ManagedWorktreeOwnershipConflict => "managed-worktree-ownership-conflict",
        NodeFailureCode::ManagedWorktreeProfileRevisionMismatch => {
            "managed-worktree-profile-revision-mismatch"
        }
        NodeFailureCode::ManagedWorktreeRecoveryRequired => "managed-worktree-recovery-required",
        NodeFailureCode::StandaloneWorkspaceRecoveryRequired => {
            "standalone-workspace-recovery-required"
        }
        NodeFailureCode::UnknownSession => "unknown-session",
        NodeFailureCode::UnknownSessionRecord => "unknown-session-record",
        NodeFailureCode::SessionRecordNotResumable => "session-record-not-resumable",
        NodeFailureCode::SessionRecordBusy => "session-record-busy",
        NodeFailureCode::SessionRecordConflict => "session-record-conflict",
        NodeFailureCode::SessionWorkspaceMismatch => "session-workspace-mismatch",
        NodeFailureCode::WorkspaceRegistrationRequired => {
            "workspace-registration-required"
        }
        NodeFailureCode::StaleNativeSessionCatalog => "stale-native-session-catalog",
        NodeFailureCode::StaleGeneration => "stale-generation",
        NodeFailureCode::BackendBusy => "backend-busy",
        NodeFailureCode::BackendDisconnected => "backend-disconnected",
        NodeFailureCode::BackendOperationFailed => "backend-operation-failed",
        NodeFailureCode::ShuttingDown => "shutting-down",
    }
}

fn durable_state_server_error(error: io::Error, category: &'static str) -> NodeServerError {
    NodeServerError::DurableState(io::Error::new(error.kind(), category))
}

fn durable_state_load_error(error: io::Error) -> NodeServerError {
    let category = match session_registry::state_load_refusal(&error) {
        Some(session_registry::StateLoadRefusal::UnsupportedSchema) => {
            DURABLE_STATE_SCHEMA_UNSUPPORTED_ERROR
        }
        Some(session_registry::StateLoadRefusal::PathSemanticsUnsupported) => {
            DURABLE_STATE_PATH_SEMANTICS_UNSUPPORTED_ERROR
        }
        Some(session_registry::StateLoadRefusal::NodeIdentityMismatch) => {
            DURABLE_STATE_CONFLICT_ERROR
        }
        _ => DURABLE_STATE_LOAD_FAILED_ERROR,
    };
    durable_state_server_error(error, category)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn detached_record_state(record: &ManagedSessionRecord) -> ManagedSessionState {
    if record.provider_session.is_some() {
        ManagedSessionState::Dormant
    } else {
        ManagedSessionState::Unavailable
    }
}

fn active_record_state(
    identity_admitted: bool,
    provider_session_is_known: bool,
) -> ManagedSessionState {
    if identity_admitted && !provider_session_is_known {
        ManagedSessionState::IdentityPending
    } else {
        ManagedSessionState::Live
    }
}

fn managed_worktree_record_holders(
    records: &[ManagedSessionRecord],
) -> Vec<(SessionRecordId, WorkspaceId)> {
    records
        .iter()
        .filter(|record| record.provider_session.is_some())
        .map(|record| (record.record_id.clone(), record.workspace_id.clone()))
        .collect()
}

fn managed_spawn_record_ownership(
    binding: &SessionBinding,
    effective_runtime_policy: ProviderRuntimePolicy,
) -> (Option<SessionRecordId>, Option<SessionRecordId>) {
    debug_assert_eq!(binding.runtime_policy, effective_runtime_policy);
    if effective_runtime_policy.provider_session_identity {
        (binding.record_id.clone(), None)
    } else {
        (None, binding.record_id.clone())
    }
}

#[derive(Debug, Error)]
pub enum NodeServerError {
    #[error("node endpoint must be a bounded absolute local transport path")]
    InvalidEndpoint,
    #[error("node access token must contain 1..=4096 bytes")]
    InvalidAccessToken,
    #[error("node HTTP observer must listen on a loopback address: {0}")]
    InvalidApiListen(std::net::SocketAddr),
    #[error("node durable state path must be an absolute file path: {0}")]
    InvalidStatePath(String),
    #[error("harness MCP helper must be an exact reviewed absolute regular file")]
    InvalidHarnessMcpHelper,
    #[error("the local state directory is unavailable; supply an explicit state path")]
    LocalStateDirectoryUnavailable,
    #[error("the local runtime directory is unavailable; supply an explicit endpoint")]
    LocalRuntimeDirectoryUnavailable,
    #[error("node durable state failed: {0}")]
    DurableState(io::Error),
    #[error("node requires at least one configured workspace")]
    NoWorkspaces,
    #[error("workspace '{workspace_id}' root '{path}' is invalid: {message}")]
    InvalidWorkspaceRoot {
        workspace_id: WorkspaceId,
        path: String,
        message: String,
    },
    #[error("duplicate workspace ID: {0}")]
    DuplicateWorkspaceId(WorkspaceId),
    #[error("workspaces '{first}' and '{second}' resolve to the same root '{root}'")]
    DuplicateWorkspaceRoot {
        first: WorkspaceId,
        second: WorkspaceId,
        root: String,
    },
    #[error("workspace '{0}' uses managed worktrees but has no local profile")]
    ManagedWorktreeProfileRequired(WorkspaceId),
    #[error("workspace '{0}' has managed worktree profiles but is not in managed mode")]
    ManagedWorktreeProfileModeMismatch(WorkspaceId),
    #[error("workspace '{workspace_id}' has duplicate managed worktree profile '{profile_id}'")]
    DuplicateManagedWorktreeProfile {
        workspace_id: WorkspaceId,
        profile_id: WorktreeProfileId,
    },
    #[error("workspace '{workspace_id}' exceeds the {max}-profile managed worktree limit")]
    ManagedWorktreeProfileCapacity {
        workspace_id: WorkspaceId,
        max: usize,
    },
    #[error("workspace '{workspace_id}' managed worktree profile '{profile_id}' is invalid: {message}")]
    InvalidManagedWorktreeProfile {
        workspace_id: WorkspaceId,
        profile_id: WorktreeProfileId,
        message: String,
    },
    #[error("duplicate node environment profile: {0}")]
    DuplicateEnvironmentProfile(SpawnEnvironmentProfileId),
    #[error("node environment profile provider is unavailable: {0}")]
    EnvironmentProfileProviderUnavailable(AgentId),
    #[error("node environment profile capacity is {max}")]
    EnvironmentProfileCapacity { max: usize },
    #[error("node environment profiles reuse native profile ID '{0}'")]
    DuplicateNativeEnvironmentProfile(String),
    #[error("node native environment profile is invalid: {0}")]
    NativeEnvironmentProfile(String),
    #[error("node session-environment materialization root must be absolute")]
    InvalidSessionEnvironmentRoot,
    #[error("node session-environment materializer is unavailable")]
    SessionEnvironmentMaterializerRequired,
    #[error("node session-environment materializer failed to initialize")]
    SessionEnvironmentMaterializer,
    #[error("node bundle catalog is invalid: {0}")]
    BundleCatalog(String),
    #[error("node delivery store failed to initialize")]
    DeliveryStore,
    #[error("node context pack store failed to initialize")]
    ContextPackStore,
    #[error("active agent registry failed: {0}")]
    Registry(String),
    #[error("node provider contract manifest is invalid: {0}")]
    ProviderContractManifest(String),
    #[error("node hook ingress startup failed: {0}")]
    HookIngressStartup(String),
    #[error("named pipe I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("node HTTP observer failed: {0}")]
    HttpApi(io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("node handshake failed: {0}")]
    Handshake(String),
    #[error("node authentication frame was not received before the bounded deadline")]
    AuthenticationTimedOut,
    #[error("node authentication primitive failed: {0}")]
    Authentication(String),
    #[error("node incarnation identity generation failed: {0}")]
    IncarnationIdentity(String),
    #[error("node pre-authentication limiter is closed")]
    PreauthClosed,
    #[error("node authenticated connection limit was reached")]
    AuthenticatedConnectionLimit,
    #[error("node Ctrl+C/CtrlBreak signal stream closed")]
    SignalStreamClosed,
    #[error("node stop-all dispatch failed: {0:?}")]
    ShutdownDispatch(NodeFailure),
    #[error("node shutdown timed out with {active_native_sessions} physical sessions retained")]
    ShutdownTimedOut { active_native_sessions: usize },
}

#[cfg(test)]
mod observation_projection_tests {
    use super::*;
    use gate4agent_types::{
        AdapterId, AdapterVerification, ContextWindowUsage, ProviderSource, TokenUsage,
    };

    fn observation_test_shared() -> NodeShared {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("observation-test").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-observation-test").unwrap(),
            vec![workspace],
            vec![AgentId::new("claude").unwrap()],
        )
    }

    fn provider_control_event(
        family: AdapterFamily,
        provider_event: ProviderEvent,
    ) -> ControlEvent {
        provider_control_event_at(family, "fixture", 9, provider_event)
    }

    fn provider_control_event_at(
        family: AdapterFamily,
        adapter: &str,
        source_sequence: u64,
        provider_event: ProviderEvent,
    ) -> ControlEvent {
        ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 41,
            command_id: None,
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(3),
            event: ControlEventKind::ProviderEvent {
                sequence: 8,
                source: provider_source(family, adapter),
                source_sequence,
                event: provider_event,
            },
        }
    }

    fn address() -> SessionAddress {
        SessionAddress {
            workspace_id: WorkspaceId::new("observation-test").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(3),
            },
        }
    }

    fn provider_source(family: AdapterFamily, adapter: &str) -> ProviderSource {
        ProviderSource {
            family,
            binding: AdapterBinding::new(
                AdapterId::new(adapter).unwrap(),
                "fixture/v1",
                AdapterVerification::SyntheticFixture,
            )
            .unwrap(),
        }
    }

    fn timeline_observations(observations: &[ObservationV1]) -> Vec<&ObservationV1> {
        observations
            .iter()
            .filter(|observation| {
                !matches!(observation.kind, ObservationKindV1::SourceCapabilities { .. })
            })
            .collect()
    }

    #[test]
    fn hook_capability_matrix_claims_only_current_normalizer_emitters() {
        let (_, pty) = observation_source_capabilities(&provider_source(
            AdapterFamily::PtySemantic,
            "codex",
        ));
        assert_eq!(pty, ObservationCapabilitiesV1::default());

        for (adapter, tools, attention, subagents) in [
            ("claude-code", true, true, true),
            ("codex", true, true, false),
            ("gemini", true, false, false),
            ("opencode", false, true, false),
            ("mimo-code", false, true, false),
            ("pi", true, false, false),
            ("omp", true, false, false),
            ("antigravity", true, true, false),
            ("amp", true, false, false),
            ("command-code", true, false, false),
            ("hermes", true, true, false),
            ("devin", true, true, false),
            ("grok", true, true, true),
            ("kimi", true, true, true),
            ("copilot", true, true, true),
            ("droid", true, true, true),
            ("cursor", true, false, true),
        ] {
            let (family, actual) = observation_source_capabilities(&provider_source(
                AdapterFamily::Hook,
                adapter,
            ));
            assert_eq!(family, ObservationSourceFamilyV1::Hook, "{adapter}");
            assert_eq!(
                actual,
                ObservationCapabilitiesV1 {
                    tools,
                    attention,
                    subagents,
                    ..ObservationCapabilitiesV1::default()
                },
                "{adapter}",
            );
        }
        assert_eq!(
            observation_source_capabilities(&provider_source(
                AdapterFamily::Hook,
                "unsupported",
            )),
            (
                ObservationSourceFamilyV1::Hook,
                ObservationCapabilitiesV1::default(),
            )
        );

        let (_, pipe) = observation_source_capabilities(&provider_source(
            AdapterFamily::Pipe,
            "codex",
        ));
        assert!(pipe.tools && pipe.usage);
        assert!(!pipe.attention && !pipe.subagents && !pipe.todo && !pipe.file_changes);
        let (_, kimi_pipe) = observation_source_capabilities(&provider_source(
            AdapterFamily::Pipe,
            "kimi",
        ));
        assert!(kimi_pipe.tools);
        assert!(!kimi_pipe.usage);
        let (_, qwen_pipe) = observation_source_capabilities(&provider_source(
            AdapterFamily::Pipe,
            "qwen-code",
        ));
        assert!(qwen_pipe.tools && qwen_pipe.attention && qwen_pipe.usage);
        assert!(!qwen_pipe.subagents && !qwen_pipe.todo && !qwen_pipe.file_changes);
        assert!(observation_evidence(AdapterFamily::History).is_none());
    }

    #[test]
    fn current_five_enabled_provider_matrix_is_exact() {
        let registry = active_registry().unwrap();
        let (providers, adapters) = provider_contract_manifest(&registry).unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|contract| contract.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "grok", "kimi", "qwen-code"],
        );
        assert_eq!(
            adapters
                .iter()
                .filter(|contract| contract.family == AdapterFamily::Hook)
                .map(|contract| (contract.provider.as_str(), contract.adapter_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("claude", "claude-code"),
                ("codex", "codex"),
                ("grok", "grok"),
                ("kimi", "kimi"),
            ],
        );
        assert!(!adapters.iter().any(|contract| {
            contract.provider.as_str() == "qwen-code" && contract.family == AdapterFamily::Hook
        }));

        for (adapter, subagents) in [
            ("claude-code", true),
            ("codex", false),
            ("grok", true),
            ("kimi", true),
        ] {
            let (family, capabilities) = observation_source_capabilities(&provider_source(
                AdapterFamily::Hook,
                adapter,
            ));
            assert_eq!(family, ObservationSourceFamilyV1::Hook);
            assert!(capabilities.tools && capabilities.attention);
            assert!(!capabilities.usage);
            assert_eq!(capabilities.subagents, subagents, "{adapter}");
            assert!(!capabilities.todo && !capabilities.file_changes && !capabilities.owned_processes);
        }
    }

    #[test]
    fn subagent_capability_is_exact() {
        for adapter in [
            "claude-code",
            "grok",
            "kimi",
            "copilot",
            "droid",
            "cursor",
        ] {
            assert!(hook_observation_capabilities(adapter).subagents, "{adapter}");
        }
        for adapter in [
            "codex",
            "gemini",
            "opencode",
            "mimo-code",
            "pi",
            "omp",
            "antigravity",
            "amp",
            "command-code",
            "hermes",
            "devin",
            "unsupported",
        ] {
            assert!(!hook_observation_capabilities(adapter).subagents, "{adapter}");
        }
    }

    #[test]
    fn acp_source_capabilities_match_emitted_tool_and_usage_events() {
        let (family, capabilities) = observation_source_capabilities(&provider_source(
            AdapterFamily::Acp,
            "codex",
        ));
        assert_eq!(family, ObservationSourceFamilyV1::Acp);
        assert!(capabilities.tools);
        assert!(capabilities.usage);
        assert!(!capabilities.attention);
        assert!(!capabilities.subagents);
        assert!(!capabilities.todo);
        assert!(!capabilities.owned_processes);
        assert!(!capabilities.file_changes);
        assert!(!capabilities.history_summary);
    }

    #[test]
    fn non_session_start_hook_event_receives_capabilities() {
        let event = provider_control_event_at(
            AdapterFamily::Hook,
            "claude-code",
            1,
            ProviderEvent::WorkingObserved,
        );
        let projected = provider_observations(&event);
        assert_eq!(projected.len(), 2);
        let ObservationKindV1::SourceCapabilities {
            source_family,
            source_adapter,
            capabilities,
        } = &projected[0].kind else {
            panic!("expected source capabilities");
        };
        assert_eq!(*source_family, ObservationSourceFamilyV1::Hook);
        assert_eq!(source_adapter, "claude-code");
        assert_eq!(*capabilities, hook_observation_capabilities("claude-code"));
        assert!(!projected[0].kind.requires_workflow_detail_capability());
        assert_eq!(projected[1].kind, ObservationKindV1::Working);
        let base = project_event_without_observation_workflow_detail(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::Observation {
                address: address(),
                observation: projected[0].clone(),
            },
        });
        assert!(base.is_some());
    }

    #[test]
    fn later_hook_events_do_not_repeat_capabilities() {
        let text = provider_control_event_at(
            AdapterFamily::Hook,
            "claude-code",
            2,
            ProviderEvent::Text {
                text: "private text delta".to_owned(),
                is_delta: true,
            },
        );
        assert!(provider_observations(&text).is_empty());

        let working = provider_control_event_at(
            AdapterFamily::Hook,
            "claude-code",
            3,
            ProviderEvent::WorkingObserved,
        );
        let projected = provider_observations(&working);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].kind, ObservationKindV1::Working);
    }

    #[test]
    fn first_identity_only_event_emits_capabilities() {
        let identity = provider_control_event_at(
            AdapterFamily::Hook,
            "claude-code",
            1,
            ProviderEvent::SessionIdentityObserved {
                identity: ProviderSessionIdentity {
                    key: ProviderSessionKey::SessionId,
                    id: "private-provider-session".to_owned(),
                    transcript_path: None,
                },
            },
        );
        let projected = provider_observations(&identity);
        assert_eq!(projected.len(), 1);
        assert!(matches!(
            &projected[0].kind,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::Hook,
                source_adapter,
                ..
            } if source_adapter == "claude-code"
        ));
    }

    #[test]
    fn hook_stop_turn_completed_has_no_usage_observation() {
        let completed = provider_control_event_at(
            AdapterFamily::Hook,
            "claude-code",
            1,
            ProviderEvent::TurnCompleted {
                usage: TokenUsage::default(),
                is_cumulative: false,
            },
        );
        let projected = provider_observations(&completed);
        let ObservationKindV1::SourceCapabilities { capabilities, .. } = &projected[0].kind else {
            panic!("expected source capabilities");
        };
        assert!(!capabilities.usage);
        assert_eq!(timeline_observations(&projected).len(), 1);
        assert_eq!(projected[1].kind, ObservationKindV1::TurnCompleted);
        assert!(!projected
            .iter()
            .any(|observation| matches!(observation.kind, ObservationKindV1::Usage { .. })));
    }

    #[test]
    fn token_bearing_pipe_turn_completed_projects_usage() {
        let completed = provider_control_event_at(
            AdapterFamily::Pipe,
            "codex",
            1,
            ProviderEvent::TurnCompleted {
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 30,
                    cache_write_tokens: 40,
                    reasoning_tokens: 50,
                    context_window: Some(128_000),
                },
                is_cumulative: true,
            },
        );
        let projected = provider_observations(&completed);
        let ObservationKindV1::SourceCapabilities { capabilities, .. } = &projected[0].kind else {
            panic!("expected source capabilities");
        };
        assert!(capabilities.usage);
        assert_eq!(projected[1].kind, ObservationKindV1::TurnCompleted);
        assert!(matches!(
            projected[2].kind,
            ObservationKindV1::Usage {
                input_tokens: 10,
                output_tokens: 20,
                is_cumulative: true,
                ..
            }
        ));
    }

    #[test]
    fn exact_context_projects_only_from_structured_non_pty_provider_events() {
        let usage = ContextWindowUsage {
            uncached_input_tokens: 70,
            cache_read_tokens: 20,
            cache_write_tokens: 0,
            output_tokens: 10,
            unattributed_tokens: 5,
            used_tokens: 105,
            capacity_tokens: 100,
        };
        let structured = provider_observations(&provider_control_event_at(
            AdapterFamily::Pipe,
            "codex",
            2,
            ProviderEvent::ContextWindowUsage { usage },
        ));
        let timeline = timeline_observations(&structured);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].evidence, ObservationEvidenceV1::StructuredProvider);
        assert_eq!(
            timeline[0].kind,
            ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: 70,
                cache_read_tokens: 20,
                cache_write_tokens: 0,
                output_tokens: 10,
                unattributed_tokens: 5,
                used_tokens: 105,
                capacity_tokens: 100,
            }
        );

        for family in [AdapterFamily::PtySemantic, AdapterFamily::ManagedHook] {
            let projected = provider_observations(&provider_control_event_at(
                family,
                "codex",
                2,
                ProviderEvent::ContextWindowUsage { usage },
            ));
            assert!(
                timeline_observations(&projected).is_empty(),
                "{family:?} must not project an authoritative context fact"
            );
        }
    }

    #[test]
    fn qwen_pipe_events_project_private_categorical_tools_attention_and_usage() {
        let mut shared = observation_test_shared();
        let (_, adapters) = provider_contract_manifest(&active_registry().unwrap()).unwrap();
        shared.provider_adapter_contracts = adapters;
        let raw = ProviderRuntimePolicy::raw_pty();
        let observed = shared.admit_qwen_sidecar_observation_policy(
            &AgentId::new("qwen-code").unwrap(),
            SessionMode::Pty,
            raw,
        );
        assert!(observed.raw_pty_lifecycle && observed.semantic_readiness);
        assert!(!observed.structured_prompt);
        assert!(!observed.provider_session_identity);
        assert!(!observed.semantic_resume);
        assert_eq!(
            shared.admit_qwen_sidecar_observation_policy(
                &AgentId::new("qwen-code").unwrap(),
                SessionMode::Inline,
                raw,
            ),
            raw
        );
        assert_eq!(
            shared.admit_qwen_sidecar_observation_policy(
                &AgentId::new("codex").unwrap(),
                SessionMode::Pty,
                raw,
            ),
            raw
        );

        let ready = provider_control_event_at(
            AdapterFamily::Pipe,
            "qwen-code",
            1,
            ProviderEvent::Ready,
        );
        let projected = provider_observations(&ready);
        let ObservationKindV1::SourceCapabilities { capabilities, .. } = &projected[0].kind else {
            panic!("expected source capabilities");
        };
        assert!(capabilities.tools && capabilities.attention && capabilities.usage);
        assert!(!capabilities.subagents && !capabilities.todo && !capabilities.file_changes);

        let tool = provider_observations(&provider_control_event_at(
            AdapterFamily::Pipe,
            "qwen-code",
            2,
            ProviderEvent::ToolStarted {
                id: "private-provider-tool-id".to_owned(),
                name: "run_shell_command".to_owned(),
                input_json: String::new(),
                agent_id: None,
            },
        ));
        assert!(matches!(
            timeline_observations(&tool)[0].kind,
            ObservationKindV1::ToolStarted { ref class, .. } if class == "Shell"
        ));
        let attention = provider_observations(&provider_control_event_at(
            AdapterFamily::Pipe,
            "qwen-code",
            3,
            ProviderEvent::InteractionRequested {
                request_id: Some("private-request-id".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "Shell".to_owned(),
                prompt: String::new(),
                agent_id: None,
            },
        ));
        let ObservationKindV1::ApprovalRequested {
            correlation_id: requested_correlation,
            tool_class,
        } = &timeline_observations(&attention)[0].kind else {
            panic!("expected Qwen approval observation");
        };
        assert_eq!(tool_class, "Shell");
        let requested_correlation = requested_correlation.clone();
        let raw_resolution = provider_observations(&provider_control_event_at(
            AdapterFamily::Pipe,
            "qwen-code",
            4,
            ProviderEvent::InteractionResolved {
                request_id: "private-request-id".to_owned(),
                outcome: gate4agent_types::ProviderInteractionOutcome::Approved,
            },
        ));
        assert!(raw_resolution.is_empty());
        let resolved = provider_observations(&ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 42,
            command_id: None,
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(3),
            event: ControlEventKind::InteractionResolved {
                interaction_id: gate4agent_types::ProviderInteractionId(8),
                outcome: gate4agent_types::ProviderInteractionOutcome::Approved,
            },
        });
        let ObservationKindV1::InteractionResolved {
            correlation_id,
            outcome,
        } = &resolved[0].kind else {
            panic!("expected Qwen interaction resolution observation");
        };
        assert_eq!(correlation_id, &requested_correlation);
        assert_eq!(*outcome, ObservationInteractionOutcomeV1::Approved);
        let usage = provider_observations(&provider_control_event_at(
            AdapterFamily::Pipe,
            "qwen-code",
            5,
            ProviderEvent::TurnCompleted {
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 8,
                    ..TokenUsage::default()
                },
                is_cumulative: false,
            },
        ));
        assert!(timeline_observations(&usage).iter().any(|observation| matches!(
            observation.kind,
            ObservationKindV1::Usage { input_tokens: 5, output_tokens: 8, .. }
        )));
        let encoded = serde_json::to_string(&(
            projected,
            tool,
            attention,
            raw_resolution,
            resolved,
            usage,
        ))
        .unwrap();
        assert!(!encoded.contains("private-provider-tool-id"));
        assert!(!encoded.contains("private-request-id"));
    }

    #[test]
    fn observation_projection_is_private_categorical_and_capability_gated() {
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_OBSERVATION_EVENTS_CAPABILITY
        }));

        let tool = provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::ToolStarted {
                id: "private-tool-id".to_owned(),
                name: "PowerShell command containing a private path".to_owned(),
                input_json: r#"{"prompt":"secret","path":"C:\\private"}"#.to_owned(),
                agent_id: Some("private-provider-agent".to_owned()),
            },
        );
        let projected = provider_observations(&tool);
        assert_eq!(projected.len(), 1);
        let timeline = timeline_observations(&projected);
        assert_eq!(timeline.len(), 1);
        let tool_observation = (*timeline[0]).clone();
        assert_eq!(tool_observation.source_sequence, 9);
        assert_eq!(tool_observation.evidence, ObservationEvidenceV1::ManagedHook);
        let ObservationKindV1::ToolStarted {
            correlation_id,
            class,
        } = &tool_observation.kind
        else {
            panic!("expected tool start observation");
        };
        assert!(correlation_id.starts_with("tool-"));
        assert_eq!(class, "Shell");
        let tool_correlation = correlation_id.clone();
        let completed = provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::ToolCompleted {
                id: "private-tool-id".to_owned(),
                output: "private output".to_owned(),
                is_error: false,
                duration_ms: Some(7),
                agent_id: None,
            },
        );
        let completed = provider_observations(&completed);
        let completed = timeline_observations(&completed);
        let ObservationKindV1::ToolCompleted { correlation_id, .. } = &completed[0].kind
        else {
            panic!("expected tool completion observation");
        };
        assert_eq!(correlation_id, &tool_correlation);
        let gated = project_event_without_observation(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::Observation {
                address: address(),
                observation: tool_observation.clone(),
            },
        });
        assert!(gated.is_none());
        let safe = project_event_without_observation_workflow_detail(NodeEventEnvelope {
            sequence: 2,
            event: NodeEvent::Observation {
                address: address(),
                observation: tool_observation,
            },
        });
        assert!(safe.is_some());
        let detail = project_event_without_observation_workflow_detail(NodeEventEnvelope {
            sequence: 3,
            event: NodeEvent::Observation {
                address: address(),
                observation: ObservationV1 {
                    source_sequence: 10,
                    observed_at_unix_ms: Some(1),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                    kind: ObservationKindV1::Error {
                        detail: "provider-error".to_owned(),
                    },
                    truncated: false,
                },
            },
        });
        assert!(detail.is_none());
        let wire = serde_json::to_string(&projected).unwrap();
        for private in [
            "private-tool-id",
            "private-provider-agent",
            "secret",
            "C:\\\\private",
            "PowerShell command containing a private path",
        ] {
            assert!(!wire.contains(private));
        }

        let subagent = provider_control_event(
            AdapterFamily::OneShot,
            ProviderEvent::SubagentStarted {
                agent_id: "raw-provider-subagent-id".to_owned(),
                agent_type: Some("research-agent-with-private-label".to_owned()),
                description: Some("private task description".to_owned()),
            },
        );
        let projected = provider_observations(&subagent);
        let timeline = timeline_observations(&projected);
        let ObservationKindV1::SubagentStarted {
            correlation_id,
            class,
        } = &timeline[0].kind
        else {
            panic!("expected subagent observation");
        };
        assert!(correlation_id.starts_with("sub-"));
        assert_ne!(correlation_id, "raw-provider-subagent-id");
        assert_eq!(class, "Search");
        let wire = serde_json::to_string(&projected).unwrap();
        assert!(!wire.contains("raw-provider-subagent-id"));
        assert!(!wire.contains("private task description"));
        assert!(!wire.contains("research-agent-with-private-label"));

        let same_source_local_id = ProviderEvent::SubagentStarted {
            agent_id: "same-provider-local-id".to_owned(),
            agent_type: Some("task".to_owned()),
            description: None,
        };
        let managed = provider_observations(&provider_control_event(
            AdapterFamily::Hook,
            same_source_local_id.clone(),
        ));
        let structured = provider_observations(&provider_control_event(
            AdapterFamily::OneShot,
            same_source_local_id,
        ));
        let managed = timeline_observations(&managed);
        let structured = timeline_observations(&structured);
        let ObservationKindV1::SubagentStarted {
            correlation_id: managed_correlation,
            ..
        } = &managed[0].kind
        else {
            panic!("expected managed subagent observation");
        };
        let ObservationKindV1::SubagentStarted {
            correlation_id: structured_correlation,
            ..
        } = &structured[0].kind
        else {
            panic!("expected structured subagent observation");
        };
        assert_ne!(managed_correlation, structured_correlation);

        let thinking = provider_control_event(
            AdapterFamily::Pipe,
            ProviderEvent::Thinking {
                text: "private hidden reasoning canary".to_owned(),
            },
        );
        let thinking = provider_observations(&thinking);
        let timeline = timeline_observations(&thinking);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].kind, ObservationKindV1::Working);
        assert!(!serde_json::to_string(&thinking)
            .unwrap()
            .contains("private hidden reasoning canary"));

        let requested = provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::InteractionRequested {
                request_id: Some("private-request".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "PowerShell".to_owned(),
                prompt: "private approval prompt".to_owned(),
                agent_id: None,
            },
        );
        let requested = provider_observations(&requested);
        let requested = timeline_observations(&requested);
        let ObservationKindV1::ApprovalRequested { correlation_id, .. } = &requested[0].kind
        else {
            panic!("expected approval request observation");
        };
        let interaction_correlation = correlation_id.clone();
        let resolved = ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 42,
            command_id: None,
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(3),
            event: ControlEventKind::InteractionResolved {
                interaction_id: gate4agent_types::ProviderInteractionId(8),
                outcome: gate4agent_types::ProviderInteractionOutcome::Approved,
            },
        };
        let resolved = provider_observations(&resolved);
        let ObservationKindV1::InteractionResolved {
            correlation_id,
            outcome,
        } = &resolved[0].kind
        else {
            panic!("expected interaction resolution observation");
        };
        assert_eq!(correlation_id, &interaction_correlation);
        assert_eq!(*outcome, ObservationInteractionOutcomeV1::Approved);
    }

    #[test]
    fn pty_hint_never_projects_authoritative_completion() {
        let completed = provider_control_event(
            AdapterFamily::PtySemantic,
            ProviderEvent::TurnCompleted {
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 30,
                    cache_write_tokens: 40,
                    reasoning_tokens: 50,
                    context_window: Some(128_000),
                },
                is_cumulative: true,
            },
        );
        let projected = provider_observations(&completed);
        assert!(projected.is_empty());
        assert!(timeline_observations(&projected).is_empty());
        let tool_completed = provider_control_event(
            AdapterFamily::PtySemantic,
            ProviderEvent::ToolCompleted {
                id: "private-tool-id".to_owned(),
                output: "private output".to_owned(),
                is_error: false,
                duration_ms: Some(5),
                agent_id: None,
            },
        );
        let projected = provider_observations(&tool_completed);
        assert!(projected.is_empty());
        assert!(timeline_observations(&projected).is_empty());
        let subagent_completed = provider_control_event(
            AdapterFamily::PtySemantic,
            ProviderEvent::SubagentStopped {
                agent_id: "private-subagent-id".to_owned(),
            },
        );
        let projected = provider_observations(&subagent_completed);
        assert!(projected.is_empty());
        assert!(timeline_observations(&projected).is_empty());

        let working = provider_control_event(
            AdapterFamily::PtySemantic,
            ProviderEvent::WorkingObserved,
        );
        let projected = provider_observations(&working);
        assert_eq!(projected.len(), 1);
        let timeline = timeline_observations(&projected);
        assert_eq!(timeline[0].evidence, ObservationEvidenceV1::PtyHint);
        assert_eq!(timeline[0].kind, ObservationKindV1::Working);
    }

    #[test]
    fn subagent_stop_without_provider_outcome_projects_unknown_success() {
        let stopped = provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::SubagentStopped {
                agent_id: "private-subagent-id".to_owned(),
            },
        );
        let projected = provider_observations(&stopped);
        let timeline = timeline_observations(&projected);
        let ObservationKindV1::SubagentCompleted { success, .. } = &timeline[0].kind else {
            panic!("expected subagent completion observation");
        };
        assert_eq!(*success, None);
    }

    #[test]
    fn unproven_codex_workflow_tool_payloads_do_not_project_detail() {
        for (name, input_json, private_value) in [
            (
                "plan_update",
                r#"{"summary":"private plan summary","status":"in_progress"}"#,
                "private plan summary",
            ),
            (
                "FileChange",
                r#"{"path":"C:\\private\\host.rs","status":"in_progress"}"#,
                r#"C:\\private\\host.rs"#,
            ),
        ] {
            let event = provider_control_event(
                AdapterFamily::Pipe,
                ProviderEvent::ToolStarted {
                    id: "private-workflow-id".to_owned(),
                    name: name.to_owned(),
                    input_json: input_json.to_owned(),
                    agent_id: None,
                },
            );
            let projected = provider_observations(&event);
            assert_eq!(projected.len(), 1);
            let timeline = timeline_observations(&projected);
            assert_eq!(
                timeline[0].evidence,
                ObservationEvidenceV1::StructuredProvider,
            );
            assert!(matches!(
                &timeline[0].kind,
                ObservationKindV1::ToolStarted { .. }
            ));
            assert!(!matches!(
                &timeline[0].kind,
                ObservationKindV1::TodoSnapshot { .. }
                    | ObservationKindV1::FileChanged { .. }
            ));
            let wire = serde_json::to_string(&projected).unwrap();
            assert!(!wire.contains("private-workflow-id"));
            assert!(!wire.contains(private_value));
        }

        for name in ["plan_update", "FileChange"] {
            let pty = provider_control_event(
                AdapterFamily::PtySemantic,
                ProviderEvent::ToolStarted {
                    id: "private-workflow-id".to_owned(),
                    name: name.to_owned(),
                    input_json: r#"{"path":"C:\\private\\host.rs"}"#.to_owned(),
                    agent_id: None,
                },
            );
            let projected = provider_observations(&pty);
            assert_eq!(projected.len(), 1);
            let timeline = timeline_observations(&projected);
            assert_eq!(timeline[0].evidence, ObservationEvidenceV1::PtyHint);
            assert!(matches!(
                &timeline[0].kind,
                ObservationKindV1::ToolStarted { .. }
            ));
        }
    }

    #[test]
    fn resync_strips_observations_without_capability() {
        let mut events = vec![
            NodeEventEnvelope {
                sequence: 1,
                event: NodeEvent::Observation {
                    address: address(),
                    observation: ObservationV1 {
                        source_sequence: 1,
                        observed_at_unix_ms: Some(1),
                        evidence: ObservationEvidenceV1::NodeLifecycle,
                        kind: ObservationKindV1::SessionStarted,
                        truncated: false,
                    },
                },
            },
            NodeEventEnvelope {
                sequence: 2,
                event: NodeEvent::ManagedObservation {
                    record_id: SessionRecordId::new("record-a").unwrap(),
                    observation: ObservationV1 {
                        source_sequence: 1,
                        observed_at_unix_ms: Some(1),
                        evidence: ObservationEvidenceV1::NodeLifecycle,
                        kind: ObservationKindV1::SessionStarted,
                        truncated: false,
                    },
                },
            },
            NodeEventEnvelope {
                sequence: 3,
                event: NodeEvent::ControllerChanged { controller: None },
            },
        ];
        strip_observation_events(&mut events);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event, NodeEvent::ControllerChanged { .. }));

        let runtime = NodeEventEnvelope {
            sequence: 4,
            event: NodeEvent::Observation {
                address: address(),
                observation: ObservationV1 {
                    source_sequence: 2,
                    observed_at_unix_ms: Some(2),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                    kind: ObservationKindV1::Working,
                    truncated: false,
                },
            },
        };
        assert!(project_event_without_managed_observation(runtime).is_some());
        let managed = NodeEventEnvelope {
            sequence: 5,
            event: NodeEvent::ManagedObservation {
                record_id: SessionRecordId::new("record-a").unwrap(),
                observation: ObservationV1 {
                    source_sequence: 2,
                    observed_at_unix_ms: Some(2),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                    kind: ObservationKindV1::Working,
                    truncated: false,
                },
            },
        };
        assert!(project_event_without_managed_observation(managed).is_none());
    }

    #[test]
    fn provider_gap_receives_capabilities_and_preserves_source_sequence() {
        let control = ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 99,
            command_id: None,
            instance_id: AgentInstanceId(7),
            generation: SessionGeneration(3),
            event: ControlEventKind::ProviderGap {
                sequence: 44,
                source: provider_source(AdapterFamily::Hook, "grok"),
                source_sequence: 11,
                missed: 2,
            },
        };
        let projected = provider_observations(&control);
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].source_sequence, 11);
        assert!(matches!(
            &projected[0].kind,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::Hook,
                source_adapter,
                capabilities,
            } if source_adapter == "grok"
                && capabilities == &hook_observation_capabilities("grok")
        ));
        assert_eq!(projected[1].source_sequence, 11);
        assert_eq!(projected[1].kind, ObservationKindV1::Gap { missed: 2 });
    }

    #[test]
    fn managed_inline_observation_uses_exact_record_binding() {
        let shared = observation_test_shared();
        let address = address();
        let pending_record_id = SessionRecordId::new("record-inline-pending").unwrap();
        let durable_record_id = SessionRecordId::new("record-inline-durable").unwrap();
        let canonical_root = shared.snapshot().workspaces[0].canonical_root.clone();
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "provider-session-a".to_owned(),
            transcript_path: None,
        };
        shared
            .insert_record(ManagedSessionRecord {
                record_id: durable_record_id.clone(),
                display_name: "inline durable".to_owned(),
                provider: AgentId::new("claude").unwrap(),
                mode: SessionMode::Inline,
                state: ManagedSessionState::Dormant,
                workspace_id: address.workspace_id.clone(),
                canonical_root: canonical_root.clone(),
                provider_session: Some(identity.clone()),
                active_session: None,
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: pending_record_id.clone(),
                display_name: "inline pending".to_owned(),
                provider: AgentId::new("claude").unwrap(),
                mode: SessionMode::Inline,
                state: ManagedSessionState::IdentityPending,
                workspace_id: address.workspace_id.clone(),
                canonical_root,
                provider_session: None,
                active_session: Some(address.clone()),
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 2,
                updated_at_unix_ms: 2,
                last_error: None,
            })
            .unwrap();
        shared.bind_managed_session(
            &address,
            pending_record_id.clone(),
            ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
            None,
        );

        shared.publish_control(provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::SessionIdentityObserved { identity },
        ));
        let rebound_record_id = shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .and_then(|binding| binding.record_id.clone());
        assert_eq!(rebound_record_id.as_ref(), Some(&durable_record_id));

        shared.publish_control(provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::WorkingObserved,
        ));

        let history = shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let runtime = history.events.iter().find_map(|envelope| match &envelope.event {
            NodeEvent::Observation {
                address: observed_address,
                observation,
            } if observed_address == &address => Some(observation),
            _ => None,
        });
        let managed = history.events.iter().find_map(|envelope| match &envelope.event {
            NodeEvent::ManagedObservation {
                record_id: observed_record_id,
                observation,
            } if observed_record_id == &durable_record_id => Some(observation),
            _ => None,
        });
        assert!(runtime.is_some());
        assert_eq!(managed, runtime);
        assert!(!history.events.iter().any(|envelope| matches!(
            &envelope.event,
            NodeEvent::ManagedObservation { record_id, .. }
                if record_id == &pending_record_id
        )));
    }

    #[test]
    fn runtime_unmanaged_has_no_managed_event() {
        let shared = observation_test_shared();
        let address = address();
        shared.bind_session_with_policy(
            &address,
            ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
            None,
        );

        shared.publish_control(provider_control_event(
            AdapterFamily::Hook,
            ProviderEvent::WorkingObserved,
        ));

        let history = shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(history.events.iter().any(|envelope| matches!(
            &envelope.event,
            NodeEvent::Observation { address: observed, .. } if observed == &address
        )));
        assert!(!history.events.iter().any(|envelope| matches!(
            &envelope.event,
            NodeEvent::ManagedObservation { .. }
        )));
    }
}

#[cfg(test)]
mod standalone_capability_platform_tests {
    use super::*;

    #[test]
    fn standalone_workspace_capability_requires_windows_identity_guards() {
        let capabilities = baseline_capabilities().unwrap();
        let advertised = capabilities.iter().any(|capability| {
            capability.as_str() == NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY
        });
        assert_eq!(advertised, cfg!(windows));

        let request = NodeRequest::CreateStandaloneWorkspace {
            workspace_id: WorkspaceId::new("standalone-platform").unwrap(),
            root: OpaqueHostPath::utf8("standalone-platform-root".to_owned()).unwrap(),
            initial_branch: None,
        };
        assert_eq!(
            request_uses_unnegotiated_capability(&request, &capabilities),
            !cfg!(windows),
        );
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use gate4agent_types::ProviderActivity;
    use crate::session_environment::{
        NodeSecretReference, NodeSecretResolveError, NodeSecretValue,
        NodeSessionEnvironmentMutation, NodeSessionFile, NodeSessionPathBinding,
        NodeSessionPathClass,
    };
    use gate4agent_catalog::EnvMutation;
    use gate4agent_runtime_native::{
        NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver,
        NativeLaunchProfile, NativeLaunchProfileId,
    };
    use std::ffi::OsString;
    use std::sync::Arc;

    struct EmptyEnvironmentResolver;

    struct FixtureSecretResolver {
        deny: Arc<AtomicBool>,
    }

    impl NodeSecretResolver for FixtureSecretResolver {
        fn resolve(
            &self,
            _reference: &NodeSecretReference,
        ) -> Result<NodeSecretValue, NodeSecretResolveError> {
            if self.deny.load(Ordering::Acquire) {
                Err(NodeSecretResolveError::Denied)
            } else {
                NodeSecretValue::text("fixture-secret")
                    .map_err(|_| NodeSecretResolveError::Unavailable)
            }
        }
    }

    impl NativeChildEnvironmentResolver for EmptyEnvironmentResolver {
        fn resolve_child_environment(
            &self,
        ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
            Ok(vec![EnvMutation {
                key: OsString::from("GATE4AGENT_TEST_PROFILE"),
                value: None,
            }])
        }
    }

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn terminal_test_shared_with_mode(mode: WorktreeServiceMode) -> NodeShared {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()
        .with_worktree_service_mode(mode);
        NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-terminal-test").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        )
    }

    fn terminal_test_shared() -> NodeShared {
        terminal_test_shared_with_mode(WorktreeServiceMode::Manual)
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn managed_worktree_v2_failure_probe_keeps_only_the_latest_code() {
        let probe = SpawnManagedWorktreeV2FailureProbe::default();
        assert_eq!(probe.latest_code(), None);

        probe.record(Some(NodeFailureCode::ManagedWorktreeOwnershipConflict));
        assert_eq!(
            probe.latest_code(),
            Some(NodeFailureCode::ManagedWorktreeOwnershipConflict),
        );

        probe.record(Some(NodeFailureCode::SpawnDeadlineExceeded));
        assert_eq!(
            probe.latest_code(),
            Some(NodeFailureCode::SpawnDeadlineExceeded),
        );

        probe.record(None);
        assert_eq!(probe.latest_code(), None);
    }

    #[test]
    fn launch_snapshot_is_authoritative_bounded_and_path_free() {
        let source = temporary_workspace_root("launch-inventory-source");
        let allocation = temporary_workspace_root("launch-inventory-allocation");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&allocation).unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let mut workspace = WorkspaceConfig::new(workspace_id.clone(), &source)
            .unwrap()
            .with_worktree_service_mode(WorktreeServiceMode::Managed);
        for index in 0..MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE {
            workspace = workspace
                .with_managed_worktree_profile(
                    ManagedWorktreeProfile::new(
                        WorktreeProfileId::new(format!("profile-{index}")).unwrap(),
                        crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
                        &allocation,
                        "gate4agent",
                        "HEAD",
                        ManagedWorktreeRetention::RemoveWhenReleased,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let overflow = workspace.clone().with_managed_worktree_profile(
            ManagedWorktreeProfile::new(
                WorktreeProfileId::new("overflow").unwrap(),
                crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
                &allocation,
                "gate4agent",
                "HEAD",
                ManagedWorktreeRetention::Retain,
            )
            .unwrap(),
        );
        assert!(matches!(
            overflow,
            Err(NodeServerError::ManagedWorktreeProfileCapacity {
                max: MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE,
                ..
            }),
        ));

        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-launch-inventory").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let snapshot = shared.snapshot();
        let launch = snapshot.launch_inventory.as_ref().unwrap();
        assert_eq!(launch.spawn_profiles.as_ref().unwrap().len(), 1);
        assert!(launch.bundles.as_ref().unwrap().is_empty());
        let profiles = snapshot.workspaces[0]
            .managed_worktree_profiles
            .as_ref()
            .unwrap();
        assert_eq!(
            snapshot.workspaces[0].worktree_service_mode,
            Some(WorktreeServiceMode::Managed),
        );
        assert_eq!(
            profiles.profiles.len(),
            MAX_MANAGED_WORKTREE_PROFILES_PER_WORKSPACE,
        );
        let public_inventory = serde_json::to_string(&(launch, profiles)).unwrap();
        for forbidden in ["allocation_root", "branch_prefix", "base", "prompt", "environment", "home"] {
            assert!(!public_inventory.contains(forbidden), "launch inventory leaked {forbidden}");
        }
        let legacy = snapshot_for_wire(
            &shared,
            true,
            true,
            false,
            false,
            true,
            false,
            true,
            false,
            true,
        );
        assert!(legacy.launch_inventory.is_none());
        assert!(legacy.workspaces[0].managed_worktree_profiles.is_none());

        drop(shared);
        std::fs::remove_dir_all(source).unwrap();
        std::fs::remove_dir_all(allocation).unwrap();
    }

    #[test]
    fn agent_progress_projection_omits_sensitive_provider_payloads_and_is_capability_gated() {
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY
        }));
        let source = gate4agent_types::ProviderSource {
            family: AdapterFamily::PtySemantic,
            binding: AdapterBinding::new(
                gate4agent_types::AdapterId::new("claude").unwrap(),
                "fixture/v1",
                gate4agent_types::AdapterVerification::SyntheticFixture,
            )
            .unwrap(),
        };
        let provider = ProviderSnapshot {
            sequence: 17,
            session: Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "PRIVATE_PROVIDER_SESSION".to_owned(),
                transcript_path: Some(r"C:\PRIVATE_PROVIDER_PATH".to_owned()),
            }),
            model: Some("PRIVATE_MODEL".to_owned()),
            tools: vec!["PRIVATE_TOOL_INVENTORY".to_owned()],
            completed_turns: 3,
            usage: gate4agent_types::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                reasoning_tokens: 50,
                context_window: Some(200_000),
            },
            lead_activity: ProviderActivity::WaitingForInput,
            activity: ProviderActivity::WaitingForInput,
            current_prompt: Some("PRIVATE_CURRENT_PROMPT".to_owned()),
            active_tools: (0..9)
                .map(|index| gate4agent_types::ActiveProviderTool {
                    id: format!("private-tool-id-{index}"),
                    name: if index == 0 {
                        format!("  Read\u{0000}{}", "x".repeat(80))
                    } else {
                        format!("Tool-{index}")
                    },
                    input_json: "PRIVATE_TOOL_INPUT_JSON".to_owned(),
                })
                .collect(),
            interactions: vec![gate4agent_types::ProviderInteraction {
                id: gate4agent_types::ProviderInteractionId(17),
                source: source.clone(),
                provider_request_id: Some("PRIVATE_PROVIDER_REQUEST".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: " Approve\u{0007}Tool ".to_owned(),
                prompt: "PRIVATE_INTERACTION_PROMPT".to_owned(),
                agent_id: Some("PRIVATE_INTERACTION_AGENT".to_owned()),
                resume_lead_activity: Some(ProviderActivity::Working),
                status: ProviderInteractionStatus::Pending,
            }],
            subagents: vec![gate4agent_types::ProviderSubagent {
                source: source.clone(),
                provider_agent_id: "PRIVATE_SUBAGENT_ID".to_owned(),
                agent_type: Some("PRIVATE_SUBAGENT_TYPE".to_owned()),
                description: Some("PRIVATE_SUBAGENT_DESCRIPTION".to_owned()),
            }],
            sources: vec![gate4agent_types::ProviderSourceCursor {
                source,
                sequence: 17,
                gap_count: 2,
                stale: true,
            }],
            last_event: Some(ProviderEvent::Error {
                message: "PRIVATE_PROVIDER_ERROR".to_owned(),
            }),
            gap_count: 2,
            stale: true,
        };
        let address = terminal_address(3);
        let entry = agent_progress_from_provider_snapshot(address.clone(), &provider).unwrap();
        assert_eq!(entry.address, address);
        assert_eq!(entry.progress.provider_sequence, 17);
        assert_eq!(entry.progress.active_tool_count, 9);
        assert_eq!(
            entry.progress.active_tool_labels.len(),
            MAX_AGENT_PROGRESS_ACTIVE_TOOL_LABELS,
        );
        assert_eq!(entry.progress.subagent_count, 1);
        assert_eq!(entry.progress.last_event_kind, Some(AgentProgressEventKindV1::Error));
        assert!(entry.progress.truncated);
        let encoded = serde_json::to_string(&entry).unwrap();
        for forbidden in [
            "PRIVATE_PROVIDER_SESSION",
            "PRIVATE_PROVIDER_PATH",
            "PRIVATE_MODEL",
            "PRIVATE_TOOL_INVENTORY",
            "PRIVATE_CURRENT_PROMPT",
            "PRIVATE_TOOL_INPUT_JSON",
            "PRIVATE_PROVIDER_REQUEST",
            "PRIVATE_INTERACTION_PROMPT",
            "PRIVATE_INTERACTION_AGENT",
            "PRIVATE_SUBAGENT_ID",
            "PRIVATE_SUBAGENT_TYPE",
            "PRIVATE_SUBAGENT_DESCRIPTION",
            "PRIVATE_PROVIDER_ERROR",
            "input_json",
            "current_prompt",
            "session_id",
        ] {
            assert!(!encoded.contains(forbidden), "agent progress leaked {forbidden}");
        }

        let snapshot = NodeSnapshot {
            node_id: NodeId::new("node-progress-test").unwrap(),
            enabled_providers: vec![agent("claude")],
            provider_runtime_statuses: ProviderRuntimeStatuses::default(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            agent_progress: vec![entry],
        };
        let mut legacy_reply = ResponseEnvelope {
            request_id: 1,
            result: Ok(NodeResponse::Snapshot {
                event_sequence: 0,
                controller: None,
                snapshot: snapshot.clone(),
            }),
        };
        project_response_without_agent_progress(&mut legacy_reply);
        let Ok(NodeResponse::Snapshot { snapshot: legacy, .. }) = legacy_reply.result else {
            panic!("agent progress projection changed response kind");
        };
        assert!(legacy.agent_progress.is_empty());
        assert_eq!(snapshot.agent_progress.len(), 1);
    }

    fn terminal_address(generation: u64) -> SessionAddress {
        SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(41),
                generation: SessionGeneration(generation),
            },
        }
    }

    fn spawn_spec_fixture() -> SpawnSpec {
        SpawnSpec {
            target: crate::protocol::SpawnTarget {
                node_id: NodeId::new("node-terminal-test").unwrap(),
                workspace_id: WorkspaceId::new("primary").unwrap(),
                worktree_id: None,
            },
            profile_id: crate::protocol::SpawnProfileId::new("default").unwrap(),
            expected_profile_revision: crate::protocol::SpawnProfileRevision::new("builtin-v1")
                .unwrap(),
            overrides: crate::protocol::SpawnOverrides::default(),
            deadline_ms: crate::protocol::SpawnDeadlineMs::new(30_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("spawn-spec-fixture").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
        }
    }

    fn environment_profile_test_shared(
    ) -> (NodeShared, NativeRuntime, ResolvedEnvironmentProfileReceipt) {
        let profile_id = SpawnEnvironmentProfileId::new("local-claude").unwrap();
        let profile_revision = crate::protocol::SpawnEnvironmentProfileRevision::new(
            "local-claude-r1",
        )
        .unwrap();
        let spawn_profiles = SpawnProfileRegistry::new([
            crate::protocol::SpawnProfileDefaults {
                profile_id: crate::protocol::SpawnProfileId::new("default").unwrap(),
                revision: crate::protocol::SpawnProfileRevision::new("test-r1").unwrap(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                terminal_size: gate4agent_types::TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                prompt: None,
                bundle_id: None,
                context_id: None,
                environment_profile_id: Some(profile_id.clone()),
            },
        ])
        .unwrap();
        let catalog = active_registry().unwrap();
        let (handle, mut runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let control = runtime.native_launch_profile_control();
        let node_profile = NodeEnvironmentProfile::new(
            profile_id.clone(),
            profile_revision.clone(),
            agent("claude"),
            [NativeLaunchProfile::new(
                NativeLaunchProfileId::new("local-claude-pty").unwrap(),
                agent("claude"),
                TransportKind::Pty,
                vec![OsString::from("GATE4AGENT_TEST_PROFILE")],
                Arc::new(EmptyEnvironmentResolver),
            )
            .unwrap()],
        )
        .unwrap();
        let (binding, native_profiles, _materialization) = node_profile.into_parts();
        for native_profile in native_profiles {
            runtime.upsert_native_launch_profile(native_profile).unwrap();
        }
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new_with_incarnation(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-terminal-test").unwrap(),
            NodeIncarnationId::from_bytes([0; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            vec![workspace],
            vec![agent("claude")],
            ProviderRuntimeStatuses::default(),
            None,
            Vec::new(),
            Vec::new(),
            spawn_profiles,
            Some(control),
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            MUTATION_SETTLE_TIMEOUT_MS.saturating_add(READINESS_SETTLE_HEADROOM_MS),
        );
        shared
            .environment_profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(binding.id.clone(), binding);
        (
            shared,
            runtime,
            ResolvedEnvironmentProfileReceipt {
                profile_id,
                profile_revision,
            },
        )
    }

    fn materialization_test_shared(
        deny_secret: bool,
    ) -> (
        NodeShared,
        NativeRuntime,
        ResolvedEnvironmentProfileReceipt,
        PathBuf,
        Arc<AtomicBool>,
    ) {
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        materialization_test_shared_with_workspace(deny_secret, workspace)
    }

    fn materialization_test_shared_with_workspace(
        deny_secret: bool,
        workspace: WorkspaceConfig,
    ) -> (
        NodeShared,
        NativeRuntime,
        ResolvedEnvironmentProfileReceipt,
        PathBuf,
        Arc<AtomicBool>,
    ) {
        let root = temporary_workspace_root(if deny_secret {
            "materialization-denied"
        } else {
            "materialization-ready"
        });
        std::fs::create_dir_all(&root).unwrap();
        let materialization_root = root.join("session-environments");
        let deny = Arc::new(AtomicBool::new(deny_secret));
        let resolver = Arc::new(FixtureSecretResolver {
            deny: Arc::clone(&deny),
        });
        let materializer = SessionEnvironmentMaterializer::new(
            materialization_root,
            resolver,
        )
        .unwrap();
        let profile_id = SpawnEnvironmentProfileId::new("materialized-claude").unwrap();
        let profile_revision = crate::protocol::SpawnEnvironmentProfileRevision::new(
            "materialized-claude-r1",
        )
        .unwrap();
        let materialization = NodeSessionMaterializationProfile::new(
            vec![NodeSessionEnvironmentMutation::SetSecret {
                key: "GATE4AGENT_SESSION_TOKEN".to_owned(),
                reference: NodeSecretReference::new("fixture-token").unwrap(),
            }],
            vec![NodeSessionPathBinding::new(
                "GATE4AGENT_PROVIDER_HOME",
                NodeSessionPathClass::ProviderHome,
            )
            .unwrap()],
            vec![NodeSessionFile::secret(
                NodeSessionPathClass::Config,
                "auth/token",
                NodeSecretReference::new("fixture-token").unwrap(),
            )
            .unwrap()],
        )
        .unwrap();
        let catalog = active_registry().unwrap();
        let (handle, mut runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let control = runtime.native_launch_profile_control();
        let node_profile = NodeEnvironmentProfile::new_with_materialization(
            profile_id.clone(),
            profile_revision.clone(),
            agent("claude"),
            [NativeLaunchProfile::new(
                NativeLaunchProfileId::new("materialized-claude-pty").unwrap(),
                agent("claude"),
                TransportKind::Pty,
                vec![OsString::from("GATE4AGENT_TEST_PROFILE")],
                Arc::new(EmptyEnvironmentResolver),
            )
            .unwrap()],
            Some(materialization),
        )
        .unwrap();
        let (binding, native_profiles, materialization) = node_profile.into_parts();
        for native_profile in native_profiles {
            runtime.upsert_native_launch_profile(native_profile).unwrap();
        }
        let shared = NodeShared::new_with_incarnation(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-materialization-test").unwrap(),
            NodeIncarnationId::from_bytes([9; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            vec![workspace],
            vec![agent("claude")],
            ProviderRuntimeStatuses::default(),
            None,
            Vec::new(),
            Vec::new(),
            SpawnProfileRegistry::default(),
            Some(control),
            None,
            Some(materializer),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            MUTATION_SETTLE_TIMEOUT_MS.saturating_add(READINESS_SETTLE_HEADROOM_MS),
        );
        shared
            .environment_profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(binding.id.clone(), binding);
        shared
            .environment_materialization_profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(profile_id.clone(), materialization.unwrap());
        (
            shared,
            runtime,
            ResolvedEnvironmentProfileReceipt {
                profile_id,
                profile_revision,
            },
            root,
            deny,
        )
    }

    #[cfg(feature = "fixture")]
    #[derive(Clone, Copy)]
    enum CodexHomeProfile {
        Missing,
        WrongClass,
        Exact,
    }

    #[cfg(feature = "fixture")]
    fn codex_bundle_test_shared(
        home_profile: CodexHomeProfile,
    ) -> (
        NodeShared,
        NativeRuntime,
        ResolvedEnvironmentProfileReceipt,
        NodeBundle,
        PathBuf,
    ) {
        const ROOT_MANIFEST: &[u8] = br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#;
        const CLAUDE_MANIFEST: &[u8] =
            br#"{"name":"review-tools","version":"1.0.0","description":"Review helpers"}"#;
        const SKILL: &[u8] = b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n";
        const DIGEST: &str =
            "sha256:941f20868a6ef49b4329bf1bb1368515763f5b64d8279b05cd9700966083d707";

        let (shared, mut runtime, _claude_receipt, root, _deny) =
            materialization_test_shared(false);
        let profile_id = SpawnEnvironmentProfileId::new(match home_profile {
            CodexHomeProfile::Missing => "codex-home-missing",
            CodexHomeProfile::WrongClass => "codex-home-wrong",
            CodexHomeProfile::Exact => "codex-home-exact",
        })
        .unwrap();
        let profile_revision = crate::protocol::SpawnEnvironmentProfileRevision::new(
            "codex-home-r1",
        )
        .unwrap();
        let materialization = match home_profile {
            CodexHomeProfile::Missing => None,
            CodexHomeProfile::WrongClass | CodexHomeProfile::Exact => {
                Some(NodeSessionMaterializationProfile::new(
                    Vec::new(),
                    vec![NodeSessionPathBinding::new(
                        "CODEX_HOME",
                        if matches!(home_profile, CodexHomeProfile::Exact) {
                            NodeSessionPathClass::ProviderHome
                        } else {
                            NodeSessionPathClass::Config
                        },
                    )
                    .unwrap()],
                    Vec::new(),
                )
                .unwrap())
            }
        };
        let node_profile = NodeEnvironmentProfile::new_with_materialization(
            profile_id.clone(),
            profile_revision.clone(),
            agent("codex"),
            [NativeLaunchProfile::new(
                NativeLaunchProfileId::new(format!("{profile_id}-pty")).unwrap(),
                agent("codex"),
                TransportKind::Pty,
                vec![OsString::from("GATE4AGENT_TEST_PROFILE")],
                Arc::new(EmptyEnvironmentResolver),
            )
            .unwrap()],
            materialization,
        )
        .unwrap();
        let (binding, native_profiles, materialization) = node_profile.into_parts();
        for native_profile in native_profiles {
            runtime.upsert_native_launch_profile(native_profile).unwrap();
        }
        shared
            .environment_profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(binding.id.clone(), binding);
        if let Some(materialization) = materialization {
            shared
                .environment_materialization_profiles
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(profile_id.clone(), materialization);
        }

        let bundle_source = root.join(format!("bundle-source-{profile_id}"));
        std::fs::create_dir_all(bundle_source.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(bundle_source.join("skills/review-code")).unwrap();
        std::fs::write(bundle_source.join("plugin.json"), ROOT_MANIFEST).unwrap();
        std::fs::write(
            bundle_source.join(".claude-plugin/plugin.json"),
            CLAUDE_MANIFEST,
        )
        .unwrap();
        std::fs::write(bundle_source.join("skills/review-code/SKILL.md"), SKILL).unwrap();
        crate::bundle_catalog::protect_bundle_source_tree_fixture(&bundle_source).unwrap();
        let bundle = NodeBundle::new(
            crate::protocol::SpawnBundleId::new("review-tools").unwrap(),
            crate::protocol::SpawnBundleRevision::new("review-tools-r1").unwrap(),
            crate::protocol::SpawnBundleDigest::new(DIGEST).unwrap(),
            &bundle_source,
        )
        .unwrap();
        *shared
            .bundle_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            BundleCatalog::new([bundle.clone()]).unwrap();
        (
            shared,
            runtime,
            ResolvedEnvironmentProfileReceipt {
                profile_id,
                profile_revision,
            },
            bundle,
            root,
        )
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn codex_bundle_preflight_rejects_missing_or_wrong_home_before_materialization() {
        for home_profile in [CodexHomeProfile::Missing, CodexHomeProfile::WrongClass] {
            let (shared, runtime, receipt, bundle, root) =
                codex_bundle_test_shared(home_profile);
            let error = shared
                .bundle_layout(
                    &agent("codex"),
                    SessionMode::Pty,
                    Some(&receipt),
                    &bundle,
                )
                .unwrap_err();
            assert_eq!(error.code, NodeFailureCode::BundleBindingMismatch);
            assert!(shared
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty());
            let materialization_root = root.join("session-environments");
            assert!(!std::fs::read_dir(&materialization_root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir())));
            drop(shared);
            drop(runtime);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn codex_bundle_exact_home_profile_materializes_only_the_skill_tree() {
        let (shared, runtime, receipt, bundle, root) =
            codex_bundle_test_shared(CodexHomeProfile::Exact);
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(310),
                generation: SessionGeneration(1),
            },
        };
        let (overlay, guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("codex"),
                SessionMode::Pty,
                Some(&receipt),
                Some(&bundle.receipt()),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(matches!(overlay, Some(PreparedNativeLaunchOverlay::Instance(_))));
        let ownership = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(guard.id().unwrap())
            .unwrap()
            .clone();
        let (profile, _, _) = shared
            .materialization_profile(
                &agent("codex"),
                SessionMode::Pty,
                Some(&receipt),
                Some(&bundle.receipt()),
                None,
            )
            .unwrap()
            .unwrap();
        let environment = shared
            .session_environment_materializer
            .as_ref()
            .unwrap()
            .resolve_environment(&ownership, &profile)
            .unwrap();
        assert!(environment.iter().any(|mutation| {
            mutation.key == OsString::from("CODEX_HOME")
                && mutation.value.as_deref() == Some(ownership.provider_home().as_os_str())
        }));
        assert_eq!(
            std::fs::read(ownership.provider_home().join("skills/review-code/SKILL.md"))
                .unwrap(),
            b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n",
        );
        assert!(std::fs::read_dir(ownership.bundle_root())
            .unwrap()
            .next()
            .is_none());
        drop(guard);
        drop(shared);
        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn codex_cold_record_resume_revalidates_the_isolated_skill_tree() {
        let (shared, runtime, receipt, bundle, root) =
            codex_bundle_test_shared(CodexHomeProfile::Exact);
        let bundle_receipt = bundle.receipt();
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(311),
                generation: SessionGeneration(1),
            },
        };
        let (_overlay, guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("codex"),
                SessionMode::Pty,
                Some(&receipt),
                Some(&bundle_receipt),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let materialization_id = guard.id().unwrap().clone();
        let skill_path = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&materialization_id)
            .unwrap()
            .provider_home()
            .join("skills/review-code/SKILL.md");
        let record_id = shared
            .bind_spawn_session_with_materialization(
                &address,
                agent("codex"),
                SessionMode::Pty,
                ProviderRuntimePolicy::new(true, false, false, true, false).unwrap(),
                SpawnRecordPolicy::ProviderIdentityOnly,
                Some(receipt.clone()),
                Some(bundle_receipt.clone()),
                None,
                Some(materialization_id.clone()),
            )
            .unwrap()
            .unwrap();
        guard.retain();
        std::fs::write(skill_path, b"changed-after-record-bind").unwrap();

        let error = match shared.resolve_record_materialization(
                &record_id,
                &agent("codex"),
                SessionMode::Pty,
                Some(&receipt),
                Some(&bundle_receipt),
                None,
            ) {
                Ok(_) => panic!("changed Codex skill tree unexpectedly revalidated"),
                Err(error) => error,
            };
        assert_eq!(error.code, NodeFailureCode::BundleMaterializationFailed);
        assert_eq!(
            shared
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&materialization_id)
                .unwrap()
                .state(),
            MaterializationState::RecoveryRequired,
        );
        drop(shared);
        drop(runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    fn bind_record_owned_materialization_fixture(
        shared: &NodeShared,
        receipt: &ResolvedEnvironmentProfileReceipt,
        instance_id: u64,
    ) -> (SessionAddress, SessionRecordId, MaterializationId, PathBuf) {
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(instance_id),
                generation: SessionGeneration(1),
            },
        };
        let (overlay, materialization_guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("claude"),
                SessionMode::Pty,
                Some(receipt),
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let materialization_id = materialization_guard.id().unwrap().clone();
        let materialization_root = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&materialization_id)
            .unwrap()
            .root()
            .to_path_buf();
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(receipt),
            )
            .unwrap()
            .unwrap();
        assert!(shared
            .install_prepared_launch_overlay(address.session.instance_id, overlay.unwrap())
            .unwrap()
            .is_none());
        let policy = ProviderRuntimePolicy::new(true, false, false, true, false).unwrap();
        let record_id = shared
            .bind_spawn_session_with_materialization(
                &address,
                agent("claude"),
                SessionMode::Pty,
                policy,
                SpawnRecordPolicy::ProviderIdentityOnly,
                Some(receipt.clone()),
                None,
                None,
                Some(materialization_id.clone()),
            )
            .unwrap()
            .unwrap();
        selection.retain();
        materialization_guard.retain();
        (address, record_id, materialization_id, materialization_root)
    }

    #[test]
    fn child_environment_profile_capability_is_state_aware_and_legacy_safe() {
        let (shared, _runtime, environment_profile) = environment_profile_test_shared();
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY
        }));
        let support = node_compatibility_support_for_manifest(&[], &[]).unwrap();
        assert_eq!(support.state_schema.versions.maximum(), NODE_STATE_SCHEMA_V10);
        let mut spec = spawn_spec_fixture();
        spec.expected_profile_revision =
            crate::protocol::SpawnProfileRevision::new("test-r1").unwrap();
        assert!(matches!(
            &spec.overrides.environment_profile_id,
            crate::protocol::SpawnOverride::Inherit,
        ));
        assert!(!NodeRequest::SpawnSpec { spec: spec.clone() }
            .requires_child_environment_profile_capability());
        assert!(request_requires_child_environment_profile(
            &shared,
            &NodeRequest::SpawnSpec { spec: spec.clone() },
        ));

        let address = terminal_address(1);
        shared.bind_session_with_policy(
            &address,
            ProviderRuntimePolicy::raw_pty(),
            Some(environment_profile.clone()),
        );
        assert!(request_requires_child_environment_profile(
            &shared,
            &NodeRequest::Input {
                session: address.clone(),
                text: "x".to_owned(),
            },
        ));

        let mut record = record("claude", "profile-record");
        record.active_session = Some(address.clone());
        record.environment_profile = Some(environment_profile.clone());
        let record_id = record.record_id.clone();
        shared.insert_record(record.clone()).unwrap();
        assert!(request_requires_child_environment_profile(
            &shared,
            &NodeRequest::ResumeSessionRecord {
                record_id,
                terminal_size: gate4agent_types::TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                initial_prompt: None,
            },
        ));

        let snapshot = snapshot_for_wire(
            &shared, false, true, true, true, false, false, false, false, true,
        );
        assert_eq!(snapshot.session_records.len(), 1);
        assert!(snapshot.session_records[0].environment_profile.is_none());
        let event = project_event_without_child_environment_profile(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::SessionRecordUpserted { record },
        });
        let NodeEvent::SessionRecordUpserted { record } = event.event else {
            panic!("record projection changed the event kind");
        };
        assert!(record.environment_profile.is_none());

        let resolved = shared.resolve_spawn_spec(&spec).unwrap();
        let mut reply = ResponseEnvelope {
            request_id: 1,
            result: Ok(NodeResponse::SpawnSpecAccepted {
                receipt: resolved.receipt_with_environment(
                    shared.incarnation_id,
                    address,
                    Some(environment_profile),
                ),
            }),
        };
        project_response_without_child_environment_profile(&mut reply);
        assert_eq!(
            reply.result.unwrap_err().code,
            NodeFailureCode::UnsupportedCapability,
        );
    }

    #[test]
    fn spawn_profile_environment_authority_is_exact_and_legacy_safe() {
        let (shared, _runtime, environment_profile) = environment_profile_test_shared();
        let snapshot = shared.snapshot();
        let profiles = snapshot
            .launch_inventory
            .as_ref()
            .and_then(|inventory| inventory.spawn_profiles.as_ref())
            .unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].environment_profile, Some(environment_profile));
        assert!(snapshot.requires_child_environment_profile_capability());

        let mut legacy = snapshot.clone();
        clear_snapshot_child_environment_profiles(&mut legacy);
        assert!(legacy
            .launch_inventory
            .as_ref()
            .unwrap()
            .spawn_profiles
            .as_ref()
            .unwrap()[0]
            .environment_profile
            .is_none());
        assert!(!legacy.requires_child_environment_profile_capability());

        let profile = shared.spawn_profiles.iter().next().unwrap().clone();
        let bindings = shared
            .environment_profiles
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut provider_mismatch = bindings.clone();
        provider_mismatch
            .values_mut()
            .next()
            .unwrap()
            .provider = agent("codex");
        assert!(resolved_spawn_profile_summary(&profile, &provider_mismatch).is_none());
        let mut mode_mismatch = profile;
        mode_mismatch.mode = SessionMode::Inline;
        assert!(resolved_spawn_profile_summary(&mode_mismatch, &bindings).is_none());

        shared
            .environment_profiles
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        assert!(shared
            .snapshot()
            .launch_inventory
            .unwrap()
            .spawn_profiles
            .unwrap()
            .is_empty());
    }

    #[test]
    fn session_bundle_capability_is_state_aware_and_legacy_safe() {
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY
        }));
        let shared = terminal_test_shared();
        let bundle = ResolvedBundleReceipt {
            id: crate::protocol::SpawnBundleId::new("review-bundle").unwrap(),
            revision: crate::protocol::SpawnBundleRevision::new("r1").unwrap(),
            digest: crate::protocol::SpawnBundleDigest::new(format!(
                "sha256:{}",
                "0".repeat(64),
            ))
            .unwrap(),
        };
        let mut bundled = record("claude", "bundle-record");
        bundled.bundle = Some(bundle.clone());
        let record_id = bundled.record_id.clone();
        shared.insert_record(bundled.clone()).unwrap();
        assert!(request_requires_session_bundle(
            &shared,
            &NodeRequest::ForgetSessionRecord {
                record_id: record_id.clone(),
            },
        ));

        let snapshot = snapshot_for_wire(
            &shared, false, true, true, true, true, false, false, false, true,
        );
        assert!(snapshot.session_records[0].bundle.is_none());
        let launch = snapshot.launch_inventory.as_ref().unwrap();
        assert!(launch.spawn_profiles.is_some());
        assert!(launch.bundles.is_none());
        let event = project_event_without_session_bundle(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::SessionRecordUpserted { record: bundled },
        });
        let NodeEvent::SessionRecordUpserted { record } = event.event else {
            panic!("record projection changed the event kind");
        };
        assert!(record.bundle.is_none());

        let resolved = shared.resolve_spawn_spec(&spawn_spec_fixture()).unwrap();
        let mut reply = ResponseEnvelope {
            request_id: 1,
            result: Ok(NodeResponse::SpawnSpecAccepted {
                receipt: resolved.receipt_with_materialization(
                    shared.incarnation_id,
                    terminal_address(17),
                    None,
                    Some(bundle),
                    None,
                ),
            }),
        };
        project_response_without_session_bundle(&mut reply);
        assert_eq!(
            reply.result.unwrap_err().code,
            NodeFailureCode::UnsupportedCapability,
        );
    }

    #[test]
    fn history_context_pack_capability_redacts_loaded_messages_and_legacy_metadata() {
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_HISTORY_CONTEXT_PACK_CAPABILITY
        }));
        let context_id = SpawnContextId::new("context-review").unwrap();
        let context = ResolvedContextPackReceipt {
            id: context_id.clone(),
            digest: crate::protocol::SpawnContextDigest::new(format!(
                "sha256:{}",
                "c".repeat(64),
            ))
            .unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("node-source").unwrap(),
                source_session: terminal_address(1),
                source_provider: agent("qwen-code"),
            },
            source_message_count: 2,
            retained_message_count: 2,
            byte_len: 256,
            truncated: false,
        };
        assert!(context.is_valid());
        let mut bound_record = record("codex", "context-record");
        bound_record.context_id = Some(context_id);
        bound_record.context = Some(context.clone());
        let session = gate4agent_types::SessionSnapshot {
            instance_id: AgentInstanceId(91),
            agent_id: agent("qwen-code"),
            transport: TransportKind::Pty,
            generation: SessionGeneration(3),
            status: SessionStatus::Running,
            pending_operation: None,
            pending_input: None,
            process_id: Some(9001),
            terminal_size: None,
            terminal_frame: None,
            terminal_stale: None,
            session_options: None,
            capabilities: gate4agent_types::CapabilitySnapshot::default(),
            history: gate4agent_types::HistorySnapshot {
                pending: None,
                candidates: vec![HistoryCandidateSummary {
                    id: "candidate-opaque".to_owned(),
                    session_id_hint: "session-hint".to_owned(),
                    modified_at_unix_ms: Some(1),
                }],
                loaded_candidate_id: Some("candidate-opaque".to_owned()),
                loaded: Some(HistorySessionRecord {
                    session_id: "private-vendor-session".to_owned(),
                    title: Some("private title".to_owned()),
                    cwd: Some(r"C:\private\history-root".to_owned()),
                    model: Some("private-model".to_owned()),
                    message_count: 2,
                    completed_turn_count: None,
                    total_tokens: 19,
                    messages: vec![
                        gate4agent_types::HistoryMessageRecord {
                            role: gate4agent_types::HistoryMessageRole::User,
                            text: "F7_PRIVATE_HISTORY_MESSAGE".to_owned(),
                        },
                        gate4agent_types::HistoryMessageRecord {
                            role: gate4agent_types::HistoryMessageRole::Assistant,
                            text: "private answer".to_owned(),
                        },
                    ],
                }),
                last_error: None,
            },
            resume: gate4agent_types::ResumeSnapshot::default(),
            foreground: gate4agent_types::ForegroundSnapshot::default(),
            provider: gate4agent_types::ProviderSnapshot::default(),
        };
        let original = NodeSnapshot {
            node_id: NodeId::new("node-terminal-test").unwrap(),
            enabled_providers: vec![agent("qwen-code"), agent("codex")],
            provider_runtime_statuses: ProviderRuntimeStatuses::default(),
            workspaces: vec![WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                canonical_root: opaque_windows_path(
                    std::env::current_dir().unwrap().to_string_lossy().into_owned(),
                ),
                sessions: vec![session],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
            session_records: vec![bound_record],
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            agent_progress: Vec::new(),
        };

        let mut negotiated = original.clone();
        project_snapshot_history_for_wire(&mut negotiated, true);
        assert_eq!(negotiated.workspaces[0].sessions[0].history.candidates.len(), 1);
        assert!(negotiated.workspaces[0].sessions[0].history.loaded.is_none());
        assert_eq!(negotiated.session_records[0].context.as_ref(), Some(&context));
        assert!(!serde_json::to_string(&negotiated)
            .unwrap()
            .contains("F7_PRIVATE_HISTORY_MESSAGE"));

        let mut legacy = original;
        project_snapshot_history_for_wire(&mut legacy, false);
        clear_snapshot_context_packs(&mut legacy);
        assert_eq!(
            legacy.workspaces[0].sessions[0].history,
            gate4agent_types::HistorySnapshot::default(),
        );
        assert!(legacy.session_records[0].context_id.is_none());
        assert!(legacy.session_records[0].context.is_none());
        let encoded = serde_json::to_string(&legacy).unwrap();
        assert!(!encoded.contains("F7_PRIVATE_HISTORY_MESSAGE"));
        assert!(!encoded.contains("context-review"));

        let mut reply = ResponseEnvelope {
            request_id: 7,
            result: Ok(NodeResponse::ContextPackExported { context }),
        };
        project_response_without_context_pack(&mut reply);
        assert_eq!(
            reply.result.unwrap_err().code,
            NodeFailureCode::UnsupportedCapability,
        );
    }

    #[test]
    fn forgetting_unbound_context_pack_evicts_replay_but_retained_record_is_busy() {
        let shared = terminal_test_shared();
        let history = HistorySessionRecord {
            session_id: "fixture-history".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![gate4agent_types::HistoryMessageRecord {
                role: gate4agent_types::HistoryMessageRole::User,
                text: "bounded handoff".to_owned(),
            }],
        };
        let pack = NodeContextPack::export(
            ContextPackLineageReceipt {
                source_node_id: shared.node_id.clone(),
                source_session: terminal_address(1),
                source_provider: agent("claude"),
            },
            &history,
        )
        .unwrap();
        let context = shared
            .context_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pack)
            .unwrap();
        let mut retained = record("codex", "context-retained-record");
        retained.context_id = Some(context.id.clone());
        retained.context = Some(context.clone());
        let record_id = retained.record_id.clone();
        shared.insert_record(retained).unwrap();
        assert_eq!(
            shared.forget_context_pack(&context.id).unwrap_err().code,
            NodeFailureCode::ContextPackBusy,
        );
        assert!(shared
            .context_catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&context.id)
            .is_some());
        shared
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .remove(&record_id);

        let mut spec = spawn_spec_fixture();
        spec.overrides.context_id = crate::protocol::SpawnOverride::Set {
            value: context.id.clone(),
        };
        let resolved = shared.resolve_spawn_spec(&spec).unwrap();
        let receipt = resolved.receipt_with_materialization(
            shared.incarnation_id,
            terminal_address(2),
            None,
            None,
            Some(context.clone()),
        );
        assert!(receipt.context_binding_is_valid());
        shared.remember_spawn_spec(spec.clone(), Ok(receipt), Instant::now());

        shared.forget_context_pack(&context.id).unwrap();
        assert!(shared
            .context_catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&context.id)
            .is_none());
        assert!(shared
            .replay_spawn_spec(&spec, Instant::now())
            .unwrap()
            .is_none());
    }

    #[test]
    fn environment_profile_selection_is_owned_by_the_exact_session_binding() {
        let (shared, mut runtime, environment_profile) = environment_profile_test_shared();
        let address = terminal_address(1);
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&environment_profile),
            )
            .unwrap()
            .unwrap();
        shared.bind_session_with_policy(
            &address,
            ProviderRuntimePolicy::raw_pty(),
            Some(environment_profile.clone()),
        );
        selection.retain();
        let native_id = NativeLaunchProfileId::new("local-claude-pty").unwrap();
        assert!(matches!(
            runtime.remove_native_launch_profile(&native_id),
            Err(gate4agent_runtime_native::NativeLaunchProfileError::ProfileInUse),
        ));
        assert!(shared.remove_binding(&address).is_some());
        assert_eq!(runtime.remove_native_launch_profile(&native_id), Ok(true));

        let mismatched = ResolvedEnvironmentProfileReceipt {
            profile_id: environment_profile.profile_id,
            profile_revision: crate::protocol::SpawnEnvironmentProfileRevision::new(
                "local-claude-r2",
            )
            .unwrap(),
        };
        let error = match shared.select_environment_profile(
                AgentInstanceId(42),
                &agent("claude"),
                SessionMode::Pty,
                Some(&mismatched),
            ) {
            Ok(_) => panic!("mismatched environment profile revision was selected"),
            Err(error) => error,
        };
        assert_eq!(
            error.code,
            NodeFailureCode::EnvironmentProfileBindingMismatch,
        );
        assert!(!shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&AgentInstanceId(42)));
    }

    #[test]
    fn combined_environment_and_bundle_overlay_clears_with_its_binding() {
        let (shared, mut runtime, environment_profile) = environment_profile_test_shared();
        let address = terminal_address(1);
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&environment_profile),
            )
            .unwrap()
            .unwrap();
        let overlay = NativeInstanceLaunchOverlay::new(
            agent("claude"),
            TransportKind::Pty,
            vec![EnvMutation {
                key: OsString::from("GATE4AGENT_SESSION_TOKEN"),
                value: Some(OsString::from("fixture-secret")),
            }],
            vec![
                OsString::from("--plugin-dir"),
                std::env::current_dir().unwrap().into_os_string(),
            ],
        )
        .unwrap();
        let instance_overlay = shared
            .install_prepared_launch_overlay(
                address.session.instance_id,
                PreparedNativeLaunchOverlay::Instance(overlay),
            )
            .unwrap()
            .unwrap();
        let bundle = ResolvedBundleReceipt {
            id: crate::protocol::SpawnBundleId::new("combined-bundle").unwrap(),
            revision: crate::protocol::SpawnBundleRevision::new("combined-r1").unwrap(),
            digest: crate::protocol::SpawnBundleDigest::new(format!(
                "sha256:{}",
                "0".repeat(64),
            ))
            .unwrap(),
        };
        shared.bind_session_with_materialization(
            &address,
            ProviderRuntimePolicy::raw_pty(),
            Some(environment_profile),
            Some(bundle),
            None,
            None,
        );
        selection.retain();
        instance_overlay.retain();

        let native_id = NativeLaunchProfileId::new("local-claude-pty").unwrap();
        assert!(matches!(
            runtime.remove_native_launch_profile(&native_id),
            Err(gate4agent_runtime_native::NativeLaunchProfileError::ProfileInUse),
        ));
        assert!(shared.remove_binding(&address).is_some());
        assert!(!shared
            .native_launch_profile_control
            .as_ref()
            .unwrap()
            .clear_native_instance_launch_overlay(address.session.instance_id));
        assert_eq!(runtime.remove_native_launch_profile(&native_id), Ok(true));
    }

    #[test]
    fn session_environment_materialization_failure_is_pre_child_and_durable() {
        let (shared, runtime, receipt, root, _deny) = materialization_test_shared(true);
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(201),
                generation: SessionGeneration(1),
            },
        };
        let error = match shared.prepare_session_materialization(
            &address,
            &agent("claude"),
            SessionMode::Pty,
            Some(&receipt),
            None,
            None,
            None,
        ) {
            Ok(_) => panic!("denied local reference unexpectedly materialized"),
            Err(error) => error,
        };
        assert_eq!(error.code, NodeFailureCode::BackendOperationFailed);
        assert!(shared.handle.snapshot().sessions.is_empty());
        assert!(shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_environment_binding_retains_on_stop_and_cleans_after_remove_reap() {
        let (shared, runtime, receipt, root, _deny) = materialization_test_shared(false);
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(202),
                generation: SessionGeneration(1),
            },
        };
        let (overlay, materialization_guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let materialization_id = materialization_guard.id().unwrap().clone();
        let materialization_root = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&materialization_id)
            .unwrap()
            .root()
            .to_path_buf();
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
            )
            .unwrap()
            .unwrap();
        assert!(shared
            .install_prepared_launch_overlay(address.session.instance_id, overlay.unwrap())
            .unwrap()
            .is_none());
        shared.bind_session_with_materialization(
            &address,
            ProviderRuntimePolicy::raw_pty(),
            Some(receipt),
            None,
            None,
            Some(materialization_id),
        );
        selection.retain();
        materialization_guard.retain();

        assert!(materialization_root.is_dir());
        assert!(shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&address.session.instance_id));

        let removed = shared.remove_binding(&address).unwrap();
        shared
            .cleanup_session_owned_materialization(&address, &removed)
            .unwrap();
        assert!(!materialization_root.exists());
        assert!(shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn record_owned_session_environment_reuses_on_resume_and_forget_cleans() {
        let (shared, runtime, receipt, root, _deny) = materialization_test_shared(false);
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(203),
                generation: SessionGeneration(1),
            },
        };
        let (overlay, materialization_guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let materialization_id = materialization_guard.id().unwrap().clone();
        let materialization_root = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&materialization_id)
            .unwrap()
            .root()
            .to_path_buf();
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
            )
            .unwrap()
            .unwrap();
        assert!(shared
            .install_prepared_launch_overlay(address.session.instance_id, overlay.unwrap())
            .unwrap()
            .is_none());
        let policy = ProviderRuntimePolicy::new(true, false, false, true, false).unwrap();
        let record_id = shared
            .bind_spawn_session_with_materialization(
                &address,
                agent("claude"),
                SessionMode::Pty,
                policy,
                SpawnRecordPolicy::ProviderIdentityOnly,
                Some(receipt.clone()),
                None,
                None,
                Some(materialization_id.clone()),
            )
            .unwrap()
            .unwrap();
        selection.retain();
        materialization_guard.retain();
        assert_eq!(
            shared
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&materialization_id)
                .unwrap()
                .owner(),
            &MaterializationOwner::Record {
                record_id: record_id.clone(),
            },
        );

        let removed = shared.remove_binding(&address).unwrap();
        shared
            .cleanup_session_owned_materialization(&address, &removed)
            .unwrap();
        assert!(materialization_root.is_dir());
        assert!(shared
            .resolve_record_materialization(
                &record_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
                None,
                None,
            )
            .unwrap()
            .is_some());
        {
            let mut records = shared
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let record = records.records.get_mut(&record_id).unwrap();
            record.active_session = None;
            record.state = ManagedSessionState::Unavailable;
        }
        shared.forget_session_record(&record_id).await.unwrap();
        assert!(!materialization_root.exists());
        assert!(shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn record_materialization_revalidation_failure_persists_recovery_required() {
        let (mut shared, runtime, receipt, root, _deny) = materialization_test_shared(false);
        let state_path = root.join("node-state-v6.json");
        shared.state_path = Some(state_path.clone());
        let (_address, _record_id, materialization_id, materialization_root) =
            bind_record_owned_materialization_fixture(&shared, &receipt, 205);
        std::fs::write(
            materialization_root.join(".gate4agent-materialization-owner"),
            b"tampered",
        )
        .unwrap();

        let error = match shared.resolve_record_materialization(
            &_record_id,
            &agent("claude"),
            SessionMode::Pty,
            Some(&receipt),
            None,
            None,
        ) {
            Ok(_) => panic!("tampered owner marker unexpectedly revalidated"),
            Err(error) => error,
        };
        assert_eq!(error.code, NodeFailureCode::BackendOperationFailed);
        assert_eq!(
            shared
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&materialization_id)
                .unwrap()
                .state(),
            MaterializationState::RecoveryRequired,
        );
        let loaded = session_registry::load(Some(&state_path), &shared.node_id).unwrap();
        assert_eq!(loaded.materializations.len(), 1);
        assert_eq!(
            loaded.materializations[0].state(),
            MaterializationState::RecoveryRequired,
        );
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transient_record_materialization_resolution_failure_remains_ready() {
        let (shared, runtime, receipt, root, deny) = materialization_test_shared(false);
        let (_address, record_id, materialization_id, _materialization_root) =
            bind_record_owned_materialization_fixture(&shared, &receipt, 206);
        deny.store(true, Ordering::Release);

        let error = match shared.resolve_record_materialization(
            &record_id,
            &agent("claude"),
            SessionMode::Pty,
            Some(&receipt),
            None,
            None,
        ) {
            Ok(_) => panic!("denied transient resolver unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.code, NodeFailureCode::BackendOperationFailed);
        assert_eq!(
            shared
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&materialization_id)
                .unwrap()
                .state(),
            MaterializationState::Ready,
        );
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_missing_record_materialization_is_not_resumable() {
        let (shared, runtime, receipt, root, _deny) = materialization_test_shared(false);
        let record_id = SessionRecordId::new("sr-missing-materialization").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: record_id.clone(),
                display_name: "missing materialization".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::Dormant,
                workspace_id: WorkspaceId::new("primary").unwrap(),
                canonical_root: opaque_windows_path(
                    std::env::current_dir().unwrap().to_string_lossy().into_owned(),
                ),
                provider_session: Some(gate4agent_types::ProviderSessionIdentity {
                    key: gate4agent_types::ProviderSessionKey::SessionId,
                    id: "provider-session-missing-materialization".to_owned(),
                    transcript_path: None,
                }),
                active_session: None,
                environment_profile: Some(receipt),
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();

        shared.reconcile_materializations();
        let record = shared.record(&record_id).unwrap();
        assert_eq!(record.state, ManagedSessionState::Unavailable);
        assert!(record.provider_session.is_none());
        assert_eq!(
            record.last_error.as_deref(),
            Some("environment-profile-unavailable"),
        );
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn managed_worktree_cleanup_waits_for_session_environment_cleanup() {
        let (shared, runtime, receipt, root, _deny) = materialization_test_shared(false);
        let lease = install_managed_lease(&shared);
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(204),
                generation: SessionGeneration(1),
            },
        };
        let (overlay, materialization_guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
                None,
                None,
                Some(lease.lease_id.clone()),
            )
            .unwrap()
            .unwrap();
        let materialization_id = materialization_guard.id().unwrap().clone();
        let materialization_root = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&materialization_id)
            .unwrap()
            .root()
            .to_path_buf();
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
            )
            .unwrap()
            .unwrap();
        assert!(shared
            .install_prepared_launch_overlay(address.session.instance_id, overlay.unwrap())
            .unwrap()
            .is_none());
        shared.bind_session_with_materialization(
            &address,
            ProviderRuntimePolicy::raw_pty(),
            Some(receipt),
            None,
            None,
            Some(materialization_id),
        );
        selection.retain();
        materialization_guard.retain();

        let blocked = shared
            .cleanup_managed_worktree(&lease.lease_id, false)
            .await
            .unwrap_err();
        assert_eq!(blocked.code, NodeFailureCode::ManagedWorktreeRecoveryRequired);
        assert!(materialization_root.is_dir());
        let removed = shared.remove_binding(&address).unwrap();
        shared
            .cleanup_session_owned_materialization(&address, &removed)
            .unwrap();
        assert!(!materialization_root.exists());
        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn raw_managed_record_is_inventory_only_and_does_not_block_session_cleanup() {
        let git_root = temporary_workspace_root("raw-managed-inventory");
        let source = git_root.join("source");
        let allocation = git_root.join("allocation");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&allocation).unwrap();
        run_test_git(&source, &["init"]);
        run_test_git(&source, &["config", "user.email", "gate4agent@example.invalid"]);
        run_test_git(&source, &["config", "user.name", "Gate4Agent Test"]);
        std::fs::write(source.join("seed.txt"), "seed\n").unwrap();
        run_test_git(&source, &["add", "seed.txt"]);
        run_test_git(&source, &["commit", "-m", "seed"]);
        let profile = ManagedWorktreeProfile::new(
            WorktreeProfileId::new("default").unwrap(),
            crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
            &allocation,
            "gate4agent",
            "HEAD",
            ManagedWorktreeRetention::RemoveWhenReleased,
        )
        .unwrap();
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            &source,
        )
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(profile.clone())
        .unwrap();
        let (shared, mut runtime, receipt, materialization_fixture_root, _deny) =
            materialization_test_shared_with_workspace(false, workspace);
        let target = PathBuf::from(profile.allocation_root()).join("mw-raw-inventory");
        let source_text = source.to_string_lossy().into_owned();
        let target_text = target.to_string_lossy().into_owned();
        let created = create_git_worktree(
            &source_text,
            &target_text,
            "gate4agent/mw-raw-inventory",
            Some("HEAD"),
        )
        .await
        .unwrap();
        let lease = ManagedWorktreeLeaseRecord {
            lease_id: ManagedWorktreeLeaseId::new("mw-raw-inventory").unwrap(),
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("managed-raw-inventory").unwrap(),
            profile_id: profile.profile_id().clone(),
            profile_revision: profile.revision().clone(),
            target_root: created.path.clone(),
            branch: "gate4agent/mw-raw-inventory".to_owned(),
            base_commit: created.head.clone(),
            expected_head: Some(created.head.clone()),
            retention: ManagedWorktreeRetention::RemoveWhenReleased,
            state: ManagedWorktreeLeaseState::Ready,
            session_holders: Vec::new(),
            record_holders: Vec::new(),
            cleanup_failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        shared
            .workspaces
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(lease.workspace_id.clone(), created.path.clone());
        *shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagedWorktreeRegistry::from_records(vec![lease.clone()], Vec::new()).unwrap();
        let address = SessionAddress {
            workspace_id: lease.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(205),
                generation: SessionGeneration(1),
            },
        };
        let (overlay, materialization_guard) = shared
            .prepare_session_materialization(
                &address,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
                None,
                None,
                Some(lease.lease_id.clone()),
            )
            .unwrap()
            .unwrap();
        let materialization_id = materialization_guard.id().unwrap().clone();
        let materialization_root = shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&materialization_id)
            .unwrap()
            .root()
            .to_path_buf();
        let selection = shared
            .select_environment_profile(
                address.session.instance_id,
                &agent("claude"),
                SessionMode::Pty,
                Some(&receipt),
            )
            .unwrap()
            .unwrap();
        assert!(shared
            .install_prepared_launch_overlay(address.session.instance_id, overlay.unwrap())
            .unwrap()
            .is_none());
        let record_id = shared
            .bind_spawn_session_with_materialization(
                &address,
                agent("claude"),
                SessionMode::Pty,
                ProviderRuntimePolicy::raw_pty(),
                SpawnRecordPolicy::Always,
                Some(receipt.clone()),
                None,
                None,
                Some(materialization_id.clone()),
            )
            .unwrap()
            .unwrap();
        selection.retain();
        materialization_guard.retain();
        let record = shared.record(&record_id).unwrap();
        assert_eq!(record.state, ManagedSessionState::Live);
        assert!(record.provider_session.is_none());
        assert!(record.environment_profile.is_none());
        assert!(record.bundle.is_none());
        assert!(record.context_id.is_none());
        assert!(record.context.is_none());
        assert_eq!(record.active_session.as_ref(), Some(&address));
        assert_eq!(
            shared
                .materializations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&materialization_id)
                .unwrap()
                .owner(),
            &MaterializationOwner::Session {
                incarnation_id: shared.incarnation_id,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            },
        );
        {
            let mut bindings = shared
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            bindings
                .get_mut(&address.session.instance_id)
                .unwrap()
                .managed_worktree_lease_id = Some(lease.lease_id.clone());
        }
        let lease_snapshot = shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bind_session(
                &lease.lease_id,
                ManagedWorktreeSessionHolder {
                    incarnation_id: shared.incarnation_id,
                    instance_id: address.session.instance_id,
                    generation: address.session.generation,
                },
                None,
                unix_time_ms(),
            )
            .unwrap();
        assert_eq!(lease_snapshot.active_session_count, 1);
        assert_eq!(lease_snapshot.managed_record_count, 0);
        shared.persist_state().unwrap();
        assert!(shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&lease.lease_id)
            .unwrap()
            .record_holders
            .is_empty());
        shared
            .handle
            .dispatch(shared.prepare_command(ControlCommand::Register {
                instance_id: address.session.instance_id,
                agent_id: agent("claude"),
                transport: TransportKind::Pty,
            }))
            .unwrap();
        runtime.tick().await;
        shared.reconcile_managed_record(
            &record_id,
            &address,
            &ControlEvent {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                sequence: 1,
                command_id: None,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
                event: ControlEventKind::Removed,
            },
            false,
            false,
        );
        let remove = shared.remove_session(&address);
        let drive_runtime = async {
            runtime.tick().await;
        };
        let (removed, ()) = tokio::join!(remove, drive_runtime);
        removed.unwrap();

        assert!(!materialization_root.exists());
        assert!(!Path::new(&created.path).exists());
        assert_eq!(
            shared
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&lease.lease_id)
                .unwrap()
                .state,
            ManagedWorktreeLeaseState::Removed,
        );
        let renamed = shared
            .rename_session_record(&record_id, "raw managed inventory".to_owned())
            .unwrap();
        assert_eq!(renamed.record_id, record_id);
        assert_eq!(renamed.display_name, "raw managed inventory");
        assert_eq!(renamed.state, ManagedSessionState::Unavailable);
        assert!(renamed.active_session.is_none());

        drop(shared);
        drop(runtime);
        std::fs::remove_dir_all(materialization_fixture_root).unwrap();
        std::fs::remove_dir_all(git_root).unwrap();
    }

    fn install_managed_lease(shared: &NodeShared) -> ManagedWorktreeLeaseRecord {
        let root = std::env::current_dir().unwrap()
            .join("managed-test-target")
            .to_string_lossy()
            .into_owned();
        let lease = ManagedWorktreeLeaseRecord {
            lease_id: ManagedWorktreeLeaseId::new("mw-test").unwrap(),
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("managed-test").unwrap(),
            profile_id: WorktreeProfileId::new("default").unwrap(),
            profile_revision: crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
            target_root: root.clone(),
            branch: "gate4agent/mw-test".to_owned(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            expected_head: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            retention: ManagedWorktreeRetention::RemoveWhenReleased,
            state: ManagedWorktreeLeaseState::Ready,
            session_holders: Vec::new(),
            record_holders: Vec::new(),
            cleanup_failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        shared.workspaces.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(lease.workspace_id.clone(), root);
        *shared.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagedWorktreeRegistry::from_records(vec![lease.clone()], Vec::new()).unwrap();
        lease
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn monitoring_fixture_managed_spawn_retains_effective_record_holder_and_converges_live() {
        let mut shared = terminal_test_shared();
        shared.fixture_semantic_hook_policy = true;
        let lease = install_managed_lease(&shared);
        let address = SessionAddress {
            workspace_id: lease.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(206),
                generation: SessionGeneration(1),
            },
        };
        let pre_admission_policy = ProviderRuntimePolicy::raw_pty();
        let effective_runtime_policy = shared.effective_spawn_runtime_policy(
            &agent("claude"),
            SessionMode::Pty,
            pre_admission_policy,
        );
        assert_ne!(effective_runtime_policy, pre_admission_policy);
        assert!(effective_runtime_policy.provider_session_identity);

        let record_id = shared
            .bind_spawn_session(
                &address,
                agent("claude"),
                SessionMode::Pty,
                effective_runtime_policy,
                SpawnRecordPolicy::Always,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            shared.record(&record_id).unwrap().state,
            ManagedSessionState::IdentityPending,
        );
        let (record_holder, raw_inventory_record_id) = {
            let mut bindings = shared
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let binding = bindings.get_mut(&address.session.instance_id).unwrap();
            binding.managed_worktree_lease_id = Some(lease.lease_id.clone());
            managed_spawn_record_ownership(binding, effective_runtime_policy)
        };
        assert_eq!(record_holder.as_ref(), Some(&record_id));
        assert!(raw_inventory_record_id.is_none());
        let snapshot = shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bind_session(
                &lease.lease_id,
                ManagedWorktreeSessionHolder {
                    incarnation_id: shared.incarnation_id,
                    instance_id: address.session.instance_id,
                    generation: address.session.generation,
                },
                record_holder,
                unix_time_ms(),
            )
            .unwrap();
        assert_eq!(snapshot.managed_record_count, 1);
        assert_eq!(
            shared
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&lease.lease_id)
                .unwrap()
                .record_holders,
            vec![record_id.clone()],
        );

        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "monitoring-fixture-managed-session".to_owned(),
            transcript_path: None,
        };
        shared.publish_control(ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 1,
            command_id: None,
            instance_id: address.session.instance_id,
            generation: address.session.generation,
            event: ControlEventKind::ProviderEvent {
                sequence: 1,
                source: gate4agent_types::ProviderSource {
                    family: AdapterFamily::Hook,
                    binding: AdapterBinding::new(
                        gate4agent_types::AdapterId::new("claude-code").unwrap(),
                        "fixture/v1",
                        gate4agent_types::AdapterVerification::SyntheticFixture,
                    )
                    .unwrap(),
                },
                source_sequence: 1,
                event: ProviderEvent::SessionIdentityObserved {
                    identity: identity.clone(),
                },
            },
        });

        let live = shared.record(&record_id).unwrap();
        assert_eq!(live.state, ManagedSessionState::Live);
        assert_eq!(live.provider_session.as_ref(), Some(&identity));
        assert_eq!(live.active_session.as_ref(), Some(&address));
        assert_eq!(
            shared
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&lease.lease_id)
                .unwrap()
                .record_holders,
            vec![record_id],
        );
    }

    fn managed_v2_test_shared(
        allocation_root: &Path,
        state_path: Option<PathBuf>,
        profile_revision: &str,
    ) -> NodeShared {
        let profile = ManagedWorktreeProfile::new(
            WorktreeProfileId::new("default").unwrap(),
            WorktreeProfileRevision::new(profile_revision).unwrap(),
            allocation_root,
            "gate4agent",
            "HEAD",
            ManagedWorktreeRetention::RemoveWhenReleased,
        )
        .unwrap();
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(profile)
        .unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-terminal-test").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        shared.state_path = state_path;
        shared
    }

    fn managed_v2_request(idempotency_key: &str) -> ManagedWorktreeSpawnRequestV2 {
        let mut spawn_spec = spawn_spec_fixture();
        spawn_spec.idempotency_key = SpawnIdempotencyKey::new(idempotency_key).unwrap();
        ManagedWorktreeSpawnRequestV2 {
            spawn_spec,
            worktree_profile_id: WorktreeProfileId::new("default").unwrap(),
            expected_profile_revision: WorktreeProfileRevision::new("v1").unwrap(),
        }
    }

    #[test]
    fn managed_spawn_receipt_rebinds_allocated_worktree_and_roundtrips_wire() {
        let shared = terminal_test_shared();
        let mut lease = install_managed_lease(&shared);
        let session = SessionAddress {
            workspace_id: lease.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(73),
                generation: SessionGeneration(4),
            },
        };
        lease.state = ManagedWorktreeLeaseState::InUse;
        lease.session_holders.push(ManagedWorktreeSessionHolder {
            incarnation_id: shared.incarnation_id,
            instance_id: session.session.instance_id,
            generation: session.session.generation,
        });
        let resolved = shared.resolve_spawn_spec(&spawn_spec_fixture()).unwrap();
        assert!(resolved.target.worktree_id.is_none());

        let receipt = managed_spawn_receipt(
            &resolved,
            shared.incarnation_id,
            session.clone(),
            lease.snapshot(),
            None,
            None,
            None,
        );
        assert!(resolved.target.worktree_id.is_none());
        assert_eq!(receipt.spawn.target.workspace_id, lease.source_workspace_id);
        assert_eq!(receipt.spawn.target.worktree_id.as_ref(), Some(&lease.workspace_id));
        assert_eq!(receipt.spawn.session, session);
        assert_eq!(receipt.lease.workspace_id, lease.workspace_id);
        let encoded = serde_json::to_vec(&receipt).unwrap();
        let decoded: ManagedWorktreeSpawnReceipt = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, receipt);
    }

    #[tokio::test]
    async fn managed_spawn_v2_rejects_profile_revision_before_allocation() {
        let root = temporary_workspace_root("managed-v2-revision");
        let allocation = root.join("allocation");
        std::fs::create_dir_all(&allocation).unwrap();
        let shared = managed_v2_test_shared(
            &allocation,
            Some(root.join("node-state.json")),
            "v1",
        );
        let mut request = managed_v2_request("managed-v2-revision");
        request.expected_profile_revision = WorktreeProfileRevision::new("v2").unwrap();

        let failure = shared.spawn_managed_worktree_v2(request).await.unwrap_err();

        assert_eq!(
            failure.code,
            NodeFailureCode::ManagedWorktreeProfileRevisionMismatch,
        );
        assert!(shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records()
            .is_empty());
        assert!(shared
            .managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_spawn_v2_concurrent_same_key_has_one_durable_reservation() {
        let root = temporary_workspace_root("managed-v2-concurrent");
        let allocation = root.join("allocation");
        std::fs::create_dir_all(&allocation).unwrap();
        let shared = Arc::new(managed_v2_test_shared(
            &allocation,
            Some(root.join("node-state.json")),
            "v1",
        ));
        let request = managed_v2_request("managed-v2-concurrent");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let shared = Arc::clone(&shared);
            let request = request.clone();
            let barrier = Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                let digest = managed_spawn_request_digest_v2(&request).unwrap();
                barrier.wait();
                shared.acquire_durable_managed_spawn_v2(&request, digest)
            }));
        }
        barrier.wait();
        let results = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    matches!(result, Ok(DurableManagedSpawnReplayDecision::Reserved))
                })
                .count(),
            1,
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    matches!(result, Err(error) if error.code == NodeFailureCode::BackendBusy)
                })
                .count(),
            1,
        );
        assert_eq!(
            shared
                .managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
        );
        assert!(shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records()
            .is_empty());
        drop(shared);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn managed_spawn_v2_linked_pending_reopen_fails_closed_without_reallocation() {
        let root = temporary_workspace_root("managed-v2-linked-pending");
        let allocation = root.join("allocation");
        std::fs::create_dir_all(&allocation).unwrap();
        let state_path = root.join("node-state.json");
        let shared = managed_v2_test_shared(&allocation, Some(state_path.clone()), "v1");
        let lease = install_managed_lease(&shared);
        let request = managed_v2_request("managed-v2-linked-pending");
        assert!(matches!(
            shared
                .acquire_durable_managed_spawn_v2(
                    &request,
                    managed_spawn_request_digest_v2(&request).unwrap(),
                )
                .unwrap(),
            DurableManagedSpawnReplayDecision::Reserved,
        ));
        {
            let _transaction = shared
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            shared
                .link_durable_managed_spawn_v2_lease_locked(
                    &request.spawn_spec.idempotency_key,
                    &lease.lease_id,
                )
                .unwrap();
            shared.persist_state_locked().unwrap();
        }
        drop(shared);

        let mut loaded = session_registry::load(
            Some(&state_path),
            &NodeId::new("node-terminal-test").unwrap(),
        )
        .unwrap();
        let reopened = managed_v2_test_shared(&allocation, Some(state_path), "v2");
        *reopened
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagedWorktreeRegistry::from_records(
                std::mem::take(&mut loaded.managed_worktrees),
                std::mem::take(&mut loaded.managed_worktree_tombstones),
            )
            .unwrap();
        *reopened
            .managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = loaded
            .managed_spawn_replays
            .into_iter()
            .map(|record| (record.idempotency_key.clone(), record))
            .collect();
        let lease_count = reopened
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records()
            .len();

        let failure = reopened
            .spawn_managed_worktree_v2(request)
            .await
            .unwrap_err();

        assert_eq!(
            failure.code,
            NodeFailureCode::ManagedWorktreeRecoveryRequired,
        );
        assert_eq!(
            reopened
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records()
                .len(),
            lease_count,
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn managed_spawn_v2_replays_exact_receipt_after_durable_reopen_and_conflicts() {
        let root = temporary_workspace_root("managed-v2-reopen");
        let allocation = root.join("allocation");
        std::fs::create_dir_all(&allocation).unwrap();
        let state_path = root.join("node-state.json");
        let shared = managed_v2_test_shared(&allocation, Some(state_path.clone()), "v1");
        let lease = install_managed_lease(&shared);
        let request = managed_v2_request("managed-v2-reopen");
        let session = SessionAddress {
            workspace_id: lease.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(91),
                generation: SessionGeneration(1),
            },
        };
        let snapshot = {
            let mut registry = shared
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current = registry.get_mut(&lease.lease_id).unwrap();
            current.state = ManagedWorktreeLeaseState::InUse;
            current.session_holders.push(ManagedWorktreeSessionHolder {
                incarnation_id: shared.incarnation_id,
                instance_id: session.session.instance_id,
                generation: session.session.generation,
            });
            current.snapshot()
        };
        let resolved = shared.resolve_spawn_spec(&request.spawn_spec).unwrap();
        let receipt = managed_spawn_receipt(
            &resolved,
            shared.incarnation_id,
            session,
            snapshot,
            None,
            None,
            None,
        );
        assert!(matches!(
            shared
                .acquire_durable_managed_spawn_v2(
                    &request,
                    managed_spawn_request_digest_v2(&request).unwrap(),
                )
                .unwrap(),
            DurableManagedSpawnReplayDecision::Reserved,
        ));
        {
            let _transaction = shared
                .state_transaction
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            shared
                .link_durable_managed_spawn_v2_lease_locked(
                    &request.spawn_spec.idempotency_key,
                    &lease.lease_id,
                )
                .unwrap();
            shared.persist_state_locked().unwrap();
        }
        shared
            .commit_durable_managed_spawn_v2(&request, receipt.clone())
            .unwrap();
        let legacy_same_key = ManagedWorktreeSpawnRequest {
            spawn_spec: request.spawn_spec.clone(),
            worktree_profile_id: request.worktree_profile_id.clone(),
        };
        shared.remember_managed_spawn_attempt(
            Some(&request.spawn_spec.idempotency_key),
            legacy_same_key.clone(),
            Ok(receipt.clone()),
            Instant::now(),
        );
        assert!(shared
            .replay_managed_spawn(&legacy_same_key, Instant::now())
            .unwrap()
            .is_none());
        drop(shared);

        let mut loaded = session_registry::load(
            Some(&state_path),
            &NodeId::new("node-terminal-test").unwrap(),
        )
        .unwrap();
        assert_eq!(loaded.managed_spawn_replays.len(), 1);
        let reopened = managed_v2_test_shared(&allocation, Some(state_path), "v2");
        *reopened
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagedWorktreeRegistry::from_records(
                std::mem::take(&mut loaded.managed_worktrees),
                std::mem::take(&mut loaded.managed_worktree_tombstones),
            )
            .unwrap();
        *reopened
            .managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = loaded
            .managed_spawn_replays
            .into_iter()
            .map(|record| (record.idempotency_key.clone(), record))
            .collect();
        let lease_count_before = reopened
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records()
            .len();

        assert_eq!(
            reopened
                .spawn_managed_worktree_v2(request.clone())
                .await
                .unwrap(),
            receipt,
        );
        assert_eq!(
            reopened
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records()
                .len(),
            lease_count_before,
        );
        let mut changed = request;
        changed.spawn_spec.deadline_ms = crate::protocol::SpawnDeadlineMs::new(29_000).unwrap();
        let failure = reopened
            .spawn_managed_worktree_v2(changed)
            .await
            .unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnIdempotencyConflict);
        assert_eq!(
            reopened
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records()
                .len(),
            lease_count_before,
        );
        let mut new_key = managed_v2_request("managed-v2-new-after-profile-change");
        new_key.expected_profile_revision = WorktreeProfileRevision::new("v1").unwrap();
        let failure = reopened
            .spawn_managed_worktree_v2(new_key)
            .await
            .unwrap_err();
        assert_eq!(
            failure.code,
            NodeFailureCode::ManagedWorktreeProfileRevisionMismatch,
        );
        assert_eq!(
            reopened
                .managed_worktrees
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records()
                .len(),
            lease_count_before,
        );
        assert_eq!(
            reopened
                .managed_spawn_replays
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_managed_spawn_idempotency_remains_transient_and_exact() {
        let shared = terminal_test_shared();
        let request = ManagedWorktreeSpawnRequest {
            spawn_spec: spawn_spec_fixture(),
            worktree_profile_id: WorktreeProfileId::new("default").unwrap(),
        };
        let expected = failure(NodeFailureCode::BackendBusy, "legacy fixture failure");
        shared.remember_managed_spawn(
            request.clone(),
            Err(expected.clone()),
            Instant::now(),
        );

        assert_eq!(
            shared
                .replay_managed_spawn(&request, Instant::now())
                .unwrap()
                .unwrap(),
            Err(expected),
        );
        assert!(shared
            .managed_spawn_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
    }

    #[tokio::test]
    async fn ordinary_spawn_and_spawn_spec_cannot_bypass_managed_lease_holder_fence() {
        let shared = terminal_test_shared();
        let lease = install_managed_lease(&shared);
        let raw = shared.spawn_session(
            lease.workspace_id.clone(),
            agent("claude"),
            SessionMode::Pty,
            gate4agent_types::TerminalSize { rows: 24, columns: 80 },
            None,
        ).await.unwrap_err();
        assert_eq!(raw.code, NodeFailureCode::ManagedWorktreeOwnershipConflict);
        let mut spec = spawn_spec_fixture();
        spec.target.workspace_id = lease.workspace_id.clone();
        let specified = shared.spawn_from_spec(spec).await.unwrap_err();
        assert_eq!(specified.code, NodeFailureCode::ManagedWorktreeOwnershipConflict);
        let mut selected = spawn_spec_fixture();
        selected.idempotency_key = SpawnIdempotencyKey::new("managed-selected-bypass").unwrap();
        selected.target.worktree_id = Some(lease.workspace_id);
        let selected = shared.spawn_from_spec(selected).await.unwrap_err();
        assert_eq!(selected.code, NodeFailureCode::ManagedWorktreeOwnershipConflict);
        assert!(shared.handle.snapshot().sessions.is_empty());
    }

    #[test]
    fn legacy_projection_omits_managed_snapshot_events_and_path_bearing_diagnostics() {
        let shared = terminal_test_shared();
        let lease = install_managed_lease(&shared);
        let snapshot = snapshot_for_wire(
            &shared, true, true, true, false, true, true, false, false, true,
        );
        assert!(snapshot.managed_worktrees.is_empty());
        assert!(snapshot.workspaces.iter().all(|workspace| {
            workspace.managed_worktree_profiles.is_none()
        }));
        let mut reply = ResponseEnvelope {
            request_id: 1,
            result: Ok(NodeResponse::Resync {
                event_sequence: 1,
                oldest_available_sequence: 1,
                snapshot: shared.snapshot(),
                events: vec![NodeEventEnvelope {
                    sequence: 1,
                    event: NodeEvent::ManagedWorktreeUpserted { lease: lease.snapshot() },
                }],
            }),
        };
        project_response_without_managed_worktrees(&mut reply);
        let Ok(NodeResponse::Resync { snapshot, events, .. }) = reply.result else {
            panic!("projection changed the response kind");
        };
        assert!(snapshot.managed_worktrees.is_empty());
        assert!(events.is_empty());
        let mut git = git_snapshot_for_parser();
        git.branch = Some(lease.branch.clone());
        git.managed_worktree = Some(crate::protocol::ManagedWorktreeGitScope {
            lease_id: lease.lease_id.clone(),
            source_workspace_id: lease.source_workspace_id.clone(),
            branch: lease.branch.clone(),
            base_commit: GitObjectId::new(lease.base_commit.clone()).unwrap(),
            active_session_count: 1,
            managed_record_count: 0,
        });
        let mut inspection_reply = ResponseEnvelope {
            request_id: 2,
            result: Ok(NodeResponse::WorkspaceInspected {
                inspection: WorkspaceInspection {
                    workspace_id: lease.workspace_id.clone(),
                    entries: Vec::new(),
                    tree_truncated: false,
                    git,
                    truncation: None,
                },
            }),
        };
        project_response_without_managed_worktrees(&mut inspection_reply);
        let Ok(NodeResponse::WorkspaceInspected { inspection }) = inspection_reply.result else {
            panic!("projection changed the inspection response kind");
        };
        assert!(inspection.git.managed_worktree.is_none());
        let failure = managed_git_worktree_failure(GitWorktreeError {
            kind: GitWorktreeErrorKind::Failed,
            message: r"git failed at C:\private\managed-target".to_owned(),
        });
        assert!(!failure.message.contains("private"));
        assert!(!failure.message.contains("managed-target"));
    }

    #[tokio::test]
    async fn restart_reconciliation_retains_exact_owned_worktree_and_explicit_cleanup_removes_it() {
        let root = temporary_workspace_root("managed-restart");
        let source = root.join("source");
        let allocation = root.join("allocation");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&allocation).unwrap();
        run_test_git(&source, &["init"]);
        run_test_git(&source, &["config", "user.email", "gate4agent@example.invalid"]);
        run_test_git(&source, &["config", "user.name", "Gate4Agent Test"]);
        std::fs::write(source.join("seed.txt"), "seed\n").unwrap();
        run_test_git(&source, &["add", "seed.txt"]);
        run_test_git(&source, &["commit", "-m", "seed"]);
        let profile = ManagedWorktreeProfile::new(
            WorktreeProfileId::new("default").unwrap(),
            crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
            &allocation,
            "gate4agent",
            "HEAD",
            ManagedWorktreeRetention::Retain,
        ).unwrap();
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            &source,
        ).unwrap()
            .with_worktree_service_mode(WorktreeServiceMode::Managed)
            .with_managed_worktree_profile(profile.clone()).unwrap();
        let target = PathBuf::from(profile.allocation_root()).join("mw-restart");
        let source_text = workspace.canonical_root().to_owned();
        let target_text = target.to_string_lossy().into_owned();
        let created = create_git_worktree(
            &source_text,
            &target_text,
            "gate4agent/mw-restart",
            Some("HEAD"),
        ).await.unwrap();
        let lease = ManagedWorktreeLeaseRecord {
            lease_id: ManagedWorktreeLeaseId::new("mw-restart").unwrap(),
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("managed-restart").unwrap(),
            profile_id: profile.profile_id().clone(),
            profile_revision: profile.revision().clone(),
            target_root: created.path.clone(),
            branch: "gate4agent/mw-restart".to_owned(),
            base_commit: created.head.clone(),
            expected_head: None,
            retention: ManagedWorktreeRetention::Retain,
            state: ManagedWorktreeLeaseState::Allocating,
            session_holders: Vec::new(),
            record_holders: Vec::new(),
            cleanup_failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new_with_incarnation(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-managed-restart").unwrap(),
            NodeIncarnationId::from_bytes([7; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            vec![workspace],
            vec![agent("claude")],
            ProviderRuntimeStatuses::default(),
            None,
            Vec::new(),
            Vec::new(),
            SpawnProfileRegistry::default(),
            None,
            None,
            None,
            None,
            Vec::new(),
            vec![lease.clone()],
            Vec::new(),
            Vec::new(),
            None,
            MUTATION_SETTLE_TIMEOUT_MS.saturating_add(READINESS_SETTLE_HEADROOM_MS),
        );
        shared.reconcile_managed_worktrees().await;
        let retained = shared.managed_worktrees.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&lease.lease_id).unwrap().clone();
        assert_eq!(retained.state, ManagedWorktreeLeaseState::Retained);
        assert!(shared.workspaces.read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&lease.workspace_id));
        let removed = shared.cleanup_managed_worktree(&lease.lease_id, true).await.unwrap();
        assert_eq!(removed.state, ManagedWorktreeLeaseState::Removed);
        assert!(!Path::new(&created.path).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn run_test_git(root: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C").arg(root).args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[tokio::test]
    async fn spawn_spec_validation_fails_before_session_mutation() {
        let shared = terminal_test_shared();
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
        }));
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_SPAWN_PROFILE_REVISION_CAPABILITY
        }));
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_WORKTREE_SELECTION_CAPABILITY
        }));

        let mut target_mismatch = spawn_spec_fixture();
        target_mismatch.target.node_id = NodeId::new("other-node").unwrap();
        let failure = shared.spawn_from_spec(target_mismatch).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnTargetMismatch);

        let mut unknown_profile = spawn_spec_fixture();
        unknown_profile.idempotency_key = SpawnIdempotencyKey::new("unknown-profile").unwrap();
        unknown_profile.profile_id = crate::protocol::SpawnProfileId::new("missing").unwrap();
        let failure = shared.spawn_from_spec(unknown_profile).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::UnknownSpawnProfile);

        let mut revision_mismatch = spawn_spec_fixture();
        revision_mismatch.idempotency_key =
            SpawnIdempotencyKey::new("profile-revision-mismatch").unwrap();
        revision_mismatch.expected_profile_revision =
            crate::protocol::SpawnProfileRevision::new("test-r2").unwrap();
        let failure = shared.spawn_from_spec(revision_mismatch).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnProfileRevisionMismatch);

        let mut unsupported_materializer = spawn_spec_fixture();
        unsupported_materializer.idempotency_key =
            SpawnIdempotencyKey::new("unsupported-materializer").unwrap();
        unsupported_materializer.overrides.bundle_id = crate::protocol::SpawnOverride::Set {
            value: crate::protocol::SpawnBundleId::new("bundle-v1").unwrap(),
        };
        let failure = shared
            .spawn_from_spec(unsupported_materializer)
            .await
            .unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::UnknownBundle);

        let failure = shared
            .spawn_session_with_deadline(
                WorkspaceId::new("primary").unwrap(),
                agent("claude"),
                SessionMode::Pty,
                gate4agent_types::TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                None,
                None,
                None,
                None,
                None,
                None,
                SpawnRecordPolicy::ProviderIdentityOnly,
                Some(Instant::now()),
                &[],
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnDeadlineExceeded);
        assert_eq!(shared.next_instance_id.load(Ordering::Acquire), 1);
        assert!(shared.handle.snapshot().sessions.is_empty());
    }

    #[test]
    fn worktree_selection_requires_its_separate_negotiated_capability() {
        let mut spec = spawn_spec_fixture();
        spec.target.worktree_id = Some(WorkspaceId::new("selected-worktree").unwrap());
        let request = NodeRequest::SpawnSpec { spec };
        let spawn_spec_capability =
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap();
        assert!(request_uses_unnegotiated_capability(
            &request,
            &[spawn_spec_capability.clone()],
        ));
        let worktree_capability =
            CapabilityId::new(NODE_WORKTREE_SELECTION_CAPABILITY).unwrap();
        assert!(request_uses_unnegotiated_capability(
            &request,
            &[spawn_spec_capability.clone(), worktree_capability.clone()],
        ));
        let profile_revision_capability =
            CapabilityId::new(NODE_SPAWN_PROFILE_REVISION_CAPABILITY).unwrap();
        assert!(request_uses_unnegotiated_capability(
            &request,
            &[
                spawn_spec_capability.clone(),
                worktree_capability.clone(),
                profile_revision_capability.clone(),
            ],
        ));
        // Default `SpawnOverrides` leaves `bundle_id`/`context_id` at
        // `Inherit`, not `Clear` — the profile's own defaults might still
        // materialize a bundle or a context pack, so both capabilities stay
        // conservatively required until negotiated (see
        // `requires_session_bundle_materialization_capability` /
        // `requires_history_context_pack_capability`).
        let bundle_materialization_capability =
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap();
        assert!(request_uses_unnegotiated_capability(
            &request,
            &[
                spawn_spec_capability.clone(),
                worktree_capability.clone(),
                profile_revision_capability.clone(),
                bundle_materialization_capability.clone(),
            ],
        ));
        let history_context_pack_capability =
            CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap();
        assert!(!request_uses_unnegotiated_capability(
            &request,
            &[
                spawn_spec_capability,
                worktree_capability,
                profile_revision_capability,
                bundle_materialization_capability,
                history_context_pack_capability,
            ],
        ));
    }

    #[tokio::test]
    async fn spawn_spec_idempotency_replays_receipt_and_conflicts_before_session_mutation() {
        let mut shared = terminal_test_shared();
        let spec = spawn_spec_fixture();
        let receipt = shared
            .resolve_spawn_spec(&spec)
            .unwrap()
            .receipt(shared.incarnation_id, terminal_address(1));
        shared.remember_spawn_spec(spec.clone(), Ok(receipt.clone()), Instant::now());

        let mut revised = shared.spawn_profiles.get(&spec.profile_id).unwrap().clone();
        revised.revision = crate::protocol::SpawnProfileRevision::new("builtin-v2").unwrap();
        shared.spawn_profiles = SpawnProfileRegistry::new([revised]).unwrap();

        assert_eq!(shared.spawn_from_spec(spec.clone()).await.unwrap(), receipt);

        let mut revision_conflicting = spec.clone();
        revision_conflicting.expected_profile_revision =
            crate::protocol::SpawnProfileRevision::new("builtin-v2").unwrap();
        let failure = shared.spawn_from_spec(revision_conflicting).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnIdempotencyConflict);

        let mut stale_cache_miss = spec.clone();
        stale_cache_miss.idempotency_key =
            SpawnIdempotencyKey::new("stale-profile-cache-miss").unwrap();
        let failure = shared.spawn_from_spec(stale_cache_miss).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnProfileRevisionMismatch);

        let mut conflicting = spec;
        conflicting.deadline_ms = crate::protocol::SpawnDeadlineMs::new(31_000).unwrap();
        let failure = shared.spawn_from_spec(conflicting).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::SpawnIdempotencyConflict);
        assert_eq!(shared.next_instance_id.load(Ordering::Acquire), 1);
        assert!(shared.handle.snapshot().sessions.is_empty());
    }

    #[tokio::test]
    async fn off_worktree_service_rejects_selection_and_mutation_before_git() {
        let shared = terminal_test_shared_with_mode(WorktreeServiceMode::Off);
        let mut spec = spawn_spec_fixture();
        spec.target.worktree_id = Some(WorkspaceId::new("unregistered-worktree").unwrap());
        let failure = shared.spawn_from_spec(spec).await.unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::UnsupportedSpawnCapability);

        let failure = shared
            .create_worktree(
                WorkspaceId::new("primary").unwrap(),
                WorkspaceId::new("new-worktree").unwrap(),
                "not-an-absolute-path".to_owned(),
                "invalid branch".to_owned(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::UnsupportedSpawnCapability);

        let failure = shared
            .remove_worktree(
                WorkspaceId::new("primary").unwrap(),
                "not-an-absolute-path".to_owned(),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, NodeFailureCode::UnsupportedSpawnCapability);
        assert_eq!(shared.next_instance_id.load(Ordering::Acquire), 1);
        assert!(shared.handle.snapshot().sessions.is_empty());
    }

    #[tokio::test]
    async fn manual_worktree_registration_failure_removes_the_created_worktree() {
        let root = temporary_workspace_root("manual-worktree-compensation");
        let source = root.join("source");
        let target = root.join("target");
        let invalid_state_parent = root.join("state-parent-is-a-file");
        let invalid_state_path = invalid_state_parent.join("state.json");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(&invalid_state_parent, "not a directory").unwrap();
        run_test_git(&source, &["init"]);
        run_test_git(&source, &["config", "user.email", "gate4agent@example.invalid"]);
        run_test_git(&source, &["config", "user.name", "Gate4Agent Test"]);
        std::fs::write(source.join("seed.txt"), "seed\n").unwrap();
        run_test_git(&source, &["add", "seed.txt"]);
        run_test_git(&source, &["commit", "-m", "seed"]);

        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            &source,
        ).unwrap();
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-worktree-compensation").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        shared.state_path = Some(invalid_state_path);

        let error = shared.create_worktree(
            WorkspaceId::new("primary").unwrap(),
            WorkspaceId::new("compensated").unwrap(),
            target.to_string_lossy().into_owned(),
            "gate4agent/compensated".to_owned(),
            Some("HEAD".to_owned()),
        ).await.unwrap_err();

        assert_eq!(error.code, NodeFailureCode::BackendOperationFailed);
        assert!(!target.exists());
        assert!(list_git_worktrees(source.to_str().unwrap()).await.unwrap()
            .iter()
            .all(|worktree| !worktree_paths_equal(&worktree.path, &target.to_string_lossy())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_worktree_requires_authoritative_non_main_non_bare_entry() {
        fn worktree(path: &str, is_main: bool, is_bare: bool) -> NativeGitWorktreeSnapshot {
            NativeGitWorktreeSnapshot {
                path: path.to_owned(),
                head: "0123456789abcdef".to_owned(),
                branch: Some("refs/heads/test".to_owned()),
                is_bare,
                is_main,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: None,
            }
        }

        let source = r"C:\repo";
        let selected = r"C:\repo-worktree";
        let listed = vec![
            worktree(source, true, false),
            worktree(selected, false, false),
        ];
        assert!(validate_selected_worktree(source, selected, &listed).is_ok());

        let main_failure = validate_selected_worktree(source, source, &listed).unwrap_err();
        assert_eq!(main_failure.code, NodeFailureCode::WorktreeProtected);

        let bare = vec![
            worktree(source, true, false),
            worktree(selected, false, true),
        ];
        let bare_failure = validate_selected_worktree(source, selected, &bare).unwrap_err();
        assert_eq!(bare_failure.code, NodeFailureCode::WorktreeProtected);

        let foreign_failure =
            validate_selected_worktree(source, r"C:\foreign", &listed).unwrap_err();
        assert_eq!(foreign_failure.code, NodeFailureCode::WorktreeProtected);
    }

    fn terminal_frame(sequence: u64) -> TerminalFrame {
        TerminalFrame {
            sequence,
            size: gate4agent_types::TerminalSize {
                rows: 24,
                columns: 80,
            },
            cursor_row: 2,
            cursor_column: 3,
            contents: format!("frame-{sequence}"),
            formatted: vec![1, 2, 3],
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: Default::default(),
        }
    }

    #[test]
    fn terminal_frame_watermark_coalesces_and_resets_for_exact_session_address() {
        let shared = terminal_test_shared();
        let mut receiver = shared.terminal_event_tx.subscribe();
        let original = terminal_address(1);

        shared.publish_terminal_frame_candidates(vec![(original.clone(), terminal_frame(7))]);
        let first = receiver.try_recv().unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0].event,
            NodeEvent::TerminalFrame { address, frame }
                if address == &original && frame.sequence == 7
        ));

        shared.publish_terminal_frame_candidates(vec![(original.clone(), terminal_frame(7))]);
        shared.publish_terminal_frame_candidates(vec![(original.clone(), terminal_frame(6))]);
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        shared.publish_terminal_frame_candidates(vec![(original.clone(), terminal_frame(8))]);
        let advanced = receiver.try_recv().unwrap();
        assert_eq!(advanced[0].sequence, 2);

        let resumed = terminal_address(2);
        shared.publish_terminal_frame_candidates(vec![(resumed.clone(), terminal_frame(1))]);
        let reset = receiver.try_recv().unwrap();
        assert_eq!(reset[0].sequence, 3);
        assert!(matches!(
            &reset[0].event,
            NodeEvent::TerminalFrame { address, frame }
                if address == &resumed && frame.sequence == 1
        ));
        shared.clear_terminal_frame_watermark(&resumed);
        assert!(!shared
            .terminal_frame_watermarks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&resumed.session.instance_id));
    }

    #[test]
    fn terminal_frame_batches_replace_without_entering_node_event_history() {
        let shared = terminal_test_shared();
        let mut receiver = shared.terminal_event_tx.subscribe();
        let address = terminal_address(1);
        let count = NODE_EVENT_HISTORY_MAX as u64 + 17;

        for sequence in 1..=count {
            shared.publish_terminal_frame_candidates(vec![(
                address.clone(),
                terminal_frame(sequence),
            )]);
        }

        let history = shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(history.events.is_empty());
        assert_eq!(history.last_sequence, count);
        assert_eq!(history.replay_floor_sequence, 1);
        drop(history);
        let NodeResponse::Resync {
            event_sequence,
            oldest_available_sequence,
            events,
            ..
        } = shared.resync(0) else {
            unreachable!("resync helper returned another response")
        };
        assert_eq!(event_sequence, count);
        assert_eq!(oldest_available_sequence, 1);
        assert!(events.is_empty());
        assert!(matches!(
            resync_required_event(&shared).event,
            NodeEvent::ResyncRequired {
                oldest_available_sequence: 1,
            }
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(skipped)) if skipped == count - 1
        ));
        let latest = receiver.try_recv().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].sequence, count);
    }

    #[test]
    fn real_durable_eviction_advances_authoritative_replay_floor() {
        let shared = terminal_test_shared();
        for _ in 0..=NODE_EVENT_HISTORY_MAX {
            shared.publish(NodeEvent::ControllerChanged { controller: None });
        }

        let NodeResponse::Resync {
            event_sequence,
            oldest_available_sequence,
            events,
            ..
        } = shared.resync(0) else {
            unreachable!("resync helper returned another response")
        };
        assert_eq!(event_sequence, NODE_EVENT_HISTORY_MAX as u64 + 1);
        assert_eq!(oldest_available_sequence, 2);
        assert_eq!(events.first().map(|event| event.sequence), Some(2));
        assert_eq!(events.len(), NODE_EVENT_HISTORY_MAX);
        assert!(matches!(
            resync_required_event(&shared).event,
            NodeEvent::ResyncRequired {
                oldest_available_sequence: 2,
            }
        ));
    }

    #[test]
    fn legacy_terminal_frame_connection_has_no_live_subscription() {
        let shared = terminal_test_shared();
        let without_capability = terminal_event_subscription(&shared, false);
        let with_capability = terminal_event_subscription(&shared, true);
        assert!(without_capability.is_none());
        assert!(with_capability.is_some());
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_TERMINAL_FRAME_EVENTS_CAPABILITY
        }));
    }

    #[test]
    fn continuous_terminal_output_leaves_request_service_after_bounded_event_burst() {
        let mut pending = Vec::new();
        let mut event_sequence = 0_u64;
        for frame_sequence in 1..=4 {
            for instance in 1..=(NODE_CONNECTION_EVENT_BURST_MAX as u64 * 2) {
                event_sequence += 1;
                let mut address = terminal_address(1);
                address.session.instance_id = AgentInstanceId(instance);
                queue_connection_event(
                    &mut pending,
                    NodeEventEnvelope {
                        sequence: event_sequence,
                        event: NodeEvent::TerminalFrame {
                            address,
                            frame: terminal_frame(frame_sequence),
                        },
                    },
                    0,
                );
            }
        }
        assert_eq!(pending.len(), NODE_CONNECTION_EVENT_BURST_MAX * 2);
        pending.sort_unstable_by_key(|event| event.sequence);

        let mut uninterrupted = pending.clone();
        let first_burst = connection_event_burst_len(uninterrupted.len());
        uninterrupted.drain(..first_burst);
        let second_burst = connection_event_burst_len(uninterrupted.len());
        uninterrupted.drain(..second_burst);
        assert!(uninterrupted.is_empty());

        let (frame_tx, mut frame_rx) =
            mpsc::channel::<Result<ClientFrame, FrameError>>(1);
        frame_tx
            .try_send(Ok(ClientFrame::Request(RequestEnvelope {
                request_id: 77,
                request: NodeRequest::Snapshot,
            })))
            .unwrap();
        let burst_len = connection_event_burst_len(pending.len());
        assert_eq!(burst_len, NODE_CONNECTION_EVENT_BURST_MAX);
        pending.drain(..burst_len);
        assert_eq!(pending.len(), NODE_CONNECTION_EVENT_BURST_MAX);
        assert!(matches!(
            frame_rx.try_recv(),
            Ok(Ok(ClientFrame::Request(RequestEnvelope {
                request_id: 77,
                request: NodeRequest::Snapshot,
            })))
        ));
    }

    #[test]
    fn terminal_lag_watermark_discards_retained_and_replayed_batches() {
        let mut pending = Vec::new();
        for sequence in [7_u64, 8] {
            let mut address = terminal_address(1);
            address.session.instance_id = AgentInstanceId(sequence);
            queue_connection_event(
                &mut pending,
                NodeEventEnvelope {
                    sequence,
                    event: NodeEvent::TerminalFrame {
                        address,
                        frame: terminal_frame(sequence),
                    },
                },
                0,
            );
        }
        let mut discard_through = 0;
        apply_connection_resync_watermark(&mut pending, &mut discard_through, 8);
        assert!(pending.is_empty());

        let replayed = NodeEventEnvelope {
            sequence: 8,
            event: NodeEvent::TerminalFrame {
                address: terminal_address(1),
                frame: terminal_frame(8),
            },
        };
        let advanced = NodeEventEnvelope {
            sequence: 9,
            event: NodeEvent::TerminalFrame {
                address: terminal_address(2),
                frame: terminal_frame(1),
            },
        };
        queue_connection_event_batch(
            &mut pending,
            &[replayed, advanced.clone()],
            discard_through,
        );
        assert_eq!(pending, vec![advanced]);
    }

    fn record(provider: &str, record_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            record_id: SessionRecordId::new(record_id).unwrap(),
            display_name: format!("{provider} record"),
            provider: agent(provider),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Unavailable,
            workspace_id: WorkspaceId::new("primary").unwrap(),
            canonical_root: opaque_windows_path(
                std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            ),
            provider_session: None,
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: None,
            task_binding: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            last_error: None,
        }
    }

    #[test]
    fn session_record_context_export_selects_one_exact_history_identity() {
        let candidates = vec![
            HistoryCandidateSummary {
                id: "candidate-other".to_owned(),
                session_id_hint: "provider-other".to_owned(),
                modified_at_unix_ms: Some(1),
            },
            HistoryCandidateSummary {
                id: "candidate-exact".to_owned(),
                session_id_hint: "provider-exact".to_owned(),
                modified_at_unix_ms: Some(2),
            },
        ];
        assert_eq!(
            unique_history_candidate_for_provider_session(&candidates, "provider-exact")
                .unwrap(),
            "candidate-exact",
        );
    }

    #[test]
    fn session_record_context_export_rejects_missing_and_ambiguous_history_identity() {
        let candidate = |id: &str, hint: &str| HistoryCandidateSummary {
            id: id.to_owned(),
            session_id_hint: hint.to_owned(),
            modified_at_unix_ms: None,
        };
        let missing = unique_history_candidate_for_provider_session(
            &[candidate("candidate-other", "provider-other")],
            "provider-exact",
        )
        .unwrap_err();
        assert_eq!(missing.code, NodeFailureCode::SessionRecordConflict);
        let ambiguous = unique_history_candidate_for_provider_session(
            &[
                candidate("candidate-a", "provider-exact"),
                candidate("candidate-b", "provider-exact"),
            ],
            "provider-exact",
        )
        .unwrap_err();
        assert_eq!(ambiguous.code, NodeFailureCode::SessionRecordConflict);
    }

    #[test]
    fn session_record_context_export_revalidation_rejects_record_change() {
        let session = terminal_address(1);
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "provider-exact".to_owned(),
            transcript_path: Some(r"C:\private\provider.jsonl".to_owned()),
        };
        let mut expected = record("claude", "record-context-export");
        expected.state = ManagedSessionState::Live;
        expected.provider_session = Some(identity.clone());
        expected.active_session = Some(session.clone());
        let binding = SessionBinding {
            workspace_id: session.workspace_id.clone(),
            generation: session.session.generation,
            runtime_policy: ProviderRuntimePolicy::raw_pty(),
            pending_resume: None,
            record_id: Some(expected.record_id.clone()),
            managed_worktree_lease_id: None,
            environment_profile: None,
            bundle: None,
            context: None,
            materialization_id: None,
        };
        let current_root = windows_path_text(&expected.canonical_root).to_owned();
        assert!(session_record_context_export_binding_is_exact(
            &expected,
            &expected,
            &identity,
            &session,
            &binding,
            &expected.provider,
            &current_root,
            false,
        ));
        let mut changed = expected.clone();
        changed.updated_at_unix_ms += 1;
        assert!(!session_record_context_export_binding_is_exact(
            &expected,
            &changed,
            &identity,
            &session,
            &binding,
            &expected.provider,
            &current_root,
            false,
        ));
        // A same-instant timestamp bump alone (state and active_session
        // otherwise unchanged) is not the clean-detachment transition
        // either, even with the reactive flag on: only the exact Live ->
        // Dormant + active_session cleared shape qualifies.
        assert!(!session_record_context_export_binding_is_exact(
            &expected,
            &changed,
            &identity,
            &session,
            &binding,
            &expected.provider,
            &current_root,
            true,
        ));
        let mut detached = expected.clone();
        detached.state = ManagedSessionState::Dormant;
        detached.active_session = None;
        detached.updated_at_unix_ms += 1;
        assert!(!session_record_context_export_binding_is_exact(
            &expected,
            &detached,
            &identity,
            &session,
            &binding,
            &expected.provider,
            &current_root,
            false,
        ));
        assert!(session_record_context_export_binding_is_exact(
            &expected,
            &detached,
            &identity,
            &session,
            &binding,
            &expected.provider,
            &current_root,
            true,
        ));
    }

    #[test]
    fn session_record_context_export_preflight_requires_live_usable_source() {
        let mut source = record("claude", "record-context-export-preflight");
        source.state = ManagedSessionState::Live;
        assert!(session_record_context_export_source_is_usable(
            &source,
            &SessionStatus::Running,
            false,
        ));
        assert!(session_record_context_export_source_is_usable(
            &source,
            &SessionStatus::Exited { exit_code: Some(0) },
            false,
        ));

        source.state = ManagedSessionState::Dormant;
        assert!(!session_record_context_export_source_is_usable(
            &source,
            &SessionStatus::Running,
            false,
        ));
        // The reactive auto-export-at-exit path (allow_clean_detachment) is
        // the only caller that may treat a Dormant, cleanly-exited record as
        // still usable — the on-demand path above never does.
        assert!(session_record_context_export_source_is_usable(
            &source,
            &SessionStatus::Exited { exit_code: Some(0) },
            true,
        ));
        assert!(!session_record_context_export_source_is_usable(
            &source,
            &SessionStatus::Exited { exit_code: Some(0) },
            false,
        ));
        source.state = ManagedSessionState::Live;
        assert!(!session_record_context_export_source_is_usable(
            &source,
            &SessionStatus::Starting,
            false,
        ));
    }

    #[test]
    fn session_record_context_export_final_conflict_leaves_catalog_empty() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-context-export-conflict").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let history = HistorySessionRecord {
            session_id: "provider-exact".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![gate4agent_types::HistoryMessageRecord {
                role: gate4agent_types::HistoryMessageRole::User,
                text: "bounded context".to_owned(),
            }],
        };
        let pack = NodeContextPack::export(
            ContextPackLineageReceipt {
                source_node_id: shared.node_id.clone(),
                source_session: terminal_address(1),
                source_provider: agent("claude"),
            },
            &history,
        )
        .unwrap();
        let context_id = pack.receipt().id.clone();
        let session = terminal_address(1);
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: history.session_id.clone(),
            transcript_path: None,
        };
        let mut expected = record("claude", "record-context-export-final-conflict");
        expected.state = ManagedSessionState::Live;
        expected.provider_session = Some(identity.clone());
        expected.active_session = Some(session.clone());
        let mut changed = expected.clone();
        changed.updated_at_unix_ms += 1;
        shared.insert_record(changed).unwrap();

        let error = shared
            .commit_context_pack_for_session_record(
                &expected,
                &identity,
                &session,
                "candidate-exact",
                &history,
                pack,
                false,
            )
            .unwrap_err();

        assert_eq!(error.code, NodeFailureCode::SessionRecordConflict);
        assert!(shared
            .context_catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&context_id)
            .is_none());
    }

    #[tokio::test]
    async fn session_task_binding_mints_assigns_clears_with_cas_and_persists_v9() {
        let root = temporary_workspace_root("session-task-binding");
        std::fs::create_dir_all(&root).unwrap();
        let state_path = root.join("node-state.json");
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(WorkspaceId::new("primary").unwrap(), &root).unwrap();
        let node_id = NodeId::new("node-1").unwrap();
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            node_id.clone(),
            vec![workspace],
            vec![agent("claude")],
        );
        shared.state_path = Some(state_path.clone());
        let record = record("claude", "sr-task");
        let record_id = record.record_id.clone();
        shared.insert_record(record).unwrap();
        shared.persist_state().unwrap();
        shared
            .acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS)
            .unwrap();

        let minted = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::SetSessionTask {
                record_id: record_id.clone(),
                expected_revision: 0,
                target: SessionTaskTargetV1::New,
            },
        )
        .await
        .unwrap();
        let NodeResponse::SessionRecordUpdated { record: minted } = minted else {
            panic!("session task mutation returned another response");
        };
        let minted_binding = minted.task_binding.clone().unwrap();
        let minted_task_id = minted_binding.task_id.clone().unwrap();
        assert_eq!(minted_binding.revision, 1);
        assert!(minted_task_id.as_str().starts_with("task-"));
        assert_eq!(minted_task_id.as_str().len(), 29);

        let event_count = shared.current_sequence();
        let repeated = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::SetSessionTask {
                record_id: record_id.clone(),
                expected_revision: 1,
                target: SessionTaskTargetV1::Existing {
                    task_id: minted_task_id.clone(),
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(repeated, NodeResponse::SessionRecordUpdated { record: minted.clone() });
        assert_eq!(shared.current_sequence(), event_count);

        let stale = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::SetSessionTask {
                record_id: record_id.clone(),
                expected_revision: 0,
                target: SessionTaskTargetV1::Existing {
                    task_id: minted_task_id,
                },
            },
        )
        .await
        .unwrap_err();
        assert_eq!(stale.code, NodeFailureCode::SessionRecordConflict);

        let assigned_id: TaskId = "task-ffeeddccbbaa998877665544".parse().unwrap();
        let assigned = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::SetSessionTask {
                record_id: record_id.clone(),
                expected_revision: 1,
                target: SessionTaskTargetV1::Existing {
                    task_id: assigned_id.clone(),
                },
            },
        )
        .await
        .unwrap();
        let NodeResponse::SessionRecordUpdated { record: assigned } = assigned else {
            panic!("session task assignment returned another response");
        };
        assert_eq!(assigned.task_binding.as_ref().unwrap().revision, 2);
        assert_eq!(assigned.task_binding.as_ref().unwrap().task_id.as_ref(), Some(&assigned_id));

        let cleared = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::SetSessionTask {
                record_id: record_id.clone(),
                expected_revision: 2,
                target: SessionTaskTargetV1::Clear,
            },
        )
        .await
        .unwrap();
        let NodeResponse::SessionRecordUpdated { record: cleared } = cleared else {
            panic!("session task clear returned another response");
        };
        assert_eq!(cleared.task_binding.as_ref().unwrap().revision, 3);
        assert!(cleared.task_binding.as_ref().unwrap().task_id.is_none());
        assert!(cleared.task_binding.as_ref().unwrap().changed_at_unix_ms > 0);
        assert_eq!(
            cleared.task_binding.as_ref().unwrap().changed_at_unix_ms,
            cleared.updated_at_unix_ms,
        );

        let persisted = std::fs::read(&state_path).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&persisted).unwrap()["version"], NODE_STATE_SCHEMA_V10);
        let loaded = session_registry::load(Some(&state_path), &node_id).unwrap();
        assert_eq!(loaded.records[0].task_binding, cleared.task_binding);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_task_capability_strips_snapshots_events_and_replies() {
        let mut bound = record("claude", "sr-task-projection");
        bound.updated_at_unix_ms = 2;
        bound.task_binding = Some(SessionTaskBindingV1 {
            revision: 1,
            task_id: Some("task-00112233445566778899aaff".parse().unwrap()),
            changed_at_unix_ms: 2,
        });
        let request = NodeRequest::SetSessionTask {
            record_id: bound.record_id.clone(),
            expected_revision: 1,
            target: SessionTaskTargetV1::Clear,
        };
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_SESSION_TASK_CORRELATION_CAPABILITY
        }));
        assert!(!request_uses_unnegotiated_capability(
            &request,
            &baseline_capabilities().unwrap(),
        ));
        assert!(request_uses_unnegotiated_capability(&request, &[]));

        let mut snapshot = terminal_test_shared().snapshot();
        snapshot.session_records.push(bound.clone());
        let mut reply = ResponseEnvelope {
            request_id: 1,
            result: Ok(NodeResponse::Resync {
                event_sequence: 1,
                oldest_available_sequence: 1,
                snapshot,
                events: vec![NodeEventEnvelope {
                    sequence: 1,
                    event: NodeEvent::SessionRecordUpserted {
                        record: bound.clone(),
                    },
                }],
            }),
        };
        project_response_without_session_task_binding(&mut reply);
        let Ok(NodeResponse::Resync { snapshot, events, .. }) = reply.result else {
            panic!("task projection changed the response kind");
        };
        assert!(snapshot.session_records[0].task_binding.is_none());
        let NodeEvent::SessionRecordUpserted { record } = &events[0].event else {
            panic!("task projection changed the event kind");
        };
        assert!(record.task_binding.is_none());

        let projected = project_event_without_session_task_binding(NodeEventEnvelope {
            sequence: 2,
            event: NodeEvent::SessionRecordUpserted { record: bound.clone() },
        });
        let NodeEvent::SessionRecordUpserted { record } = projected.event else {
            panic!("task event projection changed the event kind");
        };
        assert!(record.task_binding.is_none());

        let mut direct = ResponseEnvelope {
            request_id: 2,
            result: Ok(NodeResponse::SessionRecordUpdated { record: bound }),
        };
        project_response_without_session_task_binding(&mut direct);
        let Ok(NodeResponse::SessionRecordUpdated { record }) = direct.result else {
            panic!("task reply projection changed the response kind");
        };
        assert!(record.task_binding.is_none());
    }

    #[test]
    fn windows_runtime_default_node_endpoint_is_exact_and_valid() {
        assert_eq!(DEFAULT_NODE_ENDPOINT, r"\\.\pipe\gate4agent-node");
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let config = NodeServerConfig::new(
            DEFAULT_NODE_ENDPOINT,
            "fixture-token",
            NodeId::new("node-1").unwrap(),
            [workspace],
        )
        .unwrap();
        assert_eq!(config.endpoint, DEFAULT_NODE_ENDPOINT);
    }

    #[cfg(all(feature = "fixture", windows))]
    #[test]
    fn provider_bundle_argv_hold_fixture_is_parent_bounded_and_waits_after_proof() {
        let root = temporary_workspace_root("provider-bundle-argv-hold");
        let outside = temporary_workspace_root("provider-bundle-argv-hold-outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let proof_path = root.join("provider.proof");
        let release_signal = root.join("release.signal");
        let config = NodeServerConfig::new(
            format!(
                r"\\.\pipe\gate4agent-provider-bundle-hold-{}",
                std::process::id(),
            ),
            "fixture-token",
            NodeId::new("provider-bundle-hold-node").unwrap(),
            [WorkspaceConfig::new(
                WorkspaceId::new("primary").unwrap(),
                std::env::current_dir().unwrap(),
            )
            .unwrap()],
        )
        .unwrap()
        .with_spawn_profiles(SpawnProfileRegistry::new([
            crate::protocol::SpawnProfileDefaults {
                profile_id: crate::protocol::SpawnProfileId::new("kimi-bundle-hold").unwrap(),
                revision: crate::protocol::SpawnProfileRevision::new("kimi-bundle-hold-r1")
                    .unwrap(),
                provider: agent("kimi"),
                mode: SessionMode::Pty,
                terminal_size: gate4agent_types::TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                prompt: None,
                bundle_id: None,
                context_id: None,
                environment_profile_id: None,
            },
        ])
        .unwrap());
        let server = NodeServer::new_provider_bundle_argv_hold_fixture(
            config,
            agent("kimi"),
            proof_path.clone(),
            release_signal.clone(),
        )
        .unwrap();
        assert_eq!(server.shared.enabled_providers, [agent("kimi")]);

        let spec = NodeServer::provider_bundle_argv_fixture_spec(
            agent("kimi"),
            proof_path.clone(),
            Some(release_signal.clone()),
        )
        .unwrap();
        let wrapper = &spec.launch.fixed_args[spec.launch.fixed_args.len() - 3];
        let proof_write = wrapper.find("WriteAllLines($proofPath").unwrap();
        let release_wait = wrapper
            .find("while (-not (Test-Path -LiteralPath $releaseSignal -PathType Leaf))")
            .unwrap();
        let fixture_ready = wrapper.find("fixture-ready>").unwrap();
        assert!(proof_write < release_wait);
        assert!(release_wait < fixture_ready);
        assert!(wrapper.contains("[DateTime]::UtcNow.AddSeconds(45)"));
        assert!(wrapper.contains("Start-Sleep -Milliseconds 20"));
        assert_eq!(
            &spec.launch.fixed_args[spec.launch.fixed_args.len() - 2..],
            &[
                proof_path.to_string_lossy().into_owned(),
                release_signal.to_string_lossy().into_owned(),
            ],
        );

        let error = NodeServer::provider_bundle_argv_fixture_spec(
            agent("kimi"),
            proof_path,
            Some(outside.join("release.signal")),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NodeServerError::Registry(message)
                if message == "provider bundle argv release path must share the proof parent"
        ));
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn five_provider_context_fixture_registry_is_unambiguous_and_history_bound() {
        let root = temporary_workspace_root("context-fixture-registry");
        std::fs::create_dir_all(&root).unwrap();
        let proof_path = root.join("context.proof");
        let config = NodeServerConfig::new(
            format!(
                r"\\.\pipe\gate4agent-context-fixture-registry-{}",
                std::process::id(),
            ),
            "fixture-token",
            NodeId::new("context-fixture-node").unwrap(),
            [WorkspaceConfig::new(
                WorkspaceId::new("primary").unwrap(),
                std::env::current_dir().unwrap(),
            )
            .unwrap()],
        )
        .unwrap();
        let server = NodeServer::new_context_pack_fixture(config, proof_path).unwrap();
        let mut enabled = server
            .shared
            .enabled_providers
            .iter()
            .map(AgentId::as_str)
            .collect::<Vec<_>>();
        enabled.sort_unstable();
        assert_eq!(enabled, ["claude", "codex", "grok", "kimi", "qwen-code"]);
        for provider in &server.shared.enabled_providers {
            assert!(server
                .shared
                .provider_adapter_contracts
                .iter()
                .any(|contract| {
                    &contract.provider == provider
                        && contract.family == AdapterFamily::History
                }));
        }
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn monitoring_context_fixture_combines_claude_and_codex_hooks_with_history() {
        let root = temporary_workspace_root("monitoring-context-fixture");
        std::fs::create_dir_all(&root).unwrap();
        let proof_path = root.join("context.proof");
        let standard = NodeServer::context_pack_fixture_catalog(
            proof_path.clone(),
            None,
            false,
        ).unwrap();
        let monitoring = NodeServer::context_pack_fixture_catalog(
            proof_path.clone(),
            None,
            true,
        ).unwrap();
        let claude = AgentId::new("claude").unwrap();
        let standard_claude = standard.get(&claude).unwrap();
        let monitoring_claude = monitoring.get(&claude).unwrap();
        assert_eq!(
            monitoring_claude.capabilities.adapters.hook.as_ref().unwrap().id.as_str(),
            "claude-code",
        );
        assert_eq!(
            monitoring_claude.capabilities.adapters.history,
            standard_claude.capabilities.adapters.history,
        );
        assert_ne!(monitoring_claude.launch, standard_claude.launch);
        let codex = AgentId::new("codex").unwrap();
        let standard_codex = standard.get(&codex).unwrap();
        let monitoring_codex = monitoring.get(&codex).unwrap();
        assert_eq!(
            monitoring_codex
                .capabilities
                .adapters
                .hook
                .as_ref()
                .unwrap()
                .id
                .as_str(),
            "codex",
        );
        assert_eq!(
            monitoring_codex.capabilities.adapters.history,
            standard_codex.capabilities.adapters.history,
        );
        assert_ne!(monitoring_codex.launch, standard_codex.launch);
        let codex_validation = monitoring_codex.launch.fixed_args.iter()
            .find(|argument| argument.contains("F7_CODEX_BUNDLE_CONTEXT_VALIDATED"))
            .unwrap();
        for required in [
            "$allowedTopLevel = @('schema', 'source_provider', 'source_message_count', 'retained_messages', 'repository', 'truncated')",
            "$requiredTopLevel = @('schema', 'source_provider', 'source_message_count', 'retained_messages', 'truncated')",
            "@('role', 'text')",
            "g4a-private-provider-session-canary",
            "private-provider-login-identity",
            "$rawProof",
            "$env:GATE4AGENT_HOOK_URL",
            "g4a-private-provider-session-canary",
        ] {
            assert!(codex_validation.contains(required), "missing privacy validation: {required}");
        }
        for provider in ["grok", "kimi", "qwen-code"] {
            let provider = AgentId::new(provider).unwrap();
            assert_eq!(monitoring.get(&provider), standard.get(&provider));
        }

        let config = NodeServerConfig::new(
            format!(
                r"\\.\pipe\gate4agent-monitoring-context-fixture-{}",
                std::process::id(),
            ),
            "fixture-token",
            NodeId::new("monitoring-context-fixture-node").unwrap(),
            [WorkspaceConfig::new(
                WorkspaceId::new("primary").unwrap(),
                std::env::current_dir().unwrap(),
            )
            .unwrap()],
        )
        .unwrap();
        let server = NodeServer::new_monitoring_context_pack_fixture(config, proof_path).unwrap();
        assert!(server.shared.fixture_semantic_hook_policy);
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn kimi_context_only_fixture_is_bounded_and_preserves_codex_validation() {
        let root = temporary_workspace_root("kimi-context-only-fixture");
        std::fs::create_dir_all(&root).unwrap();
        let proof_path = root.join("context.proof");
        let standard = NodeServer::context_pack_fixture_catalog(
            proof_path.clone(),
            None,
            false,
        ).unwrap();
        let kimi = AgentId::new("kimi").unwrap();
        let context_only = NodeServer::context_pack_fixture_catalog(
            proof_path,
            Some(&kimi),
            false,
        )
        .unwrap();

        let codex = AgentId::new("codex").unwrap();
        assert_eq!(
            standard.get(&codex).unwrap().launch.fixed_args,
            context_only.get(&codex).unwrap().launch.fixed_args,
        );
        let script = context_only
            .get(&kimi)
            .unwrap()
            .launch
            .fixed_args
            .iter()
            .find(|argument| argument.contains("F7_KIMI_CONTEXT_ONLY_VALIDATED"))
            .expect("Kimi context-only validation script is missing");
        for required in [
            "$bundleArgs.Count -ne 0",
            "GATE4AGENT_CONTEXT_ROOT",
            "context-pack.json",
            "$entries.Count -ne 1",
            "g4a-context-pack-v1",
            "$document.PSObject.Properties['cwd']",
            "'claude', 'codex', 'grok', 'kimi', 'qwen-code'",
            "$hasUser",
            "$hasAssistant",
            "SHA256",
            "context-only",
        ] {
            assert!(script.contains(required), "missing validation: {required}");
        }
        for forbidden in ["CODEX_HOME", "USERPROFILE", "auth", "token", "global config"] {
            assert!(!script.contains(forbidden), "forbidden home/auth access: {forbidden}");
        }
        assert!(!standard
            .get(&kimi)
            .unwrap()
            .launch
            .fixed_args
            .iter()
            .any(|argument| argument.contains("F7_KIMI_CONTEXT_ONLY_VALIDATED")));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(feature = "fixture", windows))]
    #[tokio::test]
    async fn kimi_context_only_fixture_spawns_catalogued_context_pack_through_spawn_spec() {
        let root = temporary_workspace_root("kimi-context-only-spawn");
        let workspace_root = root.join("workspace");
        std::fs::create_dir_all(&workspace_root).unwrap();
        let materialization_root = root.join("private-materializations");
        let proof_path = root.join("context.proof");
        let node_id = NodeId::new("node-kimi-context-only-spawn").unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let profile_id = crate::protocol::SpawnProfileId::new("target-kimi").unwrap();
        let endpoint = format!(
            r"\\.\pipe\gate4agent-kimi-context-only-spawn-{}-{}",
            std::process::id(),
            unix_time_ms(),
        );
        let spawn_profiles = SpawnProfileRegistry::new([
            crate::protocol::SpawnProfileDefaults {
                profile_id: profile_id.clone(),
                revision: crate::protocol::SpawnProfileRevision::new("target-kimi-r1").unwrap(),
                provider: agent("kimi"),
                mode: SessionMode::Pty,
                terminal_size: gate4agent_types::TerminalSize {
                    rows: 30,
                    columns: 120,
                },
                prompt: None,
                bundle_id: None,
                context_id: None,
                environment_profile_id: None,
            },
        ])
        .unwrap();
        let mut config = NodeServerConfig::new(
            &endpoint,
            "fixture-token",
            node_id.clone(),
            [WorkspaceConfig::new(workspace_id.clone(), &workspace_root).unwrap()],
        )
        .unwrap()
        .with_spawn_profiles(spawn_profiles)
        .with_session_environment_materialization(
            materialization_root,
            Arc::new(FixtureSecretResolver {
                deny: Arc::new(AtomicBool::new(false)),
            }),
        )
        .unwrap();
        config.fixture_raw_pty_runtime = true;
        let server = NodeServer::new_context_only_proof_fixture(
            config,
            agent("kimi"),
            proof_path.clone(),
        )
        .unwrap();
        let shared = Arc::clone(&server.shared);
        let shutdown = server.shutdown_handle();

        let history = HistorySessionRecord {
            session_id: "context-only-source-history".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: 2,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![
                gate4agent_types::HistoryMessageRecord {
                    role: gate4agent_types::HistoryMessageRole::User,
                    text: "continue the bounded parent run through native C2".to_owned(),
                },
                gate4agent_types::HistoryMessageRecord {
                    role: gate4agent_types::HistoryMessageRole::Assistant,
                    text: "the exact parent context is ready for the child run".to_owned(),
                },
            ],
        };
        let pack = NodeContextPack::export(
            ContextPackLineageReceipt {
                source_node_id: node_id.clone(),
                source_session: SessionAddress {
                    workspace_id: workspace_id.clone(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(71),
                        generation: SessionGeneration(1),
                    },
                },
                source_provider: agent("qwen-code"),
            },
            &history,
        )
        .unwrap();
        let expected_context_hash = digest(&SHA256, pack.bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let context = shared
            .context_catalog
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pack)
            .unwrap();
        let spec = SpawnSpec {
            target: crate::protocol::SpawnTarget {
                node_id,
                workspace_id: workspace_id.clone(),
                worktree_id: None,
            },
            profile_id,
            expected_profile_revision: crate::protocol::SpawnProfileRevision::new(
                "target-kimi-r1",
            ).unwrap(),
            overrides: crate::protocol::SpawnOverrides {
                context_id: crate::protocol::SpawnOverride::Set {
                    value: context.id.clone(),
                },
                ..crate::protocol::SpawnOverrides::default()
            },
            deadline_ms: crate::protocol::SpawnDeadlineMs::new(10_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("kimi-context-only-spawn").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
        };
        let resolved = match shared.resolve_spawn_spec(&spec) {
            Ok(resolved) => resolved,
            Err(error) => panic!(
                "Kimi context-only resolve rejected: exact_code={:?} message={}",
                error.code,
                error.message,
            ),
        };
        let resolved_context = match shared.resolve_context(&resolved) {
            Ok(Some(resolved_context)) => resolved_context,
            Ok(None) => panic!("Kimi context-only resolve omitted the exact ContextPack"),
            Err(error) => panic!(
                "Kimi context catalog resolve rejected: exact_code={:?} message={}",
                error.code,
                error.message,
            ),
        };
        assert_eq!(resolved_context, context);
        let runtime_policy = match shared
            .admit_provider_runtime(&agent("kimi"), ProviderRuntimeRequirement::RawPty)
            .await
        {
            Ok(runtime_policy) => runtime_policy,
            Err(error) => panic!(
                "Kimi RawPty admission rejected: exact_code={:?} message={}",
                error.code,
                error.message,
            ),
        };
        assert!(runtime_policy.raw_pty_lifecycle);

        let launch_control = shared.native_launch_profile_control.as_ref().unwrap();
        let generic_overlay = NativeLaunchEnvironmentOverlay::new(
            agent("kimi"),
            TransportKind::Pty,
            vec![EnvMutation {
                key: OsString::from("GATE4AGENT_UNPROFILED_GENERIC"),
                value: Some(OsString::from("must-remain-rejected")),
            }],
        )
        .unwrap();
        assert_eq!(
            launch_control
                .install_native_launch_environment_overlay(
                    AgentInstanceId(72),
                    generic_overlay,
                )
                .unwrap_err(),
            gate4agent_runtime_native::NativeLaunchProfileError::EnvironmentOverlaySelectionMissing,
        );

        let diagnostic_address = SessionAddress {
            workspace_id: workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(73),
                generation: SessionGeneration(1),
            },
        };
        let (diagnostic_overlay, diagnostic_materialization) = match shared
            .prepare_session_materialization(
                &diagnostic_address,
                &agent("kimi"),
                SessionMode::Pty,
                None,
                None,
                Some(&resolved_context),
                None,
            )
        {
            Ok(Some(prepared)) => prepared,
            Ok(None) => panic!("Kimi context-only materialization produced no launch overlay"),
            Err(error) => panic!(
                "Kimi context-only materialization rejected: exact_code={:?} message={}",
                error.code,
                error.message,
            ),
        };
        let diagnostic_overlay = diagnostic_overlay
            .expect("Kimi context-only materialization omitted its environment overlay");
        assert!(matches!(
            &diagnostic_overlay,
            PreparedNativeLaunchOverlay::Environment(_),
        ));
        match shared.install_prepared_launch_overlay(
            diagnostic_address.session.instance_id,
            diagnostic_overlay,
        ) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("Kimi context-only overlay unexpectedly retained argv authority"),
            Err(error) => panic!(
                "Kimi context-only overlay installation rejected: exact_code={:?} message={}",
                error.code,
                error.message,
            ),
        }
        assert!(launch_control.clear_native_instance_launch_overlay(
            diagnostic_address.session.instance_id,
        ));
        drop(diagnostic_materialization);
        assert!(shared
            .materializations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());

        let node_task = tokio::spawn(server.run());
        let ready_client = timeout(Duration::from_secs(5), async {
            loop {
                match gate4agent_node_wire::LocalNodeClient::connect(
                    &endpoint,
                    &shared.node_id,
                    ClientRole::Observer,
                    "fixture-token",
                )
                .await
                {
                    Ok(client) => break client,
                    Err(_) if !node_task.is_finished() => sleep(Duration::from_millis(10)).await,
                    Err(error) => panic!("Kimi context-only Node stopped before readiness: {error}"),
                }
            }
        })
        .await
        .expect("Kimi context-only Node did not become ready");
        assert_eq!(
            ready_client
                .hello()
                .snapshot
                .provider_runtime_statuses
                .iter()
                .find(|status| status.provider().as_str() == "kimi")
                .expect("Kimi runtime status is missing")
                .mode(),
            crate::protocol::ProviderRuntimeMode::RawPassthrough,
        );
        drop(ready_client);

        let receipt = match shared.spawn_from_spec(spec).await {
            Ok(receipt) => receipt,
            Err(error) => panic!(
                "Kimi context-only SpawnSpec rejected: exact_code={:?} message={}",
                error.code,
                error.message,
            ),
        };
        assert_eq!(receipt.provider, agent("kimi"));
        assert_eq!(receipt.context.as_ref(), Some(&context));
        assert!(receipt.context_binding_is_valid());
        timeout(Duration::from_secs(5), async {
            while !proof_path.is_file() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Kimi context-only fixture did not write its proof");

        let proof = std::fs::read_to_string(&proof_path).unwrap();
        let lines = proof.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0], "context-only");
        assert!(Path::new(lines[1]).is_absolute());
        assert_ne!(
            std::fs::canonicalize(Path::new(lines[1])).unwrap(),
            std::fs::canonicalize(&workspace_root).unwrap(),
        );
        assert_eq!(
            std::fs::canonicalize(Path::new(lines[2])).unwrap(),
            std::fs::canonicalize(&workspace_root).unwrap(),
        );
        assert_eq!(lines[3], expected_context_hash);
        assert_eq!(lines[4], "g4a-context-pack-v1");
        assert_eq!(lines[5], "qwen-code");
        assert_eq!(lines[6], "2");
        assert!(shared.handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == receipt.session.session.instance_id
                && session.agent_id.as_str() == "kimi"
        }));

        shutdown.request_shutdown().await.unwrap();
        timeout(Duration::from_secs(5), node_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(shutdown);
        drop(shared);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn clean_exit_fixture_constructor_is_fixture_scoped_and_path_bounded() {
        let root = temporary_workspace_root("clean-exit-constructor");
        std::fs::create_dir_all(&root).unwrap();
        let config = construction_test_config("clean-exit-constructor")
            .with_state_path(root.join("node-state.json"))
            .unwrap();
        let server = NodeServer::new_clean_exit_fixture(
            config,
            root.clone(),
            root.join("started.marker"),
            root.join("release.signal"),
        )
        .unwrap();
        assert_eq!(
            server
                .shared
                .enabled_providers
                .iter()
                .map(AgentId::as_str)
                .collect::<Vec<_>>(),
            ["claude"],
        );
        drop(server);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_registry_provider_contract_manifest_is_exact() {
        let registry = active_registry().unwrap();
        let (providers, adapters) = provider_contract_manifest(&registry).unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|contract| (contract.provider.as_str(), contract.revision.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("claude", "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
                ("codex", "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
                ("grok", "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
                ("kimi", "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
                ("qwen-code", "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
            ]
        );
        assert_eq!(
            adapters
                .iter()
                .map(|contract| (
                    contract.provider.as_str(),
                    contract.family,
                    contract.adapter_id.as_str(),
                    contract.revision.as_str(),
                ))
                .collect::<Vec<_>>(),
            vec![
                ("claude", AdapterFamily::PtySemantic, "claude-code", "gate4agent-adapter/v1"),
                ("claude", AdapterFamily::Pipe, "claude-code", "gate4agent-adapter/v1"),
                ("claude", AdapterFamily::Hook, "claude-code", "gate4agent-adapter/v1"),
                ("claude", AdapterFamily::ManagedHook, "claude", "gate4agent-managed-hooks/orca-d8629c4/v1"),
                ("claude", AdapterFamily::OneShot, "claude", "gate4agent-inline/claude-code-2.1/v1"),
                ("claude", AdapterFamily::History, "claude-code", "gate4agent-adapter/v1"),
                ("claude", AdapterFamily::Resume, "claude-code", "gate4agent-adapter/v1"),
                ("claude", AdapterFamily::SessionOptions, "claude-code", "gate4agent-session-options/orca-d8629c4/v1"),
                ("codex", AdapterFamily::PtySemantic, "codex", "gate4agent-adapter/v1"),
                ("codex", AdapterFamily::Pipe, "codex", "gate4agent-adapter/v1"),
                ("codex", AdapterFamily::Hook, "codex", "gate4agent-adapter/v1"),
                ("codex", AdapterFamily::ManagedHook, "codex", "gate4agent-managed-hooks/orca-d8629c4/v1"),
                ("codex", AdapterFamily::OneShot, "codex", "gate4agent-inline/codex-cli-0.144/v1"),
                ("codex", AdapterFamily::History, "codex", "gate4agent-adapter/v1"),
                ("codex", AdapterFamily::Resume, "codex", "gate4agent-adapter/v1"),
                ("codex", AdapterFamily::SessionOptions, "codex", "gate4agent-session-options/orca-d8629c4/v1"),
                ("grok", AdapterFamily::Hook, "grok", "gate4agent-adapter/v1"),
                ("grok", AdapterFamily::ManagedHook, "grok", "gate4agent-managed-hooks/orca-d8629c4/v1"),
                ("grok", AdapterFamily::History, "grok", "gate4agent-adapter/v1"),
                ("grok", AdapterFamily::Resume, "grok", "gate4agent-adapter/v1"),
                ("kimi", AdapterFamily::PtySemantic, "kimi", "gate4agent-adapter/v1"),
                ("kimi", AdapterFamily::Pipe, "kimi", "gate4agent-adapter/v1"),
                ("kimi", AdapterFamily::Hook, "kimi", "gate4agent-adapter/v1"),
                ("kimi", AdapterFamily::ManagedHook, "kimi", "gate4agent-managed-hooks/orca-d8629c4/v1"),
                ("kimi", AdapterFamily::OneShot, "kimi", "gate4agent-inline/kimi-code-0.31/v1"),
                ("kimi", AdapterFamily::History, "kimi", "gate4agent-adapter/v1"),
                ("kimi", AdapterFamily::Resume, "kimi", "gate4agent-adapter/v1"),
                ("qwen-code", AdapterFamily::Pipe, "qwen-code", "qwen-code-dual-output/v1"),
                ("qwen-code", AdapterFamily::History, "qwen-code", "gate4agent-adapter/v1"),
            ]
        );
    }

    fn construction_test_config(name: &str) -> NodeServerConfig {
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        NodeServerConfig::new(
            format!(r"\\.\pipe\gate4agent-{name}"),
            "fixture-token",
            NodeId::new(name).unwrap(),
            [workspace],
        )
        .unwrap()
    }

    fn maximum_size_provider_manifest_registry() -> AgentRegistry {
        let adapter = AdapterBinding::new(
            gate4agent_types::AdapterId::new("a".repeat(96)).unwrap(),
            "r".repeat(crate::protocol::MAX_ADAPTER_CONTRACT_REVISION_BYTES),
            gate4agent_types::AdapterVerification::SyntheticFixture,
        )
        .unwrap();
        let agent_ids = ["claude", "codex", "kimi"]
            .into_iter()
            .map(|agent_id| AgentId::new(agent_id).unwrap())
            .collect::<Vec<_>>();
        let adapters = gate4agent_catalog::AdapterRegistry::new(
            [
                AdapterFamily::PtySemantic,
                AdapterFamily::Pipe,
                AdapterFamily::OneShot,
                AdapterFamily::Acp,
                AdapterFamily::Hook,
                AdapterFamily::ManagedHook,
                AdapterFamily::History,
                AdapterFamily::Resume,
                AdapterFamily::SessionOptions,
                AdapterFamily::CapabilityProbe,
            ]
            .into_iter()
            .map(|family| gate4agent_catalog::AdapterDescriptor {
                family,
                binding: adapter.clone(),
                agents: agent_ids.clone(),
            }),
        )
        .unwrap();
        let specs = agent_ids
            .iter()
            .map(|agent_id| {
                let agent_id = agent_id.as_str();
                let mut spec = builtin_registry().get_by_id(agent_id).unwrap().clone();
                spec.revision =
                    "r".repeat(crate::protocol::MAX_PROVIDER_CONTRACT_REVISION_BYTES);
                spec.capabilities.transports.pty_adapter = Some(adapter.clone());
                spec.capabilities.transports.pipe = Some(gate4agent_types::PipeTransportSpec {
                    adapter: adapter.clone(),
                    protocol: gate4agent_types::PipeProtocol::SemanticNdjson,
                    launch_override: None,
                    prompt_delivery: gate4agent_types::PipePromptDelivery::None,
                });
                spec.capabilities.transports.acp = Some(gate4agent_types::AcpTransportSpec {
                    adapter: adapter.clone(),
                    launch_override: None,
                });
                spec.capabilities.adapters.hook = Some(adapter.clone());
                spec.capabilities.adapters.managed_hook = Some(adapter.clone());
                spec.capabilities.adapters.one_shot = Some(adapter.clone());
                spec.capabilities.adapters.history = Some(adapter.clone());
                spec.capabilities.adapters.resume = Some(adapter.clone());
                spec.capabilities.adapters.session_options = Some(adapter.clone());
                spec.capabilities.adapters.capability_probe = Some(adapter.clone());
                spec
            })
            .collect::<Vec<_>>();
        AgentRegistry::new_with_adapters(specs, &adapters).unwrap()
    }

    #[test]
    fn node_server_construction_accepts_five_provider_production_manifest() {
        let registry = active_registry().unwrap();
        let (providers, _) = provider_contract_manifest(&registry).unwrap();
        assert_eq!(providers.len(), 5);
        let server = NodeServer::new_with_registry(
            construction_test_config("manifest-production-bound"),
            registry,
        )
        .unwrap();
        drop(server);
    }

    #[test]
    fn node_server_construction_rejects_count_valid_manifest_over_handshake_capacity() {
        let registry = maximum_size_provider_manifest_registry();
        let (providers, adapters) = provider_contract_manifest(&registry).unwrap();
        assert_eq!(providers.len(), 3);
        assert_eq!(adapters.len(), 30);
        assert!(adapters.len() <= crate::protocol::MAX_PROVIDER_ADAPTER_CONTRACTS);
        let error = match NodeServer::new_with_registry(
            construction_test_config("manifest-handshake-overflow"),
            registry,
        ) {
            Ok(_) => panic!("oversized negotiated manifest was accepted"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            NodeServerError::ProviderContractManifest(message)
                if message.contains("authentication binding length")
                    && message.contains("8192-byte limit")
        ));
    }

    #[test]
    fn provider_contract_manifest_rejects_wire_revision_overflow_before_startup() {
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        spec.revision = "x".repeat(crate::protocol::MAX_PROVIDER_CONTRACT_REVISION_BYTES + 1);
        let registry = AgentRegistry::new([spec]).unwrap();
        assert!(matches!(
            provider_contract_manifest(&registry),
            Err(NodeServerError::ProviderContractManifest(message))
                if message.contains("exceeds the 128-byte limit")
        ));
    }

    #[test]
    fn provider_contract_manifest_rejects_invalid_wire_revision_before_startup() {
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        spec.revision = "invalid revision".to_owned();
        let registry = AgentRegistry::new([spec]).unwrap();
        assert!(matches!(
            provider_contract_manifest(&registry),
            Err(NodeServerError::ProviderContractManifest(message))
                if message.contains("bounded lowercase ASCII")
        ));
    }

    #[test]
    fn provider_contract_manifest_rejects_conflicting_duplicate_family_before_startup() {
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        let transport_binding = spec.capabilities.adapters.one_shot.clone().unwrap();
        let conflicting_binding = builtin_registry()
            .get_by_id("codex")
            .unwrap()
            .capabilities
            .adapters
            .one_shot
            .clone()
            .unwrap();
        let pipe = spec.capabilities.transports.pipe.as_mut().unwrap();
        pipe.protocol = gate4agent_types::PipeProtocol::OneShotText;
        pipe.adapter = transport_binding;
        spec.capabilities.adapters.one_shot = Some(conflicting_binding);
        let registry = AgentRegistry::new([spec]).unwrap();
        assert!(matches!(
            provider_contract_manifest(&registry),
            Err(NodeServerError::ProviderContractManifest(message))
                if message.contains("conflicting OneShot adapter bindings")
        ));
    }

    #[test]
    fn public_node_failures_expose_only_fixed_categories() {
        let provider = failure(
            NodeFailureCode::BackendOperationFailed,
            r"provider bearer-secret failed at C:\private\session.jsonl",
        );
        assert_eq!(provider.message, "backend-operation-failed");
        assert!(!provider.message.contains("bearer-secret"));
        assert!(!provider.message.contains(r"C:\private"));
        let persistence = persistence_failure(io::Error::new(
            io::ErrorKind::Other,
            r"state write failed at C:\private\state-v1.json",
        ));
        assert_eq!(persistence.message, DURABLE_STATE_COMMIT_FAILED_ERROR);
        assert!(!persistence.message.contains(r"C:\private"));

        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Unsupported] {
            let generic = durable_state_load_error(io::Error::new(kind, "host I/O failure"));
            assert_eq!(
                generic.to_string(),
                "node durable state failed: durable-state-load-failed",
            );
        }
    }

    #[test]
    fn terminal_byte_sequences_are_bounded_before_dispatch() {
        let sequence = b"\x1b[1;5D".to_vec();
        assert_eq!(
            terminal_bytes_action(sequence.clone()).unwrap(),
            InputAction::TerminalBytes(sequence),
        );
        assert_eq!(
            terminal_bytes_action(Vec::new()).unwrap_err().code,
            NodeFailureCode::InvalidRequest,
        );
        assert_eq!(
            terminal_bytes_action(vec![0; MAX_NODE_TERMINAL_BYTES + 1])
                .unwrap_err()
                .code,
            NodeFailureCode::InvalidRequest,
        );
    }

    #[test]
    fn node_server_rejects_a_second_owner_of_the_same_durable_state_path() {
        let root = temporary_workspace_root("exclusive-state-owner");
        std::fs::create_dir_all(&root).unwrap();
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            &root,
        )
        .unwrap();
        let state_path = root.join("state-v1.json");
        let endpoint = format!(
            r"\\.\pipe\gate4agent-node-state-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        );
        let config = NodeServerConfig::new(
            endpoint,
            "fixture-token",
            NodeId::new("node-1").unwrap(),
            [workspace],
        )
        .unwrap()
        .with_state_path(&state_path)
        .unwrap();

        let first = NodeServer::new(config.clone()).unwrap();
        let error = match NodeServer::new(config.clone()) {
            Ok(_) => panic!("a second node server acquired the same durable state path"),
            Err(error) => error,
        };
        assert!(matches!(
            &error,
            NodeServerError::DurableState(error)
                if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert_eq!(
            error.to_string(),
            "node durable state failed: durable-state-lock-failed",
        );
        assert!(!error.to_string().contains(&state_path.to_string_lossy().into_owned()));
        drop(first);
        let second = NodeServer::new(config).unwrap();
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_binding_is_workspace_scoped_and_removed_fail_closed() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace.clone()],
            vec![agent("claude"), agent("codex"), agent("kimi")],
        );
        let instance_id = AgentInstanceId(17);
        let generation = SessionGeneration(3);
        assert!(shared.address_for(instance_id, generation).is_none());
        shared.bind_session(instance_id, workspace_id.clone(), generation);
        let address = SessionAddress {
            workspace_id,
            session: SessionKey { instance_id, generation },
        };
        assert_eq!(
            shared.workspace_root(&address.workspace_id).unwrap(),
            workspace.canonical_root,
        );
        assert_eq!(shared.address_for(instance_id, generation), Some(address.clone()));
        shared.remove_binding(&address);
        assert!(shared.address_for(instance_id, generation).is_none());
    }

    #[test]
    fn open_provider_request_gate_is_state_aware_and_fail_closed() {
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(2),
            },
        };
        let record_id = SessionRecordId::new("record-a").unwrap();
        let size = gate4agent_types::TerminalSize {
            rows: 24,
            columns: 80,
        };
        let address_requests = [
            NodeRequest::Resume {
                session: address.clone(),
                terminal_size: size,
                initial_prompt: None,
            },
            NodeRequest::Prompt { session: address.clone(), text: "p".to_owned() },
            NodeRequest::Paste { session: address.clone(), text: "p".to_owned() },
            NodeRequest::Input { session: address.clone(), text: "p".to_owned() },
            NodeRequest::TerminalBytes { session: address.clone(), bytes: vec![1] },
            NodeRequest::TerminalControl {
                session: address.clone(),
                control: TerminalControl::Enter,
            },
            NodeRequest::Resize { session: address.clone(), size },
            NodeRequest::Interrupt { session: address.clone() },
            NodeRequest::Stop { session: address.clone(), force: false },
            NodeRequest::Remove { session: address },
        ];
        for request in &address_requests {
            assert!(request_requires_open_provider_ids_with(
                request,
                |_| Some(agent("qwen-code")),
                |_| Some(agent("claude")),
            ));
            assert!(!request_requires_open_provider_ids_with(
                request,
                |_| Some(agent("claude")),
                |_| Some(agent("claude")),
            ));
            assert!(request_requires_open_provider_ids_with(
                request,
                |_| None,
                |_| Some(agent("claude")),
            ));
        }

        let record_requests = [
            NodeRequest::RenameSessionRecord {
                record_id: record_id.clone(),
                display_name: "renamed".to_owned(),
            },
            NodeRequest::ResumeSessionRecord {
                record_id: record_id.clone(),
                terminal_size: size,
                initial_prompt: None,
            },
            NodeRequest::ForgetSessionRecord { record_id },
        ];
        for request in &record_requests {
            assert!(request_requires_open_provider_ids_with(
                request,
                |_| Some(agent("claude")),
                |_| Some(agent("grok")),
            ));
            assert!(!request_requires_open_provider_ids_with(
                request,
                |_| Some(agent("claude")),
                |_| Some(agent("kimi")),
            ));
            assert!(request_requires_open_provider_ids_with(
                request,
                |_| Some(agent("claude")),
                |_| None,
            ));
        }

        assert!(request_requires_open_provider_ids_with(
            &NodeRequest::Spawn {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                provider: agent("qwen-code"),
                mode: SessionMode::Pty,
                terminal_size: size,
                initial_prompt: None,
            },
            |_| None,
            |_| None,
        ));
        assert!(!request_requires_open_provider_ids_with(
            &NodeRequest::Snapshot,
            |_| None,
            |_| None,
        ));
        assert!(request_requires_open_provider_ids_with(
            &NodeRequest::ForgetContextPack {
                context_id: SpawnContextId::new("ctx-forget").unwrap(),
            },
            |_| None,
            |_| None,
        ));
    }

    #[test]
    fn legacy_projection_filters_open_record_replies_and_event_history() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude"), agent("grok")],
        );
        let legacy = record("claude", "legacy-record");
        let open = record("grok", "open-record");
        let legacy_upsert = shared.publish(NodeEvent::SessionRecordUpserted {
            record: legacy.clone(),
        });
        let open_upsert = shared.publish(NodeEvent::SessionRecordUpserted {
            record: open.clone(),
        });
        let legacy_removed = shared.publish(NodeEvent::SessionRecordRemoved {
            record_id: legacy.record_id.clone(),
        });
        let open_removed = shared.publish(NodeEvent::SessionRecordRemoved {
            record_id: open.record_id.clone(),
        });
        assert!(project_event_legacy_provider_ids(&shared, legacy_upsert.clone()).is_some());
        assert!(project_event_legacy_provider_ids(&shared, open_upsert.clone()).is_none());
        assert!(project_event_legacy_provider_ids(&shared, legacy_removed.clone()).is_some());
        assert!(project_event_legacy_provider_ids(&shared, open_removed.clone()).is_none());

        let mut record_reply = ResponseEnvelope {
            request_id: 1,
            result: Ok(NodeResponse::SessionRecordUpdated { record: open }),
        };
        project_response_legacy_provider_ids(&shared, &mut record_reply);
        assert!(matches!(
            record_reply.result,
            Err(NodeFailure { code: NodeFailureCode::UnsupportedCapability, .. })
        ));

        let mut resync = ResponseEnvelope {
            request_id: 2,
            result: Ok(NodeResponse::Resync {
                event_sequence: open_removed.sequence,
                oldest_available_sequence: 1,
                snapshot: shared.snapshot(),
                events: vec![legacy_upsert, open_upsert, legacy_removed, open_removed],
            }),
        };
        project_response_legacy_provider_ids(&shared, &mut resync);
        let Ok(NodeResponse::Resync { snapshot, events, .. }) = resync.result else {
            panic!("legacy projection changed the resync response kind");
        };
        assert!(snapshot.session_records.iter().all(|item| {
            provider_id_is_legacy(&item.provider)
        }));
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| matches!(
            &event.event,
            NodeEvent::SessionRecordUpserted { record }
                if provider_id_is_legacy(&record.provider)
        ) || matches!(&event.event, NodeEvent::SessionRecordRemoved { .. })));
    }

    #[test]
    fn session_binding_capacity_is_bounded_by_the_control_session_limit() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        for index in 0..CONTROL_SESSIONS_MAX {
            shared.bind_session(
                AgentInstanceId((index + 1) as u64),
                workspace_id.clone(),
                SessionGeneration(1),
            );
        }
        let error = shared.ensure_binding_capacity().unwrap_err();
        assert_eq!(error.code, NodeFailureCode::BackendBusy);
    }

    #[tokio::test]
    async fn provider_spawn_admission_rejects_before_session_mutation() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-provider-admission").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        shared.provider_runtime_statuses = ProviderRuntimeStatuses::new([
            crate::protocol::ProviderRuntimeStatus::unavailable(agent("claude")),
        ])
        .unwrap();
        let terminal_size = gate4agent_types::TerminalSize {
            rows: 24,
            columns: 80,
        };

        let unavailable = shared
            .spawn_session(
                WorkspaceId::new("primary").unwrap(),
                agent("claude"),
                SessionMode::Pty,
                terminal_size,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(unavailable.code, NodeFailureCode::BackendOperationFailed);
        assert_eq!(unavailable.message, "backend-operation-failed");

        shared.provider_runtime_statuses = ProviderRuntimeStatuses::new([
            crate::protocol::ProviderRuntimeStatus::raw_passthrough(
                agent("claude"),
                Some(crate::protocol::ProviderRuntimeVersion::new("999.0.0").unwrap()),
            ),
        ])
        .unwrap();
        let semantic = shared
            .spawn_session(
                WorkspaceId::new("primary").unwrap(),
                agent("claude"),
                SessionMode::Inline,
                terminal_size,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(semantic.code, NodeFailureCode::UnsupportedCapability);
        assert_eq!(semantic.message, "unsupported-capability");

        assert_eq!(shared.next_instance_id.load(Ordering::Acquire), 1);
        assert!(shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
        assert!(shared
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .is_empty());
        assert!(shared.handle.snapshot().sessions.is_empty());
    }

    #[tokio::test]
    async fn raw_spawn_binding_is_visible_with_a_durable_live_record() {
        let catalog = active_registry().unwrap();
        let (handle, mut runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-raw-spawn-record").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let address = SessionAddress {
            workspace_id,
            session: SessionKey {
                instance_id: AgentInstanceId(71),
                generation: SessionGeneration::default(),
            },
        };

        let record_id = shared
            .bind_spawn_session(
                &address,
                agent("claude"),
                SessionMode::Pty,
                ProviderRuntimePolicy::raw_pty(),
                SpawnRecordPolicy::Always,
                None,
            )
            .unwrap()
            .unwrap();
        shared
            .handle
            .dispatch(shared.prepare_command(ControlCommand::Register {
                instance_id: address.session.instance_id,
                agent_id: agent("claude"),
                transport: TransportKind::Pty,
            }))
            .unwrap();
        runtime.tick().await;

        assert!(shared.handle.snapshot().sessions.iter().any(|session| {
            session.instance_id == address.session.instance_id
                && session.generation == address.session.generation
        }));
        let record = shared.record(&record_id).unwrap();
        assert_eq!(record.display_name, "claude #1");
        assert_eq!(record.state, ManagedSessionState::Live);
        assert_eq!(record.active_session.as_ref(), Some(&address));
        assert!(record.provider_session.is_none());
        let binding_record_id = shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .and_then(|binding| binding.record_id.clone());
        assert_eq!(binding_record_id.as_ref(), Some(&record_id));

        let renamed = shared
            .rename_session_record(&record_id, "raw review".to_owned())
            .unwrap();
        assert_eq!(renamed.record_id, record_id);
        assert_eq!(renamed.display_name, "raw review");
        shared.reconcile_managed_record(
            &record_id,
            &address,
            &ControlEvent {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                sequence: 1,
                command_id: None,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
                event: ControlEventKind::Running { process_id: None },
            },
            false,
            false,
        );
        let running = shared.record(&record_id).unwrap();
        assert_eq!(running.state, ManagedSessionState::Live);
        assert_eq!(running.active_session.as_ref(), Some(&address));
        assert_eq!(running.display_name, "raw review");

        shared.reconcile_managed_record(
            &record_id,
            &address,
            &ControlEvent {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                sequence: 2,
                command_id: None,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
                event: ControlEventKind::Exited {
                    exit_code: Some(0),
                    forced: false,
                },
            },
            false,
            false,
        );
        let detached = shared.record(&record_id).unwrap();
        assert_eq!(detached.state, ManagedSessionState::Unavailable);
        assert!(detached.active_session.is_none());
        assert_eq!(detached.display_name, "raw review");
        let renamed_detached = shared
            .rename_session_record(&record_id, "raw review detached".to_owned())
            .unwrap();
        assert_eq!(renamed_detached.record_id, record_id);
        assert_eq!(renamed_detached.state, ManagedSessionState::Unavailable);
        assert!(renamed_detached.active_session.is_none());
        assert_eq!(renamed_detached.display_name, "raw review detached");
        assert_eq!(shared.snapshot().session_records, vec![renamed_detached]);
    }

    #[test]
    fn legacy_raw_spawn_binding_remains_untracked() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-legacy-raw-spawn").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let address = SessionAddress {
            workspace_id,
            session: SessionKey {
                instance_id: AgentInstanceId(72),
                generation: SessionGeneration::default(),
            },
        };

        let record_id = shared
            .bind_spawn_session(
                &address,
                agent("claude"),
                SessionMode::Pty,
                ProviderRuntimePolicy::raw_pty(),
                SpawnRecordPolicy::ProviderIdentityOnly,
                None,
            )
            .unwrap();

        assert!(record_id.is_none());
        let binding_record_id = shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .and_then(|binding| binding.record_id.clone());
        assert!(binding_record_id.is_none());
        assert!(shared.snapshot().session_records.is_empty());
    }

    #[tokio::test]
    async fn raw_runtime_policy_rejects_prompt_paste_and_resume_before_mutation() {
        let catalog = active_registry().unwrap();
        let (handle, mut runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-raw-semantic-gate").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        shared.provider_runtime_statuses = ProviderRuntimeStatuses::new([
            crate::protocol::ProviderRuntimeStatus::raw_passthrough(agent("claude"), None),
        ])
        .unwrap();
        shared
            .acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS)
            .unwrap();
        let address = SessionAddress {
            workspace_id,
            session: SessionKey {
                instance_id: AgentInstanceId(72),
                generation: SessionGeneration::default(),
            },
        };
        shared.bind_session_with_policy(
            &address,
            ProviderRuntimePolicy::raw_pty(),
            None,
        );
        shared
            .handle
            .dispatch(shared.prepare_command(ControlCommand::Register {
                instance_id: address.session.instance_id,
                agent_id: agent("claude"),
                transport: TransportKind::Pty,
            }))
            .unwrap();
        runtime.tick().await;
        let command_id_before = shared.next_command_id.load(Ordering::Acquire);
        let revision_before = shared.handle.snapshot().revision;
        let history_sequence_before = shared.current_sequence();
        let terminal_size = gate4agent_types::TerminalSize {
            rows: 24,
            columns: 80,
        };

        for request in [
            NodeRequest::Prompt {
                session: address.clone(),
                text: "prompt".to_owned(),
            },
            NodeRequest::Paste {
                session: address.clone(),
                text: "paste".to_owned(),
            },
            // `initial_prompt: Some(_)` selects `ResumeWithPrompt`, which needs
            // the same semantic capabilities as `Prompt`/`Paste`. A bare
            // (promptless) resume is a raw PTY relaunch and IS admitted under
            // `raw_pty()` — see `raw_pty_admits_provider_native_resume_without_prompt_only`
            // in `provider_runtime.rs`; it is deliberately not exercised here.
            NodeRequest::Resume {
                session: address.clone(),
                terminal_size,
                initial_prompt: Some("resume".to_owned()),
            },
        ] {
            let error = process_request_inner(
                &shared,
                77,
                ClientRole::Operator,
                request,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, NodeFailureCode::UnsupportedCapability);
        }

        assert_eq!(shared.next_command_id.load(Ordering::Acquire), command_id_before);
        assert_eq!(shared.handle.snapshot().revision, revision_before);
        assert_eq!(shared.current_sequence(), history_sequence_before);
        let bindings = shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = bindings.get(&address.session.instance_id).unwrap();
        assert_eq!(binding.generation, address.session.generation);
        assert!(binding.pending_resume.is_none());
        assert!(binding.record_id.is_none());
    }

    #[tokio::test]
    async fn windows_rejects_unix_bytes_workspace_path_before_filesystem_access() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let primary = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![primary],
            vec![agent("claude")],
        );
        shared
            .acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS)
            .unwrap();
        let before = shared.snapshot();
        let error = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::RegisterWorkspace {
                workspace_id: WorkspaceId::new("foreign").unwrap(),
                root: OpaqueHostPath::unix_bytes(b"/srv/repo".to_vec()).unwrap(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, NodeFailureCode::InvalidRequest);
        assert_eq!(error.message, "invalid-request");
        assert_eq!(shared.snapshot().workspaces, before.workspaces);
    }

    #[tokio::test]
    async fn dynamic_workspaces_reject_duplicates_busy_and_last_then_publish_ordered_events() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let primary_id = WorkspaceId::new("primary").unwrap();
        let primary = WorkspaceConfig::new(
            primary_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![primary.clone()],
            vec![agent("claude")],
        );
        let secondary_id = WorkspaceId::new("secondary").unwrap();
        let secondary = shared
            .register_workspace(
                secondary_id.clone(),
                std::env::temp_dir().to_string_lossy().into_owned(),
            )
            .await
            .unwrap();
        assert_eq!(secondary.workspace_id, secondary_id);
        assert!(secondary.sessions.is_empty());
        assert_eq!(shared.snapshot().workspaces.len(), 2);

        let duplicate_id = shared
            .register_workspace(
                secondary_id.clone(),
                primary.canonical_root().to_owned(),
            )
            .await
            .unwrap_err();
        assert_eq!(duplicate_id.code, NodeFailureCode::DuplicateWorkspaceId);
        let duplicate_root = shared
            .register_workspace(
                WorkspaceId::new("duplicate-root").unwrap(),
                windows_path_text(&secondary.canonical_root).to_owned(),
            )
            .await
            .unwrap_err();
        assert_eq!(duplicate_root.code, NodeFailureCode::DuplicateWorkspaceRoot);

        let address = SessionAddress {
            workspace_id: secondary_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(41),
                generation: SessionGeneration(1),
            },
        };
        shared.bind_session(
            address.session.instance_id,
            secondary_id.clone(),
            address.session.generation,
        );
        let busy = shared.unregister_workspace(&secondary_id).unwrap_err();
        assert_eq!(busy.code, NodeFailureCode::WorkspaceBusy);
        shared.remove_binding(&address);
        shared.unregister_workspace(&secondary_id).unwrap();
        assert_eq!(shared.snapshot().workspaces.len(), 1);

        let last = shared.unregister_workspace(&primary_id).unwrap_err();
        assert_eq!(last.code, NodeFailureCode::LastWorkspace);
        let history = shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(matches!(
            &history.events[0],
            NodeEventEnvelope {
                sequence: 1,
                event: NodeEvent::WorkspaceAdded { workspace },
            } if workspace.workspace_id == secondary_id
        ));
        assert!(matches!(
            &history.events[1],
            NodeEventEnvelope {
                sequence: 2,
                event: NodeEvent::WorkspaceRemoved { workspace_id },
            } if workspace_id == &secondary_id
        ));
    }

    #[tokio::test]
    async fn provider_session_reference_index_is_controller_gated_and_idempotent() {
        let secondary_root = temporary_workspace_root("provider-session-reference-index");
        std::fs::create_dir_all(&secondary_root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let primary_id = WorkspaceId::new("primary").unwrap();
        let secondary_id = WorkspaceId::new("secondary").unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-session-reference-index").unwrap(),
            vec![
                WorkspaceConfig::new(primary_id.clone(), std::env::current_dir().unwrap())
                    .unwrap(),
                WorkspaceConfig::new(secondary_id.clone(), &secondary_root).unwrap(),
            ],
            vec![agent("claude"), agent("codex")],
        );
        let identity = ProviderSessionIdentity {
            key: gate4agent_types::ProviderSessionKey::SessionId,
            id: "provider-session-42".to_owned(),
            transcript_path: None,
        };
        let index_request = |workspace_id: WorkspaceId,
                             provider: AgentId,
                             identity: ProviderSessionIdentity,
                             display_name: &str| {
            NodeRequest::IndexProviderSession {
                workspace_id,
                provider,
                identity,
                display_name: display_name.to_owned(),
            }
        };
        let request = index_request(
            primary_id.clone(),
            agent("claude"),
            identity.clone(),
            "release shepherd",
        );
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY
        }));
        let observer = process_request_inner(
            &shared,
            77,
            ClientRole::Observer,
            request.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        let uncontrolled = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            request.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(uncontrolled.code, NodeFailureCode::ControllerRequired);
        shared
            .acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS)
            .unwrap();

        let indexed = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            request.clone(),
        )
        .await
        .unwrap();
        let NodeResponse::ProviderSessionIndexed { record } = indexed else {
            panic!("provider session reference index returned another response");
        };
        assert_eq!(record.state, ManagedSessionState::Dormant);
        assert_eq!(record.mode, SessionMode::Pty);
        assert_eq!(record.workspace_id, primary_id);
        assert_eq!(record.provider_session.as_ref(), Some(&identity));
        assert!(record.active_session.is_none());
        assert!(record.environment_profile.is_none());
        assert!(record.bundle.is_none());
        assert!(record.context.is_none());

        let repeated = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            index_request(
                primary_id.clone(),
                agent("claude"),
                identity.clone(),
                "ignored replacement",
            ),
        )
        .await
        .unwrap();
        assert!(matches!(
            repeated,
            NodeResponse::ProviderSessionIndexed { record: ref existing }
                if existing.record_id == record.record_id
                    && existing.display_name == "release shepherd"
        ));
        let conflicting_workspace = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            index_request(
                secondary_id,
                agent("claude"),
                identity.clone(),
                "release shepherd",
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(conflicting_workspace.code, NodeFailureCode::SessionRecordConflict);
        let conflicting_provider = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            index_request(
                primary_id.clone(),
                agent("codex"),
                identity.clone(),
                "release shepherd",
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(conflicting_provider.code, NodeFailureCode::SessionRecordConflict);
        let transcript_rejected = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            index_request(
                primary_id,
                agent("claude"),
                ProviderSessionIdentity {
                    transcript_path: Some("C:/provider/transcript.jsonl".to_owned()),
                    ..identity
                },
                "release shepherd",
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(transcript_rejected.code, NodeFailureCode::InvalidRequest);
        let upserts = shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .events
            .iter()
            .filter(|event| matches!(
                &event.event,
                NodeEvent::SessionRecordUpserted { record: published }
                    if published.record_id == record.record_id
            ))
            .count();
        assert_eq!(upserts, 1);
        std::fs::remove_dir_all(secondary_root).unwrap();
    }

    #[tokio::test]
    async fn standalone_workspace_is_independent_registered_and_controller_gated() {
        let fixture_root = temporary_workspace_root("standalone-independent");
        let target = fixture_root.join("independent");
        std::fs::create_dir_all(&fixture_root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let selected = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let selected_root = selected.canonical_root().to_owned();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-standalone").unwrap(),
            vec![selected],
            vec![agent("claude")],
        );
        let workspace_id = WorkspaceId::new("independent").unwrap();
        let request = NodeRequest::CreateStandaloneWorkspace {
            workspace_id: workspace_id.clone(),
            root: opaque_windows_path(target.to_string_lossy().into_owned()),
            initial_branch: Some("main".to_owned()),
        };
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY
        }));
        assert!(!request_uses_unnegotiated_capability(
            &request,
            &baseline_capabilities().unwrap(),
        ));
        assert!(request_uses_unnegotiated_capability(&request, &[]));

        let observer = process_request_inner(&shared, 77, ClientRole::Observer, request.clone())
            .await
            .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        assert!(!target.exists());
        let uncontrolled = process_request_inner(&shared, 77, ClientRole::Operator, request.clone())
            .await
            .unwrap_err();
        assert_eq!(uncontrolled.code, NodeFailureCode::ControllerRequired);
        assert!(!target.exists());
        shared
            .acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS)
            .unwrap();

        let response = process_request_inner(&shared, 77, ClientRole::Operator, request)
            .await
            .unwrap();
        let NodeResponse::StandaloneWorkspaceCreated { workspace } = response else {
            panic!("standalone workspace creation returned another response");
        };
        assert_eq!(workspace.workspace_id, workspace_id);
        assert!(target.join(".git").is_dir());
        let branch = run_git_bounded(
            target.to_str().unwrap(),
            &["branch", "--show-current"],
            GIT_OUTPUT_MAX_BYTES,
        )
        .await
        .unwrap();
        assert!(branch.success);
        assert_eq!(String::from_utf8(branch.stdout).unwrap().trim(), "main");
        let worktrees = list_git_worktrees(target.to_str().unwrap()).await.unwrap();
        assert_eq!(worktrees.len(), 1);
        let listed_root = WorkspaceConfig::new(
            WorkspaceId::new("listed-independent").unwrap(),
            &worktrees[0].path,
        )
        .unwrap();
        assert!(platform::roots_equal(
            listed_root.canonical_root(),
            windows_path_text(&workspace.canonical_root),
        ));
        assert!(!platform::roots_equal(listed_root.canonical_root(), &selected_root));
        assert!(shared.snapshot().workspaces.iter().any(|snapshot| {
            snapshot.workspace_id == workspace_id
                && snapshot.canonical_root == workspace.canonical_root
        }));
        assert!(shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .events
            .iter()
            .any(|event| matches!(
                &event.event,
                NodeEvent::WorkspaceAdded { workspace }
                    if workspace.workspace_id == workspace_id
            )));
        std::fs::remove_dir_all(fixture_root).unwrap();
    }

    #[tokio::test]
    async fn standalone_workspace_registration_failure_preserves_guarded_repository_for_recovery() {
        let fixture_root = temporary_workspace_root("standalone-compensation");
        let created_target = fixture_root.join("created-target");
        let existing_target = fixture_root.join("existing-target");
        let invalid_state_parent = fixture_root.join("state-parent-is-a-file");
        let invalid_state_path = invalid_state_parent.join("state.json");
        std::fs::create_dir_all(&existing_target).unwrap();
        std::fs::write(&invalid_state_parent, "not a directory").unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let primary = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-standalone-compensation").unwrap(),
            vec![primary],
            vec![agent("claude")],
        );
        shared.state_path = Some(invalid_state_path);

        let created_recovery = shared
            .create_standalone_workspace(
                WorkspaceId::new("compensated").unwrap(),
                created_target.to_string_lossy().into_owned(),
                Some("main".to_owned()),
            )
            .await
            .unwrap_err();
        assert_eq!(
            created_recovery.code,
            NodeFailureCode::StandaloneWorkspaceRecoveryRequired,
        );
        assert!(created_target.join(".git").is_dir());
        assert!(shared.snapshot().workspaces.iter().all(|workspace| {
            workspace.workspace_id.as_str() != "compensated"
        }));

        let existing_recovery = shared
            .create_standalone_workspace(
                WorkspaceId::new("restored").unwrap(),
                existing_target.to_string_lossy().into_owned(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            existing_recovery.code,
            NodeFailureCode::StandaloneWorkspaceRecoveryRequired,
        );
        assert!(existing_target.is_dir());
        assert!(existing_target.join(".git").is_dir());
        assert!(shared.snapshot().workspaces.iter().all(|workspace| {
            workspace.workspace_id.as_str() != "restored"
        }));

        let registered_empty = fixture_root.join("registered-empty");
        std::fs::create_dir(&registered_empty).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let registered = WorkspaceConfig::new(
            WorkspaceId::new("registered-empty").unwrap(),
            &registered_empty,
        )
        .unwrap();
        let duplicate_shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-standalone-duplicate-root").unwrap(),
            vec![registered],
            vec![agent("claude")],
        );
        let duplicate = duplicate_shared
            .create_standalone_workspace(
                WorkspaceId::new("duplicate-root").unwrap(),
                registered_empty.to_string_lossy().into_owned(),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(duplicate.code, NodeFailureCode::DuplicateWorkspaceRoot);
        assert!(std::fs::read_dir(&registered_empty).unwrap().next().is_none());
        std::fs::remove_dir_all(fixture_root).unwrap();
    }

    #[test]
    fn managed_record_id_collision_preserves_the_existing_record() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace.clone()],
            vec![agent("claude")],
        );
        let record_id = SessionRecordId::new("sr-collision").unwrap();
        let original = ManagedSessionRecord {
            record_id: record_id.clone(),
            display_name: "original".to_owned(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: workspace.workspace_id,
            canonical_root: opaque_windows_path(workspace.canonical_root),
            provider_session: None,
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: None,
            task_binding: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            last_error: None,
        };
        shared.insert_record(original.clone()).unwrap();
        let mut collision = original.clone();
        collision.display_name = "replacement".to_owned();
        let error = shared.insert_record(collision).unwrap_err();
        assert_eq!(error.code, NodeFailureCode::SessionRecordConflict);
        assert_eq!(shared.record(&record_id).unwrap(), original);
    }

    #[test]
    fn managed_record_runtime_errors_are_exposed_only_as_allowlisted_categories() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace.clone()],
            vec![agent("claude")],
        );
        let record_id = SessionRecordId::new("sr-runtime-error").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: record_id.clone(),
                display_name: "runtime error".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: workspace.workspace_id,
                canonical_root: opaque_windows_path(workspace.canonical_root),
                provider_session: None,
                active_session: None,
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();

        shared
            .mark_record_error(
                &record_id,
                r"provider failed with bearer-secret-123 at C:\private\session.jsonl",
            )
            .unwrap();
        let record = shared.record(&record_id).unwrap();
        assert_eq!(record.state, ManagedSessionState::Unavailable);
        assert_eq!(
            record.last_error.as_deref(),
            Some("provider-runtime-failed"),
        );
        let snapshot = shared.snapshot();
        assert_eq!(snapshot.session_records, vec![record]);
    }

    #[test]
    fn provider_identity_update_is_ignored_without_exact_session_policy() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-identity-policy").unwrap(),
            vec![workspace.clone()],
            vec![agent("claude")],
        );
        let address = SessionAddress {
            workspace_id: workspace.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(90),
                generation: SessionGeneration(1),
            },
        };
        let record_id = SessionRecordId::new("sr-policy-denied").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: record_id.clone(),
                display_name: "policy denied".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: workspace.workspace_id,
                canonical_root: opaque_windows_path(workspace.canonical_root),
                provider_session: None,
                active_session: Some(address.clone()),
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();
        let event = ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 1,
            command_id: None,
            instance_id: address.session.instance_id,
            generation: address.session.generation,
            event: ControlEventKind::ProviderEvent {
                sequence: 1,
                source: gate4agent_types::ProviderSource {
                    family: AdapterFamily::PtySemantic,
                    binding: AdapterBinding::new(
                        gate4agent_types::AdapterId::new("claude").unwrap(),
                        "fixture/v1",
                        gate4agent_types::AdapterVerification::SyntheticFixture,
                    )
                    .unwrap(),
                },
                source_sequence: 1,
                event: gate4agent_types::ProviderEvent::SessionIdentityObserved {
                    identity: gate4agent_types::ProviderSessionIdentity {
                        key: gate4agent_types::ProviderSessionKey::SessionId,
                        id: "provider-session".to_owned(),
                        transcript_path: None,
                    },
                },
            },
        };

        shared.reconcile_managed_record(&record_id, &address, &event, false, false);

        let record = shared.record(&record_id).unwrap();
        assert!(record.provider_session.is_none());
        assert_eq!(record.state, ManagedSessionState::IdentityPending);
        assert_eq!(record.active_session.as_ref(), Some(&address));
    }

    #[test]
    fn provider_identity_cannot_rebind_a_record_across_workspaces() {
        let secondary_root = temporary_workspace_root("identity-scope");
        std::fs::create_dir_all(&secondary_root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let primary = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let secondary = WorkspaceConfig::new(
            WorkspaceId::new("secondary").unwrap(),
            &secondary_root,
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![primary.clone(), secondary.clone()],
            vec![agent("claude")],
        );
        let identity = gate4agent_types::ProviderSessionIdentity {
            key: gate4agent_types::ProviderSessionKey::SessionId,
            id: "provider-session".to_owned(),
            transcript_path: None,
        };
        let dormant_id = SessionRecordId::new("sr-dormant").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: dormant_id.clone(),
                display_name: "dormant".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::Dormant,
                workspace_id: primary.workspace_id,
                canonical_root: opaque_windows_path(primary.canonical_root),
                provider_session: Some(identity.clone()),
                active_session: None,
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();
        let address = SessionAddress {
            workspace_id: secondary.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(91),
                generation: SessionGeneration(1),
            },
        };
        let current_id = SessionRecordId::new("sr-current").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: current_id.clone(),
                display_name: "current".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: secondary.workspace_id,
                canonical_root: opaque_windows_path(secondary.canonical_root),
                provider_session: None,
                active_session: Some(address.clone()),
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 2,
                updated_at_unix_ms: 2,
                last_error: None,
            })
            .unwrap();
        shared.bind_managed_session(
            &address,
            current_id.clone(),
            ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
            None,
        );
        let event = ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 1,
            command_id: None,
            instance_id: address.session.instance_id,
            generation: address.session.generation,
            event: ControlEventKind::ProviderEvent {
                sequence: 1,
                source: gate4agent_types::ProviderSource {
                    family: gate4agent_types::AdapterFamily::PtySemantic,
                    binding: gate4agent_types::AdapterBinding::new(
                        gate4agent_types::AdapterId::new("claude").unwrap(),
                        "fixture/v1",
                        gate4agent_types::AdapterVerification::SyntheticFixture,
                    )
                    .unwrap(),
                },
                source_sequence: 1,
                event: gate4agent_types::ProviderEvent::SessionIdentityObserved {
                    identity,
                },
            },
        };
        shared.reconcile_managed_record(&current_id, &address, &event, true, false);

        let binding = shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .cloned()
            .unwrap();
        assert_eq!(binding.record_id.as_ref(), Some(&current_id));
        assert_eq!(shared.record(&dormant_id).unwrap().state, ManagedSessionState::Dormant);
        let current = shared.record(&current_id).unwrap();
        assert_eq!(current.state, ManagedSessionState::Unavailable);
        assert_eq!(current.active_session.as_ref(), Some(&address));
        assert_eq!(
            current.last_error.as_deref(),
            Some(PROVIDER_SESSION_SCOPE_CONFLICT_ERROR),
        );
        std::fs::remove_dir_all(secondary_root).unwrap();
    }

    #[tokio::test]
    async fn semantic_identity_with_transcript_path_reuses_record_and_survives_reload() {
        let root = temporary_workspace_root("semantic-provider-session");
        std::fs::create_dir_all(&root).unwrap();
        let state_path = root.join("state-v1.json");
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            &root,
        )
        .unwrap();
        let node_id = NodeId::new("node-semantic-identity").unwrap();
        let shared = NodeShared::new_with_incarnation(
            handle,
            "fixture-token".to_owned(),
            node_id.clone(),
            NodeIncarnationId::from_bytes([0; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            vec![workspace.clone()],
            vec![agent("claude")],
            ProviderRuntimeStatuses::default(),
            None,
            Vec::new(),
            Vec::new(),
            SpawnProfileRegistry::default(),
            None,
            None,
            None,
            Some(state_path.clone()),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            MUTATION_SETTLE_TIMEOUT_MS.saturating_add(READINESS_SETTLE_HEADROOM_MS),
        );
        let persisted_identity = gate4agent_types::ProviderSessionIdentity {
            key: gate4agent_types::ProviderSessionKey::SessionId,
            id: "provider-session".to_owned(),
            transcript_path: None,
        };
        let durable_id = SessionRecordId::new("sr-durable").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: durable_id.clone(),
                display_name: "durable".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::Dormant,
                workspace_id: workspace.workspace_id.clone(),
                canonical_root: opaque_windows_path(workspace.canonical_root.clone()),
                provider_session: Some(persisted_identity.clone()),
                active_session: None,
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();
        let address = SessionAddress {
            workspace_id: workspace.workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(92),
                generation: SessionGeneration(2),
            },
        };
        let pending_id = SessionRecordId::new("sr-pending").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: pending_id.clone(),
                display_name: "pending".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: workspace.workspace_id.clone(),
                canonical_root: opaque_windows_path(workspace.canonical_root.clone()),
                provider_session: None,
                active_session: Some(address.clone()),
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 2,
                updated_at_unix_ms: 2,
                last_error: None,
            })
            .unwrap();
        shared.bind_managed_session(
            &address,
            pending_id.clone(),
            ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
            None,
        );
        let observed_identity = gate4agent_types::ProviderSessionIdentity {
            transcript_path: Some(r"C:\private\provider-transcript.jsonl".to_owned()),
            ..persisted_identity
        };
        let event = ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 1,
            command_id: None,
            instance_id: address.session.instance_id,
            generation: address.session.generation,
            event: ControlEventKind::ProviderEvent {
                sequence: 1,
                source: gate4agent_types::ProviderSource {
                    family: gate4agent_types::AdapterFamily::PtySemantic,
                    binding: gate4agent_types::AdapterBinding::new(
                        gate4agent_types::AdapterId::new("claude").unwrap(),
                        "fixture/v1",
                        gate4agent_types::AdapterVerification::SyntheticFixture,
                    )
                    .unwrap(),
                },
                source_sequence: 1,
                event: gate4agent_types::ProviderEvent::SessionIdentityObserved {
                    identity: observed_identity.clone(),
                },
            },
        };
        shared.reconcile_managed_record(&pending_id, &address, &event, true, false);

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.session_records.len(), 1);
        let live = &snapshot.session_records[0];
        assert_eq!(live.record_id, durable_id);
        assert_eq!(live.state, ManagedSessionState::Live);
        assert_eq!(live.active_session.as_ref(), Some(&address));
        assert_eq!(live.provider_session.as_ref(), Some(&observed_identity));
        let binding = shared
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&address.session.instance_id)
            .cloned()
            .unwrap();
        assert_eq!(binding.record_id.as_ref(), Some(&durable_id));

        let mut duplicate = live.clone();
        duplicate.record_id = SessionRecordId::new("sr-duplicate").unwrap();
        duplicate.display_name = "duplicate".to_owned();
        duplicate.state = ManagedSessionState::Dormant;
        duplicate.active_session = None;
        duplicate.provider_session.as_mut().unwrap().transcript_path = None;
        let duplicate_error = shared.insert_record(duplicate).unwrap_err();
        assert_eq!(duplicate_error.code, NodeFailureCode::SessionRecordConflict);
        assert_eq!(shared.snapshot().session_records.len(), 1);

        let terminal_size = gate4agent_types::TerminalSize {
            rows: 24,
            columns: 80,
        };
        let (first_busy, second_busy) = tokio::join!(
            shared.resume_session_record(&durable_id, terminal_size, None),
            shared.resume_session_record(&durable_id, terminal_size, None),
        );
        for busy in [first_busy.unwrap_err(), second_busy.unwrap_err()] {
            assert_eq!(busy.code, NodeFailureCode::SessionRecordBusy);
            assert_eq!(busy.message, "session-record-busy");
        }

        let loaded = session_registry::load(Some(&state_path), &node_id).unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert_eq!(loaded.records[0].record_id, durable_id);
        assert_eq!(loaded.records[0].state, ManagedSessionState::Dormant);
        assert_eq!(
            loaded.records[0]
                .provider_session
                .as_ref()
                .unwrap()
                .transcript_path,
            None,
        );
        let catalog = active_registry().unwrap();
        let (reloaded_handle, _reloaded_runtime) =
            NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let reloaded = NodeShared::new_with_incarnation(
            reloaded_handle,
            "fixture-token".to_owned(),
            node_id,
            NodeIncarnationId::from_bytes([1; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            vec![workspace],
            vec![agent("claude")],
            ProviderRuntimeStatuses::default(),
            None,
            Vec::new(),
            Vec::new(),
            SpawnProfileRegistry::default(),
            None,
            None,
            None,
            None,
            loaded.records,
            loaded.managed_worktrees,
            loaded.managed_worktree_tombstones,
            loaded.materializations,
            loaded.warning,
            MUTATION_SETTLE_TIMEOUT_MS.saturating_add(READINESS_SETTLE_HEADROOM_MS),
        );
        assert_eq!(reloaded.snapshot().session_records.len(), 1);
        assert_eq!(reloaded.snapshot().session_records[0].record_id, durable_id);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn workspace_reregistration_requires_the_recorded_root_and_restores_records() {
        let secondary_root = temporary_workspace_root("workspace-restore");
        let other_root = temporary_workspace_root("workspace-conflict");
        std::fs::create_dir_all(&secondary_root).unwrap();
        std::fs::create_dir_all(&other_root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let primary = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let secondary = WorkspaceConfig::new(
            WorkspaceId::new("secondary").unwrap(),
            &secondary_root,
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![primary, secondary.clone()],
            vec![agent("claude")],
        );
        let record_id = SessionRecordId::new("sr-workspace").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: record_id.clone(),
                display_name: "workspace record".to_owned(),
                provider: agent("claude"),
                mode: SessionMode::Pty,
                state: ManagedSessionState::Dormant,
                workspace_id: secondary.workspace_id.clone(),
                canonical_root: opaque_windows_path(secondary.canonical_root.clone()),
                provider_session: Some(gate4agent_types::ProviderSessionIdentity {
                    key: gate4agent_types::ProviderSessionKey::SessionId,
                    id: "provider-session".to_owned(),
                    transcript_path: None,
                }),
                active_session: None,
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                exported_context: None,
                task_binding: None,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
                last_error: None,
            })
            .unwrap();
        shared.unregister_workspace(&secondary.workspace_id).unwrap();
        let unavailable = shared.record(&record_id).unwrap();
        assert_eq!(unavailable.state, ManagedSessionState::Unavailable);
        assert_eq!(windows_path_text(&unavailable.canonical_root), secondary.canonical_root);

        let conflict = shared
            .register_workspace(
                secondary.workspace_id.clone(),
                other_root.to_string_lossy().into_owned(),
            )
            .await
            .unwrap_err();
        assert_eq!(conflict.code, NodeFailureCode::SessionRecordConflict);
        shared
            .register_workspace(
                secondary.workspace_id,
                secondary_root.to_string_lossy().into_owned(),
            )
            .await
            .unwrap();
        let restored = shared.record(&record_id).unwrap();
        assert_eq!(restored.state, ManagedSessionState::Dormant);
        assert!(restored.last_error.is_none());
        std::fs::remove_dir_all(secondary_root).unwrap();
        std::fs::remove_dir_all(other_root).unwrap();
    }

    #[test]
    fn dynamic_workspace_root_is_rejected_before_filesystem_canonicalization() {
        let error = validate_workspace_request_root(
            &"x".repeat(MAX_WORKSPACE_ROOT_BYTES + 1),
        )
        .unwrap_err();
        assert_eq!(error.code, NodeFailureCode::InvalidWorkspaceRoot);
        let control = validate_workspace_request_root("C:\\repo\nchild").unwrap_err();
        assert_eq!(control.code, NodeFailureCode::InvalidWorkspaceRoot);
    }

    #[test]
    fn concurrent_publish_and_resync_never_acknowledge_an_unappended_event() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let publishers = 4_u64;
        let events_per_publisher = 256_u64;
        let start = std::sync::Barrier::new(publishers as usize + 1);
        let finished = AtomicU64::new(0);

        std::thread::scope(|scope| {
            for _ in 0..publishers {
                scope.spawn(|| {
                    start.wait();
                    for _ in 0..events_per_publisher {
                        shared.publish(NodeEvent::ControllerChanged { controller: None });
                    }
                    finished.fetch_add(1, Ordering::Release);
                });
            }
            start.wait();
            while finished.load(Ordering::Acquire) != publishers {
                assert_resync_history_is_linearized(shared.resync(0));
                std::thread::yield_now();
            }
        });
        let response = shared.resync(0);
        assert_resync_history_is_linearized(response.clone());
        let NodeResponse::Resync { event_sequence, events, .. } = response else {
            unreachable!("resync helper returned another response");
        };
        assert_eq!(event_sequence, publishers * events_per_publisher);
        assert_eq!(events.len() as u64, event_sequence);
    }

    fn assert_resync_history_is_linearized(response: NodeResponse) {
        let NodeResponse::Resync { event_sequence, events, .. } = response else {
            unreachable!("resync helper returned another response");
        };
        assert!(events.windows(2).all(|pair| {
            pair[0].sequence.checked_add(1) == Some(pair[1].sequence)
        }));
        assert_eq!(events.last().map_or(0, |event| event.sequence), event_sequence);
    }

    #[test]
    fn canonical_workspace_roots_are_safe_for_cmd_exe() {
        assert_eq!(
            platform::normalize_canonical_root(r"\\?\C:\repo\worktree".to_owned()),
            r"C:\repo\worktree",
        );
        assert_eq!(
            platform::normalize_canonical_root(r"\\?\UNC\server\share\repo".to_owned()),
            r"\\server\share\repo",
        );
        assert!(validate_workspace_root(
            &WorkspaceId::new("network").unwrap(),
            r"\\server\share\repo",
        )
        .is_err());
        let workspace = WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        assert!(!workspace.canonical_root.starts_with(r"\\?\"));
    }

    #[test]
    fn resume_rebinds_only_on_the_expected_authorized_generation() {
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let instance_id = AgentInstanceId(17);
        let original = SessionAddress {
            workspace_id,
            session: SessionKey {
                instance_id,
                generation: SessionGeneration(3),
            },
        };
        shared.bind_session(
            instance_id,
            original.workspace_id.clone(),
            original.session.generation,
        );
        shared
            .arm_resume(
                &original,
                CommandId(41),
                ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
            )
            .unwrap();

        shared.publish_control(ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 1,
            command_id: None,
            instance_id,
            generation: SessionGeneration(4),
            event: ControlEventKind::Running { process_id: None },
        });
        assert_eq!(
            shared.address_for(instance_id, SessionGeneration(3)),
            Some(original.clone()),
        );
        assert!(shared.address_for(instance_id, SessionGeneration(4)).is_none());

        shared.publish_control(ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 2,
            command_id: Some(CommandId(42)),
            instance_id,
            generation: SessionGeneration(3),
            event: ControlEventKind::CommandRejected {
                message: "unrelated command".to_owned(),
            },
        });

        shared.publish_control(ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence: 3,
            command_id: None,
            instance_id,
            generation: SessionGeneration(4),
            event: ControlEventKind::ResumeAuthorized {
                session: gate4agent_types::ResumeSessionSummary {
                    key: gate4agent_types::ProviderSessionKey::SessionId,
                    id: "fixture-session".to_owned(),
                },
            },
        });
        assert!(shared.address_for(instance_id, SessionGeneration(3)).is_none());
        let rebound = shared.address_for(instance_id, SessionGeneration(4)).unwrap();
        assert_eq!(rebound.workspace_id, original.workspace_id);
        let history = shared
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(history.events.iter().any(|envelope| matches!(
            &envelope.event,
            NodeEvent::Control { address, event }
                if address == &rebound
                    && matches!(&event.event, ControlEventKind::ResumeAuthorized { .. })
        )));
    }

    #[test]
    fn all_node_pty_prompts_use_bracketed_paste_framing() {
        for agent in ["claude", "codex", "kimi"] {
            assert_eq!(
                prompt_framing(&AgentId::new(agent).unwrap()),
                PromptFraming::BracketedPaste
            );
        }
    }

    fn generous_walk_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn workspace_tree_is_relative_and_skips_heavy_directories() {
        let root = temporary_workspace_root("tree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/package")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn fixture() {}\n").unwrap();
        std::fs::write(root.join("README.md"), b"fixture\n").unwrap();
        // A renamed/agent-scoped build directory (`target-a2`-style, per the
        // real repository's own build layout) that carries the Cache
        // Directory Tagging Specification marker cargo writes into every
        // `--target-dir` it creates: must be skipped even though its name
        // does not exactly match `target`.
        std::fs::create_dir_all(root.join("target-a2/debug")).unwrap();
        std::fs::write(root.join("target-a2/CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55\n").unwrap();
        // A directory that merely *starts with* `target` but carries no
        // CACHEDIR.TAG: the new prefix-adjacent check must not false-match
        // on the name alone, so this stays in the response.
        std::fs::create_dir_all(root.join("target-notes")).unwrap();
        std::fs::write(root.join("target-notes/plan.md"), b"not a build directory\n").unwrap();

        let (entries, truncated, _budget) =
            collect_workspace_entries(&root, generous_walk_deadline(), WORKSPACE_INSPECTION_ENTRY_CAP_DEFAULT);
        assert!(!truncated);
        assert!(entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("src")
                && entry.kind == WorkspaceEntryKind::Directory
        }));
        assert!(entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("src/lib.rs")
                && entry.kind == WorkspaceEntryKind::File
        }));
        assert!(entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("target-notes")
                && entry.kind == WorkspaceEntryKind::Directory
        }));
        assert!(entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("target-notes/plan.md")
                && entry.kind == WorkspaceEntryKind::File
        }));
        assert!(entries.iter().all(|entry| {
            let path = entry.relative_path.as_utf8().unwrap();
            !path.starts_with(".git")
                && !(path == "target" || path.starts_with("target/"))
                && !(path == "target-a2" || path.starts_with("target-a2/"))
                && !path.starts_with("node_modules")
        }));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn workspace_walk_stops_at_its_entry_cap_and_marks_a_partial_result() {
        let root = temporary_workspace_root("tree-entry-cap");
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..20 {
            std::fs::write(root.join(format!("file-{index:02}.txt")), b"fixture\n").unwrap();
        }

        let (entries, truncated, budget) =
            collect_workspace_entries(&root, generous_walk_deadline(), 5);
        assert!(truncated);
        assert!(budget.entry_cap_exceeded);
        assert!(!budget.time_budget_exceeded);
        assert!(entries.len() <= 5);

        let inspection_truncation = workspace_inspection_truncation(&budget, false, Duration::from_millis(1))
            .expect("an entry-cap-truncated walk must report a truncation marker");
        assert!(inspection_truncation.walk_entry_cap_exceeded);
        assert!(!inspection_truncation.walk_time_budget_exceeded);
        assert!(!inspection_truncation.git_time_budget_exceeded);
        assert!(inspection_truncation.entries_visited >= 5);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn workspace_walk_within_budget_reports_no_truncation() {
        let root = temporary_workspace_root("tree-no-truncation");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("only-file.txt"), b"fixture\n").unwrap();

        let (_entries, truncated, budget) =
            collect_workspace_entries(&root, generous_walk_deadline(), WORKSPACE_INSPECTION_ENTRY_CAP_DEFAULT);
        assert!(!truncated);
        assert!(workspace_inspection_truncation(&budget, false, Duration::from_millis(1)).is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn workspace_walk_emits_strict_global_byte_order_for_prefix_named_siblings() {
        let root = temporary_workspace_root("tree-prefix-siblings");
        std::fs::create_dir_all(root.join("gate4agent-c2/src")).unwrap();
        std::fs::write(root.join("gate4agent-c2/src/lib.rs"), b"fixture\n").unwrap();
        std::fs::create_dir_all(root.join("gate4agent-c2-client")).unwrap();
        std::fs::write(root.join("gate4agent-c2-client/lib.rs"), b"fixture\n").unwrap();

        let (entries, truncated, _budget) =
            collect_workspace_entries(&root, generous_walk_deadline(), WORKSPACE_INSPECTION_ENTRY_CAP_DEFAULT);
        assert!(!truncated);
        let paths = entries
            .iter()
            .map(|entry| entry.relative_path.as_utf8().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            paths,
            vec![
                "gate4agent-c2".to_owned(),
                "gate4agent-c2-client".to_owned(),
                "gate4agent-c2-client/lib.rs".to_owned(),
                "gate4agent-c2/src".to_owned(),
                "gate4agent-c2/src/lib.rs".to_owned(),
            ],
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn git_parsers_cap_status_and_commit_results() {
        let mut status_output = Vec::new();
        for index in 0..=GIT_STATUS_MAX_ENTRIES {
            status_output.extend_from_slice(format!(" M src/file-{index}.rs\0").as_bytes());
        }
        let mut snapshot = git_snapshot_for_parser();
        parse_git_status(&status_output, &mut snapshot);
        assert_eq!(snapshot.status.len(), GIT_STATUS_MAX_ENTRIES);
        assert!(snapshot.truncated);
        assert_eq!(snapshot.status[0].index_status, " ");
        assert_eq!(snapshot.status[0].worktree_status, "M");

        snapshot.truncated = false;
        let commit_output = (0..=GIT_COMMIT_MAX_ENTRIES)
            .map(|index| format!("abc{index}\u{1f}commit {index}\n"))
            .collect::<String>();
        parse_git_commits(commit_output.as_bytes(), &mut snapshot);
        assert_eq!(snapshot.recent_commits.len(), GIT_COMMIT_MAX_ENTRIES);
        assert!(snapshot.truncated);
    }

    #[test]
    fn porcelain_v1_z_parser_preserves_current_then_previous_rename_and_copy_order() {
        let mut snapshot = git_snapshot_for_parser();
        parse_git_status(
            b"R  src/current name.rs\0src/previous name.rs\0 C src/copy.rs\0src/original.rs\0?? src/line\nname.rs\0",
            &mut snapshot,
        );

        assert!(!snapshot.truncated);
        assert_eq!(snapshot.status.len(), 3);
        assert_eq!(snapshot.status[0].index_status, "R");
        assert_eq!(snapshot.status[0].worktree_status, " ");
        assert_eq!(snapshot.status[0].path.as_utf8(), Some("src/current name.rs"));
        assert_eq!(
            snapshot.status[0]
                .previous_path
                .as_ref()
                .and_then(RepositoryPath::as_utf8),
            Some("src/previous name.rs"),
        );
        assert_eq!(snapshot.status[1].index_status, " ");
        assert_eq!(snapshot.status[1].worktree_status, "C");
        assert_eq!(snapshot.status[1].path.as_utf8(), Some("src/copy.rs"));
        assert_eq!(
            snapshot.status[1]
                .previous_path
                .as_ref()
                .and_then(RepositoryPath::as_utf8),
            Some("src/original.rs"),
        );
        assert_eq!(snapshot.status[2].path.as_utf8(), Some("src/line\nname.rs"));
        assert!(snapshot.status[2].previous_path.is_none());
    }

    #[test]
    fn porcelain_v1_z_parser_never_invents_invalid_or_incomplete_path_identity() {
        let mut snapshot = git_snapshot_for_parser();
        parse_git_status(
            b"?? valid.rs\0?? invalid-\xff.rs\0?? invalid\\windows.rs\0R  current.rs\0",
            &mut snapshot,
        );

        assert!(snapshot.truncated);
        assert_eq!(snapshot.status.len(), 1);
        assert_eq!(snapshot.status[0].path.as_utf8(), Some("valid.rs"));
        assert!(snapshot.status[0].previous_path.is_none());
    }

    #[tokio::test]
    async fn real_git_inspection_end_to_end_preserves_rename_path_identity() {
        let root = temporary_workspace_root("git-status-rename-e2e");
        std::fs::create_dir_all(&root).unwrap();
        run_git_fixture(&root, &["init", "--quiet"]).await;
        std::fs::write(root.join("previous name.rs"), b"fn original() {}\n").unwrap();
        run_git_fixture(&root, &["add", "--", "previous name.rs"]).await;
        run_git_fixture(
            &root,
            &[
                "-c",
                "user.name=Gate4Agent Fixture",
                "-c",
                "user.email=fixture@gate4agent.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        )
        .await;
        std::fs::rename(
            root.join("previous name.rs"),
            root.join("current name.rs"),
        )
        .unwrap();
        run_git_fixture(&root, &["add", "-A", "--", "."]).await;

        let (snapshot, git_time_budget_exceeded) =
            inspect_git_workspace(root.to_str().unwrap(), generous_walk_deadline()).await;
        assert!(!git_time_budget_exceeded);

        assert!(snapshot.is_repository, "{:?}", snapshot.diagnostic);
        let rename = snapshot
            .status
            .iter()
            .find(|entry| entry.index_status == "R")
            .expect("real git inspection did not report the staged rename");
        assert_eq!(rename.path.as_utf8(), Some("current name.rs"));
        assert_eq!(
            rename
                .previous_path
                .as_ref()
                .and_then(RepositoryPath::as_utf8),
            Some("previous name.rs"),
        );
        assert!(snapshot.recent_commits.iter().all(|commit| {
            matches!(commit.id.len(), 40 | 64)
                && commit.id.bytes().all(|byte| {
                    byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                })
        }));
        assert!(!snapshot.truncated, "{:?}", snapshot.diagnostic);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn observer_can_browse_host_directories_without_controller() {
        let root = temporary_workspace_root("host-directory-observer");
        std::fs::create_dir_all(root.join("directory-b")).unwrap();
        std::fs::create_dir_all(root.join("directory-a")).unwrap();
        std::fs::write(root.join("ordinary-file"), b"not a directory").unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id, &root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let request = NodeRequest::BrowseHostDirectories {
            directory: Some(OpaqueHostPath::utf8(root.to_string_lossy().into_owned()).unwrap()),
            after: None,
        };
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == CAPABILITY_HOST_DIRECTORY_BROWSE_V1
        }));
        assert!(!request_uses_unnegotiated_capability(
            &request,
            &baseline_capabilities().unwrap(),
        ));
        assert!(request_uses_unnegotiated_capability(&request, &[]));

        let response = process_request_inner(&shared, 77, ClientRole::Observer, request)
            .await
            .unwrap();
        let NodeResponse::HostDirectoriesBrowsed { listing } = response else {
            panic!("host directory browse returned another response");
        };
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["directory-a", "directory-b"],
        );
        assert!(listing.entries.iter().all(|entry| !entry.is_link));
        assert!(!listing.incomplete);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn observer_can_inspect_only_a_registered_workspace() {
        let root = temporary_workspace_root("observer");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {}\n").unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );

        let response = process_request_inner(
            &shared,
            77,
            ClientRole::Observer,
            NodeRequest::InspectWorkspace {
                workspace_id: workspace_id.clone(),
            },
        )
        .await
        .unwrap();
        let NodeResponse::WorkspaceInspected { inspection } = response else {
            panic!("workspace inspection returned another response");
        };
        assert_eq!(inspection.workspace_id, workspace_id);
        assert!(inspection.entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("src/main.rs")
        }));

        let unknown = process_request_inner(
            &shared,
            77,
            ClientRole::Observer,
            NodeRequest::InspectWorkspace {
                workspace_id: WorkspaceId::new("unknown").unwrap(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(unknown.code, NodeFailureCode::UnknownWorkspace);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn context_pack_repository_reads_only_the_registered_source_workspace() {
        let root = temporary_workspace_root("context-pack-registered-root");
        let other_root = temporary_workspace_root("context-pack-unregistered-root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other_root).unwrap();
        std::fs::write(
            root.join("README.md"),
            b"REGISTERED_CONTEXT_MARKER\n",
        )
        .unwrap();
        std::fs::write(
            other_root.join("README.md"),
            b"UNREGISTERED_CONTEXT_MARKER\n",
        )
        .unwrap();
        run_git_fixture(&root, &["init", "--quiet"]).await;
        run_git_fixture(&root, &["add", "--", "README.md"]).await;
        run_git_fixture(
            &root,
            &[
                "-c",
                "user.name=Gate4Agent Fixture",
                "-c",
                "user.email=fixture@gate4agent.invalid",
                "commit",
                "--quiet",
                "-m",
                "registered context commit",
            ],
        )
        .await;
        let workspace_id = WorkspaceId::new("source-workspace").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-context-pack-root").unwrap(),
            vec![workspace],
            vec![agent("codex")],
        );
        let repository = shared
            .context_pack_repository(&workspace_id)
            .await
            .unwrap();
        let history = HistorySessionRecord {
            session_id: "provider-session".to_owned(),
            title: None,
            cwd: Some(other_root.to_string_lossy().into_owned()),
            model: Some("codex".to_owned()),
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![gate4agent_types::HistoryMessageRecord {
                role: gate4agent_types::HistoryMessageRole::User,
                text: "summarize the bounded context".to_owned(),
            }],
        };
        let pack = NodeContextPack::export_with_repository(
            ContextPackLineageReceipt {
                source_node_id: shared.node_id.clone(),
                source_session: SessionAddress {
                    workspace_id,
                    session: SessionKey {
                        instance_id: AgentInstanceId(7),
                        generation: SessionGeneration(1),
                    },
                },
                source_provider: agent("codex"),
            },
            &history,
            Some(repository),
        )
        .unwrap();
        let encoded = String::from_utf8(pack.bytes().to_vec()).unwrap();

        assert!(encoded.contains("REGISTERED_CONTEXT_MARKER"));
        assert!(encoded.contains("registered context commit"));
        assert!(!encoded.contains("UNREGISTERED_CONTEXT_MARKER"));
        assert!(!encoded.contains(root.to_string_lossy().as_ref()));
        assert!(!encoded.contains(other_root.to_string_lossy().as_ref()));
        assert!(!encoded.contains("diagnostic"));
        assert!(!encoded.contains("worktrees"));

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&other_root).unwrap();
    }

    #[test]
    fn context_pack_source_accepts_only_running_or_successful_exit() {
        assert!(context_pack_source_status_is_usable(&SessionStatus::Running));
        assert!(context_pack_source_status_is_usable(&SessionStatus::Exited {
            exit_code: Some(0),
        }));
        assert!(!context_pack_source_status_is_usable(&SessionStatus::Exited {
            exit_code: Some(1),
        }));
        assert!(!context_pack_source_status_is_usable(&SessionStatus::Exited {
            exit_code: None,
        }));
        assert!(!context_pack_source_status_is_usable(&SessionStatus::Failed {
            message: "provider failed".to_owned(),
        }));
        assert!(!context_pack_source_status_is_usable(&SessionStatus::Registered));
        assert!(!context_pack_source_status_is_usable(&SessionStatus::Starting));
        assert!(!context_pack_source_status_is_usable(&SessionStatus::Stopping));
    }

    #[test]
    fn context_pack_repository_rejects_changed_head_with_same_git_snapshot() {
        let git = git_snapshot_for_parser();
        let first = ContextPackRepositoryObservation {
            head: ContextPackRepositoryHead::Commit("a".repeat(40)),
            git: git.clone(),
            files: Vec::new(),
        };
        let second = ContextPackRepositoryObservation {
            head: ContextPackRepositoryHead::Commit("b".repeat(40)),
            git,
            files: Vec::new(),
        };

        let error = stable_context_pack_repository(first, second).unwrap_err();

        assert_eq!(
            error.code,
            NodeFailureCode::ContextPackMaterializationFailed,
        );
    }

    #[test]
    fn context_pack_repository_rejects_changed_file_with_same_git_snapshot() {
        let git = git_snapshot_for_parser();
        let first = ContextPackRepositoryObservation {
            head: ContextPackRepositoryHead::Commit("a".repeat(40)),
            git: git.clone(),
            files: vec![ContextPackRepositoryFileSource::utf8(
                "README.md",
                "first\n".to_owned(),
                6,
            )],
        };
        let second = ContextPackRepositoryObservation {
            head: ContextPackRepositoryHead::Commit("a".repeat(40)),
            git,
            files: vec![ContextPackRepositoryFileSource::utf8(
                "README.md",
                "other\n".to_owned(),
                6,
            )],
        };

        let error = stable_context_pack_repository(first, second).unwrap_err();

        assert_eq!(
            error.code,
            NodeFailureCode::ContextPackMaterializationFailed,
        );
    }

    #[tokio::test]
    async fn native_session_catalog_zero_active_session_fixture() {
        let workspace_root = temporary_workspace_root("native-catalog-workspace");
        let history_root = temporary_workspace_root("native-catalog-history");
        let outside_root = temporary_workspace_root("native-catalog-outside");
        std::fs::create_dir_all(&workspace_root).unwrap();
        std::fs::create_dir_all(&history_root).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();
        let transcript = |session_id: &str, cwd: &Path, title: &str| {
            [
                serde_json::json!({
                    "type": "user",
                    "sessionId": session_id,
                    "cwd": cwd.to_str().unwrap(),
                    "message": { "content": "raw user text must not cross the catalog wire" }
                }),
                serde_json::json!({
                    "type": "assistant",
                    "sessionId": session_id,
                    "cwd": cwd.to_str().unwrap(),
                    "message": { "model": "claude-sonnet", "content": "raw answer" }
                }),
                serde_json::json!({
                    "type": "ai-title",
                    "sessionId": session_id,
                    "aiTitle": title
                }),
            ]
            .into_iter()
            .map(|record| record.to_string())
            .collect::<Vec<_>>()
            .join("\n")
        };
        std::fs::write(
            history_root.join("stable-session.jsonl"),
            transcript("stable-session", &workspace_root, "Workspace review"),
        )
        .unwrap();
        std::fs::write(
            history_root.join("outside-session.jsonl"),
            transcript("outside-session", &outside_root, "Outside review"),
        )
        .unwrap();
        let history = NativeHistoryConfig::new(vec![
            NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                history_root.clone(),
            )
            .unwrap(),
        ])
        .unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        assert!(handle.snapshot().sessions.is_empty());
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &workspace_root).unwrap();
        let mut shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-native-catalog").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        shared.native_session_catalog = Some(Arc::new(Mutex::new(
            NativeSessionCatalogAuthority::new(history),
        )));
        let route = NativeSessionCatalogRoute::workspace(
            workspace_id.clone(),
            agent("claude"),
        );
        let request = NodeRequest::CatalogNativeSessions {
            route: route.clone(),
            limit: 2,
        };
        let observer = process_request_inner(&shared, 7, ClientRole::Observer, request.clone())
            .await
            .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        let response = process_request_inner(&shared, 7, ClientRole::Operator, request)
            .await
            .unwrap();
        let NodeResponse::NativeSessionsCataloged { entries, summary, .. } = response else {
            panic!("native catalog returned a different response");
        };
        let summary = summary.expect("node must emit native session catalog summary metadata");
        assert_eq!(entries.len(), 1);
        assert!(gate4agent_types::validate_candidate_id(&entries[0].selection_id).is_ok());
        assert_eq!(entries[0].external_group, None);
        assert_eq!(entries[0].record_id, None);
        assert_eq!(entries[0].title, None);
        assert_eq!(entries[0].model, None);
        assert_eq!(entries[0].message_count, 0);
        let encoded_entries = serde_json::to_string(&entries).unwrap();
        assert!(!encoded_entries.contains("stable-session"));
        assert!(!encoded_entries.contains(history_root.to_string_lossy().as_ref()));
        let page_request = NodeRequest::PageNativeSessions {
            route: route.clone(),
            window: crate::protocol::NativeSessionCatalogWindow::Recent,
            catalog_revision: summary.catalog_revision,
            recent_cutoff_unix_ms: summary.recent_cutoff_unix_ms,
            after_selection_id: None,
            limit: 2,
        };
        let observer = process_request_inner(
            &shared,
            7,
            ClientRole::Observer,
            page_request.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        let page_response = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            page_request.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            page_response,
            NodeResponse::NativeSessionsPaged { page, .. }
                if page.entries.len() == 1 && !page.has_more && page.remaining_count == 0
        ));
        let stale = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::PageNativeSessions {
                route: route.clone(),
                window: crate::protocol::NativeSessionCatalogWindow::Recent,
                catalog_revision: summary.catalog_revision.wrapping_add(1),
                recent_cutoff_unix_ms: summary.recent_cutoff_unix_ms,
                after_selection_id: None,
                limit: 2,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(stale.code, NodeFailureCode::StaleNativeSessionCatalog);
        let selection = NativeSessionSelection {
            route: route.clone(),
            catalog_revision: summary.catalog_revision,
            recent_cutoff_unix_ms: summary.recent_cutoff_unix_ms,
            selection_id: entries[0].selection_id.clone(),
        };
        let preview_response = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::PreviewNativeSession {
                selection: selection.clone(),
                message_limit: 1,
            },
        )
        .await
        .unwrap();
        let NodeResponse::NativeSessionPreviewed { preview, .. } = preview_response else {
            panic!("native preview returned a different response");
        };
        assert_eq!(preview.messages.len(), 1);
        assert!(preview.message_count_exact);
        assert_eq!(preview.messages[0].text, "raw answer");
        assert!(preview.truncated);

        let index_request = NodeRequest::IndexNativeSession {
            selection: selection.clone(),
            display_name: "Saved Claude".to_owned(),
        };
        let observer = process_request_inner(
            &shared,
            7,
            ClientRole::Observer,
            index_request.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        let without_controller = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            index_request.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(without_controller.code, NodeFailureCode::ControllerRequired);
        let acquired = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::AcquireController { lease_ms: 5_000 },
        )
        .await
        .unwrap();
        assert!(matches!(acquired, NodeResponse::Controller { controller: Some(_) }));
        let indexed = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            index_request,
        )
        .await
        .unwrap();
        assert!(indexed.native_session_index_contract_is_valid());
        let NodeResponse::NativeSessionIndexed {
            selection: echoed_selection,
            record,
        } = indexed else {
            panic!("native session index returned a different response");
        };
        assert_eq!(echoed_selection, selection);
        let recataloged = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::CatalogNativeSessions {
                route,
                limit: 2,
            },
        )
        .await
        .unwrap();
        let NodeResponse::NativeSessionsCataloged { entries, .. } = recataloged else {
            panic!("native catalog returned a different response after indexing");
        };
        assert_eq!(entries[0].record_id.as_ref(), Some(&record.record_id));

        let unregistered_route = NativeSessionCatalogRoute::unregistered(agent("claude"));
        let external_catalog = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::CatalogNativeSessions {
                route: unregistered_route.clone(),
                limit: 2,
            },
        )
        .await
        .unwrap();
        let external_json = serde_json::to_string(&external_catalog).unwrap();
        assert!(!external_json.contains("outside-session"));
        assert!(!external_json.contains(outside_root.to_string_lossy().as_ref()));
        assert!(!external_json.contains(history_root.to_string_lossy().as_ref()));
        assert!(!external_json.contains("\"session_id\""));
        assert!(!external_json.contains("\"cwd\""));
        assert!(!external_json.contains("\"path\""));
        let NodeResponse::NativeSessionsCataloged {
            route: echoed_route,
            entries: external_entries,
            summary: Some(external_summary),
        } = external_catalog
        else {
            panic!("unregistered native catalog returned a different response");
        };
        assert_eq!(echoed_route, unregistered_route);
        assert_eq!(external_entries.len(), 1);
        assert_eq!(external_entries[0].record_id, None);
        let external_group = external_entries[0]
            .external_group
            .as_ref()
            .expect("unregistered session must have an external group");
        assert_eq!(
            external_group.kind,
            gate4agent_types::NativeSessionExternalGroupKind::Project,
        );
        assert!(external_group.group_id.starts_with("external-"));
        assert_eq!(
            external_group.display_name,
            outside_root.file_name().unwrap().to_string_lossy(),
        );
        let external_selection = NativeSessionSelection {
            route: unregistered_route,
            catalog_revision: external_summary.catalog_revision,
            recent_cutoff_unix_ms: external_summary.recent_cutoff_unix_ms,
            selection_id: external_entries[0].selection_id.clone(),
        };
        let external_preview = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::PreviewNativeSession {
                selection: external_selection.clone(),
                message_limit: 1,
            },
        )
        .await
        .unwrap();
        let external_preview_json = serde_json::to_string(&external_preview).unwrap();
        assert!(!external_preview_json.contains("outside-session"));
        assert!(!external_preview_json.contains(outside_root.to_string_lossy().as_ref()));
        assert!(!external_preview_json.contains("\"session_id\""));
        let NodeResponse::NativeSessionPreviewed { preview, .. } = external_preview else {
            panic!("unregistered native preview returned a different response");
        };
        assert_eq!(preview.messages.len(), 1);
        let external_index = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::IndexNativeSession {
                selection: external_selection,
                display_name: "External Claude".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            external_index.code,
            NodeFailureCode::WorkspaceRegistrationRequired,
        );
        let record_response = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::PreviewSessionRecord {
                record_id: record.record_id.clone(),
                message_limit: 2,
            },
        )
        .await
        .unwrap();
        let NodeResponse::SessionRecordPreviewed { record_id, preview } = record_response else {
            panic!("record preview returned a different response");
        };
        assert_eq!(record_id, record.record_id);
        assert_eq!(preview.messages.len(), 2);
        let encoded = serde_json::to_string(&preview).unwrap();
        assert!(!encoded.contains("stable-session"));
        assert!(!encoded.contains(history_root.to_string_lossy().as_ref()));
        let NodeResponse::Resync { events, .. } = shared.resync(0) else {
            panic!("record preview observation history returned a different response");
        };
        let history_observations = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                NodeEvent::ManagedObservation {
                    record_id,
                    observation,
                } if record_id == &record.record_id
                    && observation.evidence == ObservationEvidenceV1::HistoryProjection =>
                {
                    Some(observation)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(history_observations.len(), 2);
        assert!(history_observations.iter().any(|observation| matches!(
            observation.kind,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::History,
                capabilities: ObservationCapabilitiesV1 {
                    history_summary: true,
                    ..
                },
                ..
            }
        )));
        let history_snapshot = history_observations
            .iter()
            .find_map(|observation| match &observation.kind {
                ObservationKindV1::HistorySnapshot {
                    message_count,
                    message_count_exact,
                    completed_turn_count,
                    total_tokens,
                } => Some((
                    *message_count,
                    *message_count_exact,
                    *completed_turn_count,
                    *total_tokens,
                )),
                _ => None,
            })
            .expect("managed history preview must emit an aggregate snapshot");
        assert_eq!(
            history_snapshot,
            (
                preview.message_count,
                preview.message_count_exact,
                preview.completed_turn_count,
                preview.total_tokens,
            ),
        );
        let history_observation_json = serde_json::to_string(&history_observations).unwrap();
        for private in [
            "stable-session",
            "raw user text must not cross the catalog wire",
            "raw answer",
            "Workspace review",
            "claude-sonnet",
            history_root.to_string_lossy().as_ref(),
            workspace_root.to_string_lossy().as_ref(),
        ] {
            assert!(!history_observation_json.contains(private), "{private}");
        }
        let identity = record.provider_session.clone().unwrap();
        assert_eq!(
            shared.revalidate_session_record_preview(&record, &identity),
            Ok(())
        );
        shared.remove_record_memory(&record.record_id);
        assert_eq!(
            shared
                .revalidate_session_record_preview(&record, &identity)
                .unwrap_err()
                .code,
            NodeFailureCode::UnknownSessionRecord
        );
        let released = process_request_inner(
            &shared,
            7,
            ClientRole::Operator,
            NodeRequest::ReleaseController,
        )
        .await
        .unwrap();
        assert!(matches!(released, NodeResponse::Controller { controller: None }));
        assert!(shared.controller_state().is_none());
        std::fs::remove_dir_all(workspace_root).unwrap();
        std::fs::remove_dir_all(history_root).unwrap();
        std::fs::remove_dir_all(outside_root).unwrap();
    }

    #[tokio::test]
    async fn native_session_catalog_concurrent_operation_fails_fast_without_waiting_on_mutex() {
        let history_root = temporary_workspace_root("native-catalog-concurrent-operation");
        std::fs::create_dir_all(&history_root).unwrap();
        let history = NativeHistoryConfig::new(vec![
            NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                history_root.clone(),
            )
            .unwrap(),
        ])
        .unwrap();
        let catalog = Arc::new(Mutex::new(NativeSessionCatalogAuthority::new(history)));
        let held = catalog.lock().unwrap();
        let result = timeout(
            Duration::from_secs(1),
            run_native_session_catalog_operation(
                Arc::clone(&catalog),
                "native session catalog",
                |_| Ok::<(), std::convert::Infallible>(()),
            ),
        )
        .await
        .expect("concurrent native catalog request must not wait for the operation deadline")
        .unwrap_err();
        assert_eq!(result.code, NodeFailureCode::BackendBusy);
        drop(held);
        std::fs::remove_dir_all(history_root).unwrap();
    }

    fn temporary_workspace_root(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-node-{label}-{}-{unique}",
            std::process::id(),
        ))
    }

    fn git_snapshot_for_parser() -> GitSnapshot {
        GitSnapshot {
            is_repository: true,
            branch: Some("main".to_owned()),
            status: Vec::new(),
            recent_commits: Vec::new(),
            worktrees: Vec::new(),
            managed_worktree: None,
            truncated: false,
            diagnostic: None,
        }
    }

    #[tokio::test]
    async fn workspace_inspection_scopes_only_exact_active_managed_branch() {
        let root = temporary_workspace_root("managed-git-scope");
        std::fs::create_dir_all(&root).unwrap();
        run_test_git(&root, &["init"]);
        run_test_git(&root, &["config", "user.email", "gate4agent@example.invalid"]);
        run_test_git(&root, &["config", "user.name", "Gate4Agent Test"]);
        std::fs::write(root.join("seed.txt"), "seed\n").unwrap();
        run_test_git(&root, &["add", "seed.txt"]);
        run_test_git(&root, &["commit", "-m", "seed"]);
        run_test_git(&root, &["checkout", "-b", "gate4agent/a"]);

        let workspace_id = WorkspaceId::new("managed-a").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let mut lease = ManagedWorktreeLeaseRecord {
            lease_id: ManagedWorktreeLeaseId::new("mw-a").unwrap(),
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: workspace_id.clone(),
            profile_id: WorktreeProfileId::new("default").unwrap(),
            profile_revision: crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
            target_root: root.to_string_lossy().into_owned(),
            branch: "gate4agent/a".to_owned(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            expected_head: None,
            retention: ManagedWorktreeRetention::Retain,
            state: ManagedWorktreeLeaseState::InUse,
            session_holders: vec![ManagedWorktreeSessionHolder {
                incarnation_id: shared.incarnation_id,
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(1),
            }],
            record_holders: Vec::new(),
            cleanup_failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        *shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagedWorktreeRegistry::from_records(vec![lease.clone()], Vec::new()).unwrap();

        let inspection = shared.inspect_workspace(workspace_id.clone()).await.unwrap();
        let scope = inspection.git.managed_worktree.unwrap();
        assert_eq!(scope.lease_id, lease.lease_id);
        assert_eq!(scope.branch, "gate4agent/a");
        assert_eq!(scope.active_session_count, 1);

        lease.branch = "gate4agent/b".to_owned();
        *shared
            .managed_worktrees
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ManagedWorktreeRegistry::from_records(vec![lease], Vec::new()).unwrap();
        let mismatch = shared.inspect_workspace(workspace_id).await.unwrap();
        assert!(mismatch.git.managed_worktree.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn workspace_file_write_requires_controller_and_rejects_stale_revision() {
        let root = temporary_workspace_root("workspace-file-write-controller");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"old\n").unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let path = RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        let expected_revision = workspace_file_revision(b"old\n");
        let request = NodeRequest::WriteWorkspaceFile {
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            expected_revision: expected_revision.clone(),
            text: "new\n".to_owned(),
        };
        let observer = process_request_inner(&shared, 77, ClientRole::Observer, request.clone())
            .await
            .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        let uncontrolled = process_request_inner(&shared, 77, ClientRole::Operator, request.clone())
            .await
            .unwrap_err();
        assert_eq!(uncontrolled.code, NodeFailureCode::ControllerRequired);
        shared.acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS).unwrap();
        let response = process_request_inner(&shared, 77, ClientRole::Operator, request).await.unwrap();
        let NodeResponse::WorkspaceFileWritten { file } = response else {
            panic!("workspace file write returned another response");
        };
        assert_eq!(file.revision, Some(workspace_file_revision(b"new\n")));
        assert_eq!(std::fs::read(root.join("src/lib.rs")).unwrap(), b"new\n");
        let stale = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::WriteWorkspaceFile {
                workspace_id,
                path,
                expected_revision,
                text: "lost update\n".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(stale.code, NodeFailureCode::RepositoryFileRevisionConflict);
        assert_eq!(std::fs::read(root.join("src/lib.rs")).unwrap(), b"new\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn workspace_entry_create_requires_controller_and_never_overwrites() {
        let root = temporary_workspace_root("workspace-entry-create-controller");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let workspace = WorkspaceConfig::new(workspace_id.clone(), &root).unwrap();
        let catalog = active_registry().unwrap();
        let (handle, _runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let shared = NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            NodeId::new("node-1").unwrap(),
            vec![workspace],
            vec![agent("claude")],
        );
        let path = RepositoryPath::utf8("src/new.rs".to_owned()).unwrap();
        let request = NodeRequest::CreateWorkspaceFile {
            workspace_id: workspace_id.clone(),
            path: path.clone(),
        };
        assert!(baseline_capabilities().unwrap().iter().any(|capability| {
            capability.as_str() == NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY
        }));
        assert!(!request_uses_unnegotiated_capability(
            &request,
            &baseline_capabilities().unwrap(),
        ));
        assert!(request_uses_unnegotiated_capability(&request, &[]));
        let observer = process_request_inner(&shared, 77, ClientRole::Observer, request.clone())
            .await
            .unwrap_err();
        assert_eq!(observer.code, NodeFailureCode::ObserverReadOnly);
        let uncontrolled = process_request_inner(&shared, 77, ClientRole::Operator, request.clone())
            .await
            .unwrap_err();
        assert_eq!(uncontrolled.code, NodeFailureCode::ControllerRequired);
        shared.acquire_controller(77, ClientRole::Operator, DEFAULT_CONTROLLER_LEASE_MS).unwrap();

        let response = process_request_inner(&shared, 77, ClientRole::Operator, request.clone())
            .await
            .unwrap();
        let NodeResponse::WorkspaceFileCreated { file } = response else {
            panic!("workspace file create returned another response");
        };
        assert_eq!(file.workspace_id, workspace_id);
        assert_eq!(file.path, path);
        assert_eq!(file.revision, Some(workspace_file_revision(&[])));
        assert_eq!(std::fs::read(root.join("src/new.rs")).unwrap(), Vec::<u8>::new());

        let duplicate = process_request_inner(&shared, 77, ClientRole::Operator, request)
            .await
            .unwrap_err();
        assert_eq!(duplicate.code, NodeFailureCode::RepositoryEntryAlreadyExists);

        let directory_path = RepositoryPath::utf8("src/nested".to_owned()).unwrap();
        let response = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::CreateWorkspaceDirectory {
                workspace_id: workspace_id.clone(),
                path: directory_path.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            NodeResponse::WorkspaceDirectoryCreated {
                workspace_id: workspace_id.clone(),
                entry: WorkspaceEntry {
                    relative_path: directory_path,
                    kind: WorkspaceEntryKind::Directory,
                },
            },
        );
        assert!(root.join("src/nested").is_dir());

        let missing_parent = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::CreateWorkspaceFile {
                workspace_id: workspace_id.clone(),
                path: RepositoryPath::utf8("missing/new.rs".to_owned()).unwrap(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(missing_parent.code, NodeFailureCode::RepositoryParentNotFound);
        let non_directory_parent = process_request_inner(
            &shared,
            77,
            ClientRole::Operator,
            NodeRequest::CreateWorkspaceFile {
                workspace_id,
                path: RepositoryPath::utf8("src/new.rs/child".to_owned()).unwrap(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            non_directory_parent.code,
            NodeFailureCode::RepositoryParentNotDirectory,
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn workspace_entry_create_timeout_cancels_before_commit_without_late_mutation() {
        let root = temporary_workspace_root("workspace-entry-create-timeout-cancel");
        std::fs::create_dir_all(root.join("src")).unwrap();
        #[cfg(windows)]
        let root = std::fs::canonicalize(root).unwrap();
        let path = RepositoryPath::utf8("src/late.rs".to_owned()).unwrap();
        let commit_state = Arc::new(AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let task_commit_state = Arc::clone(&commit_state);
        let task_barrier = Arc::clone(&barrier);
        let task_done = Arc::clone(&done);
        let task_root = root.clone();
        let task = tokio::task::spawn_blocking(move || {
            task_barrier.wait();
            let result = create_workspace_file_on_disk(
                &task_root,
                &path,
                &task_commit_state,
            );
            task_done.store(true, Ordering::Release);
            result
        });

        let error = settle_workspace_entry_create(
            task,
            Arc::clone(&commit_state),
            Duration::from_millis(10),
            "fixture deadline",
            "fixture task failed",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, NodeFailureCode::RepositoryEntryCreateTimedOut);
        assert_eq!(
            commit_state.load(Ordering::Acquire),
            WORKSPACE_ENTRY_CREATE_CANCELED,
        );

        barrier.wait();
        timeout(Duration::from_secs(1), async {
            while !done.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("canceled blocking create did not settle");
        assert!(!root.join("src/late.rs").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn git_history_filters_by_file_without_executing_repository_gpg_program() {
        let root = temporary_workspace_root("git-history-gpg-program");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), b"safe\n").unwrap();
        run_git_fixture(&root, &["init", "--quiet"]).await;
        run_git_fixture(&root, &["add", "--", "README.md"]).await;
        run_git_fixture(
            &root,
            &["-c", "user.name=Gate4Agent", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "safe commit"],
        )
        .await;
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn separate() {}\n").unwrap();
        run_git_fixture(&root, &["add", "--", "src/lib.rs"]).await;
        run_git_fixture(
            &root,
            &["-c", "user.name=Gate4Agent", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "separate commit"],
        )
        .await;
        let sentinel = root.join("gpg-program-ran");
        let program = root.join("malicious-gpg.cmd");
        std::fs::write(&program, format!("@echo off\r\necho ran>\"{}\"\r\n", sentinel.display())).unwrap();
        run_git_fixture(
            &root,
            &["config", "gpg.program", program.to_str().unwrap()],
        )
        .await;

        let path = RepositoryPath::utf8("README.md".to_owned()).unwrap();
        let page = read_git_history_bounded(
            root.to_str().unwrap(),
            Some(&path),
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(page.commits.len(), 1);
        assert_eq!(page.commits[0].subject, "safe commit");
        assert_eq!(page.commits[0].signature_status, GitSignatureStatus::NoSignature);
        assert!(!sentinel.exists(), "repository-controlled gpg.program executed");
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn run_git_fixture(root: &Path, arguments: &[&str]) {
        let output = run_git_bounded(root.to_str().unwrap(), arguments, GIT_OUTPUT_MAX_BYTES)
            .await
            .unwrap();
        assert!(
            output.success && !output.timed_out,
            "git {:?} failed (timed_out={}): {}",
            arguments,
            output.timed_out,
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
