//! Stable, serde-only inventory and control contract for Gate4Agent C2.

pub use gate4agent_node_protocol::{
    ArchitectureId, CapabilityId, ClientCompatibilityOffer, HostDescriptor,
    NodeCursor, NodeEvent, NodeFailure, NodeId, NodeIncarnationId, NodeRequest,
    NodeResponse, OpaqueHostPath, OperatingSystemId, PathEncoding, PathSemantics, PathStyle,
    ProtocolNegotiationError, ProtocolRange,
};
use gate4agent_node_protocol::{
    AgentProvider, ManagedSessionRecord, ManagedSessionState, NodeSnapshot, SessionAddress,
    SessionMode, SessionRecordId, WorkspaceId,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, OperationId, PreparedInputKind, ProviderActivity,
    SessionGeneration, SessionStatus, TerminalFrame, TerminalSize, TransportKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub const C2_API_VERSION: u16 = 2;
pub const DEFAULT_C2_API_LISTEN: &str = "127.0.0.1:18320";
pub const C2_CONTROL_PROTOCOL_VERSION: u16 = 2;
pub const C2_COMPATIBILITY_METADATA_CAPABILITY: &str = "compatibility.metadata";
pub const C2_OPAQUE_UNIX_PATH_CAPABILITY: &str =
    gate4agent_node_protocol::NODE_OPAQUE_UNIX_PATH_CAPABILITY;
pub const C2_AUTH_NONCE_BYTES: usize = 32;
pub const C2_AUTH_PROOF_BYTES: usize = 32;
pub const MAX_C2_AUTH_COMPATIBILITY_CAPABILITIES: usize = 64;
pub const MAX_C2_BOUND_AUTH_TRANSCRIPT_BYTES: usize = 16 * 1024;
pub const MAX_C2_CLIENT_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_C2_SERVER_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_C2_AUTH_FRAME_BYTES: usize = 8 * 1024;
pub const MAX_C2_HELLO_FRAME_BYTES: usize = MAX_C2_SERVER_FRAME_BYTES;
pub const MAX_C2_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_C2_NODES: usize = 64;
pub const MAX_C2_ENDPOINT_BYTES: usize = 1024;
pub const MAX_C2_WORKSPACES_PER_NODE: usize = 32;
pub const MAX_C2_SESSIONS_PER_NODE: usize = 128;
pub const MAX_C2_MANAGED_SESSIONS_PER_NODE: usize = 128;
pub const MAX_C2_GAPS_PER_NODE: usize = 64;
pub const MAX_C2_ROOT_BYTES: usize = 1024;
pub const MAX_C2_SESSION_DISPLAY_NAME_BYTES: usize =
    gate4agent_node_protocol::MAX_SESSION_DISPLAY_NAME_BYTES;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct C2RequestId(pub u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeRoute {
    pub node_id: NodeId,
    pub expected_incarnation_id: NodeIncarnationId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoutedNodeRequest {
    pub route: NodeRoute,
    pub request: NodeRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoutedNodeResponse {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
    pub response: Result<C2NodeResponse, C2NodeFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RoutedNodeEvent {
    pub node_id: NodeId,
    pub cursor: NodeCursor,
    pub event: C2NodeEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2NodeFailure {
    pub code: gate4agent_node_protocol::NodeFailureCode,
    pub message: String,
}

impl From<&NodeFailure> for C2NodeFailure {
    fn from(failure: &NodeFailure) -> Self {
        use gate4agent_node_protocol::NodeFailureCode;
        let message = match failure.code {
            NodeFailureCode::InvalidRequest => "invalid request",
            NodeFailureCode::Unauthorized => "authentication rejected",
            NodeFailureCode::ObserverReadOnly => "operator access required",
            NodeFailureCode::ControllerBusy => "controller busy",
            NodeFailureCode::ControllerRequired => "controller required",
            NodeFailureCode::UnknownWorkspace => "workspace unavailable",
            NodeFailureCode::InvalidWorkspaceRoot => "workspace root invalid",
            NodeFailureCode::DuplicateWorkspaceId => "workspace ID already registered",
            NodeFailureCode::DuplicateWorkspaceRoot => "workspace root already registered",
            NodeFailureCode::WorkspaceBusy => "workspace busy",
            NodeFailureCode::LastWorkspace => "last workspace protected",
            NodeFailureCode::NotGitRepository => "workspace is not a git repository",
            NodeFailureCode::WorktreeConflict => "worktree conflict",
            NodeFailureCode::WorktreeProtected => "worktree protected",
            NodeFailureCode::WorktreeDirty => "worktree dirty",
            NodeFailureCode::WorktreeLocked => "worktree locked",
            NodeFailureCode::UnknownSession => "session unavailable",
            NodeFailureCode::UnknownSessionRecord => "managed session unavailable",
            NodeFailureCode::SessionRecordNotResumable => "managed session cannot resume",
            NodeFailureCode::SessionRecordBusy => "managed session busy",
            NodeFailureCode::SessionRecordConflict => "managed session conflict",
            NodeFailureCode::SessionWorkspaceMismatch => "session workspace mismatch",
            NodeFailureCode::StaleGeneration => "stale session generation",
            NodeFailureCode::BackendBusy => "node backend busy",
            NodeFailureCode::BackendDisconnected => "node backend disconnected",
            NodeFailureCode::BackendOperationFailed => "node backend operation failed",
            NodeFailureCode::ShuttingDown => "node shutting down",
        };
        Self { code: failure.code, message: message.to_owned() }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ManagedSessionRecord {
    pub record_id: SessionRecordId,
    pub display_name: String,
    pub provider: AgentProvider,
    pub mode: SessionMode,
    pub state: ManagedSessionState,
    pub workspace_id: WorkspaceId,
    pub active_session: Option<SessionAddress>,
    pub provider_identity_present: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl From<&ManagedSessionRecord> for C2ManagedSessionRecord {
    fn from(record: &ManagedSessionRecord) -> Self {
        Self {
            record_id: record.record_id.clone(),
            display_name: record.display_name.clone(),
            provider: record.provider,
            mode: record.mode,
            state: record.state,
            workspace_id: record.workspace_id.clone(),
            active_session: record.active_session.clone(),
            provider_identity_present: record.provider_session.is_some(),
            created_at_unix_ms: record.created_at_unix_ms,
            updated_at_unix_ms: record.updated_at_unix_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum C2SessionStatus {
    Registered,
    Starting,
    Running,
    Stopping,
    Exited { exit_code: Option<i32> },
    Failed,
}

impl From<&SessionStatus> for C2SessionStatus {
    fn from(status: &SessionStatus) -> Self {
        match status {
            SessionStatus::Registered => Self::Registered,
            SessionStatus::Starting => Self::Starting,
            SessionStatus::Running => Self::Running,
            SessionStatus::Stopping => Self::Stopping,
            SessionStatus::Exited { exit_code } => Self::Exited { exit_code: *exit_code },
            SessionStatus::Failed { .. } => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2SessionSnapshot {
    pub instance_id: AgentInstanceId,
    pub agent_id: AgentId,
    pub transport: TransportKind,
    pub generation: SessionGeneration,
    pub status: C2SessionStatus,
    pub pending_operation: Option<OperationId>,
    pub pending_input: Option<PreparedInputKind>,
    pub process_id: Option<u32>,
    pub terminal_size: Option<TerminalSize>,
    pub terminal_frame: Option<TerminalFrame>,
    pub provider_activity: ProviderActivity,
    pub provider_interaction_pending: bool,
    pub provider_identity_present: bool,
}

impl From<&gate4agent_types::SessionSnapshot> for C2SessionSnapshot {
    fn from(session: &gate4agent_types::SessionSnapshot) -> Self {
        Self {
            instance_id: session.instance_id,
            agent_id: session.agent_id.clone(),
            transport: session.transport,
            generation: session.generation,
            status: C2SessionStatus::from(&session.status),
            pending_operation: session.pending_operation,
            pending_input: session.pending_input,
            process_id: session.process_id,
            terminal_size: session.terminal_size,
            terminal_frame: session.terminal_frame.clone(),
            provider_activity: session.provider.activity,
            provider_interaction_pending: !session.provider.interactions.is_empty(),
            provider_identity_present: session.provider.session.is_some(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2WorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub canonical_root: OpaqueHostPath,
    pub sessions: Vec<C2SessionSnapshot>,
}

impl From<&gate4agent_node_protocol::WorkspaceSnapshot> for C2WorkspaceSnapshot {
    fn from(workspace: &gate4agent_node_protocol::WorkspaceSnapshot) -> Self {
        Self {
            workspace_id: workspace.workspace_id.clone(),
            canonical_root: workspace.canonical_root.clone(),
            sessions: workspace.sessions.iter().map(C2SessionSnapshot::from).collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2NodeSnapshot {
    pub node_id: NodeId,
    pub enabled_providers: Vec<AgentProvider>,
    pub workspaces: Vec<C2WorkspaceSnapshot>,
    pub session_records: Vec<C2ManagedSessionRecord>,
}

impl From<&NodeSnapshot> for C2NodeSnapshot {
    fn from(snapshot: &NodeSnapshot) -> Self {
        Self {
            node_id: snapshot.node_id.clone(),
            enabled_providers: snapshot.enabled_providers.clone(),
            workspaces: snapshot.workspaces.iter().map(C2WorkspaceSnapshot::from).collect(),
            session_records: snapshot.session_records.iter()
                .map(C2ManagedSessionRecord::from)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2NodeEventEnvelope {
    pub sequence: u64,
    pub event: C2NodeEvent,
}

impl From<&gate4agent_node_protocol::NodeEventEnvelope> for C2NodeEventEnvelope {
    fn from(envelope: &gate4agent_node_protocol::NodeEventEnvelope) -> Self {
        Self { sequence: envelope.sequence, event: C2NodeEvent::from(&envelope.event) }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ControlEvent {
    pub protocol_version: u16,
    pub sequence: u64,
    pub command_id: Option<gate4agent_types::CommandId>,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub event: C2ControlEventKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum C2ProviderEventKind {
    SessionStarted,
    SessionIdentityObserved,
    TurnStarted,
    WorkingObserved,
    Text,
    Thinking,
    ToolStarted,
    ToolCompleted,
    TurnCompleted,
    TurnInterrupted,
    SessionEnded,
    Error,
    Ready,
    InteractionRequested,
    SubagentStarted,
    SubagentStopped,
    RateLimited,
}

impl From<&gate4agent_types::ProviderEvent> for C2ProviderEventKind {
    fn from(event: &gate4agent_types::ProviderEvent) -> Self {
        use gate4agent_types::ProviderEvent;
        match event {
            ProviderEvent::SessionStarted { .. } => Self::SessionStarted,
            ProviderEvent::SessionIdentityObserved { .. } => Self::SessionIdentityObserved,
            ProviderEvent::TurnStarted { .. } => Self::TurnStarted,
            ProviderEvent::WorkingObserved => Self::WorkingObserved,
            ProviderEvent::Text { .. } => Self::Text,
            ProviderEvent::Thinking { .. } => Self::Thinking,
            ProviderEvent::ToolStarted { .. } => Self::ToolStarted,
            ProviderEvent::ToolCompleted { .. } => Self::ToolCompleted,
            ProviderEvent::TurnCompleted { .. } => Self::TurnCompleted,
            ProviderEvent::TurnInterrupted => Self::TurnInterrupted,
            ProviderEvent::SessionEnded { .. } => Self::SessionEnded,
            ProviderEvent::Error { .. } => Self::Error,
            ProviderEvent::Ready => Self::Ready,
            ProviderEvent::InteractionRequested { .. } => Self::InteractionRequested,
            ProviderEvent::SubagentStarted { .. } => Self::SubagentStarted,
            ProviderEvent::SubagentStopped { .. } => Self::SubagentStopped,
            ProviderEvent::RateLimited { .. } => Self::RateLimited,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum C2ControlEventKind {
    CommandRejected,
    Registered,
    StartRequested,
    Running,
    StopRequested,
    InputRequested,
    InputCompleted,
    InputFailed,
    ResizeRequested,
    Resized,
    ResizeFailed,
    ForegroundRefreshRequested,
    ForegroundObserved,
    ForegroundFailed,
    CapabilityProbeRequested,
    CapabilitiesProbed,
    CapabilityProbeFailed,
    HistoryRequested,
    HistoryDiscovered,
    HistoryLoaded,
    HistoryFailed,
    ResumeRequested,
    ResumeAuthorized,
    Resumed,
    ResumeDenied,
    ResumeFailed,
    TerminalStale,
    ProviderEvent { event: C2ProviderEventKind },
    ProviderGap,
    InteractionRequested,
    InteractionResolutionRequested,
    InteractionResolutionFailed,
    InteractionResolved,
    Exited { forced: bool },
    Failed,
    Removed,
    ObservationIgnored,
}

impl From<&gate4agent_types::ControlEvent> for C2ControlEvent {
    fn from(event: &gate4agent_types::ControlEvent) -> Self {
        use gate4agent_types::ControlEventKind;
        let projected = match &event.event {
            ControlEventKind::CommandRejected { .. } => C2ControlEventKind::CommandRejected,
            ControlEventKind::Registered => C2ControlEventKind::Registered,
            ControlEventKind::StartRequested { .. } => C2ControlEventKind::StartRequested,
            ControlEventKind::Running { .. } => C2ControlEventKind::Running,
            ControlEventKind::StopRequested { .. } => C2ControlEventKind::StopRequested,
            ControlEventKind::InputRequested { .. } => C2ControlEventKind::InputRequested,
            ControlEventKind::InputCompleted { .. } => C2ControlEventKind::InputCompleted,
            ControlEventKind::InputFailed { .. } => C2ControlEventKind::InputFailed,
            ControlEventKind::ResizeRequested { .. } => C2ControlEventKind::ResizeRequested,
            ControlEventKind::Resized { .. } => C2ControlEventKind::Resized,
            ControlEventKind::ResizeFailed { .. } => C2ControlEventKind::ResizeFailed,
            ControlEventKind::ForegroundRefreshRequested { .. } => C2ControlEventKind::ForegroundRefreshRequested,
            ControlEventKind::ForegroundObserved { .. } => C2ControlEventKind::ForegroundObserved,
            ControlEventKind::ForegroundFailed { .. } => C2ControlEventKind::ForegroundFailed,
            ControlEventKind::CapabilityProbeRequested { .. } => C2ControlEventKind::CapabilityProbeRequested,
            ControlEventKind::CapabilitiesProbed { .. } => C2ControlEventKind::CapabilitiesProbed,
            ControlEventKind::CapabilityProbeFailed { .. } => C2ControlEventKind::CapabilityProbeFailed,
            ControlEventKind::HistoryRequested { .. } => C2ControlEventKind::HistoryRequested,
            ControlEventKind::HistoryDiscovered { .. } => C2ControlEventKind::HistoryDiscovered,
            ControlEventKind::HistoryLoaded { .. } => C2ControlEventKind::HistoryLoaded,
            ControlEventKind::HistoryFailed { .. } => C2ControlEventKind::HistoryFailed,
            ControlEventKind::ResumeRequested { .. } => C2ControlEventKind::ResumeRequested,
            ControlEventKind::ResumeAuthorized { .. } => C2ControlEventKind::ResumeAuthorized,
            ControlEventKind::Resumed { .. } => C2ControlEventKind::Resumed,
            ControlEventKind::ResumeDenied { .. } => C2ControlEventKind::ResumeDenied,
            ControlEventKind::ResumeFailed { .. } => C2ControlEventKind::ResumeFailed,
            ControlEventKind::TerminalStale { .. } => C2ControlEventKind::TerminalStale,
            ControlEventKind::ProviderEvent { event, .. } => C2ControlEventKind::ProviderEvent {
                event: C2ProviderEventKind::from(event),
            },
            ControlEventKind::ProviderGap { .. } => C2ControlEventKind::ProviderGap,
            ControlEventKind::InteractionRequested { .. } => C2ControlEventKind::InteractionRequested,
            ControlEventKind::InteractionResolutionRequested { .. } => C2ControlEventKind::InteractionResolutionRequested,
            ControlEventKind::InteractionResolutionFailed { .. } => C2ControlEventKind::InteractionResolutionFailed,
            ControlEventKind::InteractionResolved { .. } => C2ControlEventKind::InteractionResolved,
            ControlEventKind::Exited { forced, .. } => C2ControlEventKind::Exited { forced: *forced },
            ControlEventKind::Failed { .. } => C2ControlEventKind::Failed,
            ControlEventKind::Removed => C2ControlEventKind::Removed,
            ControlEventKind::ObservationIgnored { .. } => C2ControlEventKind::ObservationIgnored,
        };
        Self {
            protocol_version: event.protocol_version,
            sequence: event.sequence,
            command_id: event.command_id,
            instance_id: event.instance_id,
            generation: event.generation,
            event: projected,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum C2NodeEvent {
    Control {
        address: SessionAddress,
        event: C2ControlEvent,
    },
    ControllerChanged {
        controller: Option<gate4agent_node_protocol::ControllerState>,
    },
    WorkspaceAdded {
        workspace: C2WorkspaceSnapshot,
    },
    WorkspaceRemoved { workspace_id: WorkspaceId },
    SessionRecordUpserted { record: C2ManagedSessionRecord },
    SessionRecordRemoved { record_id: SessionRecordId },
    ResyncRequired { oldest_available_sequence: u64 },
}

impl From<&NodeEvent> for C2NodeEvent {
    fn from(event: &NodeEvent) -> Self {
        match event {
            NodeEvent::Control { address, event } => Self::Control {
                address: address.clone(),
                event: C2ControlEvent::from(event),
            },
            NodeEvent::ControllerChanged { controller } => Self::ControllerChanged {
                controller: controller.clone(),
            },
            NodeEvent::WorkspaceAdded { workspace } => Self::WorkspaceAdded {
                workspace: C2WorkspaceSnapshot::from(workspace),
            },
            NodeEvent::WorkspaceRemoved { workspace_id } => Self::WorkspaceRemoved {
                workspace_id: workspace_id.clone(),
            },
            NodeEvent::SessionRecordUpserted { record } => Self::SessionRecordUpserted {
                record: C2ManagedSessionRecord::from(record),
            },
            NodeEvent::SessionRecordRemoved { record_id } => Self::SessionRecordRemoved {
                record_id: record_id.clone(),
            },
            NodeEvent::ResyncRequired { oldest_available_sequence } => Self::ResyncRequired {
                oldest_available_sequence: *oldest_available_sequence,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2GitWorktreeSnapshot {
    pub path: OpaqueHostPath,
    pub head: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_main: bool,
    pub locked: bool,
    pub prunable: bool,
    pub workspace_id: Option<WorkspaceId>,
}

impl From<&gate4agent_node_protocol::GitWorktreeSnapshot> for C2GitWorktreeSnapshot {
    fn from(worktree: &gate4agent_node_protocol::GitWorktreeSnapshot) -> Self {
        Self {
            path: worktree.path.clone(),
            head: worktree.head.clone(),
            branch: worktree.branch.clone(),
            is_bare: worktree.is_bare,
            is_main: worktree.is_main,
            locked: worktree.locked,
            prunable: worktree.prunable,
            workspace_id: worktree.workspace_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2GitSnapshot {
    pub is_repository: bool,
    pub branch: Option<String>,
    pub status: Vec<gate4agent_node_protocol::GitStatusEntry>,
    pub recent_commits: Vec<gate4agent_node_protocol::GitCommitSummary>,
    pub worktrees: Vec<C2GitWorktreeSnapshot>,
    pub truncated: bool,
    pub diagnostic_present: bool,
}

impl From<&gate4agent_node_protocol::GitSnapshot> for C2GitSnapshot {
    fn from(git: &gate4agent_node_protocol::GitSnapshot) -> Self {
        Self {
            is_repository: git.is_repository,
            branch: git.branch.clone(),
            status: git.status.clone(),
            recent_commits: git.recent_commits.clone(),
            worktrees: git.worktrees.iter().map(C2GitWorktreeSnapshot::from).collect(),
            truncated: git.truncated,
            diagnostic_present: git.diagnostic.is_some(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2WorkspaceInspection {
    pub workspace_id: WorkspaceId,
    pub entries: Vec<gate4agent_node_protocol::WorkspaceEntry>,
    pub tree_truncated: bool,
    pub git: C2GitSnapshot,
}

impl From<&gate4agent_node_protocol::WorkspaceInspection> for C2WorkspaceInspection {
    fn from(inspection: &gate4agent_node_protocol::WorkspaceInspection) -> Self {
        Self {
            workspace_id: inspection.workspace_id.clone(),
            entries: inspection.entries.clone(),
            tree_truncated: inspection.tree_truncated,
            git: C2GitSnapshot::from(&inspection.git),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum C2NodeResponse {
    Snapshot {
        event_sequence: u64,
        controller: Option<gate4agent_node_protocol::ControllerState>,
        snapshot: C2NodeSnapshot,
    },
    Resync {
        event_sequence: u64,
        snapshot: C2NodeSnapshot,
        events: Vec<C2NodeEventEnvelope>,
    },
    WorkspaceInspected {
        inspection: C2WorkspaceInspection,
    },
    Controller {
        controller: Option<gate4agent_node_protocol::ControllerState>,
    },
    SpawnAccepted { session: SessionAddress },
    SessionRecordUpdated { record: C2ManagedSessionRecord },
    SessionRecordResumed {
        record: C2ManagedSessionRecord,
        session: SessionAddress,
    },
    SessionRecordForgotten { record_id: SessionRecordId },
    WorkspaceRegistered {
        workspace: C2WorkspaceSnapshot,
    },
    WorkspaceUnregistered { workspace_id: WorkspaceId },
    WorktreeCreated {
        worktree: C2GitWorktreeSnapshot,
        workspace: C2WorkspaceSnapshot,
    },
    WorktreeRemoved {
        target_root: OpaqueHostPath,
        workspace_id: Option<WorkspaceId>,
    },
    Accepted,
    ShuttingDown,
}

impl From<&NodeResponse> for C2NodeResponse {
    fn from(response: &NodeResponse) -> Self {
        match response {
            NodeResponse::Snapshot { event_sequence, controller, snapshot } => Self::Snapshot {
                event_sequence: *event_sequence,
                controller: controller.clone(),
                snapshot: C2NodeSnapshot::from(snapshot),
            },
            NodeResponse::Resync { event_sequence, snapshot, events } => Self::Resync {
                event_sequence: *event_sequence,
                snapshot: C2NodeSnapshot::from(snapshot),
                events: events.iter().map(C2NodeEventEnvelope::from).collect(),
            },
            NodeResponse::WorkspaceInspected { inspection } => Self::WorkspaceInspected {
                inspection: C2WorkspaceInspection::from(inspection),
            },
            NodeResponse::Controller { controller } => Self::Controller {
                controller: controller.clone(),
            },
            NodeResponse::SpawnAccepted { session } => Self::SpawnAccepted { session: session.clone() },
            NodeResponse::SessionRecordUpdated { record } => Self::SessionRecordUpdated {
                record: C2ManagedSessionRecord::from(record),
            },
            NodeResponse::SessionRecordResumed { record, session } => Self::SessionRecordResumed {
                record: C2ManagedSessionRecord::from(record),
                session: session.clone(),
            },
            NodeResponse::SessionRecordForgotten { record_id } => Self::SessionRecordForgotten {
                record_id: record_id.clone(),
            },
            NodeResponse::WorkspaceRegistered { workspace } => Self::WorkspaceRegistered {
                workspace: C2WorkspaceSnapshot::from(workspace),
            },
            NodeResponse::WorkspaceUnregistered { workspace_id } => Self::WorkspaceUnregistered {
                workspace_id: workspace_id.clone(),
            },
            NodeResponse::WorktreeCreated { worktree, workspace } => Self::WorktreeCreated {
                worktree: C2GitWorktreeSnapshot::from(worktree),
                workspace: C2WorkspaceSnapshot::from(workspace),
            },
            NodeResponse::WorktreeRemoved { target_root, workspace_id } => Self::WorktreeRemoved {
                target_root: target_root.clone(),
                workspace_id: workspace_id.clone(),
            },
            NodeResponse::Accepted => Self::Accepted,
            NodeResponse::ShuttingDown => Self::ShuttingDown,
        }
    }
}


#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum C2RelayFailureCode {
    UnknownNode,
    NodeOffline,
    StaleNodeIncarnation,
    RelayBusy,
    OperatorAlreadyConnected,
    RequestIdReused,
    RequestForbidden,
    ClientLagged,
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2RelayFailure {
    pub code: C2RelayFailureCode,
    pub message: String,
    pub current_incarnation_id: Option<NodeIncarnationId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ClientHello {
    pub protocol_version: u16,
    pub client_nonce: [u8; C2_AUTH_NONCE_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<ClientCompatibilityOffer>,
}

impl C2ClientHello {
    pub fn new(client_nonce: [u8; C2_AUTH_NONCE_BYTES]) -> Self {
        Self {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            client_nonce,
            compatibility: None,
        }
    }

    pub fn negotiating(
        client_nonce: [u8; C2_AUTH_NONCE_BYTES],
        compatibility: ClientCompatibilityOffer,
    ) -> Self {
        Self {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            client_nonce,
            compatibility: Some(compatibility),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ControlCompatibilitySupport {
    pub protocol_versions: ProtocolRange,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    pub host: HostDescriptor,
    pub path_semantics: PathSemantics,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NegotiatedC2ControlCompatibility {
    pub protocol_version: u16,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    pub host: HostDescriptor,
    pub path_semantics: PathSemantics,
}

impl C2ControlCompatibilitySupport {
    pub fn negotiate(
        &self,
        hello: &C2ClientHello,
    ) -> Result<NegotiatedC2ControlCompatibility, ProtocolNegotiationError> {
        ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION)?
            .highest_common(ProtocolRange::exact(hello.protocol_version)?)?;
        let legacy;
        let offer = match hello.compatibility.as_ref() {
            Some(offer) => offer,
            None => {
                legacy = ClientCompatibilityOffer::exact(C2_CONTROL_PROTOCOL_VERSION)?;
                &legacy
            }
        };
        let active_protocol = ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION)?;
        active_protocol.highest_common(self.protocol_versions)?;
        active_protocol.highest_common(offer.protocol_versions)?;
        let capabilities = self
            .capabilities
            .iter()
            .filter(|capability| offer.capabilities.contains(capability))
            .cloned()
            .collect();
        Ok(NegotiatedC2ControlCompatibility {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            capabilities,
            host: self.host.clone(),
            path_semantics: self.path_semantics.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ServerChallenge {
    pub protocol_version: u16,
    pub server_nonce: [u8; C2_AUTH_NONCE_BYTES],
    pub server_proof: [u8; C2_AUTH_PROOF_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<NegotiatedC2ControlCompatibility>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ClientAuthentication {
    pub client_proof: [u8; C2_AUTH_PROOF_BYTES],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2Hello {
    pub protocol_version: u16,
    pub connection_id: u64,
    pub status: StatusResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<NegotiatedC2ControlCompatibility>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2TopologyNode {
    pub node_id: NodeId,
    pub endpoint: String,
    pub transport: NodeTransportState,
    pub current_incarnation_id: Option<NodeIncarnationId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2Topology {
    pub nodes: Vec<C2TopologyNode>,
}

impl C2Topology {
    pub fn from_status(status: &StatusResponse) -> Self {
        let nodes = status.nodes.iter().take(MAX_C2_NODES).map(|(node_id, observed)| {
            C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: observed.endpoint.clone(),
                transport: observed.transport,
                current_incarnation_id: observed.cursor.map(|cursor| cursor.incarnation_id),
            }
        }).collect();
        Self { nodes }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2RequestEnvelope {
    pub request_id: C2RequestId,
    pub request: RoutedNodeRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct C2ReplyEnvelope {
    pub request_id: C2RequestId,
    pub result: Result<RoutedNodeResponse, C2RelayFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum C2ClientFrame {
    Hello(C2ClientHello),
    Authenticate(C2ClientAuthentication),
    Request(C2RequestEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum C2ServerFrame {
    Challenge(C2ServerChallenge),
    Hello(C2Hello),
    Reply(C2ReplyEnvelope),
    Event(RoutedNodeEvent),
    Topology(C2Topology),
    Rejected(C2RelayFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum C2AuthDirection {
    Server,
    Client,
}

pub fn c2_auth_transcript(
    direction: C2AuthDirection,
    client_nonce: &[u8; C2_AUTH_NONCE_BYTES],
    server_nonce: &[u8; C2_AUTH_NONCE_BYTES],
) -> Vec<u8> {
    let mut message = Vec::with_capacity(32 + (C2_AUTH_NONCE_BYTES * 2));
    message.extend_from_slice(b"gate4agent-c2-control-auth-v2\0");
    message.extend_from_slice(&C2_CONTROL_PROTOCOL_VERSION.to_le_bytes());
    message.push(match direction { C2AuthDirection::Server => 1, C2AuthDirection::Client => 2 });
    message.extend_from_slice(client_nonce);
    message.extend_from_slice(server_nonce);
    message
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum C2AuthTranscriptError {
    TooManyCapabilities {
        section: &'static str,
        count: usize,
        max: usize,
    },
    TooLong {
        len: usize,
        max: usize,
    },
}

impl fmt::Display for C2AuthTranscriptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyCapabilities { section, count, max } => write!(
                formatter,
                "{section} contains {count} capabilities, exceeding the {max}-entry authentication limit",
            ),
            Self::TooLong { len, max } => write!(
                formatter,
                "C2 compatibility authentication transcript is {len} bytes, exceeding the {max}-byte limit",
            ),
        }
    }
}

impl std::error::Error for C2AuthTranscriptError {}

pub fn c2_bound_auth_transcript(
    direction: C2AuthDirection,
    client_nonce: &[u8; C2_AUTH_NONCE_BYTES],
    server_nonce: &[u8; C2_AUTH_NONCE_BYTES],
    offer: &ClientCompatibilityOffer,
    selected: &NegotiatedC2ControlCompatibility,
) -> Result<Vec<u8>, C2AuthTranscriptError> {
    validate_auth_capabilities("offer", &offer.capabilities)?;
    validate_auth_capabilities("selection", &selected.capabilities)?;

    let mut message = Vec::with_capacity(512);
    message.extend_from_slice(b"gate4agent-c2-control-auth-v2-compatibility\0");
    message.extend_from_slice(&C2_CONTROL_PROTOCOL_VERSION.to_le_bytes());
    message.push(match direction { C2AuthDirection::Server => 1, C2AuthDirection::Client => 2 });
    message.extend_from_slice(client_nonce);
    message.extend_from_slice(server_nonce);

    message.extend_from_slice(b"offer\0");
    encode_protocol_range(&mut message, offer.protocol_versions);
    encode_capabilities(&mut message, &offer.capabilities);
    match offer.state_schema {
        Some(state_schema) => {
            message.push(1);
            encode_protocol_range(&mut message, state_schema.versions);
        }
        None => message.push(0),
    }

    message.extend_from_slice(b"selected\0");
    message.extend_from_slice(&selected.protocol_version.to_le_bytes());
    encode_capabilities(&mut message, &selected.capabilities);
    encode_bounded_str(&mut message, selected.host.operating_system.as_str());
    encode_bounded_str(&mut message, selected.host.architecture.as_str());
    message.push(match selected.path_semantics.style {
        PathStyle::Windows => 1,
        PathStyle::Posix => 2,
    });
    message.push(match selected.path_semantics.encoding {
        PathEncoding::Utf8 => 1,
        PathEncoding::UnixBytes => 2,
    });

    if message.len() > MAX_C2_BOUND_AUTH_TRANSCRIPT_BYTES {
        return Err(C2AuthTranscriptError::TooLong {
            len: message.len(),
            max: MAX_C2_BOUND_AUTH_TRANSCRIPT_BYTES,
        });
    }
    Ok(message)
}

fn validate_auth_capabilities(
    section: &'static str,
    capabilities: &[CapabilityId],
) -> Result<(), C2AuthTranscriptError> {
    if capabilities.len() > MAX_C2_AUTH_COMPATIBILITY_CAPABILITIES {
        return Err(C2AuthTranscriptError::TooManyCapabilities {
            section,
            count: capabilities.len(),
            max: MAX_C2_AUTH_COMPATIBILITY_CAPABILITIES,
        });
    }
    Ok(())
}

fn encode_protocol_range(message: &mut Vec<u8>, range: ProtocolRange) {
    message.extend_from_slice(&range.minimum().to_le_bytes());
    message.extend_from_slice(&range.maximum().to_le_bytes());
}

fn encode_capabilities(message: &mut Vec<u8>, capabilities: &[CapabilityId]) {
    message.extend_from_slice(&(capabilities.len() as u16).to_le_bytes());
    for capability in capabilities {
        encode_bounded_str(message, capability.as_str());
    }
}

fn encode_bounded_str(message: &mut Vec<u8>, value: &str) {
    debug_assert!(value.len() <= u16::MAX as usize);
    message.extend_from_slice(&(value.len() as u16).to_le_bytes());
    message.extend_from_slice(value.as_bytes());
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeTransportState {
    Online,
    Offline,
    Parked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeFreshness {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapKind {
    IncarnationChanged,
    HistoryEvicted,
    NonContiguousEvents,
    CursorRegression,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeGap {
    pub kind: GapKind,
    pub detected_at_unix_ms: u64,
    pub previous: Option<NodeCursor>,
    pub observed: NodeCursor,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum C2ErrorCategory {
    Authentication,
    Identity,
    Protocol,
    Transport,
    Timeout,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SanitizedError {
    pub category: C2ErrorCategory,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimSession {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub agent_id: String,
    pub transport: TransportKind,
    pub status: SlimSessionStatus,
    pub process_id: Option<u32>,
    pub terminal_size: Option<TerminalSize>,
    pub operation_pending: bool,
    pub input_pending: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SlimSessionStatus {
    Registered,
    Starting,
    Running,
    Stopping,
    Exited,
    Failed,
}

impl From<&SessionStatus> for SlimSessionStatus {
    fn from(status: &SessionStatus) -> Self {
        match status {
            SessionStatus::Registered => Self::Registered,
            SessionStatus::Starting => Self::Starting,
            SessionStatus::Running => Self::Running,
            SessionStatus::Stopping => Self::Stopping,
            SessionStatus::Exited { .. } => Self::Exited,
            SessionStatus::Failed { .. } => Self::Failed,
        }
    }
}

impl From<&C2SessionStatus> for SlimSessionStatus {
    fn from(status: &C2SessionStatus) -> Self {
        match status {
            C2SessionStatus::Registered => Self::Registered,
            C2SessionStatus::Starting => Self::Starting,
            C2SessionStatus::Running => Self::Running,
            C2SessionStatus::Stopping => Self::Stopping,
            C2SessionStatus::Exited { .. } => Self::Exited,
            C2SessionStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimWorkspace {
    pub workspace_id: WorkspaceId,
    pub canonical_root: String,
    pub canonical_root_truncated: bool,
    pub sessions: Vec<SlimSession>,
    pub session_count: usize,
    pub sessions_truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimManagedSessionRecord {
    pub record_id: SessionRecordId,
    pub display_name: String,
    pub display_name_truncated: bool,
    pub provider: AgentProvider,
    pub mode: SessionMode,
    pub state: ManagedSessionState,
    pub workspace_id: WorkspaceId,
    pub active_session: Option<SessionAddress>,
    pub provider_identity_present: bool,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SlimNodeInventory {
    pub node_id: NodeId,
    pub enabled_providers: Vec<AgentProvider>,
    pub workspaces: BTreeMap<WorkspaceId, SlimWorkspace>,
    pub workspace_count: usize,
    pub workspaces_truncated: bool,
    pub session_count: usize,
    pub sessions_truncated: bool,
    #[serde(default)]
    pub managed_sessions: Vec<SlimManagedSessionRecord>,
    #[serde(default)]
    pub managed_session_count: usize,
    #[serde(default)]
    pub managed_sessions_truncated: bool,
}

impl SlimNodeInventory {
    pub fn from_snapshot(snapshot: &NodeSnapshot) -> Self {
        let mut providers = snapshot.enabled_providers.clone();
        providers.sort_by_key(|provider| provider.agent_id());
        providers.dedup();
        let workspace_count = snapshot.workspaces.len();
        let session_count = snapshot.workspaces.iter().map(|workspace| workspace.sessions.len()).sum();
        let mut remaining_sessions = MAX_C2_SESSIONS_PER_NODE;
        let mut workspaces = BTreeMap::new();
        let mut ordered = snapshot.workspaces.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
        for workspace in ordered.into_iter().take(MAX_C2_WORKSPACES_PER_NODE) {
            let mut sessions = workspace.sessions.iter().collect::<Vec<_>>();
            sessions.sort_by_key(|session| (session.instance_id, session.generation));
            let take = remaining_sessions.min(sessions.len());
            let slim_sessions = sessions.into_iter().take(take).map(|session| SlimSession {
                instance_id: session.instance_id,
                generation: session.generation,
                agent_id: session.agent_id.as_str().to_owned(),
                transport: session.transport,
                status: SlimSessionStatus::from(&session.status),
                process_id: session.process_id,
                terminal_size: session.terminal_size,
                operation_pending: session.pending_operation.is_some(),
                input_pending: session.pending_input.is_some(),
            }).collect();
            remaining_sessions -= take;
            let display_root = sanitize_host_path_display(&workspace.canonical_root);
            let (canonical_root, canonical_root_truncated) =
                truncate_utf8(&display_root, MAX_C2_ROOT_BYTES);
            workspaces.insert(workspace.workspace_id.clone(), SlimWorkspace {
                workspace_id: workspace.workspace_id.clone(),
                canonical_root,
                canonical_root_truncated,
                sessions: slim_sessions,
                session_count: workspace.sessions.len(),
                sessions_truncated: workspace.sessions.len() > take,
            });
        }
        let included_session_count = workspaces
            .values()
            .map(|workspace| workspace.sessions.len())
            .sum::<usize>();
        let managed_session_count = snapshot.session_records.len();
        let mut ordered_records = snapshot.session_records.iter().collect::<Vec<_>>();
        ordered_records.sort_by(|left, right| left.record_id.cmp(&right.record_id));
        let managed_sessions = ordered_records
            .into_iter()
            .take(MAX_C2_MANAGED_SESSIONS_PER_NODE)
            .map(SlimManagedSessionRecord::from)
            .collect::<Vec<_>>();
        Self {
            node_id: snapshot.node_id.clone(),
            enabled_providers: providers,
            workspaces,
            workspace_count,
            workspaces_truncated: workspace_count > MAX_C2_WORKSPACES_PER_NODE,
            session_count,
            sessions_truncated: included_session_count < session_count,
            managed_sessions_truncated: managed_sessions.len() < managed_session_count,
            managed_sessions,
            managed_session_count,
        }
    }

    pub fn from_c2_snapshot(snapshot: &C2NodeSnapshot) -> Self {
        let mut providers = snapshot.enabled_providers.clone();
        providers.sort_by_key(|provider| provider.agent_id());
        providers.dedup();
        let workspace_count = snapshot.workspaces.len();
        let session_count = snapshot.workspaces.iter().map(|workspace| workspace.sessions.len()).sum();
        let mut remaining_sessions = MAX_C2_SESSIONS_PER_NODE;
        let mut workspaces = BTreeMap::new();
        let mut ordered = snapshot.workspaces.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
        for workspace in ordered.into_iter().take(MAX_C2_WORKSPACES_PER_NODE) {
            let mut sessions = workspace.sessions.iter().collect::<Vec<_>>();
            sessions.sort_by_key(|session| (session.instance_id, session.generation));
            let take = remaining_sessions.min(sessions.len());
            let slim_sessions = sessions.into_iter().take(take).map(|session| SlimSession {
                instance_id: session.instance_id,
                generation: session.generation,
                agent_id: session.agent_id.as_str().to_owned(),
                transport: session.transport,
                status: SlimSessionStatus::from(&session.status),
                process_id: session.process_id,
                terminal_size: session.terminal_size,
                operation_pending: session.pending_operation.is_some(),
                input_pending: session.pending_input.is_some(),
            }).collect();
            remaining_sessions -= take;
            let display_root = sanitize_host_path_display(&workspace.canonical_root);
            let (canonical_root, canonical_root_truncated) =
                truncate_utf8(&display_root, MAX_C2_ROOT_BYTES);
            workspaces.insert(workspace.workspace_id.clone(), SlimWorkspace {
                workspace_id: workspace.workspace_id.clone(),
                canonical_root,
                canonical_root_truncated,
                sessions: slim_sessions,
                session_count: workspace.sessions.len(),
                sessions_truncated: workspace.sessions.len() > take,
            });
        }
        let included_session_count = workspaces.values()
            .map(|workspace| workspace.sessions.len())
            .sum::<usize>();
        let managed_session_count = snapshot.session_records.len();
        let mut ordered_records = snapshot.session_records.iter().collect::<Vec<_>>();
        ordered_records.sort_by(|left, right| left.record_id.cmp(&right.record_id));
        let managed_sessions = ordered_records.into_iter()
            .take(MAX_C2_MANAGED_SESSIONS_PER_NODE)
            .map(SlimManagedSessionRecord::from)
            .collect::<Vec<_>>();
        Self {
            node_id: snapshot.node_id.clone(),
            enabled_providers: providers,
            workspaces,
            workspace_count,
            workspaces_truncated: workspace_count > MAX_C2_WORKSPACES_PER_NODE,
            session_count,
            sessions_truncated: included_session_count < session_count,
            managed_sessions_truncated: managed_sessions.len() < managed_session_count,
            managed_sessions,
            managed_session_count,
        }
    }
}

impl From<&ManagedSessionRecord> for SlimManagedSessionRecord {
    fn from(record: &ManagedSessionRecord) -> Self {
        let (display_name, display_name_truncated) =
            truncate_utf8(&record.display_name, MAX_C2_SESSION_DISPLAY_NAME_BYTES);
        Self {
            record_id: record.record_id.clone(),
            display_name,
            display_name_truncated,
            provider: record.provider,
            mode: record.mode,
            state: record.state,
            workspace_id: record.workspace_id.clone(),
            active_session: record.active_session.clone(),
            provider_identity_present: record.provider_session.is_some(),
            updated_at_unix_ms: record.updated_at_unix_ms,
        }
    }
}

impl From<&C2ManagedSessionRecord> for SlimManagedSessionRecord {
    fn from(record: &C2ManagedSessionRecord) -> Self {
        let (display_name, display_name_truncated) =
            truncate_utf8(&record.display_name, MAX_C2_SESSION_DISPLAY_NAME_BYTES);
        Self {
            record_id: record.record_id.clone(),
            display_name,
            display_name_truncated,
            provider: record.provider,
            mode: record.mode,
            state: record.state,
            workspace_id: record.workspace_id.clone(),
            active_session: record.active_session.clone(),
            provider_identity_present: record.provider_identity_present,
            updated_at_unix_ms: record.updated_at_unix_ms,
        }
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn sanitize_host_path_display(path: &OpaqueHostPath) -> String {
    let mut sanitized = String::new();
    for ch in path.display_text().chars() {
        if ch.is_control() {
            sanitized.extend(ch.escape_default());
        } else {
            sanitized.push(ch);
        }
    }
    sanitized
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedNode {
    pub endpoint: String,
    pub transport_label: String,
    pub transport: NodeTransportState,
    pub freshness: NodeFreshness,
    pub cursor: Option<NodeCursor>,
    pub inventory: Option<SlimNodeInventory>,
    pub last_attempt_unix_ms: Option<u64>,
    pub last_success_unix_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub last_error: Option<SanitizedError>,
    pub gaps: Vec<NodeGap>,
    pub gaps_truncated: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub service: String,
    pub api_version: u16,
    pub pid: u32,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadyResponse {
    pub ready: bool,
    pub api_version: u16,
    pub configured_nodes: usize,
    pub attempted_nodes: usize,
    pub online_nodes: usize,
    pub offline_nodes: usize,
    pub parked_nodes: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusResponse {
    pub api_version: u16,
    pub ready: bool,
    pub observed_at_unix_ms: u64,
    pub nodes: BTreeMap<NodeId, ObservedNode>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{
        GitSnapshot, GitWorktreeSnapshot, NodeFailureCode, NodeSnapshot, WorkspaceInspection,
        WorkspaceSnapshot,
    };
    use gate4agent_types::{
        AdapterBinding, AdapterFamily, AdapterId, AdapterVerification, AgentId, AgentInstanceId,
        CapabilitySnapshot, ControlEvent, ControlEventKind, ForegroundSnapshot, HistorySnapshot,
        OperationId, PreparedInputKind, ProviderEvent, ProviderSessionIdentity,
        ProviderSessionKey, ProviderSnapshot, ProviderSource, ResumeSessionSummary, ResumeSnapshot,
        SessionGeneration, SessionSnapshot, SessionStatus, TerminalSize, TransportKind,
    };

    fn host_path(value: impl Into<String>) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.into()).unwrap()
    }

    fn fixture_session() -> SessionSnapshot {
        SessionSnapshot {
            instance_id: AgentInstanceId(7),
            agent_id: AgentId::new("codex").unwrap(),
            transport: TransportKind::Pty,
            generation: SessionGeneration(2),
            status: SessionStatus::Running,
            pending_operation: Some(OperationId(9)),
            pending_input: Some(PreparedInputKind::TerminalText),
            process_id: Some(1234),
            terminal_size: Some(TerminalSize { rows: 40, columns: 120 }),
            terminal_frame: None,
            terminal_stale: None,
            session_options: None,
            capabilities: CapabilitySnapshot::default(),
            history: HistorySnapshot::default(),
            resume: ResumeSnapshot::default(),
            foreground: ForegroundSnapshot::default(),
            provider: ProviderSnapshot::default(),
        }
    }

    fn private_session_record() -> ManagedSessionRecord {
        ManagedSessionRecord {
            record_id: SessionRecordId::new("session-private").unwrap(),
            display_name: "release shepherd".to_owned(),
            provider: AgentProvider::Codex,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Live,
            workspace_id: WorkspaceId::new("primary").unwrap(),
            canonical_root: host_path(r"C:\private\canonical-root"),
            provider_session: Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "private-provider-session-id".to_owned(),
                transcript_path: Some(r"C:\private\transcript-secret.jsonl".to_owned()),
            }),
            active_session: Some(SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: gate4agent_node_protocol::SessionKey {
                    instance_id: AgentInstanceId(41),
                    generation: SessionGeneration(3),
                },
            }),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 20,
            last_error: Some("private-error-with-secret-token".to_owned()),
        }
    }

    fn assert_private_record_fields_absent(json: &str) {
        assert!(!json.contains("canonical_root"));
        assert!(!json.contains("provider_session"));
        assert!(!json.contains("private-provider-session-id"));
        assert!(!json.contains("transcript_path"));
        assert!(!json.contains("transcript-secret.jsonl"));
        assert!(!json.contains("last_error"));
        assert!(!json.contains("private-error-with-secret-token"));
    }

    fn routed_response(response: NodeResponse) -> RoutedNodeResponse {
        RoutedNodeResponse {
            node_id: NodeId::new("node-a").unwrap(),
            incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            response: Ok(C2NodeResponse::from(&response)),
        }
    }

    fn routed_control_event(event: ControlEventKind) -> RoutedNodeEvent {
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: AgentInstanceId(41),
                generation: SessionGeneration(3),
            },
        };
        RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([8; 16]),
                sequence: 9,
            },
            event: C2NodeEvent::from(&NodeEvent::Control {
                address,
                event: ControlEvent {
                    protocol_version: gate4agent_types::CONTROL_PROTOCOL_VERSION,
                    sequence: 12,
                    command_id: None,
                    instance_id: AgentInstanceId(41),
                    generation: SessionGeneration(3),
                    event,
                },
            }),
        }
    }

    fn provider_source() -> ProviderSource {
        ProviderSource {
            family: AdapterFamily::PtySemantic,
            binding: AdapterBinding::new(
                AdapterId::new("codex").unwrap(),
                "fixture/v1",
                AdapterVerification::SyntheticFixture,
            ).unwrap(),
        }
    }

    fn c2_compatibility_support(
        protocol_versions: ProtocolRange,
        capabilities: Vec<CapabilityId>,
    ) -> C2ControlCompatibilitySupport {
        C2ControlCompatibilitySupport {
            protocol_versions,
            capabilities,
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("darwin").unwrap(),
                architecture: ArchitectureId::new("aarch64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Posix,
                encoding: PathEncoding::Utf8,
            },
        }
    }

    #[test]
    fn c2_compatibility_legacy_client_hello_json_is_byte_equivalent() {
        let hello = C2ClientHello::new([0; C2_AUTH_NONCE_BYTES]);
        let json = serde_json::to_string(&hello).unwrap();
        let expected = concat!(
            r#"{"protocol_version":2,"client_nonce":["#,
            "0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,",
            "0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"
        );

        assert_eq!(json, expected);
        assert_eq!(
            serde_json::from_str::<C2ClientHello>(expected).unwrap(),
            hello,
        );
    }

    #[test]
    fn c2_compatibility_legacy_server_json_is_byte_equivalent() {
        #[derive(Serialize)]
        struct LegacyChallenge {
            protocol_version: u16,
            server_nonce: [u8; C2_AUTH_NONCE_BYTES],
            server_proof: [u8; C2_AUTH_PROOF_BYTES],
        }

        #[derive(Serialize)]
        struct LegacyHello<'a> {
            protocol_version: u16,
            connection_id: u64,
            status: &'a StatusResponse,
        }

        let challenge = C2ServerChallenge {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            server_nonce: [1; C2_AUTH_NONCE_BYTES],
            server_proof: [2; C2_AUTH_PROOF_BYTES],
            compatibility: None,
        };
        let legacy_challenge = LegacyChallenge {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            server_nonce: [1; C2_AUTH_NONCE_BYTES],
            server_proof: [2; C2_AUTH_PROOF_BYTES],
        };
        assert_eq!(
            serde_json::to_vec(&challenge).unwrap(),
            serde_json::to_vec(&legacy_challenge).unwrap(),
        );

        let status = StatusResponse {
            api_version: C2_API_VERSION,
            ready: true,
            observed_at_unix_ms: 7,
            nodes: BTreeMap::new(),
        };
        let hello = C2Hello {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            connection_id: 11,
            status: status.clone(),
            compatibility: None,
        };
        let legacy_hello = LegacyHello {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            connection_id: 11,
            status: &status,
        };
        assert_eq!(
            serde_json::to_vec(&hello).unwrap(),
            serde_json::to_vec(&legacy_hello).unwrap(),
        );
    }

    #[test]
    fn c2_compatibility_missing_offer_negotiates_exact_v2() {
        let support = c2_compatibility_support(
            ProtocolRange::new(1, 4).unwrap(),
            Vec::new(),
        );

        let negotiated = support
            .negotiate(&C2ClientHello::new([1; C2_AUTH_NONCE_BYTES]))
            .unwrap();

        assert_eq!(negotiated.protocol_version, C2_CONTROL_PROTOCOL_VERSION);
        assert!(negotiated.capabilities.is_empty());
    }

    #[test]
    fn c2_compatibility_selects_active_v2_and_capability_intersection() {
        let shared = CapabilityId::new("terminal-stream").unwrap();
        let server_only = CapabilityId::new("server-only").unwrap();
        let client_only = CapabilityId::new("client-only").unwrap();
        let support = c2_compatibility_support(
            ProtocolRange::new(1, 5).unwrap(),
            vec![shared.clone(), server_only],
        );
        let hello = C2ClientHello::negotiating(
            [2; C2_AUTH_NONCE_BYTES],
            ClientCompatibilityOffer {
                protocol_versions: ProtocolRange::new(2, 4).unwrap(),
                capabilities: vec![client_only, shared.clone()],
                state_schema: None,
            },
        );

        let negotiated = support.negotiate(&hello).unwrap();

        assert_eq!(negotiated.protocol_version, C2_CONTROL_PROTOCOL_VERSION);
        assert_eq!(negotiated.capabilities, vec![shared]);
    }

    #[test]
    fn c2_compatibility_disjoint_ranges_fail() {
        let support = c2_compatibility_support(
            ProtocolRange::new(2, 3).unwrap(),
            Vec::new(),
        );
        let hello = C2ClientHello::negotiating(
            [3; C2_AUTH_NONCE_BYTES],
            ClientCompatibilityOffer {
                protocol_versions: ProtocolRange::new(4, 5).unwrap(),
                capabilities: Vec::new(),
                state_schema: None,
            },
        );

        assert!(matches!(
            support.negotiate(&hello),
            Err(ProtocolNegotiationError::Disjoint { .. }),
        ));
    }

    #[test]
    fn c2_compatibility_bound_auth_transcript_is_exact_and_selection_sensitive() {
        let capability = CapabilityId::new(C2_COMPATIBILITY_METADATA_CAPABILITY).unwrap();
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![capability.clone()],
            state_schema: None,
        };
        let selected = NegotiatedC2ControlCompatibility {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![capability],
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
        };
        let transcript = c2_bound_auth_transcript(
            C2AuthDirection::Server,
            &[0x11; C2_AUTH_NONCE_BYTES],
            &[0x22; C2_AUTH_NONCE_BYTES],
            &offer,
            &selected,
        ).unwrap();
        let hex = transcript.iter().map(|byte| format!("{byte:02x}")).collect::<String>();

        assert_eq!(
            hex,
            concat!(
                "67617465346167656e742d63322d636f6e74726f6c2d617574682d76322d636f6d7061746962696c69747900",
                "020001",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222222222222222222222222222",
                "6f66666572000200020001001600636f6d7061746962696c6974792e6d6574616461746100",
                "73656c656374656400020001001600636f6d7061746962696c6974792e6d65746164617461",
                "070077696e646f777306007838365f36340101",
            ),
        );

        let mut tampered = selected;
        tampered.path_semantics.style = PathStyle::Posix;
        assert_ne!(
            transcript,
            c2_bound_auth_transcript(
                C2AuthDirection::Server,
                &[0x11; C2_AUTH_NONCE_BYTES],
                &[0x22; C2_AUTH_NONCE_BYTES],
                &offer,
                &tampered,
            ).unwrap(),
        );
    }

    #[test]
    fn c2_compatibility_preserves_foreign_host_and_opaque_path() {
        let support = c2_compatibility_support(
            ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION).unwrap(),
            Vec::new(),
        );
        let negotiated = support
            .negotiate(&C2ClientHello::new([4; C2_AUTH_NONCE_BYTES]))
            .unwrap();
        let projected = C2NodeSnapshot::from(&NodeSnapshot {
            node_id: NodeId::new("remote-mac").unwrap(),
            enabled_providers: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("repo").unwrap(),
                canonical_root: host_path("/srv/CaseSensitive/../literal-root"),
                sessions: Vec::new(),
            }],
            session_records: Vec::new(),
        });
        let json = serde_json::to_string(&(projected, negotiated)).unwrap();
        let (projected, negotiated) = serde_json::from_str::<(
            C2NodeSnapshot,
            NegotiatedC2ControlCompatibility,
        )>(&json).unwrap();

        assert_eq!(
            projected.workspaces[0].canonical_root.display_text(),
            "/srv/CaseSensitive/../literal-root",
        );
        assert_eq!(negotiated.host.operating_system.as_str(), "darwin");
        assert_eq!(negotiated.host.architecture.as_str(), "aarch64");
        assert_eq!(negotiated.path_semantics.style, PathStyle::Posix);
    }

    #[test]
    fn slim_inventory_is_deterministic_and_excludes_terminal_history() {
        let snapshot = NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![AgentProvider::Codex, AgentProvider::Claude, AgentProvider::Codex],
            workspaces: vec![
                WorkspaceSnapshot { workspace_id: WorkspaceId::new("z-work").unwrap(), canonical_root: host_path("z"), sessions: Vec::new() },
                WorkspaceSnapshot {
                    workspace_id: WorkspaceId::new("a-work").unwrap(),
                    canonical_root: host_path("a"),
                    sessions: vec![fixture_session()],
                },
            ],
            session_records: Vec::new(),
        };
        let slim = SlimNodeInventory::from_snapshot(&snapshot);
        assert_eq!(slim.enabled_providers, vec![AgentProvider::Claude, AgentProvider::Codex]);
        assert_eq!(slim.workspaces.keys().map(WorkspaceId::as_str).collect::<Vec<_>>(), vec!["a-work", "z-work"]);
        let session = &slim.workspaces[&WorkspaceId::new("a-work").unwrap()].sessions[0];
        assert_eq!(session.transport, TransportKind::Pty);
        assert_eq!(session.process_id, Some(1234));
        assert_eq!(session.terminal_size, Some(TerminalSize { rows: 40, columns: 120 }));
        assert!(session.operation_pending);
        assert!(session.input_pending);
        let json = serde_json::to_string(&slim).unwrap();
        assert!(!json.contains("terminal_frame"));
        assert!(!json.contains("history"));
    }

    #[test]
    fn slim_inventory_reports_sessions_hidden_by_workspace_truncation() {
        let mut workspaces = (0..MAX_C2_WORKSPACES_PER_NODE)
            .map(|index| WorkspaceSnapshot {
                workspace_id: WorkspaceId::new(format!("work-{index:02}")).unwrap(),
                canonical_root: host_path(format!("root-{index:02}")),
                sessions: Vec::new(),
            })
            .collect::<Vec<_>>();
        workspaces.push(WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("work-zz").unwrap(),
            canonical_root: host_path("hidden-root"),
            sessions: vec![fixture_session()],
        });
        let slim = SlimNodeInventory::from_snapshot(&NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            workspaces,
            session_records: Vec::new(),
        });
        assert!(slim.workspaces_truncated);
        assert_eq!(slim.session_count, 1);
        assert!(slim.sessions_truncated);
    }

    #[test]
    fn control_auth_transcript_is_direction_and_protocol_domain_separated() {
        assert_eq!(C2_API_VERSION, 2);
        assert_eq!(C2_CONTROL_PROTOCOL_VERSION, 2);
        let client_nonce = [3; C2_AUTH_NONCE_BYTES];
        let server_nonce = [7; C2_AUTH_NONCE_BYTES];
        let server = c2_auth_transcript(C2AuthDirection::Server, &client_nonce, &server_nonce);
        let client = c2_auth_transcript(C2AuthDirection::Client, &client_nonce, &server_nonce);
        assert_ne!(server, client);
        assert!(server.starts_with(b"gate4agent-c2-control-auth-v2\0"));
        assert!(!server.windows(b"gate4agent-node-auth-v3".len()).any(|window| window == b"gate4agent-node-auth-v3"));
        assert_eq!(&server[server.len() - (C2_AUTH_NONCE_BYTES * 2)..server.len() - C2_AUTH_NONCE_BYTES], &client_nonce);
        assert_eq!(&server[server.len() - C2_AUTH_NONCE_BYTES..], &server_nonce);
    }

    #[test]
    fn topology_projection_is_sorted_bounded_and_minimal() {
        let nodes = (0..=MAX_C2_NODES).rev().map(|index| {
            let node_id = NodeId::new(format!("node-{index:03}")).unwrap();
            let incarnation_id = NodeIncarnationId::from_bytes([index as u8; 16]);
            let observed = ObservedNode {
                endpoint: format!(r"\\.\pipe\node-{index:03}"),
                transport_label: "windows-named-pipe".to_owned(),
                transport: NodeTransportState::Online,
                freshness: NodeFreshness::Fresh,
                cursor: Some(NodeCursor { incarnation_id, sequence: 99 }),
                inventory: None,
                last_attempt_unix_ms: Some(10),
                last_success_unix_ms: Some(10),
                consecutive_failures: 0,
                last_error: None,
                gaps: Vec::new(),
                gaps_truncated: 0,
            };
            (node_id, observed)
        }).collect();
        let topology = C2Topology::from_status(&StatusResponse {
            api_version: C2_API_VERSION,
            ready: true,
            observed_at_unix_ms: 10,
            nodes,
        });

        assert_eq!(topology.nodes.len(), MAX_C2_NODES);
        assert_eq!(topology.nodes.first().unwrap().node_id.as_str(), "node-000");
        assert_eq!(topology.nodes.last().unwrap().node_id.as_str(), "node-063");
        assert_eq!(
            topology.nodes[0].current_incarnation_id,
            Some(NodeIncarnationId::from_bytes([0; 16])),
        );
        let json = serde_json::to_string(&C2ServerFrame::Topology(topology)).unwrap();
        assert!(!json.contains("observed_at_unix_ms"));
        assert!(!json.contains("sequence"));
        assert!(!json.contains("inventory"));
    }

    #[test]
    fn routed_durable_session_responses_round_trip_without_private_record_fields() {
        let record = private_session_record();
        let session = record.active_session.clone().unwrap();
        let responses = vec![
            NodeResponse::Snapshot {
                event_sequence: 4,
                controller: None,
                snapshot: NodeSnapshot {
                    node_id: NodeId::new("node-a").unwrap(),
                    enabled_providers: vec![AgentProvider::Codex],
                    workspaces: Vec::new(),
                    session_records: vec![record.clone()],
                },
            },
            NodeResponse::SessionRecordUpdated { record: record.clone() },
            NodeResponse::SessionRecordResumed {
                record: record.clone(),
                session: session.clone(),
            },
            NodeResponse::Resync {
                event_sequence: 5,
                snapshot: NodeSnapshot {
                    node_id: NodeId::new("node-a").unwrap(),
                    enabled_providers: vec![AgentProvider::Codex],
                    workspaces: Vec::new(),
                    session_records: vec![record.clone()],
                },
                events: vec![gate4agent_node_protocol::NodeEventEnvelope {
                    sequence: 5,
                    event: NodeEvent::SessionRecordUpserted { record: record.clone() },
                }],
            },
        ];

        for response in responses {
            let json = serde_json::to_string(&routed_response(response)).unwrap();
            assert_private_record_fields_absent(&json);
            assert!(json.contains("provider_identity_present"));
            let decoded = serde_json::from_str::<RoutedNodeResponse>(&json).unwrap();
            let decoded_record = match decoded.response.unwrap() {
                C2NodeResponse::Snapshot { snapshot, .. } => snapshot.session_records.into_iter().next().unwrap(),
                C2NodeResponse::SessionRecordUpdated { record }
                | C2NodeResponse::SessionRecordResumed { record, .. } => record,
                C2NodeResponse::Resync { snapshot, events, .. } => {
                    assert!(matches!(events.into_iter().next().unwrap().event,
                        C2NodeEvent::SessionRecordUpserted { ref record }
                        if record.provider_identity_present));
                    snapshot.session_records.into_iter().next().unwrap()
                }
                response => panic!("unexpected response after C2 round trip: {response:?}"),
            };
            assert_eq!(decoded_record.record_id.as_str(), "session-private");
            assert_eq!(decoded_record.display_name, "release shepherd");
            assert_eq!(decoded_record.active_session.as_ref(), Some(&session));
            assert!(decoded_record.provider_identity_present);
        }
    }

    #[test]
    fn routed_session_record_event_round_trips_without_private_record_fields() {
        let routed = RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([8; 16]),
                sequence: 9,
            },
            event: C2NodeEvent::from(&NodeEvent::SessionRecordUpserted {
                record: private_session_record(),
            }),
        };

        let json = serde_json::to_string(&routed).unwrap();
        assert_private_record_fields_absent(&json);
        assert!(json.contains("provider_identity_present"));
        let decoded = serde_json::from_str::<RoutedNodeEvent>(&json).unwrap();
        let C2NodeEvent::SessionRecordUpserted { record } = decoded.event else {
            panic!("unexpected routed event after C2 round trip");
        };
        assert_eq!(record.record_id.as_str(), "session-private");
        assert_eq!(record.display_name, "release shepherd");
        assert!(record.provider_identity_present);
    }

    #[test]
    fn routed_node_failure_replaces_raw_message_with_fixed_category() {
        let raw = NodeFailure {
            code: NodeFailureCode::BackendOperationFailed,
            message: r"provider token-secret failed at C:\private\relay.log".to_owned(),
        };
        let routed = RoutedNodeResponse {
            node_id: NodeId::new("node-a").unwrap(),
            incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            response: Err(C2NodeFailure::from(&raw)),
        };

        let json = serde_json::to_string(&routed).unwrap();
        assert!(!json.contains("token-secret"));
        assert!(!json.contains("private"));
        assert!(!json.contains("relay.log"));
        let decoded = serde_json::from_str::<RoutedNodeResponse>(&json).unwrap();
        assert_eq!(decoded.response.unwrap_err(), C2NodeFailure {
            code: NodeFailureCode::BackendOperationFailed,
            message: "node backend operation failed".to_owned(),
        });
    }

    #[test]
    fn routed_workspace_inspection_omits_raw_git_diagnostics_and_reasons() {
        let worktree = GitWorktreeSnapshot {
            path: host_path(r"C:\work\feature"),
            head: "abc123".to_owned(),
            branch: Some("feature/privacy".to_owned()),
            is_bare: false,
            is_main: false,
            locked: true,
            lock_reason: Some("lock-secret-provider-token".to_owned()),
            prunable: true,
            prunable_reason: Some("prunable-secret-private-path".to_owned()),
            workspace_id: Some(WorkspaceId::new("feature").unwrap()),
        };
        let response = routed_response(NodeResponse::WorkspaceInspected {
            inspection: WorkspaceInspection {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                entries: Vec::new(),
                tree_truncated: false,
                git: GitSnapshot {
                    is_repository: true,
                    branch: Some("main".to_owned()),
                    status: Vec::new(),
                    recent_commits: Vec::new(),
                    worktrees: vec![worktree],
                    truncated: false,
                    diagnostic: Some("diagnostic-secret C:\\private\\git.stderr".to_owned()),
                },
            },
        });

        let json = serde_json::to_string(&response).unwrap();
        for secret in [
            "lock-secret-provider-token",
            "prunable-secret-private-path",
            "diagnostic-secret",
            "git.stderr",
        ] {
            assert!(!json.contains(secret));
        }
        assert!(!json.contains("lock_reason"));
        assert!(!json.contains("prunable_reason"));
        assert!(!json.contains("\"diagnostic\":"));
        let decoded = serde_json::from_str::<RoutedNodeResponse>(&json).unwrap();
        let Ok(C2NodeResponse::WorkspaceInspected { inspection }) = decoded.response else {
            panic!("unexpected routed workspace response");
        };
        assert!(inspection.git.diagnostic_present);
        assert!(inspection.git.worktrees[0].locked);
        assert!(inspection.git.worktrees[0].prunable);
        assert_eq!(inspection.git.worktrees[0].path.display_text(), r"C:\work\feature");
    }

    #[test]
    fn c2_projection_roundtrips_non_utf8_host_paths_without_interpretation() {
        let opaque = OpaqueHostPath::unix_bytes(vec![b'/', b's', b'r', b'v', b'/', 0xff, b'\n', 0x1b]).unwrap();
        let projected = C2NodeSnapshot::from(&NodeSnapshot {
            node_id: NodeId::new("remote-linux").unwrap(),
            enabled_providers: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("repo").unwrap(),
                canonical_root: opaque.clone(),
                sessions: Vec::new(),
            }],
            session_records: Vec::new(),
        });

        let encoded = serde_json::to_string(&projected).unwrap();
        let decoded = serde_json::from_str::<C2NodeSnapshot>(&encoded).unwrap();

        assert_eq!(decoded.workspaces[0].canonical_root, opaque);
        assert_eq!(decoded.workspaces[0].workspace_id.as_str(), "repo");
        let slim = SlimNodeInventory::from_c2_snapshot(&decoded);
        let display = &slim.workspaces[&WorkspaceId::new("repo").unwrap()].canonical_root;
        assert!(!display.chars().any(char::is_control));
        assert!(display.contains("\\n"));
        assert!(display.contains("\\u{1b}"));
    }

    #[test]
    fn routed_worktree_created_omits_raw_git_reasons() {
        let response = routed_response(NodeResponse::WorktreeCreated {
            worktree: GitWorktreeSnapshot {
                path: host_path(r"C:\work\feature"),
                head: "abc123".to_owned(),
                branch: Some("feature/privacy".to_owned()),
                is_bare: false,
                is_main: false,
                locked: true,
                lock_reason: Some("created-lock-secret".to_owned()),
                prunable: true,
                prunable_reason: Some("created-prunable-secret".to_owned()),
                workspace_id: Some(WorkspaceId::new("feature").unwrap()),
            },
            workspace: WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("feature").unwrap(),
                canonical_root: host_path(r"C:\work\feature"),
                sessions: Vec::new(),
            },
        });

        let json = serde_json::to_string(&response).unwrap();
        assert!(!json.contains("created-lock-secret"));
        assert!(!json.contains("created-prunable-secret"));
        assert!(!json.contains("lock_reason"));
        assert!(!json.contains("prunable_reason"));
        let decoded = serde_json::from_str::<RoutedNodeResponse>(&json).unwrap();
        assert!(matches!(decoded.response, Ok(C2NodeResponse::WorktreeCreated {
            worktree: C2GitWorktreeSnapshot { locked: true, prunable: true, .. },
            ..
        })));
    }

    #[test]
    fn routed_resume_authorized_control_event_omits_provider_session_identity() {
        let routed = routed_control_event(ControlEventKind::ResumeAuthorized {
            session: ResumeSessionSummary {
                key: ProviderSessionKey::SessionId,
                id: "private-resume-session-id".to_owned(),
            },
        });

        let json = serde_json::to_string(&routed).unwrap();
        assert!(json.contains("resume-authorized"));
        assert!(!json.contains("private-resume-session-id"));
        assert!(!json.contains("provider_session"));
        assert!(!json.contains("transcript_path"));
        let decoded = serde_json::from_str::<RoutedNodeEvent>(&json).unwrap();
        assert!(matches!(decoded.event, C2NodeEvent::Control {
            event: C2ControlEvent { event: C2ControlEventKind::ResumeAuthorized, .. },
            ..
        }));
    }

    #[test]
    fn routed_session_identity_observed_control_event_omits_provider_identity_and_path() {
        let routed = routed_control_event(ControlEventKind::ProviderEvent {
            sequence: 19,
            source: provider_source(),
            source_sequence: 7,
            event: ProviderEvent::SessionIdentityObserved {
                identity: ProviderSessionIdentity {
                    key: ProviderSessionKey::ConversationId,
                    id: "private-observed-provider-id".to_owned(),
                    transcript_path: Some(r"C:\private\provider-transcript.jsonl".to_owned()),
                },
            },
        });

        let json = serde_json::to_string(&routed).unwrap();
        assert!(json.contains("session-identity-observed"));
        assert!(!json.contains("private-observed-provider-id"));
        assert!(!json.contains("provider-transcript.jsonl"));
        assert!(!json.contains("transcript_path"));
        assert!(!json.contains("\"identity\":"));
        let decoded = serde_json::from_str::<RoutedNodeEvent>(&json).unwrap();
        assert!(matches!(decoded.event, C2NodeEvent::Control {
            event: C2ControlEvent {
                event: C2ControlEventKind::ProviderEvent {
                    event: C2ProviderEventKind::SessionIdentityObserved,
                },
                ..
            },
            ..
        }));
    }

    #[test]
    fn routed_snapshot_recursively_omits_provider_identity_and_error_state() {
        let mut session = fixture_session();
        session.status = SessionStatus::Failed {
            message: "private-session-failure".to_owned(),
        };
        session.terminal_stale = Some("private-terminal-error".to_owned());
        session.history.last_error = Some("private-history-error".to_owned());
        session.resume.last_session = Some(ResumeSessionSummary {
            key: ProviderSessionKey::SessionId,
            id: "private-resume-summary-id".to_owned(),
        });
        session.resume.last_error = Some("private-resume-error".to_owned());
        session.foreground.stale_reason = Some("private-foreground-error".to_owned());
        session.provider.session = Some(ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "private-snapshot-provider-id".to_owned(),
            transcript_path: Some(r"C:\private\snapshot-transcript.jsonl".to_owned()),
        });
        session.provider.current_prompt = Some("private-current-prompt".to_owned());
        session.provider.last_event = Some(ProviderEvent::Error {
            message: "private-provider-error".to_owned(),
        });
        let routed = routed_response(NodeResponse::Snapshot {
            event_sequence: 4,
            controller: None,
            snapshot: NodeSnapshot {
                node_id: NodeId::new("node-a").unwrap(),
                enabled_providers: vec![AgentProvider::Codex],
                workspaces: vec![WorkspaceSnapshot {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    canonical_root: host_path(r"C:\workspace"),
                    sessions: vec![session],
                }],
                session_records: Vec::new(),
            },
        });

        let json = serde_json::to_string(&routed).unwrap();
        for secret in [
            "private-session-failure",
            "private-terminal-error",
            "private-history-error",
            "private-resume-summary-id",
            "private-resume-error",
            "private-foreground-error",
            "private-snapshot-provider-id",
            "snapshot-transcript.jsonl",
            "private-current-prompt",
            "private-provider-error",
        ] {
            assert!(!json.contains(secret));
        }
        assert!(!json.contains("provider_session"));
        assert!(!json.contains("transcript_path"));
        assert!(!json.contains("last_error"));
        let decoded = serde_json::from_str::<RoutedNodeResponse>(&json).unwrap();
        assert!(matches!(decoded.response, Ok(C2NodeResponse::Snapshot { snapshot, .. })
            if matches!(snapshot.workspaces[0].sessions[0].status, C2SessionStatus::Failed)
                && snapshot.workspaces[0].sessions[0].provider_identity_present));
    }

    #[test]
    fn slim_managed_sessions_are_sorted_bounded_and_privacy_minimized() {
        let long_name = "ж".repeat((MAX_C2_SESSION_DISPLAY_NAME_BYTES / 2) + 4);
        let make_record = |record_id: &str, display_name: String| ManagedSessionRecord {
            record_id: SessionRecordId::new(record_id).unwrap(),
            display_name,
            provider: AgentProvider::Codex,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("primary").unwrap(),
            canonical_root: host_path(r"C:\private\workspace"),
            provider_session: Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "5af75a6b-3e64-41dd-96fa-private-provider-id".to_owned(),
                transcript_path: Some(r"C:\private\transcript.jsonl".to_owned()),
            }),
            active_session: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 20,
            last_error: Some("private backend diagnostic".to_owned()),
        };
        let snapshot = NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![AgentProvider::Codex],
            workspaces: Vec::new(),
            session_records: vec![
                make_record("session-z", "z".to_owned()),
                make_record("session-a", long_name),
            ],
        };

        let slim = SlimNodeInventory::from_snapshot(&snapshot);
        assert_eq!(slim.managed_session_count, 2);
        assert!(!slim.managed_sessions_truncated);
        assert_eq!(
            slim.managed_sessions.iter().map(|record| record.record_id.as_str()).collect::<Vec<_>>(),
            vec!["session-a", "session-z"],
        );
        assert_eq!(slim.managed_sessions[0].display_name.len(), MAX_C2_SESSION_DISPLAY_NAME_BYTES);
        assert!(slim.managed_sessions[0].display_name_truncated);
        assert!(slim.managed_sessions[0].provider_identity_present);

        let json = serde_json::to_string(&slim).unwrap();
        assert!(!json.contains("5af75a6b-3e64-41dd-96fa-private-provider-id"));
        assert!(!json.contains("transcript.jsonl"));
        assert!(!json.contains("private backend diagnostic"));
        assert!(!json.contains("private\\\\workspace"));
        assert!(!json.contains("provider_session"));
        assert!(!json.contains("last_error"));
    }

    #[test]
    fn slim_inventory_defaults_managed_sessions_for_v1_payloads() {
        let legacy = r#"{"node_id":"node-a","enabled_providers":[],"workspaces":{},"workspace_count":0,"workspaces_truncated":false,"session_count":0,"sessions_truncated":false}"#;
        let inventory = serde_json::from_str::<SlimNodeInventory>(legacy).unwrap();
        assert!(inventory.managed_sessions.is_empty());
        assert_eq!(inventory.managed_session_count, 0);
        assert!(!inventory.managed_sessions_truncated);
    }

    #[test]
    fn slim_inventory_reports_managed_session_truncation() {
        let session_records = (0..=MAX_C2_MANAGED_SESSIONS_PER_NODE)
            .map(|index| ManagedSessionRecord {
                record_id: SessionRecordId::new(format!("session-{index:03}")).unwrap(),
                display_name: format!("session {index}"),
                provider: AgentProvider::Claude,
                mode: SessionMode::Inline,
                state: ManagedSessionState::Unavailable,
                workspace_id: WorkspaceId::new("primary").unwrap(),
                canonical_root: host_path(r"C:\repo"),
                provider_session: None,
                active_session: None,
                created_at_unix_ms: index as u64,
                updated_at_unix_ms: index as u64,
                last_error: None,
            })
            .collect::<Vec<_>>();
        let slim = SlimNodeInventory::from_snapshot(&NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            workspaces: Vec::new(),
            session_records,
        });
        assert_eq!(slim.managed_session_count, MAX_C2_MANAGED_SESSIONS_PER_NODE + 1);
        assert_eq!(slim.managed_sessions.len(), MAX_C2_MANAGED_SESSIONS_PER_NODE);
        assert!(slim.managed_sessions_truncated);
    }
}
