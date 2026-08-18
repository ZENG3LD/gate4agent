use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, mpsc as sync_mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use gate4agent_c2_client::{
    connect_local as connect_c2_local, C2ControlError, C2ControlHandle,
};
use gate4agent_c2_protocol::{
    C2ControlEvent, C2ControlEventKind, C2ManagedSessionRecord, C2NodeEvent, C2NodeFailure, C2NodeResponse,
    C2NodeSnapshot, C2RelayRoute, C2SessionSnapshot, C2SessionStatus, C2Topology, NodeRoute,
    NodeTransportState, C2WorkspaceInspection,
};
use gate4agent_node_protocol::{
    AgentProgressV1, CapabilityId, ControllerState, GitCommitDetails, GitCommitSummary, GitDiff, GitDiffMode,
    GitSnapshot, GitStatusEntry,
    GitDiffRequest, GitHistoryPage, GitObjectId, HostDirectoryListing, ManagedSessionRecord,
    ManagedWorktreeLeaseSnapshot, ManagedWorktreeLeaseState, ManagedWorktreeSpawnReceipt,
    ManagedWorktreeSpawnRequest,
    NativeSessionCatalogEntry, NativeSessionCatalogPage, NativeSessionCatalogRoute as WireNativeSessionCatalogRoute,
    NativeSessionCatalogSummary, NativeSessionCatalogWindow, NativeSessionSelection,
    SessionAgentProgress, SessionRecordPreview, NodeEvent, NodeFailureCode, NodeId,
    NodeRequest, NodeResponse, NodeSnapshot, ObservationV1, ResolvedContextPackReceipt, ResolvedSpawnReceipt,
    LaunchInventory, ResolvedBundleReceipt, ResolvedEnvironmentProfileReceipt, SpawnProfileSummary,
    ManagedSessionState, OpaqueHostPath, SessionAddress as WireSessionAddress, SessionKey,
    SessionMode, SessionRecordId, SpawnBundleId,
    SpawnContextId, SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides,
    SpawnProfileId, SpawnProfileRevision, SpawnPrompt, SpawnRequiredCapabilities, SpawnSpec,
    SpawnTarget,
    RepositoryPath, WorkspaceEntry, WorkspaceEntryKind, WorkspaceFileContent, WorkspaceFileRead,
    WorkspaceFileRevision, WorkspaceId,
    WorktreeProfileId, MAX_NODE_TEXT_BYTES,
    NODE_OBSERVATION_EVENTS_CAPABILITY, NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_harness_client::{
    HarnessNativeSessionCatalogEntryV1, HarnessNativeSessionCatalogScopeV1,
    HarnessNativeSessionCatalogSummaryV1, HarnessNativeSessionCatalogWindowV1,
    HarnessNativeSessionExternalGroupKindV1, HarnessNativeSessionPreviewRoleV1,
    HarnessNativeSessionPreviewV1, HarnessNativeSessionRouteV1,
    HarnessNativeSessionSelectionV1,
    HarnessOperatorActionV1, HarnessOperatorClient, HarnessOperatorCredential,
    HarnessOperatorIntentV1, HarnessOperatorMutationOutcomeV1, HarnessOperatorRequestRefV1,
    HarnessOperatorResponseV1, HarnessTaskLaunchOptionsV1,
    HarnessRunContextSourceObservationV1,
    HarnessGitDiffModeV1, HarnessGitObjectIdV1, HarnessGitStatusCodeV1,
    HarnessOperatorClientError, HarnessOperatorHostErrorV1, HarnessRepositoryPathV1,
    HarnessReverseAttributionSubjectV1, HarnessReverseAttributionV1,
    HarnessRunGitDiffV1, HarnessRunGitHistoryPageV1, HarnessRunWorkspaceFileV1,
    HarnessRunWorkspaceInspectionV1, HarnessRunWorkspaceOriginV1,
    HarnessWorkspaceEntryKindV1, HarnessWorkspaceFileContentV1,
    HarnessTaskStartOutcomeV1, RedactedBindingStateV1, RedactedRunV1, HarnessRunCorrelationV1,
    HarnessRunTransferSummaryV1,
    HarnessRuntimeManagedModeV1, HarnessRuntimeManagedSessionV1,
    HarnessRuntimeManagedStateV1, HarnessRuntimeNodeInventoryV1,
    HarnessRuntimeMouseProtocolEncodingV1, HarnessRuntimeSessionAddressV1,
    HarnessRuntimeSessionStatusV1, HarnessRuntimeSessionV1, HarnessRuntimeTerminalFrameV1,
    HarnessRuntimeTerminalPageV1, HarnessRuntimeTerminalSizeV1, HarnessRuntimeTransportV1,
    HarnessRuntimeLaunchInventoryV1,
    HarnessNodeWorkspaceFileV1, HarnessNodeWorkspaceInspectionV1,
    HarnessNodeGitDiffV1, HarnessNodeGitHistoryPageV1,
    RedactedTaskV1, RunPageV1, TaskPageV1,
    SessionMonitorV1 as HarnessSessionMonitorV1, TimelineEntryV1,
    HARNESS_TERMINAL_PAGE_LIMIT_MAX,
};
use gate4agent_harness_protocol::HarnessSelectorV1;
use gate4agent_observation_api::{
    ManagedSessionKey, NodeId as ObservationNodeId, ObservationGap, ObservationIngressEnvelope,
    ObservationIngressPayload, ObservationRecordInventory, ObservationResyncBatch, ObservationTarget,
    ObservationTransport, RuntimeSessionKey,
};
use gate4agent_observation_service::{
    ObservationCommittedRoute, ObservationCommittedSnapshot, ObservationService,
};
use gate4agent_types::{
    AgentId, ControlEvent, ControlEventKind, ProviderActivity, ProviderSessionIdentity,
    HistoryMessageRole, NativeSessionExternalGroup, NativeSessionExternalGroupKind,
    NativeSessionPreviewMessage, SessionSnapshot, SessionStatus,
    TerminalFrame, TerminalSize,
    TerminalMouseProtocolEncoding, TransportKind,
};
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use uzor_tui::{Backend, CrosstermBackend, Screen};

use crate::app::{
    host_path_display, App, AppAction, ConnectionState, NodeView, Provider, ProviderInventory, PtyColorMode,
    ManagedSessionView, NativeSessionCatalogRoute, NativeSessionCatalogRowView, NativeSessionPreviewMessageView,
    NativeSessionPreviewView, SessionAddress, SessionView, UiKey, GitCommitView,
    HarnessReadFailure, HarnessRunOrigin, HarnessRunRef, HarnessWorkspaceFileTabKey,
    HarnessWorkspaceGitRequestDestination, WorkspaceFileTabKey, WorkspaceGitDiffTarget, WorkspaceGitDiffView,
    SessionMonitorSupport, WorkspaceGitRequestDestination, WorkspaceView,
};
#[cfg(test)]
use crate::app::SessionObservationIngressOutcome;
use crate::diagnostics::RuntimeDiagnostic;
use crate::preferences::{self, UiPreferences};
use crate::render;

#[cfg(test)]
use crate::app::{SessionMonitorTarget, SurfaceTab, WorkspaceGitTabKey};

const COMMAND_QUEUE: usize = 64;
const UPDATE_QUEUE: usize = 256;
const C2_COMMAND_ROUTE: &str = "\0c2";
const HARNESS_COMMAND_ROUTE: &str = "\0harness";
const HARNESS_HISTORY_COMMAND_ROUTE: &str = "\0harness-history";
const HARNESS_DETAIL_COMMAND_ROUTE: &str = "\0harness-detail";
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);
/// The harness read surface is poll-only (no event push yet): without a
/// periodic snapshot cadence the roster/kanban only converge after an
/// operator mutation, so sessions spawned moments ago (or by any other
/// client of the same harness) never appear until a manual refresh.
const HARNESS_SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const RAW_INPUT_COALESCE: Duration = Duration::from_millis(12);
const INSPECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const HARNESS_TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PREFERENCES_SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DIRTY_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(80);
const ANIMATION_TICKS_PER_FRAME: u8 = 4;
const MAX_TERMINAL_EVENT_BATCH: usize = 256;
const OBSERVATION_COMMAND_QUEUE: usize = 128;
const HARNESS_SNAPSHOT_PAGE_SIZE: u16 = 64;
const HARNESS_TASK_PAGE_BUDGET: usize = 16;
const HARNESS_TASK_ENTITY_BUDGET: usize = 512;
const HARNESS_RUN_PAGE_BUDGET: usize = 32;
const HARNESS_RUN_ENTITY_BUDGET: usize = 1_024;
const HARNESS_RUNTIME_INVENTORY_PAGE_BUDGET: usize = 64;
const HARNESS_RUNTIME_INVENTORY_ENTITY_BUDGET: usize = 4_096;
const HARNESS_RUNTIME_INVENTORY_RETRY_BUDGET: usize = 20;
const HARNESS_RUNTIME_INVENTORY_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum ObservationWriterCommand {
    Ingress(ObservationIngressEnvelope),
    Resync(ObservationResyncBatch),
    Inventory(ObservationRecordInventory),
    Flush,
    Close,
}

enum ObservationActorUpdate {
    Committed {
        revision: u64,
        route: ObservationCommittedRoute,
    },
    Rejected {
        message: String,
        node_id: Option<ObservationNodeId>,
        incarnation_id: Option<gate4agent_node_protocol::NodeIncarnationId>,
    },
    Unavailable(String),
}

struct ObservationWriterHandle {
    commands: sync_mpsc::SyncSender<ObservationWriterCommand>,
    join: Option<JoinHandle<Result<(), String>>>,
}

impl ObservationWriterHandle {
    fn try_send(
        &self,
        command: ObservationWriterCommand,
    ) -> Result<(), sync_mpsc::TrySendError<ObservationWriterCommand>> {
        self.commands.try_send(command)
    }

    fn shutdown(
        mut self,
        updates: &sync_mpsc::Receiver<ObservationActorUpdate>,
    ) -> Result<Vec<ObservationActorUpdate>, String> {
        let mut drained = Vec::new();
        for command in [ObservationWriterCommand::Flush, ObservationWriterCommand::Close] {
            let mut pending = command;
            loop {
                match self.commands.try_send(pending) {
                    Ok(()) => break,
                    Err(sync_mpsc::TrySendError::Full(command)) => {
                        pending = command;
                        if let Ok(update) = updates.recv_timeout(Duration::from_millis(10)) {
                            drained.push(update);
                        }
                    }
                    Err(sync_mpsc::TrySendError::Disconnected(_)) => {
                        return Err("observation writer stopped during shutdown".to_owned());
                    }
                }
            }
        }
        let join = self.join.take()
            .ok_or_else(|| "observation writer join handle missing".to_owned())?;
        while !join.is_finished() {
            if let Ok(update) = updates.recv_timeout(Duration::from_millis(10)) {
                drained.push(update);
            }
        }
        let result = join.join()
            .map_err(|_| "observation writer panicked".to_owned())?;
        drained.extend(updates.try_iter());
        result.map(|()| drained)
    }
}

fn observation_database_path(preferences_path: Option<&Path>) -> Result<PathBuf, String> {
    let preferences_path = preferences_path
        .ok_or_else(|| "durable observation path is unavailable".to_owned())?;
    let parent = preferences_path
        .parent()
        .ok_or_else(|| "durable observation directory is unavailable".to_owned())?;
    Ok(parent.join("observations.sqlite3"))
}

fn start_observation_writer(
    path: &Path,
) -> Result<(
    ObservationWriterHandle,
    ObservationCommittedSnapshot,
    sync_mpsc::Receiver<ObservationActorUpdate>,
), Box<dyn std::error::Error>> {
    let parent = path.parent()
        .ok_or_else(|| "durable observation directory is unavailable".to_owned())?;
    fs::create_dir_all(parent)?;
    let service = ObservationService::open(path)?;
    let initial = service.committed_snapshot();
    let (command_tx, command_rx) = sync_mpsc::sync_channel(OBSERVATION_COMMAND_QUEUE);
    let (update_tx, update_rx) = sync_mpsc::sync_channel(OBSERVATION_COMMAND_QUEUE);
    let join = thread::Builder::new()
        .name("gate4agent-observation-writer".to_owned())
        .spawn(move || observation_writer_loop(service, command_rx, update_tx))?;
    Ok((ObservationWriterHandle {
        commands: command_tx,
        join: Some(join),
    }, initial, update_rx))
}

fn observation_writer_loop(
    mut service: ObservationService,
    commands: sync_mpsc::Receiver<ObservationWriterCommand>,
    updates: sync_mpsc::SyncSender<ObservationActorUpdate>,
) -> Result<(), String> {
    let mut revision = 0_u64;
    loop {
        let command = match commands.recv() {
            Ok(command) => command,
            Err(_) => {
                service.flush().map_err(|error| error.to_string())?;
                return service.close().map_err(|error| error.to_string());
            }
        };
        let (node_id, incarnation_id, result) = match command {
            ObservationWriterCommand::Ingress(envelope) => {
                let node_id = envelope.node_id.clone();
                let incarnation_id = envelope.cursor.incarnation_id;
                let result = service.apply_ingress(envelope).map(|_| ());
                (Some(node_id), Some(incarnation_id), result)
            }
            ObservationWriterCommand::Resync(batch) => {
                let node_id = batch.node_id.clone();
                let incarnation_id = batch.incarnation_id;
                let result = service.apply_resync(batch).map(|_| ());
                (Some(node_id), Some(incarnation_id), result)
            }
            ObservationWriterCommand::Inventory(inventory) => {
                let node_id = inventory.node_id.clone();
                let incarnation_id = inventory.incarnation_id;
                let result = service.apply_record_inventory(inventory);
                (Some(node_id), Some(incarnation_id), result)
            }
            ObservationWriterCommand::Flush => {
                let result = service.flush();
                (None, None, result)
            }
            ObservationWriterCommand::Close => {
                service.flush().map_err(|error| error.to_string())?;
                return service.close().map_err(|error| error.to_string());
            }
        };
        match result {
            Ok(()) => {
                let (Some(node_id), Some(incarnation_id)) = (node_id, incarnation_id) else {
                    continue;
                };
                revision = revision.checked_add(1)
                    .ok_or_else(|| "observation committed revision exhausted".to_owned())?;
                let route = service.committed_route(&node_id, incarnation_id);
                if updates.send(ObservationActorUpdate::Committed { revision, route }).is_err() {
                    service.flush().map_err(|error| error.to_string())?;
                    return service.close().map_err(|error| error.to_string());
                }
            }
            Err(error) => {
                let update = if service.is_poisoned() {
                    ObservationActorUpdate::Unavailable(error.to_string())
                } else {
                    ObservationActorUpdate::Rejected {
                        message: error.to_string(),
                        node_id,
                        incarnation_id,
                    }
                };
                let _ = updates.send(update);
            }
        }
    }
}

fn initial_viewport_size(cols: u16, rows: u16) -> TerminalSize {
    let sidebar_width = 26.min(cols / 2);
    TerminalSize {
        rows: rows.saturating_sub(1).max(1),
        columns: cols.saturating_sub(sidebar_width).max(1),
    }
}

#[derive(Clone)]
pub struct C2Endpoint {
    pub endpoint: String,
    pub token: String,
}

#[derive(Clone)]
pub struct HarnessOperatorEndpoint {
    pub endpoint: SocketAddr,
    pub credential: HarnessOperatorCredential,
    pub launch_plan_id: Option<HarnessSelectorV1>,
}

#[derive(Clone)]
pub enum StartupMode {
    ManualC2(C2Endpoint),
    Harness(HarnessOperatorEndpoint),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackendSelection {
    c2_worker: bool,
    harness_worker: bool,
    observation_writer: bool,
    direct_node_worker: bool,
}

fn backend_selection(mode: &StartupMode) -> BackendSelection {
    match mode {
        StartupMode::ManualC2(_) => BackendSelection {
            c2_worker: true,
            harness_worker: false,
            observation_writer: true,
            direct_node_worker: false,
        },
        StartupMode::Harness(_) => BackendSelection {
            c2_worker: false,
            harness_worker: true,
            observation_writer: false,
            direct_node_worker: false,
        },
    }
}

#[derive(Clone)]
pub struct StartupRequest {
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub provider: Provider,
}

#[derive(Clone)]
pub struct RunOptions {
    pub mode: StartupMode,
    pub startup: Option<StartupRequest>,
    pub color_mode_override: Option<PtyColorMode>,
}

enum WorkerUpdate {
    State {
        node_id: String,
        endpoint: String,
        state: ConnectionState,
    },
    /// A typed harness SpawnSession succeeded: the app opens (or queues via
    /// `pending_open`) the PTY tab for the returned address — light-mode
    /// spawn parity, where the session panel appears without any manual
    /// roster interaction.
    HarnessSessionSpawned {
        address: SessionAddress,
    },
    RelayRoute {
        node_id: String,
        route: C2RelayRoute,
    },
    TopologyNodeRemoved {
        node_id: String,
    },
    Snapshot {
        expected_node_id: String,
        endpoint: String,
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
        snapshot: NodeSnapshot,
        controller_owned: bool,
        event_sequence: u64,
    },
    C2Snapshot {
        expected_node_id: String,
        endpoint: String,
        incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
        snapshot: C2NodeSnapshot,
        event_sequence: u64,
    },
    C2Event {
        node_id: String,
        cursor: gate4agent_c2_protocol::NodeCursor,
        event: C2EventUpdate,
    },
    ObservationIngress(ObservationIngressEnvelope),
    ObservationResync(ObservationResyncBatch),
    #[cfg(test)]
    Observation {
        node_id: String,
        cursor: gate4agent_c2_protocol::NodeCursor,
        address: SessionAddress,
        observation: ObservationV1,
    },
    EventCursor {
        node_id: String,
        cursor: gate4agent_c2_protocol::NodeCursor,
    },
    ObservationSupport {
        node_id: String,
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
        support: SessionMonitorSupport,
    },
    OpenSession(SessionAddress),
    SelectWorkspace { node_id: String, workspace_id: String },
    WorkspaceUpserted {
        node_id: String,
        workspace: WorkspaceSnapshotUpdate,
    },
    WorkspaceRemoved { node_id: String, workspace_id: String },
    WorkspaceInspected {
        node_id: String,
        inspection: gate4agent_node_protocol::WorkspaceInspection,
    },
    WorkspaceFileRead {
        node_id: String,
        token: u64,
        file: WorkspaceFileRead,
    },
    WorkspaceFileWritten {
        node_id: String,
        token: u64,
        file: WorkspaceFileRead,
    },
    WorkspaceFileCreated {
        node_id: String,
        token: u64,
        file: WorkspaceFileRead,
    },
    WorkspaceDirectoryCreated {
        node_id: String,
        workspace_id: String,
        token: u64,
        entry: WorkspaceEntry,
    },
    WorkspaceEntryCreateFailed {
        node_id: String,
        workspace_id: String,
        path: RepositoryPath,
        kind: WorkspaceEntryKind,
        token: u64,
        message: String,
    },
    WorkspaceFileFailed {
        key: WorkspaceFileTabKey,
        token: u64,
        message: String,
        save: bool,
    },
    GitHistoryRead {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        page: GitHistoryPage,
    },
    GitHistoryFailed {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        message: String,
    },
    GitDiffRead {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        diff: GitDiff,
    },
    GitDiffFailed {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        message: String,
    },
    HostDirectoriesBrowsed {
        node_id: String,
        token: u64,
        append: bool,
        listing: HostDirectoryListing,
    },
    HostDirectoryBrowseFailed {
        node_id: String,
        token: u64,
        message: String,
    },
    SessionRecordUpserted(ManagedSessionView),
    ProviderSessionIndexed {
        record: ManagedSessionView,
        node_id: String,
        workspace_id: String,
        provider: Provider,
        session_id: String,
        identity_matches: bool,
        operation_token: u64,
    },
    NativeSessionIndexed {
        node_id: String,
        route: NativeSessionCatalogRoute,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        selection_id: String,
        record: ManagedSessionView,
        operation_token: u64,
    },
    SessionRecordResumed {
        record: ManagedSessionView,
        session: SessionAddress,
        operation_token: u64,
    },
    ExistingSessionOperationFailed {
        node_id: String,
        record_id: Option<String>,
        indexing: bool,
        operation_token: u64,
        message: String,
        stale_catalog: bool,
    },
    NativeSessionsCataloged {
        node_id: String,
        route: NativeSessionCatalogRoute,
        token: u64,
        entries: Vec<NativeSessionCatalogEntry>,
        summary: Option<NativeSessionCatalogSummary>,
    },
    NativeSessionsPaged {
        node_id: String,
        route: NativeSessionCatalogRoute,
        token: u64,
        page: NativeSessionCatalogPage,
    },
    NativeSessionPageFailed {
        node_id: String,
        route: NativeSessionCatalogRoute,
        window: NativeSessionCatalogWindow,
        token: u64,
        message: String,
        stale_catalog: bool,
    },
    NativeSessionCatalogFailed {
        node_id: String,
        route: NativeSessionCatalogRoute,
        token: u64,
        message: String,
        unavailable: bool,
    },
    NativeSessionPreviewed {
        node_id: String,
        route: NativeSessionCatalogRoute,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        selection_id: String,
        token: u64,
        preview: SessionRecordPreview,
    },
    NativeSessionPreviewFailed {
        node_id: String,
        route: NativeSessionCatalogRoute,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        selection_id: String,
        token: u64,
        message: String,
        unavailable: bool,
        stale_catalog: bool,
    },
    SessionRecordPreviewed {
        node_id: String,
        record_id: String,
        token: u64,
        preview: SessionRecordPreview,
    },
    SessionRecordPreviewFailed {
        node_id: String,
        record_id: String,
        token: u64,
        message: String,
        unavailable: bool,
    },
    SessionRecordHistoryRefreshed {
        node_id: String,
        record_id: String,
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    },
    SessionRecordHistoryRefreshFailed {
        node_id: String,
        record_id: String,
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
        message: String,
    },
    SessionRecordRemoved { node_id: String, record_id: String },
    HistoryDiscovered {
        source: SessionAddress,
        candidates: Vec<gate4agent_types::HistoryCandidateSummary>,
    },
    HistoryLoaded {
        source: SessionAddress,
        session_id: String,
        message_count: u64,
        completed_turn_count: Option<u64>,
    },
    ContextExported(ResolvedContextPackReceipt),
    ContextForgotten(SpawnContextId),
    SpawnReceipt(ResolvedSpawnReceipt),
    ManagedSpawnReceipt(ManagedWorktreeSpawnReceipt),
    ManagedWorktreeLease(ManagedWorktreeLeaseSnapshot),
    ManagedWorktreeRemoved { lease_id: String },
    HarnessSnapshot {
        token: u64,
        tasks: Vec<RedactedTaskV1>,
        runs: Vec<RedactedRunV1>,
        nodes: Vec<NodeView>,
    },
    HarnessRefreshFailed {
        token: u64,
        message: String,
    },
    HarnessMonitor {
        run: RedactedRunV1,
        monitor: HarnessSessionMonitorV1,
        timeline: Vec<TimelineEntryV1>,
    },
    HarnessMonitorFailed {
        run: HarnessRunRef,
        message: String,
    },
    HarnessRunTransfer {
        run: HarnessRunRef,
        token: u64,
        summary: HarnessRunTransferSummaryV1,
    },
    HarnessRunTransferFailed {
        run: HarnessRunRef,
        token: u64,
        message: String,
    },
    HarnessRunContextSourceObserved {
        run: HarnessRunRef,
        task: crate::app::HarnessTaskRef,
        token: u64,
        observation: HarnessRunContextSourceObservationV1,
        launch_options: Result<HarnessTaskLaunchOptionsV1, String>,
    },
    HarnessRunContextSourceObservationFailed {
        run: HarnessRunRef,
        token: u64,
        message: String,
    },
    HarnessTaskCorrelations {
        task_id: gate4agent_harness_client::HarnessTaskId,
        correlations: Vec<HarnessRunCorrelationV1>,
        failures: Vec<(gate4agent_harness_client::HarnessRunId, String)>,
    },
    HarnessTaskObservations {
        task_id: gate4agent_harness_client::HarnessTaskId,
        observations: Vec<(RedactedRunV1, HarnessSessionMonitorV1)>,
        failures: Vec<(HarnessRunRef, String)>,
    },
    HarnessLaunchOptionsLoaded {
        task: crate::app::HarnessTaskRef,
        token: u64,
        options: HarnessTaskLaunchOptionsV1,
    },
    HarnessLaunchOptionsLoadFailed {
        task: crate::app::HarnessTaskRef,
        token: u64,
        message: String,
    },
    HarnessLaunchSpecSaved {
        token: u64,
        task: crate::app::HarnessTaskRef,
        options: HarnessTaskLaunchOptionsV1,
        outcome: HarnessOperatorMutationOutcomeV1,
    },
    HarnessTaskStartedV2 {
        token: u64,
        task: crate::app::HarnessTaskRef,
        outcome: HarnessTaskStartOutcomeV1,
        options: HarnessTaskLaunchOptionsV1,
        transfer: Result<HarnessRunTransferSummaryV1, String>,
    },
    HarnessExecutionMutationFailed {
        token: u64,
        task: crate::app::HarnessTaskRef,
        message: String,
    },
    HarnessWorkspaceInspected {
        run: HarnessRunRef,
        token: u64,
        inspection: HarnessRunWorkspaceInspectionV1,
    },
    HarnessWorkspaceInspectionFailed {
        run: HarnessRunRef,
        token: u64,
        failure: HarnessReadFailure,
    },
    HarnessWorkspaceFileRead {
        key: HarnessWorkspaceFileTabKey,
        token: u64,
        file: HarnessRunWorkspaceFileV1,
    },
    HarnessWorkspaceFileFailed {
        key: HarnessWorkspaceFileTabKey,
        token: u64,
        failure: HarnessReadFailure,
    },
    HarnessGitHistoryRead {
        destination: HarnessWorkspaceGitRequestDestination,
        token: u64,
        page: HarnessRunGitHistoryPageV1,
    },
    HarnessGitHistoryFailed {
        destination: HarnessWorkspaceGitRequestDestination,
        token: u64,
        failure: HarnessReadFailure,
    },
    HarnessGitDiffRead {
        destination: HarnessWorkspaceGitRequestDestination,
        token: u64,
        diff: HarnessRunGitDiffV1,
    },
    HarnessGitDiffFailed {
        destination: HarnessWorkspaceGitRequestDestination,
        token: u64,
        failure: HarnessReadFailure,
    },
    // Node-scoped siblings of the four `Harness*Workspace*`/`HarnessGit*`
    // variants above: the sidebar's harness-mode reads, which land in the
    // same direct-mode state (`workspace_inspections`, `file_tabs`,
    // `git_tabs`) the direct-C2 replies fill.
    // No token: mirrors `AppAction::HarnessInspectNodeWorkspace`, tokenless
    // like direct-mode `InspectWorkspace`.
    HarnessNodeWorkspaceInspected {
        node_id: String,
        workspace_id: String,
        inspection: HarnessNodeWorkspaceInspectionV1,
    },
    HarnessNodeWorkspaceInspectionFailed {
        node_id: String,
        workspace_id: String,
        message: String,
    },
    HarnessNodeWorkspaceFileRead {
        node_id: String,
        token: u64,
        file: HarnessNodeWorkspaceFileV1,
    },
    HarnessNodeWorkspaceFileFailed {
        key: WorkspaceFileTabKey,
        token: u64,
        message: String,
    },
    HarnessNodeGitHistoryRead {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        page: HarnessNodeGitHistoryPageV1,
    },
    HarnessNodeGitHistoryFailed {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        message: String,
    },
    HarnessNodeGitDiffRead {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        diff: HarnessNodeGitDiffV1,
    },
    HarnessNodeGitDiffFailed {
        destination: WorkspaceGitRequestDestination,
        token: u64,
        message: String,
    },
    HarnessReverseAttributionLoaded {
        subject: HarnessReverseAttributionSubjectV1,
        token: u64,
        value: HarnessReverseAttributionV1,
    },
    HarnessReverseAttributionFailed {
        subject: HarnessReverseAttributionSubjectV1,
        token: u64,
        message: String,
    },
    HarnessTerminalRead(HarnessRuntimeTerminalPageV1),
    Notice(String),
}

enum C2EventUpdate {
    TerminalFrame { address: SessionAddress, frame: TerminalFrame },
    SessionRecordUpserted(ManagedSessionView),
    SessionRecordRemoved { record_id: String },
    ManagedWorktreeLease(ManagedWorktreeLeaseSnapshot),
    ManagedWorktreeRemoved { lease_id: String },
    WorkspaceUpserted(gate4agent_c2_protocol::C2WorkspaceSnapshot),
    WorkspaceRemoved { workspace_id: String },
    Notice(String),
    ResyncRequired,
    Ignored,
}

enum WorkspaceSnapshotUpdate {
    Direct(gate4agent_node_protocol::WorkspaceSnapshot),
    C2(gate4agent_c2_protocol::C2WorkspaceSnapshot),
}

#[derive(Default)]
struct C2ApplyState {
    relay_routes: BTreeMap<String, C2RelayRoute>,
    snapshot_watermarks: BTreeMap<String, gate4agent_c2_protocol::NodeCursor>,
    event_watermarks: BTreeMap<String, gate4agent_c2_protocol::NodeCursor>,
    retired_incarnations: BTreeMap<String, BTreeSet<gate4agent_node_protocol::NodeIncarnationId>>,
    terminal_watermarks: BTreeMap<SessionAddress, u64>,
    initial_observation_resyncs: BTreeSet<(String, gate4agent_node_protocol::NodeIncarnationId)>,
    pending_observation_inventories: BTreeMap<
        (String, gate4agent_node_protocol::NodeIncarnationId),
        ObservationRecordInventory,
    >,
    observation_writer: Option<ObservationWriterHandle>,
}

enum C2EventAdmission {
    Rejected,
    Accepted { gap_after: Option<u64> },
}

/// How much of a periodic snapshot may be applied. A snapshot lagging the
/// per-event watermark still carries the only wire copy of inventory-shaped
/// state (launch inventory, workspaces, capabilities) — starving those until
/// a snapshot outruns a live event stream would starve them forever — but it
/// must not regress state that events advance (session records, the displayed
/// event sequence, terminal watermarks).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotAdmission {
    Authoritative,
    InventoryOnly,
    Rejected,
}

impl C2ApplyState {
    fn accept_snapshot(
        &mut self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> SnapshotAdmission {
        if let Some(current) = self.snapshot_watermarks.get(node_id).copied() {
            if current.incarnation_id == cursor.incarnation_id {
                if cursor.sequence < current.sequence {
                    return SnapshotAdmission::Rejected;
                }
            } else {
                if self.retired_incarnations.get(node_id).is_some_and(|retired| {
                    retired.contains(&cursor.incarnation_id)
                }) {
                    return SnapshotAdmission::Rejected;
                }
                self.retired_incarnations
                    .entry(node_id.to_owned())
                    .or_default()
                    .insert(current.incarnation_id);
            }
        }
        let event_current = self.event_watermarks.get(node_id).copied();
        if let Some(previous) = event_current
            .map(|current| current.incarnation_id)
            .filter(|previous| *previous != cursor.incarnation_id)
        {
            self.retired_incarnations
                .entry(node_id.to_owned())
                .or_default()
                .insert(previous);
            self.event_watermarks.remove(node_id);
        }
        self.snapshot_watermarks.insert(node_id.to_owned(), cursor);
        // A later per-event update can already have advanced past this
        // snapshot's watermark (events and snapshots ride independent
        // pipelines, and a live terminal stream outruns every periodic
        // snapshot); such a snapshot still delivers inventory but must not
        // regress event-established state.
        if event_current.is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence < current.sequence
        }) {
            return SnapshotAdmission::InventoryOnly;
        }
        SnapshotAdmission::Authoritative
    }

    fn accept_event(
        &mut self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> C2EventAdmission {
        if self.retired_incarnations.get(node_id).is_some_and(|retired| {
            retired.contains(&cursor.incarnation_id)
        }) {
            return C2EventAdmission::Rejected;
        }
        if self.snapshot_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id != cursor.incarnation_id
        }) {
            return C2EventAdmission::Rejected;
        }
        if self.event_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence <= current.sequence
        }) {
            return C2EventAdmission::Rejected;
        }
        let gap_after = self.event_watermarks.get(node_id).and_then(|current| {
            (current.incarnation_id == cursor.incarnation_id
                && cursor.sequence > current.sequence.saturating_add(1))
                .then_some(current.sequence)
        });
        self.event_watermarks.insert(node_id.to_owned(), cursor);
        C2EventAdmission::Accepted { gap_after }
    }

    /// A session-record/terminal-frame event that is at or below the most
    /// recently accepted snapshot's own watermark is a duplicate (or older)
    /// relative to that snapshot and must not regress the state it
    /// established. Deliberately not folded into `accept_event`: observations
    /// share that function as a fallback and are allowed to trail a snapshot's
    /// `event_sequence` (they are a separate, independently queued stream).
    fn event_is_stale_relative_to_snapshot(
        &self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> bool {
        self.snapshot_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence <= current.sequence
        })
    }

    fn accept_observation(
        &mut self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> C2EventAdmission {
        if self.retired_incarnations.get(node_id).is_some_and(|retired| {
            retired.contains(&cursor.incarnation_id)
        }) {
            return C2EventAdmission::Rejected;
        }
        if self.snapshot_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id != cursor.incarnation_id
        }) {
            return C2EventAdmission::Rejected;
        }
        if self.event_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence < current.sequence
        }) {
            return C2EventAdmission::Rejected;
        }
        if self.event_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence == current.sequence
        }) {
            return C2EventAdmission::Accepted { gap_after: None };
        }
        self.accept_event(node_id, cursor)
    }

    fn observation_resync_is_admissible(
        &self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> bool {
        if self.retired_incarnations.get(node_id).is_some_and(|retired| {
            retired.contains(&cursor.incarnation_id)
        }) {
            return false;
        }
        if self.snapshot_watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id != cursor.incarnation_id
        }) {
            return false;
        }
        self.event_watermarks.get(node_id).is_none_or(|current| {
            current.incarnation_id == cursor.incarnation_id
                && cursor.sequence >= current.sequence
        })
    }

    fn record_observation_resync(
        &mut self,
        node_id: String,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) {
        self.event_watermarks.insert(node_id, cursor);
    }

    fn rebuild_terminal_watermarks(&mut self, node_id: &str, snapshot: &C2NodeSnapshot) {
        self.terminal_watermarks
            .retain(|address, _| address.node_id != node_id);
        for workspace in &snapshot.workspaces {
            for session in &workspace.sessions {
                let Some(frame) = session.terminal_frame.as_ref() else { continue; };
                self.terminal_watermarks.insert(SessionAddress {
                    node_id: node_id.to_owned(),
                    workspace_id: workspace.workspace_id.to_string(),
                    instance_id: session.instance_id.0,
                    generation: session.generation.0,
                }, frame.sequence);
            }
        }
    }

    fn terminal_frame_is_new(&self, address: &SessionAddress, sequence: u64) -> bool {
        self.terminal_watermarks
            .get(address)
            .is_none_or(|current| sequence > *current)
    }

    fn record_terminal_frame(&mut self, address: SessionAddress, sequence: u64) {
        self.terminal_watermarks.insert(address, sequence);
    }

    fn terminal_watermark(&self, address: &SessionAddress) -> Option<u64> {
        self.terminal_watermarks.get(address).copied()
    }

    fn reset_terminal_watermarks(&mut self, node_id: &str) {
        self.terminal_watermarks
            .retain(|address, _| address.node_id != node_id);
    }
}

struct PendingRaw {
    address: SessionAddress,
    text: String,
    updated_at: Instant,
}

/// Decouples terminal polling from full-screen rendering.  The event loop still
/// polls often enough to keep Node/C2 updates and PTY input responsive, but a
/// poll with no visible state change no longer writes the complete frame.
struct FrameScheduler {
    dirty: bool,
    next_dirty_frame: Instant,
    next_animation: Option<Instant>,
}

impl FrameScheduler {
    fn new(now: Instant) -> Self {
        Self {
            dirty: true,
            next_dirty_frame: now,
            next_animation: None,
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn redraw_due(&self, now: Instant, animation_active: bool) -> bool {
        (self.dirty && now >= self.next_dirty_frame)
            || (animation_active && self.next_animation.is_some_and(|deadline| now >= deadline))
    }

    fn consume_redraw(&mut self, now: Instant, animation_active: bool) -> bool {
        let advance_animation = animation_active
            && self.next_animation.is_some_and(|deadline| now >= deadline);
        self.dirty = false;
        self.next_dirty_frame = now + DIRTY_FRAME_INTERVAL;
        self.next_animation = if animation_active {
            Some(if advance_animation {
                now + ANIMATION_FRAME_INTERVAL
            } else {
                self.next_animation.unwrap_or(now + ANIMATION_FRAME_INTERVAL)
            })
        } else {
            None
        };
        advance_animation
    }

    fn poll_timeout(
        &self,
        now: Instant,
        animation_active: bool,
        deadlines: &[Option<Instant>],
    ) -> Duration {
        let mut timeout = EVENT_POLL_INTERVAL;
        if self.dirty {
            timeout = timeout.min(self.next_dirty_frame.saturating_duration_since(now));
        }
        if animation_active {
            if let Some(deadline) = self.next_animation {
                timeout = timeout.min(deadline.saturating_duration_since(now));
            }
        }
        deadlines.iter().flatten().fold(timeout, |timeout, deadline| {
            timeout.min(deadline.saturating_duration_since(now))
        })
    }
}

fn is_coalescible_drag(event: &TerminalEvent) -> bool {
    matches!(event, TerminalEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        ..
    }))
}

/// Keep only the latest coordinate from each uninterrupted left-drag run.  A
/// non-drag event flushes the pending drag first, so Down/Up and keys retain
/// their original ordering.
fn coalesce_mouse_drags(events: impl IntoIterator<Item = TerminalEvent>) -> Vec<TerminalEvent> {
    let mut coalesced = Vec::new();
    let mut latest_drag = None;
    for event in events {
        if is_coalescible_drag(&event) {
            latest_drag = Some(event);
            continue;
        }
        if let Some(drag) = latest_drag.take() {
            coalesced.push(drag);
        }
        coalesced.push(event);
    }
    if let Some(drag) = latest_drag {
        coalesced.push(drag);
    }
    coalesced
}

fn inspection_poll_deadline(
    selected_route_present: bool,
    inspection_visible: bool,
    inspection_pending: bool,
    next_inspection: Instant,
) -> Option<Instant> {
    (selected_route_present && inspection_visible && !inspection_pending).then_some(next_inspection)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub async fn run(options: RunOptions) -> Result<(), Box<dyn std::error::Error>> {
    let RunOptions { mode, startup, color_mode_override } = options;
    if matches!(mode, StartupMode::Harness(_)) && startup.is_some() {
        return Err("Harness mode cannot receive a C2/Node startup request".into());
    }
    let preferences_path = preferences::default_path();
    let loaded_preferences = preferences_path.as_deref().and_then(|path| match UiPreferences::load(path) {
        Ok(preferences) => Some(preferences),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            crate::diagnostics::record_runtime(RuntimeDiagnostic::PreferencesLoadFailed);
            None
        }
    });
    let initial_preferences = loaded_preferences.clone().unwrap_or_default();
    let _guard = TerminalGuard::enter()?;
    let (cols, rows) = terminal::size()?;
    let backend = CrosstermBackend::new(stdout());
    let mut screen = Screen::new(backend, cols, rows);
    screen.backend_mut().hide_cursor()?;

    let (updates_tx, mut updates_rx) = mpsc::channel(UPDATE_QUEUE);
    let mut commands = BTreeMap::new();
    let inspection_commands = BTreeMap::new();
    let mut app = App::default();
    let mut c2_apply_state = C2ApplyState::default();
    let mut observation_updates = None;
    let selected_backend = backend_selection(&mode);
    if selected_backend.observation_writer {
        let observation_path = observation_database_path(preferences_path.as_deref())
            .map_err(|message| io::Error::new(io::ErrorKind::NotFound, message))?;
        let (observation_writer, observation_open_snapshot, updates) =
            start_observation_writer(&observation_path)?;
        app.apply_observation_open_snapshot(
            observation_open_snapshot.projections,
            observation_open_snapshot.durable_resume_cursors,
        );
        c2_apply_state.observation_writer = Some(observation_writer);
        observation_updates = Some(updates);
    }
    initial_preferences.apply_to(&mut app);
    if let Some(color_mode) = color_mode_override {
        app.color_mode = color_mode;
    }
    app.terminal_cols = cols;
    app.terminal_rows = rows;
    match mode {
        StartupMode::ManualC2(endpoint) => {
            let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
            commands.insert(C2_COMMAND_ROUTE.to_owned(), command_tx);
            tokio::spawn(c2_worker(
                endpoint,
                startup,
                initial_viewport_size(cols, rows),
                command_rx,
                updates_tx.clone(),
            ));
        }
        StartupMode::Harness(endpoint) => {
            app.enable_harness_schedule(endpoint.launch_plan_id.clone());
            let client = HarnessOperatorClient::new(endpoint.endpoint, endpoint.credential)?;
            let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
            let (history_tx, history_rx) = mpsc::channel(COMMAND_QUEUE);
            let (detail_tx, detail_rx) = mpsc::channel(COMMAND_QUEUE);
            commands.insert(HARNESS_COMMAND_ROUTE.to_owned(), command_tx.clone());
            commands.insert(HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx);
            commands.insert(HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx);
            let runtime_inventory = Arc::new(Mutex::new(None));
            let harness_updates = updates_tx.clone();
            let operator_inventory = runtime_inventory.clone();
            let history_client = client.clone();
            let detail_client = client.clone();
            tokio::task::spawn_blocking(move || {
                harness_operator_worker(client, command_rx, harness_updates, operator_inventory)
            });
            let detail_updates = updates_tx.clone();
            tokio::task::spawn_blocking(move || {
                harness_detail_worker(detail_client, detail_rx, detail_updates)
            });
            let history_updates = updates_tx.clone();
            tokio::task::spawn_blocking(move || {
                harness_native_history_worker(
                    history_client,
                    history_rx,
                    history_updates,
                    runtime_inventory,
                )
            });
            let action = app.enable_harness_kanban();
            if command_tx.try_send(action).is_err() {
                return Err("Harness operator command queue rejected initial refresh".into());
            }
        }
    }
    drop(updates_tx);

    let mut pending_raw: Option<PendingRaw> = None;
    let mut last_terminal_sizes = BTreeMap::new();
    let mut last_notice = None;
    let mut notice_deadline = None;
    let mut auto_inspected_route = None;
    let mut next_auto_inspection = Instant::now();
    let mut harness_terminal_poll_address: Option<SessionAddress> = None;
    let mut next_harness_terminal_poll = Instant::now();
    let mut next_harness_snapshot_refresh = Instant::now();
    let mut preferred_color_mode = initial_preferences.color_mode;
    let mut observed_app_color_mode = app.color_mode;
    let mut observed_preferences = preferences_for_save(&app, preferred_color_mode);
    let mut persisted_preferences = loaded_preferences;
    let mut preferences_deadline = None;
    let mut frames = FrameScheduler::new(Instant::now());
    while !app.should_quit {
        let mut state_changed = false;
        if let Some(updates) = observation_updates.as_mut() {
            while let Ok(update) = updates.try_recv() {
                let action = apply_observation_actor_update(&mut app, update);
                queue_action(
                    &mut app,
                    &commands,
                    &inspection_commands,
                    &mut pending_raw,
                    action,
                );
                state_changed = true;
            }
        }
        while let Ok(update) = updates_rx.try_recv() {
            let action = apply_update(&mut app, &mut c2_apply_state, update);
            queue_action(
                &mut app,
                &commands,
                &inspection_commands,
                &mut pending_raw,
                action,
            );
            state_changed = true;
        }
        retry_pending_observation_inventories(&mut app, &mut c2_apply_state);
        let selected_route = app.selected_workspace_route();
        if selected_route != auto_inspected_route {
            auto_inspected_route = selected_route.clone();
            next_auto_inspection = Instant::now();
        }
        let now = Instant::now();
        // The harness-only backend serves workspace reads through the V9
        // node-scoped operator family — the periodic auto-inspection must
        // fire there too, or the Files/Git sidebar never leaves
        // "loading workspace...".
        if (selected_backend.c2_worker || selected_backend.harness_worker)
            && selected_route.is_some()
            && app.workspace_inspection_visible()
            && !app.workspace_inspection_pending()
            && now >= next_auto_inspection
        {
            let action = app.inspect_selected_workspace();
            queue_action(
                &mut app,
                &commands,
                &inspection_commands,
                &mut pending_raw,
                action,
            );
            next_auto_inspection = now + INSPECTION_REFRESH_INTERVAL;
            state_changed = true;
        }
        if selected_backend.harness_worker && now >= next_harness_snapshot_refresh {
            let action = app.request_harness_refresh();
            if !matches!(action, AppAction::None) {
                queue_action(
                    &mut app,
                    &commands,
                    &inspection_commands,
                    &mut pending_raw,
                    action,
                );
                state_changed = true;
            }
            next_harness_snapshot_refresh = now + HARNESS_SNAPSHOT_REFRESH_INTERVAL;
        }
        let focused_harness_terminal = if selected_backend.harness_worker {
            app.focused_address().cloned()
        } else {
            None
        };
        if focused_harness_terminal != harness_terminal_poll_address {
            harness_terminal_poll_address = focused_harness_terminal.clone();
            next_harness_terminal_poll = now;
        }
        if let Some(address) = focused_harness_terminal.filter(|_| now >= next_harness_terminal_poll) {
            next_harness_terminal_poll = now + HARNESS_TERMINAL_POLL_INTERVAL;
            if let Some(incarnation_id) = app.nodes.iter()
                .find(|node| node.node_id == address.node_id)
                .and_then(|node| node.incarnation_id)
            {
                let action = AppAction::HarnessOpenTerminal {
                    session: harness_terminal_session_address(&address, incarnation_id),
                    after_sequence: c2_apply_state.terminal_watermark(&address),
                };
                queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
            }
        }
        if app.notice != last_notice {
            last_notice = app.notice.clone();
            notice_deadline = app.notice.as_ref().map(|_| Instant::now() + Duration::from_secs(3));
            state_changed = true;
        } else if notice_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            app.notice = None;
            last_notice = None;
            notice_deadline = None;
            state_changed = true;
        }
        if app.color_mode != observed_app_color_mode {
            observed_app_color_mode = app.color_mode;
            preferred_color_mode = app.color_mode;
        }
        let current_preferences = preferences_for_save(&app, preferred_color_mode);
        if current_preferences != observed_preferences {
            observed_preferences = current_preferences;
            preferences_deadline = Some(Instant::now() + PREFERENCES_SAVE_DEBOUNCE);
        }
        if preferences_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            if persisted_preferences.as_ref() != Some(&observed_preferences) {
                if let Some(path) = preferences_path.as_deref() {
                    match observed_preferences.save(path) {
                        Ok(()) => persisted_preferences = Some(observed_preferences.clone()),
                        Err(_) => crate::diagnostics::record_runtime(
                            RuntimeDiagnostic::PreferencesSaveFailed,
                        ),
                    }
                }
            }
            preferences_deadline = None;
        }

        if state_changed {
            frames.mark_dirty();
        }
        let now = Instant::now();
        let animation_active = app.has_active_animation();
        if frames.redraw_due(now, animation_active) {
            if frames.consume_redraw(now, animation_active) {
                for _ in 0..ANIMATION_TICKS_PER_FRAME {
                    app.advance_animation_frame();
                }
            }
            app.layout = render::render(&app, screen.buffer_mut());
            for action in changed_terminal_sizes(&app, &mut last_terminal_sizes) {
                queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
            }
            screen.flush()?;
            sync_cursor(&app)?;
        }

        if pending_raw
            .as_ref()
            .is_some_and(|pending| pending.updated_at.elapsed() >= RAW_INPUT_COALESCE)
        {
            flush_raw(&mut app, &commands, &mut pending_raw);
        }

        let raw_deadline = pending_raw
            .as_ref()
            .map(|pending| pending.updated_at + RAW_INPUT_COALESCE);
        let inspection_deadline = inspection_poll_deadline(
            selected_backend.c2_worker && selected_route.is_some(),
            app.workspace_inspection_visible(),
            app.workspace_inspection_pending(),
            next_auto_inspection,
        );
        let harness_terminal_deadline = harness_terminal_poll_address
            .is_some()
            .then_some(next_harness_terminal_poll);
        let poll_timeout = frames.poll_timeout(
            Instant::now(),
            app.has_active_animation(),
            &[
                notice_deadline,
                preferences_deadline,
                raw_deadline,
                inspection_deadline,
                harness_terminal_deadline,
            ],
        );
        if event::poll(poll_timeout)? {
            let mut terminal_events = vec![event::read()?];
            while terminal_events.len() < MAX_TERMINAL_EVENT_BATCH
                && event::poll(Duration::ZERO)?
            {
                terminal_events.push(event::read()?);
            }
            for terminal_event in coalesce_mouse_drags(terminal_events) {
                match terminal_event {
                TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(key) = map_key(key) {
                        let action = app.reduce(key);
                        queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
                    }
                    frames.mark_dirty();
                }
                TerminalEvent::Mouse(mouse) => {
                    let action = map_mouse(&mut app, mouse);
                    queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
                    frames.mark_dirty();
                }
                TerminalEvent::Paste(text) => {
                    let action = app.paste(text);
                    queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
                    frames.mark_dirty();
                }
                TerminalEvent::Resize(cols, rows) => {
                    screen.resize(cols, rows);
                    app.terminal_cols = cols;
                    app.terminal_rows = rows;
                    frames.mark_dirty();
                }
                _ => {}
                }
            }
        }
    }
    if let (Some(writer), Some(updates)) = (
        c2_apply_state.observation_writer.take(),
        observation_updates.as_ref(),
    ) {
        for update in writer.shutdown(updates).map_err(io::Error::other)? {
            let _ = apply_observation_actor_update(&mut app, update);
        }
    }
    let final_preferences = preferences_for_save(&app, preferred_color_mode);
    if persisted_preferences.as_ref() != Some(&final_preferences) {
        if let Some(path) = preferences_path.as_deref() {
            if final_preferences.save(path).is_err() {
                crate::diagnostics::record_runtime(RuntimeDiagnostic::PreferencesSaveFailed);
            }
        }
    }
    flush_raw(&mut app, &commands, &mut pending_raw);
    screen.backend_mut().show_cursor()?;
    Ok(())
}

fn preferences_for_save(app: &App, color_mode: PtyColorMode) -> UiPreferences {
    let mut preferences = UiPreferences::from_app(app);
    preferences.color_mode = color_mode;
    preferences
}

fn queue_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    inspection_commands: &BTreeMap<String, watch::Sender<Option<WorkspaceId>>>,
    pending_raw: &mut Option<PendingRaw>,
    action: AppAction,
) {
    if let AppAction::Input { address, text } = action {
        if let Some(pending) = pending_raw.as_mut() {
            if pending.address == address && pending.text.len() + text.len() <= 16 * 1024 {
                pending.text.push_str(&text);
                pending.updated_at = Instant::now();
                return;
            }
        }
        flush_raw(app, commands, pending_raw);
        *pending_raw = Some(PendingRaw {
            address,
            text,
            updated_at: Instant::now(),
        });
        return;
    }
    flush_raw(app, commands, pending_raw);
    send_action(app, commands, inspection_commands, action);
}

fn flush_raw(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    pending_raw: &mut Option<PendingRaw>,
) {
    let Some(pending) = pending_raw.take() else {
        return;
    };
    send_operator_action(
        app,
        commands,
        AppAction::Input {
            address: pending.address,
            text: pending.text,
        },
    );
}

fn send_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    inspection_commands: &BTreeMap<String, watch::Sender<Option<WorkspaceId>>>,
    action: AppAction,
) {
    if let AppAction::InspectWorkspace { node_id, workspace_id } = &action {
        if let Some(sender) = inspection_commands.get(node_id) {
            let Ok(workspace_id_value) = WorkspaceId::new(workspace_id.clone()) else {
                app.fail_workspace_inspection(
                    node_id.clone(),
                    workspace_id.clone(),
                    "invalid workspace ID".to_owned(),
                );
                return;
            };
            sender.send_replace(Some(workspace_id_value));
            return;
        }
    }
    send_operator_action(app, commands, action);
}

/// Rewrites a direct-shaped workspace-read action into its harness-routed,
/// node-scoped sibling when the session has no direct node connection
/// (harness-operator mode: `harness_only`). The App methods that build
/// `InspectWorkspace`/`ReadWorkspaceFile`/`ReadGitHistory`/`ReadGitDiff` stay
/// mode-agnostic and keep populating the same direct-mode state
/// (`inspection_pending`, `file_tabs`, `git_tabs`) either way — only the
/// wire-level action this dispatch boundary sends onward changes. Applied
/// once, here, rather than at each of the several call sites that build
/// these four actions (sidebar open, pagination, diff selection).
fn harness_route_workspace_read(harness_only: bool, action: AppAction) -> AppAction {
    if !harness_only {
        return action;
    }
    match action {
        AppAction::InspectWorkspace { node_id, workspace_id } => {
            AppAction::HarnessInspectNodeWorkspace { node_id, workspace_id }
        }
        AppAction::ReadWorkspaceFile { node_id, workspace_id, path, token } => {
            AppAction::HarnessReadNodeWorkspaceFile { node_id, workspace_id, path, token }
        }
        AppAction::ReadGitHistory { node_id, workspace_id, path, before, limit, token, destination } => {
            AppAction::HarnessReadNodeGitHistory {
                node_id, workspace_id, path, before, limit, token, destination,
            }
        }
        AppAction::ReadGitDiff { node_id, workspace_id, target, token, destination } => {
            AppAction::HarnessReadNodeGitDiff { node_id, workspace_id, target, token, destination }
        }
        other => other,
    }
}

fn send_operator_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    action: AppAction,
) {
    let harness_only = commands.contains_key(HARNESS_COMMAND_ROUTE)
        && commands.contains_key(HARNESS_HISTORY_COMMAND_ROUTE)
        && commands.contains_key(HARNESS_DETAIL_COMMAND_ROUTE)
        && !commands.contains_key(C2_COMMAND_ROUTE)
        && commands.len() == 3;
    let action = harness_route_workspace_read(harness_only, action);
    let action = app.route_harness_session_verb(harness_only, action);
    let Some(node_id) = action_node_id(&action).map(str::to_owned) else {
        return;
    };
    let harness_native_history_read = harness_only && harness_native_history_read_action(&action);
    let harness_detail_read = harness_only && harness_detail_read_action(&action);
    if harness_only && node_id != HARNESS_COMMAND_ROUTE
        && !harness_native_history_read
        && !harness_detail_read
    {
        if !reject_history_refresh_action(app, &action, "Harness-owned session action unavailable")
            && !reject_workspace_write_action(app, &action, "Harness-owned session action unavailable")
        {
            app.notice = Some(
                "Harness-owned session action unavailable: no typed Harness intent exists"
                    .to_owned(),
            );
        }
        return;
    }
    let sender = if harness_detail_read {
        commands.get(HARNESS_DETAIL_COMMAND_ROUTE)
    } else if harness_native_history_read {
        commands.get(HARNESS_HISTORY_COMMAND_ROUTE)
    } else if node_id == HARNESS_COMMAND_ROUTE {
        commands.get(HARNESS_COMMAND_ROUTE)
    } else {
        commands.get(&node_id).or_else(|| commands.get(C2_COMMAND_ROUTE))
    };
    let Some(sender) = sender
    else {
        if reject_harness_queue_action(app, &action, HarnessQueueRejection::Unavailable) {
            return;
        }
        if !reject_history_refresh_action(app, &action, "node unavailable") {
            app.notice = Some(format!("unknown node {node_id}"));
        }
        return;
    };
    match sender.try_send(action) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(action)) => {
            if reject_harness_queue_action(app, &action, HarnessQueueRejection::Busy) {
                return;
            }
            if !reject_history_refresh_action(app, &action, "command queue busy") {
                app.notice = Some(format!("{node_id}: command queue busy"));
            }
        }
        Err(mpsc::error::TrySendError::Closed(action)) => {
            if reject_harness_queue_action(app, &action, HarnessQueueRejection::Unavailable) {
                return;
            }
            if !reject_history_refresh_action(app, &action, "command queue unavailable") {
                app.notice = Some(format!("{node_id}: command queue unavailable"));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HarnessQueueRejection {
    Busy,
    Unavailable,
}

fn reject_harness_queue_action(
    app: &mut App,
    action: &AppAction,
    rejection: HarnessQueueRejection,
) -> bool {
    let detail_failure = || HarnessReadFailure {
        category: match rejection {
            HarnessQueueRejection::Busy => "busy",
            HarnessQueueRejection::Unavailable => "unavailable",
        }.to_owned(),
        message: match rejection {
            HarnessQueueRejection::Busy => "Harness detail/read command queue is full",
            HarnessQueueRejection::Unavailable => "Harness detail/read command queue is closed",
        }.to_owned(),
    };
    match action {
        AppAction::HarnessInspectWorkspace { run, token } => {
            app.fail_harness_workspace_inspection(run, *token, detail_failure());
            return true;
        }
        AppAction::HarnessReadWorkspaceFile { origin, path, token } => {
            app.fail_harness_workspace_file(
                &HarnessWorkspaceFileTabKey { origin: origin.clone(), path: path.clone() },
                *token,
                detail_failure(),
            );
            return true;
        }
        AppAction::HarnessReadGitHistory { destination, token, .. } => {
            app.fail_harness_git_history(destination, *token, detail_failure());
            return true;
        }
        AppAction::HarnessReadGitDiff { destination, token, .. } => {
            app.fail_harness_git_diff(destination, *token, detail_failure());
            return true;
        }
        AppAction::HarnessInspectNodeWorkspace { node_id, workspace_id, .. } => {
            app.fail_workspace_inspection(
                node_id.clone(),
                workspace_id.clone(),
                detail_failure().display(),
            );
            return true;
        }
        AppAction::HarnessReadNodeWorkspaceFile { node_id, workspace_id, path, token } => {
            app.fail_workspace_file(
                &WorkspaceFileTabKey {
                    node_id: node_id.clone(),
                    workspace_id: workspace_id.clone(),
                    path: path.clone(),
                },
                *token,
                detail_failure().display(),
                false,
            );
            return true;
        }
        AppAction::HarnessReadNodeGitHistory { destination, token, .. } => {
            app.fail_git_history(destination, *token, detail_failure().display());
            return true;
        }
        AppAction::HarnessReadNodeGitDiff { destination, token, .. } => {
            app.fail_git_diff(destination, *token, detail_failure().display());
            return true;
        }
        AppAction::HarnessLoadRunTransfer { run, token } => {
            let message = match rejection {
                HarnessQueueRejection::Busy => {
                    "Harness operator busy: transfer command queue is full"
                }
                HarnessQueueRejection::Unavailable => {
                    "Harness operator unavailable: transfer command queue is closed"
                }
            }.to_owned();
            app.fail_harness_run_transfer(run, *token, message);
            return true;
        }
        AppAction::HarnessObserveRunContextSource { run, token, .. } => {
            let message = match rejection {
                HarnessQueueRejection::Busy => {
                    "Harness operator busy: context-observation queue is full"
                }
                HarnessQueueRejection::Unavailable => {
                    "Harness operator unavailable: context-observation queue is closed"
                }
            }.to_owned();
            app.fail_harness_context_source_observation(run, *token, message);
            return true;
        }
        AppAction::HarnessLoadTaskLaunchOptions { task, token } => {
            let message = match rejection {
                HarnessQueueRejection::Busy => {
                    "Harness operator busy: launch-options queue is full"
                }
                HarnessQueueRejection::Unavailable => {
                    "Harness operator unavailable: launch-options queue is closed"
                }
            }.to_owned();
            app.fail_harness_launch_options(task, *token, message);
            return true;
        }
        AppAction::HarnessLoadReverseAttribution { subject, token } => {
            let message = match rejection {
                HarnessQueueRejection::Busy => {
                    "Harness operator busy: reverse-attribution queue is full"
                }
                HarnessQueueRejection::Unavailable => {
                    "Harness operator unavailable: reverse-attribution queue is closed"
                }
            }.to_owned();
            app.fail_harness_reverse_attribution(subject, *token, message);
            return true;
        }
        _ => {}
    }
    if let AppAction::HarnessOpenMonitor { run } = action {
        let message = match rejection {
            HarnessQueueRejection::Busy => {
                "Harness operator busy: detail command queue is full"
            }
            HarnessQueueRejection::Unavailable => {
                "Harness operator unavailable: detail command queue is closed"
            }
        }.to_owned();
        app.fail_harness_monitor(run, message.clone());
        app.notice = Some(message);
        return true;
    }
    if let AppAction::HarnessLoadTaskCorrelations { task, launch_token, .. } = action {
        let message = match rejection {
            HarnessQueueRejection::Busy => {
                "Harness operator busy: correlation command queue is full"
            }
            HarnessQueueRejection::Unavailable => {
                "Harness operator unavailable: correlation command queue is closed"
            }
        }.to_owned();
        app.fail_harness_task_correlations(&task.task_id, message.clone());
        app.fail_harness_task_observations(&task.task_id, message.clone());
        app.fail_harness_launch_options(task, *launch_token, message.clone());
        app.notice = Some(message);
        return true;
    }
    if let AppAction::HarnessSaveTaskLaunchSpec { token, task, .. }
        | AppAction::HarnessStartTaskV2 { token, task, .. } = action
    {
        let message = match rejection {
            HarnessQueueRejection::Busy => {
                "Harness operator busy: execution mutation queue is full"
            }
            HarnessQueueRejection::Unavailable => {
                "Harness operator unavailable: execution mutation queue is closed"
            }
        }.to_owned();
        app.fail_harness_execution_mutation(*token, task, message);
        return true;
    }
    let token = match action {
        AppAction::HarnessRefresh { token }
        | AppAction::HarnessCreateTask { token, .. }
        | AppAction::HarnessMoveTask { token, .. }
        | AppAction::HarnessCancelTask { token, .. }
        | AppAction::HarnessRetryTask { token, .. }
        | AppAction::HarnessScheduleNext { token, .. } => *token,
        _ => return false,
    };
    if app.rollback_harness_refresh(token) {
        if let AppAction::HarnessCreateTask { title, body, .. } = action {
            app.restore_harness_composer(title.clone(), body.clone());
        }
    }
    app.notice = Some(match rejection {
        HarnessQueueRejection::Busy => "Harness operator busy: command queue is full",
        HarnessQueueRejection::Unavailable => {
            "Harness operator unavailable: command queue is closed"
        }
    }.to_owned());
    true
}

fn harness_native_history_read_action(action: &AppAction) -> bool {
    matches!(
        action,
        AppAction::CatalogNativeSessions { .. }
            | AppAction::PageNativeSessions { .. }
            | AppAction::PreviewNativeSession { .. }
    )
}

fn harness_detail_read_action(action: &AppAction) -> bool {
    matches!(
        action,
        AppAction::HarnessOpenMonitor { .. }
            | AppAction::HarnessOpenTerminal { .. }
            | AppAction::HarnessLoadTaskCorrelations { .. }
            | AppAction::HarnessLoadTaskLaunchOptions { .. }
            | AppAction::HarnessLoadRunTransfer { .. }
            | AppAction::HarnessObserveRunContextSource { .. }
            | AppAction::HarnessInspectWorkspace { .. }
            | AppAction::HarnessReadWorkspaceFile { .. }
            | AppAction::HarnessReadGitHistory { .. }
            | AppAction::HarnessReadGitDiff { .. }
            | AppAction::HarnessLoadReverseAttribution { .. }
            | AppAction::HarnessInspectNodeWorkspace { .. }
            | AppAction::HarnessReadNodeWorkspaceFile { .. }
            | AppAction::HarnessReadNodeGitHistory { .. }
            | AppAction::HarnessReadNodeGitDiff { .. }
    )
}

fn reject_history_refresh_action(app: &mut App, action: &AppAction, reason: &str) -> bool {
    let AppAction::RefreshSessionRecordHistory {
        node_id,
        node_incarnation_id,
        record_id,
        ..
    } = action else {
        return false;
    };
    app.fail_session_record_history_refresh(
        node_id.clone(),
        record_id.clone(),
        *node_incarnation_id,
        reason.to_owned(),
    );
    true
}

/// Honest-notice sibling of `reject_history_refresh_action` for the
/// sidebar's write affordances (save, create file/directory), which stay
/// direct-mode-only — harness mode has no typed Harness intent for them.
/// Without this, the optimistic "saving"/"creating" state the direct-mode
/// key handler already set (`TextEditor::mark_saving`,
/// `CreateWorkspaceEntryDialog::pending`) would never clear in harness mode:
/// a silent hang rather than the honest notice the generic rejection below
/// already gives every other untranslatable action.
fn reject_workspace_write_action(app: &mut App, action: &AppAction, reason: &str) -> bool {
    match action {
        AppAction::WriteWorkspaceFile { node_id, workspace_id, path, token, .. } => {
            app.fail_workspace_file(
                &WorkspaceFileTabKey {
                    node_id: node_id.clone(),
                    workspace_id: workspace_id.clone(),
                    path: path.clone(),
                },
                *token,
                reason.to_owned(),
                true,
            );
            true
        }
        AppAction::CreateWorkspaceFile { node_id, workspace_id, path, token }
        | AppAction::CreateWorkspaceDirectory { node_id, workspace_id, path, token } => {
            let kind = if matches!(action, AppAction::CreateWorkspaceFile { .. }) {
                WorkspaceEntryKind::File
            } else {
                WorkspaceEntryKind::Directory
            };
            app.fail_workspace_entry_create(
                node_id.clone(),
                workspace_id.clone(),
                path.clone(),
                kind,
                *token,
                reason.to_owned(),
            );
            true
        }
        _ => false,
    }
}

fn action_node_id(action: &AppAction) -> Option<&str> {
    match action {
        AppAction::Spawn { node_id, .. }
        | AppAction::SpawnSpec { node_id, .. }
        | AppAction::SpawnManagedWorktree { node_id, .. }
        | AppAction::ForgetContextPack { node_id, .. }
        | AppAction::ResumeSessionRecord { node_id, .. }
        | AppAction::IndexProviderSession { node_id, .. }
        | AppAction::CatalogNativeSessions { node_id, .. }
        | AppAction::PageNativeSessions { node_id, .. }
        | AppAction::PreviewNativeSession { node_id, .. }
        | AppAction::IndexNativeSession { node_id, .. }
        | AppAction::PreviewSessionRecord { node_id, .. }
        | AppAction::RefreshSessionRecordHistory { node_id, .. }
        | AppAction::RenameSessionRecord { node_id, .. }
        | AppAction::SetSessionTask { node_id, .. }
        | AppAction::ForgetSessionRecord { node_id, .. }
        | AppAction::RegisterWorkspace { node_id, .. }
        | AppAction::BrowseHostDirectories { node_id, .. }
        | AppAction::UnregisterWorkspace { node_id, .. }
        | AppAction::CreateWorktree { node_id, .. }
        | AppAction::CreateStandaloneWorkspace { node_id, .. }
        | AppAction::RemoveWorktree { node_id, .. }
        | AppAction::ReadWorkspaceFile { node_id, .. }
        | AppAction::WriteWorkspaceFile { node_id, .. }
        | AppAction::CreateWorkspaceFile { node_id, .. }
        | AppAction::CreateWorkspaceDirectory { node_id, .. }
        | AppAction::ReadGitHistory { node_id, .. }
        | AppAction::ReadGitDiff { node_id, .. }
        | AppAction::InspectWorkspace { node_id, .. }
        | AppAction::Resync { node_id, .. } => Some(node_id),
        AppAction::Resume { address, .. }
        | AppAction::DiscoverHistory { address, .. }
        | AppAction::LoadHistory { address, .. }
        | AppAction::ExportContextPack { address }
        | AppAction::Input { address, .. }
        | AppAction::Paste { address, .. }
        | AppAction::TerminalControl { address, .. }
        | AppAction::TerminalBytes { address, .. }
        | AppAction::Resize { address, .. }
        | AppAction::Stop { address, .. }
        | AppAction::Remove { address } => Some(&address.node_id),
        AppAction::HarnessRefresh { .. }
        | AppAction::HarnessCreateTask { .. }
        | AppAction::HarnessMoveTask { .. }
        | AppAction::HarnessCancelTask { .. }
        | AppAction::HarnessRetryTask { .. }
        | AppAction::HarnessScheduleNext { .. }
        | AppAction::HarnessOpenMonitor { .. }
        | AppAction::HarnessLoadTaskCorrelations { .. }
        | AppAction::HarnessSaveTaskLaunchSpec { .. }
        | AppAction::HarnessStartTaskV2 { .. }
        | AppAction::HarnessSpawnSession { .. }
        | AppAction::HarnessWriteSessionInput { .. }
        | AppAction::HarnessResizeSession { .. }
        | AppAction::HarnessStopSession { .. } => Some(HARNESS_COMMAND_ROUTE),
        AppAction::HarnessOpenTerminal { .. }
        | AppAction::HarnessLoadTaskLaunchOptions { .. }
        | AppAction::HarnessLoadRunTransfer { .. }
        | AppAction::HarnessObserveRunContextSource { .. }
        | AppAction::HarnessInspectWorkspace { .. }
        | AppAction::HarnessReadWorkspaceFile { .. }
        | AppAction::HarnessReadGitHistory { .. }
        | AppAction::HarnessReadGitDiff { .. }
        | AppAction::HarnessLoadReverseAttribution { .. }
        | AppAction::HarnessInspectNodeWorkspace { .. }
        | AppAction::HarnessReadNodeWorkspaceFile { .. }
        | AppAction::HarnessReadNodeGitHistory { .. }
        | AppAction::HarnessReadNodeGitDiff { .. } => Some(HARNESS_DETAIL_COMMAND_ROUTE),
        AppAction::None | AppAction::Quit => None,
    }
}

fn sync_cursor(app: &App) -> io::Result<()> {
    if let Some((column, row)) = visible_cursor_position(app) {
        execute!(stdout(), MoveTo(column, row), Show)
    } else {
        execute!(stdout(), Hide)
    }
}

fn visible_cursor_position(app: &App) -> Option<(u16, u16)> {
    if app.focus != crate::app::Focus::Viewport {
        return None;
    }
    let area = app.focused_terminal_rect();
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if let Some((_, file)) = app.focused_file() {
        if file.inline_history.is_some()
            || !file.edit_mode
            || !matches!(file.state, crate::app::WorkspaceFileState::Ready)
        {
            return None;
        }
        let cursor = file.editor.cursor_position();
        let row = cursor.line.saturating_sub(file.editor.scroll_line());
        let number_width = file.editor.line_count().max(1).to_string().len().max(3)
            + 1
            + usize::from(file.editor.scroll_column() > 0);
        let column = cursor
            .column
            .saturating_sub(file.editor.scroll_column())
            .saturating_add(number_width);
        if row >= area.height.saturating_sub(1) as usize || column >= area.width as usize {
            return None;
        }
        return Some((area.x + column as u16, area.y + row as u16));
    }
    let session = app.focused_session()?;
    if !session.running {
        return None;
    }
    if app.terminal_scroll_offset(&session.address) > 0 {
        return None;
    }
    let (row, column) = session.terminal_cursor?;
    Some((
        area.x + column.min(area.width - 1),
        area.y + row.min(area.height - 1),
    ))
}

fn changed_terminal_sizes(
    app: &App,
    last: &mut BTreeMap<SessionAddress, (u16, u16)>,
) -> Vec<AppAction> {
    diff_terminal_sizes(app.desired_terminal_sizes(), last)
}

fn diff_terminal_sizes(
    desired: Vec<(SessionAddress, u16, u16)>,
    last: &mut BTreeMap<SessionAddress, (u16, u16)>,
) -> Vec<AppAction> {
    let desired = desired
        .into_iter()
        .map(|(address, rows, cols)| (address, (rows, cols)))
        .collect::<BTreeMap<_, _>>();
    let actions = desired
        .iter()
        .filter(|(address, size)| last.get(*address) != Some(*size))
        .map(|(address, (rows, cols))| AppAction::Resize {
            address: address.clone(),
            rows: *rows,
            cols: *cols,
        })
        .collect();
    *last = desired;
    actions
}

fn map_key(key: KeyEvent) -> Option<UiKey> {
    if key.modifiers.intersects(
        KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
    ) {
        return Some(UiKey::UnsupportedModifier);
    }
    if key.code == KeyCode::Enter
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::SHIFT
            || key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT))
    {
        return Some(UiKey::ModifiedEnter);
    }
    if key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT) {
        if let KeyCode::Char(ch) = key.code {
            if ch.eq_ignore_ascii_case(&'g') {
                return Some(UiKey::OperatorEscape);
            }
        }
    }
    if key.modifiers == KeyModifiers::CONTROL {
        if let KeyCode::Char(ch) = key.code {
            return Some(UiKey::Ctrl(crate::platform::normalize_ctrl_char(ch)));
        }
    }
    if key.modifiers == KeyModifiers::SHIFT {
        return match key.code {
            KeyCode::Home => Some(UiKey::ShiftHome),
            KeyCode::End => Some(UiKey::ShiftEnd),
            KeyCode::Up => Some(UiKey::ShiftUp),
            KeyCode::Down => Some(UiKey::ShiftDown),
            KeyCode::Left => Some(UiKey::ShiftLeft),
            KeyCode::Right => Some(UiKey::ShiftRight),
            KeyCode::PageUp => Some(UiKey::ShiftPageUp),
            KeyCode::PageDown => Some(UiKey::ShiftPageDown),
            KeyCode::Char(ch) => Some(UiKey::Char(ch)),
            KeyCode::Tab | KeyCode::BackTab => Some(UiKey::BackTab),
            _ => Some(UiKey::UnsupportedModifier),
        };
    }
    if key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        if let KeyCode::Char(ch) = key.code {
            let mut bytes = Vec::with_capacity(5);
            bytes.push(0x1b);
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
            return Some(UiKey::TerminalBytes(bytes));
        }
        return Some(UiKey::UnsupportedModifier);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        || (key.modifiers.contains(KeyModifiers::SHIFT)
            && !matches!(key.code, KeyCode::Char(_) | KeyCode::Tab | KeyCode::BackTab))
    {
        return Some(UiKey::UnsupportedModifier);
    }
    match key.code {
        KeyCode::Char(ch) => Some(UiKey::Char(ch)),
        KeyCode::Enter => Some(UiKey::Enter),
        KeyCode::Esc => Some(UiKey::Escape),
        KeyCode::Backspace => Some(UiKey::Backspace),
        KeyCode::Insert => Some(UiKey::Insert),
        KeyCode::Delete => Some(UiKey::Delete),
        KeyCode::Home => Some(UiKey::Home),
        KeyCode::End => Some(UiKey::End),
        KeyCode::Up => Some(UiKey::Up),
        KeyCode::Down => Some(UiKey::Down),
        KeyCode::Left => Some(UiKey::Left),
        KeyCode::Right => Some(UiKey::Right),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(UiKey::BackTab),
        KeyCode::Tab => Some(UiKey::Tab),
        KeyCode::BackTab => Some(UiKey::BackTab),
        KeyCode::PageUp => Some(UiKey::PageUp),
        KeyCode::PageDown => Some(UiKey::PageDown),
        KeyCode::F(number) if (1..=12).contains(&number) => Some(UiKey::Function(number)),
        _ => None,
    }
}

fn map_mouse(app: &mut App, mouse: MouseEvent) -> AppAction {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => app.click(mouse.column, mouse.row),
        MouseEventKind::Down(MouseButton::Right) => {
            match crate::platform::pressed_auxiliary_mouse_button() {
                Some(crate::platform::AuxiliaryMouseButton::Back) => {
                    app.navigate_surface_tab_at(mouse.column, mouse.row, true)
                }
                Some(crate::platform::AuxiliaryMouseButton::Forward) => {
                    app.navigate_surface_tab_at(mouse.column, mouse.row, false)
                }
                None => app.right_click(mouse.column, mouse.row),
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => app.drag(mouse.column, mouse.row),
        MouseEventKind::Up(MouseButton::Left) => app.drop_at(mouse.column, mouse.row),
        MouseEventKind::ScrollUp => app.scroll(mouse.column, mouse.row, true),
        MouseEventKind::ScrollDown => app.scroll(mouse.column, mouse.row, false),
        MouseEventKind::Moved => app.hover(mouse.column, mouse.row),
        _ => AppAction::None,
    }
}

fn project_c2_topology(
    topology: &C2Topology,
) -> (
    BTreeMap<String, NodeRoute>,
    BTreeMap<String, String>,
    BTreeMap<String, C2RelayRoute>,
) {
    let mut routes = BTreeMap::new();
    let mut endpoints = BTreeMap::new();
    let mut relay_routes = BTreeMap::new();
    for node in &topology.nodes {
        let node_id = node.node_id.to_string();
        endpoints.insert(node_id.clone(), node.endpoint.clone());
        relay_routes.insert(node_id.clone(), node.relay_route);
        if node.transport == NodeTransportState::Online {
            if let Some(expected_incarnation_id) = node.current_incarnation_id {
                routes.insert(
                    node_id,
                    NodeRoute {
                        node_id: node.node_id.clone(),
                        expected_incarnation_id,
                    },
                );
            }
        }
    }
    (routes, endpoints, relay_routes)
}

async fn reconcile_c2_topology(
    topology: &C2Topology,
    routes: &mut BTreeMap<String, NodeRoute>,
    endpoints: &mut BTreeMap<String, String>,
    relay_routes: &mut BTreeMap<String, C2RelayRoute>,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Vec<NodeRoute> {
    let (next_routes, next_endpoints, next_relay_routes) = project_c2_topology(topology);
    for node in &topology.nodes {
        if let Some(incarnation_id) = node.current_incarnation_id {
            send_update(
                updates,
                WorkerUpdate::ObservationSupport {
                    node_id: node.node_id.to_string(),
                    incarnation_id,
                    support: project_c2_observation_support(node.observation_support.as_ref()),
                },
            )
            .await;
        }
    }
    for node_id in endpoints.keys() {
        if !next_endpoints.contains_key(node_id) {
            send_update(
                updates,
                WorkerUpdate::TopologyNodeRemoved {
                    node_id: node_id.clone(),
                },
            )
            .await;
        }
    }
    let mut snapshots = Vec::new();
    for (node_id, endpoint) in &next_endpoints {
        let route_changed = routes.get(node_id) != next_routes.get(node_id);
        let endpoint_changed = endpoints.get(node_id) != Some(endpoint);
        if let Some(route) = next_routes.get(node_id) {
            if route_changed || endpoint_changed {
                send_update(
                    updates,
                    WorkerUpdate::State {
                        node_id: node_id.clone(),
                        endpoint: endpoint.clone(),
                        state: ConnectionState::Connecting,
                    },
                )
                .await;
                snapshots.push(route.clone());
            }
        } else if route_changed || endpoint_changed || !endpoints.contains_key(node_id) {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: node_id.clone(),
                    endpoint: endpoint.clone(),
                    state: ConnectionState::Disconnected("node offline at C2".to_owned()),
                },
            )
            .await;
        }
    }
    for (node_id, route) in &next_relay_routes {
        if relay_routes.get(node_id) != Some(route) {
            send_update(
                updates,
                WorkerUpdate::RelayRoute {
                    node_id: node_id.clone(),
                    route: *route,
                },
            )
            .await;
        }
    }
    *routes = next_routes;
    *endpoints = next_endpoints;
    *relay_routes = next_relay_routes;
    snapshots
}

async fn dispatch_c2_startup(
    handle: &C2ControlHandle,
    routes: &BTreeMap<String, NodeRoute>,
    endpoints: &BTreeMap<String, String>,
    startup: &mut Option<StartupRequest>,
    initial_terminal_size: TerminalSize,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<bool, C2ControlError> {
    let Some(pending) = startup.as_ref() else {
        return Ok(false);
    };
    let Some(route) = routes.get(pending.node_id.as_str()).cloned() else {
        return Ok(false);
    };
    let request = startup.take().expect("checked pending C2 startup request");
    let action = AppAction::Spawn {
        node_id: request.node_id.to_string(),
        workspace_id: request.workspace_id.to_string(),
        provider: request.provider,
        rows: initial_terminal_size.rows,
        cols: initial_terminal_size.columns,
    };
    if let Some(node_request) = action_to_request(action) {
        c2_request_and_publish(handle, route, node_request, updates, endpoints).await?;
    }
    Ok(true)
}

async fn disconnect_c2_nodes(
    endpoints: &BTreeMap<String, String>,
    updates: &mpsc::Sender<WorkerUpdate>,
    reason: &str,
) {
    for (node_id, endpoint) in endpoints {
        send_update(
            updates,
            WorkerUpdate::State {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                state: ConnectionState::Disconnected(reason.to_owned()),
            },
        )
        .await;
    }
}

fn negotiated_observation_support(capabilities: &[CapabilityId]) -> SessionMonitorSupport {
    let has = |required: &str| capabilities.iter().any(|capability| capability.as_str() == required);
    let events = has(NODE_OBSERVATION_EVENTS_CAPABILITY);
    SessionMonitorSupport {
        known: true,
        events,
        managed_target: events && has(NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY),
        workflow_detail: events && has(NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY),
    }
}

fn project_c2_observation_support(
    support: Option<&gate4agent_c2_protocol::C2ObservationSupport>,
) -> SessionMonitorSupport {
    SessionMonitorSupport {
        known: true,
        events: support.is_some_and(|support| support.events),
        managed_target: support.is_some_and(|support| support.managed_target),
        workflow_detail: support.is_some_and(|support| support.workflow_detail),
    }
}

#[derive(Default)]
struct HarnessIntentFactory {
    sequence: u32,
}

impl HarnessIntentFactory {
    fn next_suffix(&mut self) -> String {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        let now_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_or(1, |duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .max(1);
        format!("{now_unix_ms:016x}{:08x}", self.sequence)
    }

    fn next_intent(
        &mut self,
        action: HarnessOperatorActionV1,
    ) -> Result<HarnessOperatorIntentV1, String> {
        let suffix = self.next_suffix();
        let submitted_at_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_or(1, |duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .max(1);
        let request_ref = HarnessOperatorRequestRefV1::new(format!("hireq_{suffix}"))
            .map_err(|error| format!("Harness operator request reference generation failed: {error}"))?;
        Ok(HarnessOperatorIntentV1 {
            request_ref,
            submitted_at_unix_ms,
            action,
        })
    }
}

fn submit_harness_intent(
    client: &HarnessOperatorClient,
    intent_factory: &mut HarnessIntentFactory,
    token: u64,
    action: HarnessOperatorActionV1,
    updates: &mpsc::Sender<WorkerUpdate>,
    runtime_inventory: &Arc<Mutex<Option<Vec<NodeView>>>>,
    require_inventory_change: bool,
) {
    let result = intent_factory.next_intent(action)
        .and_then(|intent| client.submit_intent(intent).map_err(|error| error.to_string()));
    match result {
        Ok(_) => publish_harness_snapshot(
            client,
            token,
            updates,
            runtime_inventory,
            require_inventory_change,
        ),
        Err(message) => publish_harness_failure(token, message, updates),
    }
}

fn harness_operator_worker(
    client: HarnessOperatorClient,
    mut commands: mpsc::Receiver<AppAction>,
    updates: mpsc::Sender<WorkerUpdate>,
    runtime_inventory: Arc<Mutex<Option<Vec<NodeView>>>>,
) {
    let mut intent_factory = HarnessIntentFactory::default();
    while let Some(action) = commands.blocking_recv() {
        match action {
            AppAction::HarnessRefresh { token } => {
                publish_harness_snapshot(
                    &client,
                    token,
                    &updates,
                    &runtime_inventory,
                    false,
                );
            }
            AppAction::HarnessSpawnSession {
                token,
                node_id,
                workspace_id,
                provider,
                provider_profile,
                mode,
                rows,
                cols,
            } => {
                let result = client.spawn_session(
                    node_id,
                    workspace_id,
                    provider,
                    provider_profile,
                    mode,
                    HarnessRuntimeTerminalSizeV1 { rows, columns: cols },
                );
                match result {
                    // `require_inventory_change: true` mirrors
                    // `harness_schedule_next`: the mutation just happened,
                    // so a retry-until-changed snapshot is worth the extra
                    // round trips (the runtime inventory should now include
                    // the new session).
                    Ok(session) => {
                        let _ = updates.blocking_send(WorkerUpdate::HarnessSessionSpawned {
                            address: SessionAddress {
                                node_id: session.node_id,
                                workspace_id: session.workspace_id,
                                instance_id: session.instance_id,
                                generation: session.generation,
                            },
                        });
                        publish_harness_snapshot(
                            &client,
                            token,
                            &updates,
                            &runtime_inventory,
                            true,
                        );
                    }
                    Err(error) => publish_harness_failure(token, error.to_string(), &updates),
                }
            }
            AppAction::HarnessWriteSessionInput { session, text } => {
                if let Err(error) = client.write_session_input(session, text) {
                    let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                        "harness session input failed: {error}"
                    )));
                }
            }
            AppAction::HarnessResizeSession { session, rows, cols } => {
                if let Err(error) = client.resize_session(
                    session,
                    HarnessRuntimeTerminalSizeV1 { rows, columns: cols },
                ) {
                    let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                        "harness session resize failed: {error}"
                    )));
                }
            }
            AppAction::HarnessStopSession { token, session, force } => {
                match client.stop_session(session, force) {
                    Ok(()) => publish_harness_snapshot(
                        &client,
                        token,
                        &updates,
                        &runtime_inventory,
                        true,
                    ),
                    Err(error) => publish_harness_failure(token, error.to_string(), &updates),
                }
            }
            AppAction::HarnessCreateTask {
                token,
                title,
                body,
                initial_state,
            } => {
                submit_harness_intent(
                    &client,
                    &mut intent_factory,
                    token,
                    HarnessOperatorActionV1::CreateTask {
                        title,
                        body,
                        parent_task_id: None,
                        dependencies: Vec::new(),
                        initial_state,
                    },
                    &updates,
                    &runtime_inventory,
                    false,
                );
            }
            AppAction::HarnessMoveTask {
                token,
                task_id,
                expected_revision,
                state,
            } => {
                submit_harness_intent(
                    &client,
                    &mut intent_factory,
                    token,
                    HarnessOperatorActionV1::MoveTask {
                        task_id,
                        expected_revision,
                        state,
                    },
                    &updates,
                    &runtime_inventory,
                    false,
                );
            }
            AppAction::HarnessCancelTask {
                token,
                task_id,
                expected_revision,
            } => {
                submit_harness_intent(
                    &client,
                    &mut intent_factory,
                    token,
                    HarnessOperatorActionV1::CancelTask {
                        task_id,
                        expected_revision,
                    },
                    &updates,
                    &runtime_inventory,
                    false,
                );
            }
            AppAction::HarnessRetryTask {
                token,
                task_id,
                expected_revision,
            } => {
                submit_harness_intent(
                    &client,
                    &mut intent_factory,
                    token,
                    HarnessOperatorActionV1::RetryTask {
                        task_id,
                        expected_revision,
                    },
                    &updates,
                    &runtime_inventory,
                    false,
                );
            }
            AppAction::HarnessScheduleNext { token, plan_id } => {
                submit_harness_intent(
                    &client,
                    &mut intent_factory,
                    token,
                    HarnessOperatorActionV1::ScheduleNext { plan_id },
                    &updates,
                    &runtime_inventory,
                    true,
                );
            }
            AppAction::HarnessSaveTaskLaunchSpec {
                token,
                task,
                expected_execution_spec_revision,
                selection,
                refresh_run,
            } => {
                let result = (|| {
                    let intent = intent_factory.next_intent(
                        HarnessOperatorActionV1::ReplaceTaskExecutionSpecV2 {
                            task_id: task.task_id.clone(),
                            expected_task_revision: task.task_revision,
                            expected_execution_spec_revision,
                            selection,
                        },
                    )?;
                    let outcome = match client.submit_intent(intent)
                        .map_err(|error| error.to_string())?
                    {
                        HarnessOperatorResponseV1::ExecutionSpecMutation(outcome) => outcome,
                        _ => return Err("Harness V6 launch-spec mutation returned an unexpected response".to_owned()),
                    };
                    let options = client.task_launch_options_get(task.task_id.clone())
                        .map_err(|error| error.to_string())?;
                    if options.task_revision != task.task_revision {
                        return Err(format!(
                            "Harness task revision changed from r{} to r{} while saving launch spec",
                            task.task_revision.get(),
                            options.task_revision.get(),
                        ));
                    }
                    Ok((outcome, options))
                })();
                match result {
                    Ok((outcome, options)) => {
                        if let Some(run) = refresh_run {
                            let transfer = match client.run_transfer_get(run.run_id.clone()) {
                                Ok(summary) if summary.run_revision == run.run_revision => {
                                    WorkerUpdate::HarnessRunTransfer {
                                        run,
                                        token,
                                        summary,
                                    }
                                }
                                Ok(summary) => WorkerUpdate::HarnessRunTransferFailed {
                                    run: run.clone(),
                                    token,
                                    message: format!(
                                        "Harness run revision changed from r{} to r{}",
                                        run.run_revision.get(),
                                        summary.run_revision.get(),
                                    ),
                                },
                                Err(error) => WorkerUpdate::HarnessRunTransferFailed {
                                    run,
                                    token,
                                    message: error.to_string(),
                                },
                            };
                            if updates.blocking_send(transfer).is_err() {
                                return;
                            }
                        }
                        if updates.blocking_send(WorkerUpdate::HarnessLaunchSpecSaved {
                            token,
                            task: task.clone(),
                            options,
                            outcome,
                        }).is_err() {
                            return;
                        }
                        publish_harness_snapshot(
                            &client,
                            token,
                            &updates,
                            &runtime_inventory,
                            false,
                        );
                    }
                    Err(message) => {
                        if updates.blocking_send(WorkerUpdate::HarnessExecutionMutationFailed {
                            token,
                            task,
                            message,
                        }).is_err() {
                            return;
                        }
                    }
                }
            }
            AppAction::HarnessStartTaskV2 {
                token,
                task,
                expected_execution_spec_revision,
                expected_launch_issuance,
            } => {
                let result = (|| {
                    let intent = intent_factory.next_intent(HarnessOperatorActionV1::StartTaskV2 {
                        task_id: task.task_id.clone(),
                        expected_task_revision: task.task_revision,
                        expected_execution_spec_revision,
                        expected_launch_issuance,
                    })?;
                    let outcome = match client.submit_intent(intent)
                        .map_err(|error| error.to_string())?
                    {
                        HarnessOperatorResponseV1::TaskStarted(outcome) => outcome,
                        _ => return Err("Harness V6 start-task returned an unexpected response".to_owned()),
                    };
                    let options = client.task_launch_options_get(task.task_id.clone())
                        .map_err(|error| error.to_string())?;
                    if options.task_revision != outcome.dispatch.task_revision {
                        return Err("Harness launch options did not match started task revision".to_owned());
                    }
                    let transfer = client.run_transfer_get(outcome.dispatch.run_id.clone())
                        .map_err(|error| error.to_string())
                        .and_then(|summary| {
                            if summary.run_revision == outcome.dispatch.run_revision {
                                Ok(summary)
                            } else {
                                Err("Harness transfer did not match started run revision".to_owned())
                            }
                        });
                    Ok((outcome, options, transfer))
                })();
                match result {
                    Ok((outcome, options, transfer)) => {
                        if updates.blocking_send(WorkerUpdate::HarnessTaskStartedV2 {
                            token,
                            task: task.clone(),
                            outcome,
                            options,
                            transfer,
                        }).is_err() {
                            return;
                        }
                        publish_harness_snapshot(
                            &client,
                            token,
                            &updates,
                            &runtime_inventory,
                            true,
                        );
                    }
                    Err(message) => {
                        if updates.blocking_send(WorkerUpdate::HarnessExecutionMutationFailed {
                            token,
                            task,
                            message,
                        }).is_err() {
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn harness_native_history_worker(
    client: HarnessOperatorClient,
    mut commands: mpsc::Receiver<AppAction>,
    updates: mpsc::Sender<WorkerUpdate>,
    runtime_inventory: Arc<Mutex<Option<Vec<NodeView>>>>,
) {
    while let Some(action) = commands.blocking_recv() {
        let nodes = harness_runtime_inventory_snapshot(&runtime_inventory);
        match action {
            AppAction::CatalogNativeSessions {
                node_id,
                routes,
                limit,
                token,
            } => publish_harness_native_catalogs(
                &client,
                &updates,
                nodes.as_deref(),
                node_id,
                routes,
                limit,
                token,
            ),
            AppAction::PageNativeSessions {
                node_id,
                route,
                window,
                catalog_revision,
                recent_cutoff_unix_ms,
                after_selection_id,
                limit,
                token,
            } => publish_harness_native_page(
                &client,
                &updates,
                nodes.as_deref(),
                node_id,
                route,
                window,
                catalog_revision,
                recent_cutoff_unix_ms,
                after_selection_id,
                limit,
                token,
            ),
            AppAction::PreviewNativeSession {
                node_id,
                route,
                catalog_revision,
                recent_cutoff_unix_ms,
                selection_id,
                message_limit,
                token,
            } => publish_harness_native_preview(
                &client,
                &updates,
                nodes.as_deref(),
                node_id,
                route,
                catalog_revision,
                recent_cutoff_unix_ms,
                selection_id,
                message_limit,
                token,
            ),
            _ => {}
        }
    }
}

fn harness_detail_worker(
    client: HarnessOperatorClient,
    mut commands: mpsc::Receiver<AppAction>,
    updates: mpsc::Sender<WorkerUpdate>,
) {
    while let Some(action) = commands.blocking_recv() {
        match action {
            AppAction::HarnessOpenMonitor { run } => {
                let result = (|| {
                    let loaded = client.run_get(run.run_id.clone())
                        .map_err(|error| error.to_string())?;
                    if loaded.revision != run.run_revision {
                        return Err(format!(
                            "Harness run revision changed from r{} to r{}",
                            run.run_revision.get(),
                            loaded.revision.get(),
                        ));
                    }
                    if loaded.binding == RedactedBindingStateV1::None {
                        return Err("Harness run has no redacted binding".to_owned());
                    }
                    let monitor = client.monitor_get(run.run_id.clone())
                        .map_err(|error| error.to_string())?;
                    let timeline = client.timeline_read(run.run_id.clone(), None, 128)
                        .map_err(|error| error.to_string())?;
                    if timeline.run_id != run.run_id {
                        return Err("Harness timeline referenced a different run".to_owned());
                    }
                    Ok((loaded, monitor, timeline.entries))
                })();
                let update = match result {
                    Ok((loaded, monitor, timeline)) => {
                        WorkerUpdate::HarnessMonitor { run: loaded, monitor, timeline }
                    }
                    Err(message) => WorkerUpdate::HarnessMonitorFailed { run, message },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessOpenTerminal { session, after_sequence } => {
                if let Ok(page) = client.terminal_read(
                    session,
                    after_sequence,
                    HARNESS_TERMINAL_PAGE_LIMIT_MAX,
                ) {
                    if updates.blocking_send(WorkerUpdate::HarnessTerminalRead(page)).is_err() {
                        return;
                    }
                }
            }
            AppAction::HarnessLoadRunTransfer { run, token } => {
                let update = match client.run_transfer_get(run.run_id.clone()) {
                    Ok(summary) if summary.run_revision == run.run_revision => {
                        WorkerUpdate::HarnessRunTransfer {
                            run,
                            token,
                            summary,
                        }
                    }
                    Ok(summary) => WorkerUpdate::HarnessRunTransferFailed {
                        run: run.clone(),
                        token,
                        message: format!(
                            "Harness run revision changed from r{} to r{}",
                            run.run_revision.get(),
                            summary.run_revision.get(),
                        ),
                    },
                    Err(error) => WorkerUpdate::HarnessRunTransferFailed {
                        run,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessObserveRunContextSource { run, task, token } => {
                let update = match client.observe_run_context_source(run.run_id.clone()) {
                    Ok(observation) if observation.run_revision == run.run_revision => {
                        let launch_options = if observation.feature_state
                            == gate4agent_harness_client::FeatureObservationStateV1::Observed
                        {
                            client.task_launch_options_get(task.task_id.clone())
                                .map_err(|error| error.to_string())
                        } else {
                            Err(format!(
                                "context source is {:?}",
                                observation.feature_state,
                            ))
                        };
                        WorkerUpdate::HarnessRunContextSourceObserved {
                            run,
                            task,
                            token,
                            observation,
                            launch_options,
                        }
                    }
                    Ok(observation) => WorkerUpdate::HarnessRunContextSourceObservationFailed {
                        run: run.clone(),
                        token,
                        message: format!(
                            "Harness context observation changed run revision from r{} to r{}",
                            run.run_revision.get(),
                            observation.run_revision.get(),
                        ),
                    },
                    Err(error) => WorkerUpdate::HarnessRunContextSourceObservationFailed {
                        run,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessLoadTaskLaunchOptions { task, token } => {
                let update = match client.task_launch_options_get(task.task_id.clone()) {
                    Ok(options) if options.task_revision == task.task_revision => {
                        WorkerUpdate::HarnessLaunchOptionsLoaded {
                            task,
                            token,
                            options,
                        }
                    }
                    Ok(options) => WorkerUpdate::HarnessLaunchOptionsLoadFailed {
                        task: task.clone(),
                        token,
                        message: format!(
                            "Harness task revision changed from r{} to r{}",
                            task.task_revision.get(),
                            options.task_revision.get(),
                        ),
                    },
                    Err(error) => WorkerUpdate::HarnessLaunchOptionsLoadFailed {
                        task,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessLoadTaskCorrelations { task, launch_token, runs } => {
                let task_id = task.task_id.clone();
                let mut correlations = Vec::with_capacity(runs.len());
                let mut failures = Vec::new();
                let mut observations = Vec::with_capacity(runs.len());
                let mut observation_failures = Vec::new();
                for run in runs {
                    match client.run_correlation_get(run.run_id.clone()) {
                        Ok(correlation) => correlations.push(correlation),
                        Err(error) => failures.push((run.run_id.clone(), error.to_string())),
                    }
                    let observation = (|| {
                        let loaded = client.run_get(run.run_id.clone())
                            .map_err(|error| error.to_string())?;
                        if loaded.revision != run.run_revision
                            || loaded.task_id.as_ref() != Some(&task_id)
                        {
                            return Err("Harness run identity changed while loading observation"
                                .to_owned());
                        }
                        if loaded.binding == RedactedBindingStateV1::None {
                            return Err("run has no Harness observation binding".to_owned());
                        }
                        let monitor = client.monitor_get(run.run_id.clone())
                            .map_err(|error| error.to_string())?;
                        if monitor.run_id != run.run_id {
                            return Err("Harness monitor referenced a different run".to_owned());
                        }
                        Ok((loaded, monitor))
                    })();
                    match observation {
                        Ok(observation) => observations.push(observation),
                        Err(message) => observation_failures.push((run, message)),
                    }
                }
                if updates.blocking_send(WorkerUpdate::HarnessTaskCorrelations {
                    task_id: task_id.clone(),
                    correlations,
                    failures,
                }).is_err() {
                    return;
                }
                if updates.blocking_send(WorkerUpdate::HarnessTaskObservations {
                    task_id: task_id.clone(),
                    observations,
                    failures: observation_failures,
                }).is_err() {
                    return;
                }
                let update = match client.task_launch_options_get(task_id) {
                    Ok(options) if options.task_revision == task.task_revision => {
                        WorkerUpdate::HarnessLaunchOptionsLoaded {
                            task,
                            token: launch_token,
                            options,
                        }
                    }
                    Ok(options) => WorkerUpdate::HarnessLaunchOptionsLoadFailed {
                        task: task.clone(),
                        token: launch_token,
                        message: format!(
                            "Harness task revision changed from r{} to r{}",
                            task.task_revision.get(),
                            options.task_revision.get(),
                        ),
                    },
                    Err(error) => WorkerUpdate::HarnessLaunchOptionsLoadFailed {
                        task,
                        token: launch_token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessInspectWorkspace { run, token } => {
                let update = match client.inspect_run_workspace(run.run_id.clone()) {
                    Ok(inspection) => WorkerUpdate::HarnessWorkspaceInspected {
                        run,
                        token,
                        inspection,
                    },
                    Err(error) => WorkerUpdate::HarnessWorkspaceInspectionFailed {
                        run,
                        token,
                        failure: project_harness_read_failure(error),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessReadWorkspaceFile { origin, path, token } => {
                let key = HarnessWorkspaceFileTabKey {
                    origin: origin.clone(),
                    path: path.clone(),
                };
                let result = project_harness_repository_path(&path)
                    .and_then(|path| {
                        client.read_run_workspace_file(origin.run.run_id.clone(), path)
                    });
                let update = match result {
                    Ok(file) => WorkerUpdate::HarnessWorkspaceFileRead { key, token, file },
                    Err(error) => WorkerUpdate::HarnessWorkspaceFileFailed {
                        key,
                        token,
                        failure: project_harness_read_failure(error),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessReadGitHistory {
                origin,
                path,
                before,
                limit,
                token,
                destination,
            } => {
                let result = (|| {
                    let path = path.as_ref().map(project_harness_repository_path).transpose()?;
                    let before = before.map(HarnessGitObjectIdV1::new).transpose()
                        .map_err(HarnessOperatorClientError::Api)?;
                    client.read_run_git_history(
                        origin.run.run_id.clone(),
                        path,
                        before,
                        limit,
                    )
                })();
                let update = match result {
                    Ok(page) => WorkerUpdate::HarnessGitHistoryRead {
                        destination,
                        token,
                        page,
                    },
                    Err(error) => WorkerUpdate::HarnessGitHistoryFailed {
                        destination,
                        token,
                        failure: project_harness_read_failure(error),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessReadGitDiff {
                origin,
                target,
                token,
                destination,
            } => {
                let result = (|| {
                    let (mode, path) = project_harness_git_diff_target(&target)?;
                    client.read_run_git_diff(origin.run.run_id.clone(), mode, path)
                })();
                let update = match result {
                    Ok(diff) => WorkerUpdate::HarnessGitDiffRead {
                        destination,
                        token,
                        diff,
                    },
                    Err(error) => WorkerUpdate::HarnessGitDiffFailed {
                        destination,
                        token,
                        failure: project_harness_read_failure(error),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessInspectNodeWorkspace { node_id, workspace_id } => {
                let update = match client.inspect_node_workspace(
                    node_id.clone(),
                    workspace_id.clone(),
                ) {
                    Ok(inspection) => WorkerUpdate::HarnessNodeWorkspaceInspected {
                        node_id,
                        workspace_id,
                        inspection,
                    },
                    Err(error) => WorkerUpdate::HarnessNodeWorkspaceInspectionFailed {
                        node_id,
                        workspace_id,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessReadNodeWorkspaceFile { node_id, workspace_id, path, token } => {
                let result = project_harness_repository_path(&path)
                    .and_then(|path| {
                        client.read_node_workspace_file(node_id.clone(), workspace_id.clone(), path)
                    });
                let update = match result {
                    Ok(file) => WorkerUpdate::HarnessNodeWorkspaceFileRead { node_id, token, file },
                    Err(error) => WorkerUpdate::HarnessNodeWorkspaceFileFailed {
                        key: WorkspaceFileTabKey { node_id, workspace_id, path },
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessReadNodeGitHistory {
                node_id,
                workspace_id,
                path,
                before,
                limit,
                token,
                destination,
            } => {
                let result = (|| {
                    let path = path.as_ref().map(project_harness_repository_path).transpose()?;
                    let before = before.map(HarnessGitObjectIdV1::new).transpose()
                        .map_err(HarnessOperatorClientError::Api)?;
                    client.read_node_git_history(
                        node_id.clone(),
                        workspace_id.clone(),
                        path,
                        before,
                        limit,
                    )
                })();
                let update = match result {
                    Ok(page) => WorkerUpdate::HarnessNodeGitHistoryRead {
                        destination,
                        token,
                        page,
                    },
                    Err(error) => WorkerUpdate::HarnessNodeGitHistoryFailed {
                        destination,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessReadNodeGitDiff {
                node_id,
                workspace_id,
                target,
                token,
                destination,
            } => {
                let result = (|| {
                    let (mode, path) = project_harness_git_diff_target(&target)?;
                    client.read_node_git_diff(node_id.clone(), workspace_id.clone(), mode, path)
                })();
                let update = match result {
                    Ok(diff) => WorkerUpdate::HarnessNodeGitDiffRead {
                        destination,
                        token,
                        diff,
                    },
                    Err(error) => WorkerUpdate::HarnessNodeGitDiffFailed {
                        destination,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessLoadReverseAttribution { subject, token } => {
                let update = match client.reverse_attribution_get(subject.clone()) {
                    Ok(value) => WorkerUpdate::HarnessReverseAttributionLoaded {
                        subject,
                        token,
                        value,
                    },
                    Err(error) => WorkerUpdate::HarnessReverseAttributionFailed {
                        subject,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

fn project_harness_read_failure(error: HarnessOperatorClientError) -> HarnessReadFailure {
    let category = match &error {
        HarnessOperatorClientError::Host(host) => match host {
            HarnessOperatorHostErrorV1::InvalidRequest => "invalid-request",
            HarnessOperatorHostErrorV1::Unauthorized => "unauthorized",
            HarnessOperatorHostErrorV1::NotFound => "not-found",
            HarnessOperatorHostErrorV1::Conflict => "conflict",
            HarnessOperatorHostErrorV1::TooLarge => "too-large",
            HarnessOperatorHostErrorV1::Deadline => "deadline",
            HarnessOperatorHostErrorV1::Busy => "busy",
            HarnessOperatorHostErrorV1::Unavailable => "unavailable",
            HarnessOperatorHostErrorV1::OutcomeUnknown => "outcome-unknown",
            HarnessOperatorHostErrorV1::Internal => "internal",
        },
        HarnessOperatorClientError::Api(_) => "validation",
        HarnessOperatorClientError::Deadline => "deadline",
        HarnessOperatorClientError::Unavailable
        | HarnessOperatorClientError::ConnectionClosed => "unavailable",
        HarnessOperatorClientError::RequestTooLarge
        | HarnessOperatorClientError::ResponseTooLarge => "too-large",
        HarnessOperatorClientError::InvalidResponse
        | HarnessOperatorClientError::IncompleteResponse
        | HarnessOperatorClientError::UnexpectedResponse
        | HarnessOperatorClientError::Encoding => "invalid-response",
        HarnessOperatorClientError::NonLoopbackEndpoint
        | HarnessOperatorClientError::Transport => "transport",
    };
    HarnessReadFailure {
        category: category.to_owned(),
        message: error.to_string(),
    }
}

fn project_harness_repository_path(
    path: &RepositoryPath,
) -> Result<HarnessRepositoryPathV1, HarnessOperatorClientError> {
    let value = path.as_utf8().ok_or_else(|| {
        HarnessOperatorClientError::Api(
            gate4agent_harness_client::HarnessOperatorApiError::InvalidRepositoryPath,
        )
    })?;
    HarnessRepositoryPathV1::new(value.to_owned()).map_err(HarnessOperatorClientError::Api)
}

fn project_harness_git_diff_target(
    target: &WorkspaceGitDiffTarget,
) -> Result<(HarnessGitDiffModeV1, Option<HarnessRepositoryPathV1>), HarnessOperatorClientError> {
    let (mode, path) = match target {
        WorkspaceGitDiffTarget::Working { path } => (HarnessGitDiffModeV1::Working, path),
        WorkspaceGitDiffTarget::Staged { path } => (HarnessGitDiffModeV1::Staged, path),
        WorkspaceGitDiffTarget::Commit { revision, path } => (
            HarnessGitDiffModeV1::Commit {
                revision: HarnessGitObjectIdV1::new(revision.clone())
                    .map_err(HarnessOperatorClientError::Api)?,
            },
            path,
        ),
    };
    Ok((mode, path.as_ref().map(project_harness_repository_path).transpose()?))
}

fn project_harness_origin(origin: HarnessRunWorkspaceOriginV1) -> HarnessRunOrigin {
    HarnessRunOrigin {
        run: HarnessRunRef {
            run_id: origin.run_id,
            run_revision: origin.run_revision,
        },
        node_id: origin.node_id,
        node_incarnation_id: origin.node_incarnation_id,
        workspace_id: origin.workspace_id,
    }
}

fn project_harness_path(path: HarnessRepositoryPathV1) -> RepositoryPath {
    RepositoryPath::utf8(path.as_str().to_owned())
        .expect("validated Harness repository path is a valid Node repository path")
}

fn project_harness_git_status_code(code: HarnessGitStatusCodeV1) -> String {
    match code {
        HarnessGitStatusCodeV1::Unmodified => " ",
        HarnessGitStatusCodeV1::Added => "A",
        HarnessGitStatusCodeV1::Modified => "M",
        HarnessGitStatusCodeV1::Deleted => "D",
        HarnessGitStatusCodeV1::Renamed => "R",
        HarnessGitStatusCodeV1::Copied => "C",
        HarnessGitStatusCodeV1::Unmerged => "U",
        HarnessGitStatusCodeV1::Untracked => "?",
        HarnessGitStatusCodeV1::Ignored => "!",
        HarnessGitStatusCodeV1::TypeChanged => "T",
    }.to_owned()
}

fn project_harness_workspace_inspection(
    inspection: HarnessRunWorkspaceInspectionV1,
) -> (HarnessRunOrigin, Vec<WorkspaceEntry>, bool, GitSnapshot) {
    let origin = project_harness_origin(inspection.origin);
    let entries = inspection.entries.into_iter().map(|entry| WorkspaceEntry {
        relative_path: project_harness_path(entry.relative_path),
        kind: match entry.kind {
            HarnessWorkspaceEntryKindV1::File => WorkspaceEntryKind::File,
            HarnessWorkspaceEntryKindV1::Directory => WorkspaceEntryKind::Directory,
        },
    }).collect();
    let git = GitSnapshot {
        is_repository: inspection.git.is_repository,
        branch: inspection.git.branch,
        status: inspection.git.status.into_iter().map(|entry| GitStatusEntry {
            index_status: project_harness_git_status_code(entry.index_status),
            worktree_status: project_harness_git_status_code(entry.worktree_status),
            path: project_harness_path(entry.path),
            previous_path: entry.previous_path.map(project_harness_path),
        }).collect(),
        recent_commits: inspection.git.recent_commits.into_iter().map(|commit| GitCommitSummary {
            id: commit.id.as_str().to_owned(),
            summary: commit.summary,
        }).collect(),
        worktrees: Vec::new(),
        managed_worktree: None,
        truncated: inspection.git.truncated,
        diagnostic: None,
    };
    (origin, entries, inspection.tree_truncated, git)
}

fn project_harness_file_content(content: HarnessWorkspaceFileContentV1) -> WorkspaceFileContent {
    match content {
        HarnessWorkspaceFileContentV1::Utf8 { text, byte_len } => {
            WorkspaceFileContent::Utf8 { text, byte_len }
        }
        HarnessWorkspaceFileContentV1::NonUtf8 { byte_len } => {
            WorkspaceFileContent::NonUtf8 { byte_len }
        }
        HarnessWorkspaceFileContentV1::TooLarge { limit_bytes } => {
            WorkspaceFileContent::TooLarge { limit_bytes }
        }
    }
}

fn project_harness_git_commit(
    commit: gate4agent_harness_client::HarnessGitCommitV1,
) -> GitCommitView {
    GitCommitView {
        id: commit.id.as_str().to_owned(),
        parents: commit.parents.into_iter().map(|parent| parent.as_str().to_owned()).collect(),
        subject: commit.subject,
        author_name: commit.author_name,
        author_email: String::new(),
        authored_at: commit.authored_at,
        committer_name: commit.committer_name,
        committer_email: String::new(),
        committed_at: commit.committed_at,
        signature_status: format!("{:?}", commit.signature_status),
        signer: commit.signer,
    }
}

fn harness_git_history_has_more(page: &HarnessRunGitHistoryPageV1) -> bool {
    page.next_before.is_some()
}

fn project_harness_git_diff_target_from_response(
    mode: HarnessGitDiffModeV1,
    path: Option<HarnessRepositoryPathV1>,
) -> WorkspaceGitDiffTarget {
    let path = path.map(project_harness_path);
    match mode {
        HarnessGitDiffModeV1::Working => WorkspaceGitDiffTarget::Working { path },
        HarnessGitDiffModeV1::Staged => WorkspaceGitDiffTarget::Staged { path },
        HarnessGitDiffModeV1::Commit { revision } => WorkspaceGitDiffTarget::Commit {
            revision: revision.as_str().to_owned(),
            path,
        },
    }
}

/// Node-scoped sibling of `project_harness_workspace_inspection`: the
/// response lands in the same direct-mode state (`workspace_inspections`)
/// the direct-C2 `InspectWorkspace` reply fills, so it projects straight to
/// `gate4agent_node_protocol::WorkspaceInspection` rather than to a
/// Harness-prefixed presentation type.
fn project_harness_node_workspace_inspection(
    inspection: HarnessNodeWorkspaceInspectionV1,
) -> Result<(String, gate4agent_node_protocol::WorkspaceInspection), String> {
    let node_id = inspection.origin.node_id;
    let workspace_id = WorkspaceId::new(inspection.origin.workspace_id)
        .map_err(|error| format!("invalid Harness runtime workspace ID: {error}"))?;
    let entries = inspection.entries.into_iter().map(|entry| WorkspaceEntry {
        relative_path: project_harness_path(entry.relative_path),
        kind: match entry.kind {
            HarnessWorkspaceEntryKindV1::File => WorkspaceEntryKind::File,
            HarnessWorkspaceEntryKindV1::Directory => WorkspaceEntryKind::Directory,
        },
    }).collect();
    let git = GitSnapshot {
        is_repository: inspection.git.is_repository,
        branch: inspection.git.branch,
        status: inspection.git.status.into_iter().map(|entry| GitStatusEntry {
            index_status: project_harness_git_status_code(entry.index_status),
            worktree_status: project_harness_git_status_code(entry.worktree_status),
            path: project_harness_path(entry.path),
            previous_path: entry.previous_path.map(project_harness_path),
        }).collect(),
        recent_commits: inspection.git.recent_commits.into_iter().map(|commit| GitCommitSummary {
            id: commit.id.as_str().to_owned(),
            summary: commit.summary,
        }).collect(),
        worktrees: Vec::new(),
        managed_worktree: None,
        truncated: inspection.git.truncated,
        diagnostic: None,
    };
    let truncation = inspection.truncation.map(|truncation| {
        gate4agent_node_protocol::WorkspaceInspectionTruncationV1 {
            walk_time_budget_exceeded: truncation.walk_time_budget_exceeded,
            walk_entry_cap_exceeded: truncation.walk_entry_cap_exceeded,
            git_time_budget_exceeded: truncation.git_time_budget_exceeded,
            entries_visited: truncation.entries_visited,
            elapsed_ms: truncation.elapsed_ms,
        }
    });
    Ok((node_id, gate4agent_node_protocol::WorkspaceInspection {
        workspace_id,
        entries,
        tree_truncated: inspection.tree_truncated,
        git,
        truncation,
    }))
}

/// Node-scoped sibling projecting straight to `WorkspaceFileRead` for
/// `apply_workspace_file_read`, the same direct-mode apply function the
/// direct-C2 `ReadWorkspaceFile` reply uses.
fn project_harness_node_workspace_file(
    file: HarnessNodeWorkspaceFileV1,
) -> Result<(String, gate4agent_node_protocol::WorkspaceFileRead), String> {
    let node_id = file.origin.node_id;
    let workspace_id = WorkspaceId::new(file.origin.workspace_id)
        .map_err(|error| format!("invalid Harness runtime workspace ID: {error}"))?;
    let revision = file.revision.map(|revision| {
        WorkspaceFileRevision::new(revision.as_str().to_owned())
            .map_err(|error| format!("invalid Harness runtime file revision: {error}"))
    }).transpose()?;
    Ok((node_id, gate4agent_node_protocol::WorkspaceFileRead {
        workspace_id,
        path: project_harness_path(file.path),
        content: project_harness_file_content(file.content),
        revision,
    }))
}

/// Node-scoped sibling projecting straight to the `(commits, next_before,
/// has_more)` triple `apply_git_history` expects.
fn project_harness_node_git_history(
    page: HarnessNodeGitHistoryPageV1,
) -> (Vec<GitCommitView>, Option<String>, bool) {
    let has_more = page.next_before.is_some();
    let next_before = page.next_before.map(|id| id.as_str().to_owned());
    let commits = page.commits.into_iter().map(project_harness_git_commit).collect();
    (commits, next_before, has_more)
}

/// Node-scoped sibling projecting straight to `WorkspaceGitDiffView` for
/// `apply_git_diff`.
fn project_harness_node_git_diff(diff: HarnessNodeGitDiffV1) -> WorkspaceGitDiffView {
    let target = project_harness_git_diff_target_from_response(diff.mode, diff.path);
    let byte_len = diff.text.len().min(u32::MAX as usize) as u32;
    WorkspaceGitDiffView {
        target,
        text: diff.text,
        byte_len,
        truncated: diff.truncated,
    }
}

fn load_harness_snapshot(
    client: &HarnessOperatorClient,
) -> Result<(Vec<RedactedTaskV1>, Vec<RedactedRunV1>), String> {
    let tasks = collect_harness_task_pages(|cursor| {
        client.tasks_list(cursor, None, HARNESS_SNAPSHOT_PAGE_SIZE)
            .map_err(|error| error.to_string())
    })?;
    let runs = collect_harness_run_pages(|cursor| {
        client.runs_list(None, cursor, None, HARNESS_SNAPSHOT_PAGE_SIZE)
            .map_err(|error| error.to_string())
    })?;
    Ok((tasks, runs))
}

fn publish_harness_native_catalogs(
    client: &HarnessOperatorClient,
    updates: &mpsc::Sender<WorkerUpdate>,
    nodes: Option<&[NodeView]>,
    node_id: String,
    routes: Vec<NativeSessionCatalogRoute>,
    limit: u16,
    token: u64,
) {
    for route in routes {
        let result = (|| {
            let request_route = harness_native_session_route(nodes, &node_id, &route)
                .map_err(HarnessNativeHistoryError::projection)?;
            let response = client.catalog_native_sessions(request_route.clone(), limit)
                .map_err(|error| HarnessNativeHistoryError::from_client(&error))?;
            if response.route != request_route {
                return Err(HarnessNativeHistoryError::invalid_response());
            }
            let entries = response.entries.into_iter()
                .map(project_harness_native_catalog_entry)
                .collect::<Result<Vec<_>, _>>()
                .map_err(HarnessNativeHistoryError::projection)?;
            Ok(WorkerUpdate::NativeSessionsCataloged {
                node_id: node_id.clone(),
                route: route.clone(),
                token,
                entries,
                summary: response.summary.map(project_harness_native_catalog_summary),
            })
        })();
        let update = result.unwrap_or_else(|error: HarnessNativeHistoryError| {
            WorkerUpdate::NativeSessionCatalogFailed {
                node_id: node_id.clone(),
                route,
                token,
                message: error.message,
                unavailable: error.unavailable,
            }
        });
        if updates.blocking_send(update).is_err() {
            return;
        }
    }
}

fn publish_harness_native_page(
    client: &HarnessOperatorClient,
    updates: &mpsc::Sender<WorkerUpdate>,
    nodes: Option<&[NodeView]>,
    node_id: String,
    route: NativeSessionCatalogRoute,
    window: NativeSessionCatalogWindow,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    after_selection_id: Option<String>,
    limit: u16,
    token: u64,
) {
    let result = (|| {
        let request_route = harness_native_session_route(nodes, &node_id, &route)
            .map_err(HarnessNativeHistoryError::projection)?;
        let response = client.page_native_sessions(
            request_route.clone(),
            harness_native_catalog_window(window),
            catalog_revision,
            recent_cutoff_unix_ms,
            after_selection_id,
            limit,
        ).map_err(|error| HarnessNativeHistoryError::from_client(&error))?;
        if response.route != request_route {
            return Err(HarnessNativeHistoryError::invalid_response());
        }
        Ok(WorkerUpdate::NativeSessionsPaged {
            node_id: node_id.clone(),
            route: route.clone(),
            token,
            page: project_harness_native_catalog_page(response.page)
                .map_err(HarnessNativeHistoryError::projection)?,
        })
    })();
    let update = result.unwrap_or_else(|error: HarnessNativeHistoryError| {
        WorkerUpdate::NativeSessionPageFailed {
            node_id,
            route,
            window,
            token,
            message: error.message,
            stale_catalog: error.stale_catalog,
        }
    });
    let _ = updates.blocking_send(update);
}

fn publish_harness_native_preview(
    client: &HarnessOperatorClient,
    updates: &mpsc::Sender<WorkerUpdate>,
    nodes: Option<&[NodeView]>,
    node_id: String,
    route: NativeSessionCatalogRoute,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    selection_id: String,
    message_limit: u16,
    token: u64,
) {
    let result = (|| {
        let selection = HarnessNativeSessionSelectionV1 {
            route: harness_native_session_route(nodes, &node_id, &route)
                .map_err(HarnessNativeHistoryError::projection)?,
            catalog_revision,
            recent_cutoff_unix_ms,
            selection_id: selection_id.clone(),
        };
        let response = client.preview_native_session(selection.clone(), message_limit)
            .map_err(|error| HarnessNativeHistoryError::from_client(&error))?;
        if response.selection != selection {
            return Err(HarnessNativeHistoryError::invalid_response());
        }
        Ok(WorkerUpdate::NativeSessionPreviewed {
            node_id: node_id.clone(),
            route: route.clone(),
            catalog_revision,
            recent_cutoff_unix_ms,
            selection_id: selection_id.clone(),
            token,
            preview: project_harness_native_preview(response.preview),
        })
    })();
    let update = result.unwrap_or_else(|error: HarnessNativeHistoryError| {
        WorkerUpdate::NativeSessionPreviewFailed {
            node_id,
            route,
            catalog_revision,
            recent_cutoff_unix_ms,
            selection_id,
            token,
            message: error.message,
            unavailable: error.unavailable,
            stale_catalog: error.stale_catalog,
        }
    });
    let _ = updates.blocking_send(update);
}

struct HarnessNativeHistoryError {
    message: String,
    unavailable: bool,
    stale_catalog: bool,
}

impl HarnessNativeHistoryError {
    fn from_client(error: &gate4agent_harness_client::HarnessOperatorClientError) -> Self {
        use gate4agent_harness_client::{HarnessOperatorClientError, HarnessOperatorHostErrorV1};
        Self {
            message: error.to_string(),
            unavailable: matches!(
                error,
                HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)
                    | HarnessOperatorClientError::Unavailable
                    | HarnessOperatorClientError::ConnectionClosed
            ),
            stale_catalog: matches!(
                error,
                HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)
            ),
        }
    }

    fn invalid_response() -> Self {
        Self {
            message: "Harness native-history response correlation mismatch".to_owned(),
            unavailable: false,
            stale_catalog: false,
        }
    }

    fn projection(message: String) -> Self {
        Self { message, unavailable: false, stale_catalog: false }
    }
}

fn load_harness_runtime_inventory(
    client: &HarnessOperatorClient,
) -> Result<Vec<NodeView>, String> {
    collect_harness_runtime_inventory_pages(|cursor| {
        client.runtime_inventory_list(cursor, HARNESS_SNAPSHOT_PAGE_SIZE)
            .map_err(|error| error.to_string())
    })?.into_iter().map(project_harness_inventory_node).collect()
}

fn collect_harness_runtime_inventory_pages(
    mut fetch: impl FnMut(Option<String>)
        -> Result<gate4agent_harness_client::HarnessRuntimeInventoryPageV1, String>,
) -> Result<Vec<HarnessRuntimeNodeInventoryV1>, String> {
    let mut nodes = Vec::new();
    let mut cursor = None;
    for _ in 0..HARNESS_RUNTIME_INVENTORY_PAGE_BUDGET {
        let page = fetch(cursor.clone())?;
        if let Some(previous) = cursor.as_ref() {
            if page.nodes.first().is_some_and(|node| &node.node_id <= previous)
                || page.next_cursor.as_ref().is_some_and(|next| next <= previous)
            {
                return Err("Harness runtime inventory cursor did not advance".to_owned());
            }
        }
        if nodes.len().saturating_add(page.nodes.len())
            > HARNESS_RUNTIME_INVENTORY_ENTITY_BUDGET
        {
            return Err("Harness runtime inventory exceeded entity budget".to_owned());
        }
        cursor = page.next_cursor;
        nodes.extend(page.nodes);
        if cursor.is_none() {
            return Ok(nodes);
        }
    }
    Err("Harness runtime inventory exceeded page budget".to_owned())
}

fn collect_harness_task_pages(
    mut fetch: impl FnMut(Option<gate4agent_harness_client::HarnessTaskId>) -> Result<TaskPageV1, String>,
) -> Result<Vec<RedactedTaskV1>, String> {
    let mut tasks = Vec::new();
    let mut task_cursor = None;
    for _ in 0..HARNESS_TASK_PAGE_BUDGET {
        let page = fetch(task_cursor.clone())?;
        if let Some(previous) = task_cursor.as_ref() {
            if page.tasks.first().is_some_and(|task| &task.task_id <= previous)
                || page.next_cursor.as_ref().is_some_and(|next| next <= previous)
            {
                return Err("Harness task pagination cursor did not advance".to_owned());
            }
        }
        if tasks.len().saturating_add(page.tasks.len()) > HARNESS_TASK_ENTITY_BUDGET {
            return Err("Harness task pagination exceeded entity budget".to_owned());
        }
        task_cursor = page.next_cursor;
        tasks.extend(page.tasks);
        if task_cursor.is_none() {
            return Ok(tasks);
        }
    }
    Err("Harness task pagination exceeded page budget".to_owned())
}

fn collect_harness_run_pages(
    mut fetch: impl FnMut(Option<gate4agent_harness_client::HarnessRunId>) -> Result<RunPageV1, String>,
) -> Result<Vec<RedactedRunV1>, String> {
    let mut runs = Vec::new();
    let mut run_cursor = None;
    for _ in 0..HARNESS_RUN_PAGE_BUDGET {
        let page = fetch(run_cursor.clone())?;
        if let Some(previous) = run_cursor.as_ref() {
            if page.runs.first().is_some_and(|run| &run.run_id <= previous)
                || page.next_cursor.as_ref().is_some_and(|next| next <= previous)
            {
                return Err("Harness run pagination cursor did not advance".to_owned());
            }
        }
        if runs.len().saturating_add(page.runs.len()) > HARNESS_RUN_ENTITY_BUDGET {
            return Err("Harness run pagination exceeded entity budget".to_owned());
        }
        run_cursor = page.next_cursor;
        runs.extend(page.runs);
        if run_cursor.is_none() {
            return Ok(runs);
        }
    }
    Err("Harness run pagination exceeded page budget".to_owned())
}

fn publish_harness_snapshot(
    client: &HarnessOperatorClient,
    token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
    runtime_inventory: &Arc<Mutex<Option<Vec<NodeView>>>>,
    require_inventory_change: bool,
) {
    let update = match load_harness_snapshot(client) {
        Ok((tasks, runs)) => {
            let last_runtime_inventory = harness_runtime_inventory_snapshot(runtime_inventory);
            let mut inventory_result = load_harness_runtime_inventory(client);
            for _ in 1..HARNESS_RUNTIME_INVENTORY_RETRY_BUDGET {
                let needs_retry = match &inventory_result {
                    Ok(nodes) => harness_inventory_needs_retry(
                        last_runtime_inventory.as_deref(),
                        nodes,
                        require_inventory_change,
                    ),
                    _ => false,
                };
                if !needs_retry {
                    break;
                }
                thread::sleep(HARNESS_RUNTIME_INVENTORY_RETRY_DELAY);
                inventory_result = load_harness_runtime_inventory(client);
            }
            match inventory_result {
                Ok(nodes) => {
                    let nodes = retain_shared_harness_inventory(runtime_inventory, nodes);
                    WorkerUpdate::HarnessSnapshot { token, tasks, runs, nodes }
                }
                Err(message) => WorkerUpdate::HarnessRefreshFailed { token, message },
            }
        }
        Err(message) => WorkerUpdate::HarnessRefreshFailed { token, message },
    };
    let _ = updates.blocking_send(update);
}

fn harness_runtime_inventory_snapshot(
    runtime_inventory: &Arc<Mutex<Option<Vec<NodeView>>>>,
) -> Option<Vec<NodeView>> {
    runtime_inventory.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn retain_shared_harness_inventory(
    runtime_inventory: &Arc<Mutex<Option<Vec<NodeView>>>>,
    candidate: Vec<NodeView>,
) -> Vec<NodeView> {
    let mut last_exact = runtime_inventory.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    retain_last_exact_harness_inventory(&mut last_exact, candidate)
}

fn harness_inventory_needs_retry(
    last_exact: Option<&[NodeView]>,
    candidate: &[NodeView],
    require_change: bool,
) -> bool {
    candidate.is_empty()
        || require_change && last_exact.is_some_and(|last| last == candidate)
}

fn retain_last_exact_harness_inventory(
    last_exact: &mut Option<Vec<NodeView>>,
    candidate: Vec<NodeView>,
) -> Vec<NodeView> {
    if candidate.is_empty() {
        return last_exact.clone().unwrap_or_default();
    }
    *last_exact = Some(candidate.clone());
    candidate
}

fn publish_harness_failure(
    token: u64,
    message: String,
    updates: &mpsc::Sender<WorkerUpdate>,
) {
    let _ = updates.blocking_send(WorkerUpdate::HarnessRefreshFailed { token, message });
}

async fn c2_worker(
    endpoint: C2Endpoint,
    startup: Option<StartupRequest>,
    initial_terminal_size: TerminalSize,
    mut commands: mpsc::Receiver<AppAction>,
    updates: mpsc::Sender<WorkerUpdate>,
) {
    let mut startup = startup;
    let mut last_connect_failure = None;
    loop {
        let (handle, mut events) = match connect_c2_local(&endpoint.endpoint, &endpoint.token).await {
            Ok(connection) => {
                last_connect_failure = None;
                connection
            }
            Err(error) => {
                let failure = safe_c2_error(&error);
                if last_connect_failure.as_deref() != Some(failure.as_str()) {
                    crate::diagnostics::record_runtime(RuntimeDiagnostic::C2ReconnectFailed);
                    send_update(
                        &updates,
                        WorkerUpdate::Notice(format!(
                            "C2 control unavailable: {failure}; retrying"
                        )),
                    )
                    .await;
                    last_connect_failure = Some(failure);
                }
                reject_disconnected_commands(
                    &mut commands,
                    &updates,
                    "C2",
                    "C2 control unavailable",
                )
                .await;
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        let mut topology = handle.subscribe_topology();
        let (routed_events_tx, mut routed_events_rx) = mpsc::channel(UPDATE_QUEUE);
        let event_forwarder = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if routed_events_tx.send(event).await.is_err() {
                    return;
                }
            }
        });
        let mut routes = BTreeMap::new();
        let mut node_endpoints = BTreeMap::new();
        let mut relay_routes = BTreeMap::new();
        let initial_topology = topology.borrow_and_update().clone();
        let initial_routes = reconcile_c2_topology(
            initial_topology.as_ref(),
            &mut routes,
            &mut node_endpoints,
            &mut relay_routes,
            &updates,
        )
        .await;
        let mut transport_failed = false;
        for route in initial_routes {
            if c2_request_and_publish(
                &handle,
                route,
                NodeRequest::Snapshot,
                &updates,
                &node_endpoints,
            )
            .await.is_err() {
                crate::diagnostics::record_runtime(RuntimeDiagnostic::C2InitialSnapshotFailed);
                transport_failed = true;
                break;
            }
        }
        if transport_failed {
            event_forwarder.abort();
            disconnect_c2_nodes(&node_endpoints, &updates, "C2 control disconnected").await;
            reject_disconnected_commands(
                &mut commands,
                &updates,
                "C2",
                "C2 control disconnected",
            )
            .await;
            sleep(RECONNECT_DELAY).await;
            continue;
        }
        let startup_dispatched = match dispatch_c2_startup(
            &handle,
            &routes,
            &node_endpoints,
            &mut startup,
            initial_terminal_size,
            &updates,
        )
        .await
        {
            Ok(dispatched) => dispatched,
            Err(_) => {
                crate::diagnostics::record_runtime(RuntimeDiagnostic::C2StartupFailed);
                event_forwarder.abort();
                disconnect_c2_nodes(
                    &node_endpoints,
                    &updates,
                    "C2 control disconnected",
                )
                .await;
                reject_disconnected_commands(
                    &mut commands,
                    &updates,
                    "C2",
                    "C2 control disconnected",
                )
                .await;
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        if !startup_dispatched {
            if let Some(request) = startup.as_ref() {
                send_update(
                    &updates,
                    WorkerUpdate::Notice(format!(
                        "{}: startup waits for the node to become online at C2",
                        request.node_id
                    )),
                )
                .await;
            }
        }

        let mut snapshot_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        snapshot_tick.reset();
        let _failure = 'connected: loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { return; };
                    let Some(node_id) = action_node_id(&command).map(str::to_owned) else { continue; };
                    let Some(route) = routes.get(&node_id).cloned() else {
                        if let Some(update) = workspace_document_failed_update(
                            &command,
                            "node is offline at C2".to_owned(),
                        ) {
                            send_update(&updates, update).await;
                        }
                        if let AppAction::RefreshSessionRecordHistory {
                            node_id,
                            node_incarnation_id,
                            record_id,
                            ..
                        } = &command {
                            send_update(
                                &updates,
                                WorkerUpdate::SessionRecordHistoryRefreshFailed {
                                    node_id: node_id.clone(),
                                    record_id: record_id.clone(),
                                    incarnation_id: *node_incarnation_id,
                                    message: "node offline at C2".to_owned(),
                                },
                            ).await;
                        }
                        send_update(&updates, WorkerUpdate::Notice(format!("{node_id}: node offline at C2"))).await;
                        continue;
                    };
                    if let AppAction::BrowseHostDirectories {
                        directory,
                        after,
                        token,
                        append,
                        ..
                    } = &command
                    {
                        if let Err(error) = c2_browse_host_directories(
                            &handle,
                            route,
                            directory.clone(),
                            after.clone(),
                            *token,
                            *append,
                            &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if matches!(
                        &command,
                        AppAction::ReadWorkspaceFile { .. }
                            | AppAction::WriteWorkspaceFile { .. }
                            | AppAction::CreateWorkspaceFile { .. }
                            | AppAction::CreateWorkspaceDirectory { .. }
                            | AppAction::ReadGitHistory { .. }
                            | AppAction::ReadGitDiff { .. }
                    ) {
                        if let Err(error) = c2_workspace_document_action(
                            &handle,
                            route,
                            command,
                            &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::CatalogNativeSessions {
                        routes: catalog_routes,
                        limit,
                        token,
                        ..
                    } = &command
                    {
                        if let Err(error) = c2_catalog_native_sessions(
                            &handle,
                            route,
                            catalog_routes,
                            *limit,
                            *token,
                            &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::PageNativeSessions {
                        route: catalog_route, window, catalog_revision,
                        recent_cutoff_unix_ms, after_selection_id, limit, token, ..
                    } = &command
                    {
                        if let Err(error) = c2_page_native_sessions(
                            &handle, route, catalog_route, *window, *catalog_revision,
                            *recent_cutoff_unix_ms, after_selection_id.clone(), *limit, *token,
                            &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::PreviewNativeSession {
                        route: catalog_route, catalog_revision, recent_cutoff_unix_ms, selection_id,
                        message_limit, token, ..
                    } = &command
                    {
                        if let Err(error) = c2_preview_native_session(
                            &handle, route, catalog_route, *catalog_revision,
                            *recent_cutoff_unix_ms, selection_id,
                            *message_limit, *token, &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::PreviewSessionRecord { record_id, message_limit, token, .. } = &command {
                        if let Err(error) = c2_preview_session_record(
                            &handle, route, record_id, *message_limit, *token, &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::RefreshSessionRecordHistory {
                        node_incarnation_id, record_id, message_limit, ..
                    } = &command {
                        if let Err(error) = c2_refresh_session_record_history(
                            &handle,
                            route,
                            *node_incarnation_id,
                            record_id,
                            *message_limit,
                            &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::IndexProviderSession {
                        workspace_id, provider, identity, display_name, operation_token, ..
                    } = &command {
                        if let Err(error) = c2_index_provider_session(
                            &handle, route, workspace_id, provider, identity, display_name,
                            *operation_token, &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::IndexNativeSession {
                        route: catalog_route,
                        catalog_revision,
                        recent_cutoff_unix_ms,
                        selection_id,
                        display_name,
                        operation_token,
                        ..
                    } = &command {
                        if let Err(error) = c2_index_native_session(
                            &handle,
                            route,
                            catalog_route,
                            *catalog_revision,
                            *recent_cutoff_unix_ms,
                            selection_id,
                            display_name,
                            *operation_token,
                            &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    if let AppAction::ResumeSessionRecord {
                        record_id, rows, cols, initial_prompt, operation_token, ..
                    } = &command {
                        if let Err(error) = c2_resume_session_record(
                            &handle, route, record_id, *rows, *cols, initial_prompt,
                            *operation_token, &updates,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                        continue;
                    }
                    let requests = match command {
                        AppAction::Paste { address, text } => utf8_chunks(&text, MAX_NODE_TEXT_BYTES)
                            .into_iter()
                            .map(|chunk| NodeRequest::Paste {
                                session: wire_address(&address).expect("validated TUI session address"),
                                text: chunk.to_owned(),
                            })
                            .collect::<Vec<_>>(),
                        command => action_to_request(command).into_iter().collect(),
                    };
                    for request in requests {
                        if let Err(error) = c2_request_and_publish(
                            &handle,
                            route.clone(),
                            request,
                            &updates,
                            &node_endpoints,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                    }
                }
                event = routed_events_rx.recv() => {
                    let Some(event) = event else { break "event stream closed".to_owned(); };
                    let node_id = event.node_id.to_string();
                    if routes.get(&node_id).is_some_and(|route| {
                        route.expected_incarnation_id == event.cursor.incarnation_id
                    }) {
                        publish_c2_event(event.event, event.cursor, &updates, &node_id).await;
                    }
                }
                changed = topology.changed() => {
                    if changed.is_err() {
                        break "topology stream closed".to_owned();
                    }
                    let next = topology.borrow_and_update().clone();
                    let snapshot_routes = reconcile_c2_topology(
                        next.as_ref(),
                        &mut routes,
                        &mut node_endpoints,
                        &mut relay_routes,
                        &updates,
                    ).await;
                    for route in snapshot_routes {
                        if let Err(error) = c2_request_and_publish(
                            &handle,
                            route,
                            NodeRequest::Snapshot,
                            &updates,
                            &node_endpoints,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                    }
                    if let Err(error) = dispatch_c2_startup(
                        &handle,
                        &routes,
                        &node_endpoints,
                        &mut startup,
                        initial_terminal_size,
                        &updates,
                    ).await {
                        break 'connected safe_c2_error(&error);
                    }
                }
                _ = snapshot_tick.tick() => {
                    for route in routes.values().cloned().collect::<Vec<_>>() {
                        if let Err(error) = c2_request_and_publish(
                            &handle,
                            route,
                            NodeRequest::Snapshot,
                            &updates,
                            &node_endpoints,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                    }
                }
            }
        };
        event_forwarder.abort();
        crate::diagnostics::record_runtime(RuntimeDiagnostic::C2ControlDisconnected);
        disconnect_c2_nodes(&node_endpoints, &updates, "C2 control disconnected").await;
        reject_disconnected_commands(
            &mut commands,
            &updates,
            "C2",
            "C2 control disconnected",
        )
        .await;
        sleep(RECONNECT_DELAY).await;
    }
}

async fn c2_request_and_publish(
    handle: &C2ControlHandle,
    route: NodeRoute,
    request: NodeRequest,
    updates: &mpsc::Sender<WorkerUpdate>,
    endpoints: &BTreeMap<String, String>,
) -> Result<(), C2ControlError> {
    let requested_after = match &request {
        NodeRequest::Resync { after_sequence } => Some(*after_sequence),
        _ => None,
    };
    let routed = handle.request(route, request).await?;
    let node_id = routed.node_id.to_string();
    let incarnation_id = routed.incarnation_id;
    match routed.response {
        Ok(response) => publish_c2_response_for_request(
            response,
            updates,
            &node_id,
            incarnation_id,
            endpoints.get(&node_id).cloned().unwrap_or_default(),
            requested_after,
        )
        .await,
        Err(failure) => {
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{node_id}: {}",
                    safe_node_failure_code(failure.code)
                )),
            )
            .await;
        }
    }
    Ok(())
}

async fn c2_browse_host_directories(
    handle: &C2ControlHandle,
    route: NodeRoute,
    directory: Option<gate4agent_node_protocol::OpaqueHostPath>,
    after: Option<gate4agent_node_protocol::OpaqueHostPath>,
    token: u64,
    append: bool,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let routed = handle
        .request(route, NodeRequest::BrowseHostDirectories { directory, after })
        .await?;
    let node_id = routed.node_id.to_string();
    let update = match routed.response {
        Ok(C2NodeResponse::HostDirectoriesBrowsed { listing }) => {
            WorkerUpdate::HostDirectoriesBrowsed {
                node_id,
                token,
                append,
                listing,
            }
        }
        Ok(_) => WorkerUpdate::HostDirectoryBrowseFailed {
            node_id,
            token,
            message: "C2 returned an invalid directory browser response".to_owned(),
        },
        Err(failure) => WorkerUpdate::HostDirectoryBrowseFailed {
            node_id,
            token,
            message: safe_node_failure_code(failure.code).to_owned(),
        },
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_catalog_native_sessions(
    handle: &C2ControlHandle,
    node_route: NodeRoute,
    catalog_routes: &[NativeSessionCatalogRoute],
    limit: u16,
    token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    for catalog_route in catalog_routes {
        let request = match native_session_catalog_request(catalog_route, limit) {
            Ok(request) => request,
            Err(message) => {
                send_update(updates, WorkerUpdate::NativeSessionCatalogFailed {
                    node_id: node_route.node_id.to_string(),
                    route: catalog_route.clone(),
                    token,
                    message,
                    unavailable: false,
                }).await;
                continue;
            }
        };
        let routed = handle.request(node_route.clone(), request).await?;
        let node_id = routed.node_id.to_string();
        let update = match routed.response {
            Ok(C2NodeResponse::NativeSessionsCataloged {
                route,
                entries,
                summary,
            }) => {
                native_sessions_cataloged_update(
                    node_id, token, route, entries, summary,
                )
            }
            Ok(_) => WorkerUpdate::NativeSessionCatalogFailed {
                node_id,
                route: catalog_route.clone(),
                token,
                message: "C2 returned an invalid native session catalog response".to_owned(),
                unavailable: false,
            },
            Err(failure) => {
                let (message, unavailable) = catalog_failure_fields(failure.code, &failure.message);
                WorkerUpdate::NativeSessionCatalogFailed {
                    node_id,
                    route: catalog_route.clone(),
                    token,
                    message,
                    unavailable,
                }
            }
        };
        send_update(updates, update).await;
    }
    Ok(())
}

async fn c2_page_native_sessions(
    handle: &C2ControlHandle,
    node_route: NodeRoute,
    route: &NativeSessionCatalogRoute,
    window: NativeSessionCatalogWindow,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    after_selection_id: Option<String>,
    limit: u16,
    token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = node_route.node_id.to_string();
    let request_route = match wire_native_session_route(route) {
        Ok(value) => value,
        Err(message) => {
            send_update(updates, WorkerUpdate::NativeSessionPageFailed {
                node_id,
                route: route.clone(),
                window,
                token,
                message,
                stale_catalog: false,
            }).await;
            return Ok(());
        }
    };
    let request = NodeRequest::PageNativeSessions {
        route: request_route,
        window,
        catalog_revision,
        recent_cutoff_unix_ms,
        after_selection_id,
        limit,
    };
    let routed = handle.request(node_route, request).await?;
    let update = match routed.response {
        Ok(C2NodeResponse::NativeSessionsPaged { route, page }) => {
            WorkerUpdate::NativeSessionsPaged {
                node_id: routed.node_id.to_string(),
                route: project_native_session_route(route),
                token,
                page,
            }
        }
        Ok(_) => WorkerUpdate::NativeSessionPageFailed {
            node_id: routed.node_id.to_string(),
            route: route.clone(),
            window,
            token,
            message: "C2 returned an invalid native session page response".to_owned(),
            stale_catalog: false,
        },
        Err(failure) => {
            let stale_catalog =
                failure.code == NodeFailureCode::StaleNativeSessionCatalog;
            WorkerUpdate::NativeSessionPageFailed {
                node_id: routed.node_id.to_string(),
                route: route.clone(),
                window,
                token,
                message: safe_node_failure_code(failure.code).to_owned(),
                stale_catalog,
            }
        }
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_preview_native_session(
    handle: &C2ControlHandle,
    node_route: NodeRoute,
    catalog_route: &NativeSessionCatalogRoute,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    selection_id: &str,
    message_limit: u16,
    token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = node_route.node_id.to_string();
    let selection = match wire_native_session_selection(
        catalog_route,
        catalog_revision,
        recent_cutoff_unix_ms,
        selection_id,
    ) {
        Ok(value) => value,
        Err(message) => {
            send_update(updates, WorkerUpdate::NativeSessionPreviewFailed {
                node_id, route: catalog_route.clone(), catalog_revision,
                recent_cutoff_unix_ms, selection_id: selection_id.to_owned(), token, message,
                unavailable: false, stale_catalog: false,
            }).await;
            return Ok(());
        }
    };
    let routed = handle.request(node_route, NodeRequest::PreviewNativeSession {
        selection,
        message_limit,
    }).await?;
    let update = match routed.response {
        Ok(C2NodeResponse::NativeSessionPreviewed { selection, preview }) => {
            WorkerUpdate::NativeSessionPreviewed {
                node_id,
                route: project_native_session_route(selection.route),
                catalog_revision: selection.catalog_revision,
                recent_cutoff_unix_ms: selection.recent_cutoff_unix_ms,
                selection_id: selection.selection_id,
                token,
                preview,
            }
        }
        Ok(_) => WorkerUpdate::NativeSessionPreviewFailed {
            node_id, route: catalog_route.clone(), catalog_revision, recent_cutoff_unix_ms,
            selection_id: selection_id.to_owned(), token,
            message: "C2 returned an invalid native session preview response".to_owned(),
            unavailable: false, stale_catalog: false,
        },
        Err(failure) => {
            let stale_catalog =
                failure.code == NodeFailureCode::StaleNativeSessionCatalog;
            let (message, unavailable) = catalog_failure_fields(failure.code, &failure.message);
            WorkerUpdate::NativeSessionPreviewFailed {
                node_id, route: catalog_route.clone(), catalog_revision, recent_cutoff_unix_ms,
                selection_id: selection_id.to_owned(), token, message, unavailable, stale_catalog,
            }
        }
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_preview_session_record(
    handle: &C2ControlHandle,
    route: NodeRoute,
    record_id: &str,
    message_limit: u16,
    token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = route.node_id.to_string();
    let Some(record_id_value) = SessionRecordId::new(record_id.to_owned()).ok() else {
        send_update(updates, WorkerUpdate::SessionRecordPreviewFailed {
            node_id, record_id: record_id.to_owned(), token,
            message: "invalid session record ID".to_owned(), unavailable: false,
        }).await;
        return Ok(());
    };
    let routed = handle.request(route, NodeRequest::PreviewSessionRecord {
        record_id: record_id_value,
        message_limit,
    }).await?;
    let update = match routed.response {
        Ok(C2NodeResponse::SessionRecordPreviewed { record_id, preview }) => {
            WorkerUpdate::SessionRecordPreviewed {
                node_id, record_id: record_id.to_string(), token, preview,
            }
        }
        Ok(_) => WorkerUpdate::SessionRecordPreviewFailed {
            node_id, record_id: record_id.to_owned(), token,
            message: "C2 returned an invalid session record preview response".to_owned(), unavailable: false,
        },
        Err(failure) => {
            let (message, unavailable) = catalog_failure_fields(failure.code, &failure.message);
            WorkerUpdate::SessionRecordPreviewFailed {
                node_id, record_id: record_id.to_owned(), token, message, unavailable,
            }
        }
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_refresh_session_record_history(
    handle: &C2ControlHandle,
    route: NodeRoute,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    record_id: &str,
    message_limit: u16,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = route.node_id.to_string();
    if !history_refresh_incarnation_matches(incarnation_id, route.expected_incarnation_id) {
        send_update(updates, WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id,
            record_id: record_id.to_owned(),
            incarnation_id,
            message: "node incarnation changed".to_owned(),
        }).await;
        return Ok(());
    }
    let Some(record_id_value) = SessionRecordId::new(record_id.to_owned()).ok() else {
        send_update(updates, WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id,
            record_id: record_id.to_owned(),
            incarnation_id,
            message: "invalid session record ID".to_owned(),
        }).await;
        return Ok(());
    };
    let routed = match handle.request(route, NodeRequest::PreviewSessionRecord {
        record_id: record_id_value,
        message_limit,
    }).await {
        Ok(routed) => routed,
        Err(error) => {
            send_update(updates, WorkerUpdate::SessionRecordHistoryRefreshFailed {
                node_id,
                record_id: record_id.to_owned(),
                incarnation_id,
                message: "C2 transport unavailable".to_owned(),
            }).await;
            return Err(error);
        }
    };
    let update = c2_session_record_history_refresh_update(
        node_id,
        record_id,
        incarnation_id,
        routed.response,
    );
    send_update(updates, update).await;
    Ok(())
}

async fn c2_index_provider_session(
    handle: &C2ControlHandle,
    route: NodeRoute,
    workspace_id: &str,
    provider: &Provider,
    identity: &ProviderSessionIdentity,
    display_name: &str,
    operation_token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = route.node_id.to_string();
    let source_session_id = identity.id.clone();
    let request = match WorkspaceId::new(workspace_id) {
        Ok(workspace_id) => NodeRequest::IndexProviderSession {
            workspace_id,
            provider: map_provider(provider.clone()),
            identity: identity.clone(),
            display_name: display_name.to_owned(),
        },
        Err(_) => {
            send_update(updates, WorkerUpdate::ExistingSessionOperationFailed {
                node_id, record_id: None, indexing: true, operation_token,
                message: "invalid workspace ID".to_owned(),
                stale_catalog: false,
            }).await;
            return Ok(());
        }
    };
    let routed = handle.request(route, request).await?;
    let update = match routed.response {
        Ok(C2NodeResponse::ProviderSessionIndexed { record }) => {
            let identity_matches = record.provider_identity_present;
            WorkerUpdate::ProviderSessionIndexed {
            record: project_c2_managed_session(&node_id, record), identity_matches,
            node_id, workspace_id: workspace_id.to_owned(), provider: provider.clone(),
            session_id: source_session_id, operation_token,
        }},
        Ok(_) => WorkerUpdate::ExistingSessionOperationFailed {
            node_id, record_id: None, indexing: true, operation_token,
            message: "C2 returned an invalid provider session index response".to_owned(),
            stale_catalog: false,
        },
        Err(failure) => WorkerUpdate::ExistingSessionOperationFailed {
            node_id, record_id: None, indexing: true, operation_token,
            message: safe_node_failure_code(failure.code).to_owned(),
            stale_catalog: false,
        },
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_index_native_session(
    handle: &C2ControlHandle,
    node_route: NodeRoute,
    route: &NativeSessionCatalogRoute,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    selection_id: &str,
    display_name: &str,
    operation_token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = node_route.node_id.to_string();
    let selection = match wire_native_session_selection(
        route,
        catalog_revision,
        recent_cutoff_unix_ms,
        selection_id,
    ) {
        Ok(selection) => selection,
        Err(message) => {
            send_update(updates, WorkerUpdate::ExistingSessionOperationFailed {
                node_id,
                record_id: None,
                indexing: true,
                operation_token,
                message,
                stale_catalog: false,
            }).await;
            return Ok(());
        }
    };
    let routed = handle.request(node_route, NodeRequest::IndexNativeSession {
        selection,
        display_name: display_name.to_owned(),
    }).await?;
    let update = match routed.response {
        Ok(C2NodeResponse::NativeSessionIndexed { selection, record }) => {
            WorkerUpdate::NativeSessionIndexed {
                node_id: node_id.clone(),
                route: project_native_session_route(selection.route),
                catalog_revision: selection.catalog_revision,
                recent_cutoff_unix_ms: selection.recent_cutoff_unix_ms,
                selection_id: selection.selection_id,
                record: project_c2_managed_session(&node_id, record),
                operation_token,
            }
        }
        Ok(_) => WorkerUpdate::ExistingSessionOperationFailed {
            node_id,
            record_id: None,
            indexing: true,
            operation_token,
            message: "C2 returned an invalid native session index response".to_owned(),
            stale_catalog: false,
        },
        Err(failure) => {
            let stale_catalog =
                failure.code == NodeFailureCode::StaleNativeSessionCatalog;
            WorkerUpdate::ExistingSessionOperationFailed {
                node_id,
                record_id: None,
                indexing: true,
                operation_token,
                message: safe_node_failure_code(failure.code).to_owned(),
                stale_catalog,
            }
        }
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_resume_session_record(
    handle: &C2ControlHandle,
    route: NodeRoute,
    record_id: &str,
    rows: u16,
    cols: u16,
    initial_prompt: &Option<String>,
    operation_token: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = route.node_id.to_string();
    let Some(record_id_value) = SessionRecordId::new(record_id.to_owned()).ok() else {
        send_update(updates, WorkerUpdate::ExistingSessionOperationFailed {
            node_id, record_id: Some(record_id.to_owned()), indexing: false, operation_token,
            message: "invalid session record ID".to_owned(),
            stale_catalog: false,
        }).await;
        return Ok(());
    };
    let routed = handle.request(route, NodeRequest::ResumeSessionRecord {
        record_id: record_id_value,
        terminal_size: TerminalSize { rows, columns: cols },
        initial_prompt: initial_prompt.clone(),
    }).await?;
    match routed.response {
        Ok(C2NodeResponse::SessionRecordResumed { record, session }) => {
            send_update(updates, WorkerUpdate::SessionRecordResumed {
                record: project_c2_managed_session(&node_id, record),
                session: project_wire_address(&node_id, session),
                operation_token,
            }).await;
        }
        Ok(_) => send_update(updates, WorkerUpdate::ExistingSessionOperationFailed {
            node_id, record_id: Some(record_id.to_owned()), indexing: false, operation_token,
            message: "C2 returned an invalid session resume response".to_owned(),
            stale_catalog: false,
        }).await,
        Err(failure) => send_update(updates, WorkerUpdate::ExistingSessionOperationFailed {
            node_id, record_id: Some(record_id.to_owned()), indexing: false, operation_token,
            message: safe_node_failure_code(failure.code).to_owned(),
            stale_catalog: false,
        }).await,
    }
    Ok(())
}

fn workspace_document_failed_update(
    action: &AppAction,
    message: String,
) -> Option<WorkerUpdate> {
    match action {
        AppAction::ReadWorkspaceFile {
            node_id,
            workspace_id,
            path,
            token,
        } => Some(WorkerUpdate::WorkspaceFileFailed {
            key: WorkspaceFileTabKey {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            },
            token: *token,
            message,
            save: false,
        }),
        AppAction::WriteWorkspaceFile {
            node_id,
            workspace_id,
            path,
            token,
            ..
        } => Some(WorkerUpdate::WorkspaceFileFailed {
            key: WorkspaceFileTabKey {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            },
            token: *token,
            message,
            save: true,
        }),
        AppAction::CreateWorkspaceFile {
            node_id,
            workspace_id,
            path,
            token,
        } => Some(WorkerUpdate::WorkspaceEntryCreateFailed {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            kind: WorkspaceEntryKind::File,
            token: *token,
            message,
        }),
        AppAction::CreateWorkspaceDirectory {
            node_id,
            workspace_id,
            path,
            token,
        } => Some(WorkerUpdate::WorkspaceEntryCreateFailed {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            kind: WorkspaceEntryKind::Directory,
            token: *token,
            message,
        }),
        AppAction::ReadGitHistory {
            token,
            destination,
            ..
        } => Some(WorkerUpdate::GitHistoryFailed {
            destination: destination.clone(),
            token: *token,
            message,
        }),
        AppAction::ReadGitDiff {
            token,
            destination,
            ..
        } => Some(WorkerUpdate::GitDiffFailed {
            destination: destination.clone(),
            token: *token,
            message,
        }),
        _ => None,
    }
}

fn git_history_read_update(
    destination: WorkspaceGitRequestDestination,
    token: u64,
    page: GitHistoryPage,
) -> WorkerUpdate {
    WorkerUpdate::GitHistoryRead { destination, token, page }
}

fn git_diff_read_update(
    destination: WorkspaceGitRequestDestination,
    token: u64,
    diff: GitDiff,
) -> WorkerUpdate {
    WorkerUpdate::GitDiffRead { destination, token, diff }
}

async fn c2_workspace_entry_create_action(
    handle: &C2ControlHandle,
    route: NodeRoute,
    action: AppAction,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    let node_id = route.node_id.to_string();
    let (workspace_id, path, token, kind) = match &action {
        AppAction::CreateWorkspaceFile { workspace_id, path, token, .. } => {
            (workspace_id.clone(), path.clone(), *token, WorkspaceEntryKind::File)
        }
        AppAction::CreateWorkspaceDirectory { workspace_id, path, token, .. } => {
            (workspace_id.clone(), path.clone(), *token, WorkspaceEntryKind::Directory)
        }
        _ => return Ok(()),
    };
    let Ok(wire_workspace_id) = WorkspaceId::new(workspace_id.clone()) else {
        if let Some(update) = workspace_document_failed_update(
            &action,
            "workspace route is invalid".to_owned(),
        ) {
            send_update(updates, update).await;
        }
        return Ok(());
    };
    let request = match kind {
        WorkspaceEntryKind::File => NodeRequest::CreateWorkspaceFile {
            workspace_id: wire_workspace_id,
            path: path.clone(),
        },
        WorkspaceEntryKind::Directory => NodeRequest::CreateWorkspaceDirectory {
            workspace_id: wire_workspace_id,
            path: path.clone(),
        },
    };
    let routed = match handle.request(route, request).await {
        Ok(routed) => routed,
        Err(error) => {
            if let Some(update) = workspace_document_failed_update(&action, safe_c2_error(&error)) {
                send_update(updates, update).await;
            }
            return Err(error);
        }
    };
    let update = match routed.response {
        Ok(C2NodeResponse::WorkspaceFileCreated { file }) if kind == WorkspaceEntryKind::File => {
            WorkerUpdate::WorkspaceFileCreated { node_id, token, file }
        }
        Ok(C2NodeResponse::WorkspaceDirectoryCreated { workspace_id, entry })
            if kind == WorkspaceEntryKind::Directory =>
        {
            WorkerUpdate::WorkspaceDirectoryCreated {
                node_id,
                workspace_id: workspace_id.to_string(),
                token,
                entry,
            }
        }
        Ok(_) => WorkerUpdate::WorkspaceEntryCreateFailed {
            node_id,
            workspace_id,
            path,
            kind,
            token,
            message: "C2 returned an invalid workspace entry creation response".to_owned(),
        },
        Err(failure) => WorkerUpdate::WorkspaceEntryCreateFailed {
            node_id,
            workspace_id,
            path,
            kind,
            token,
            message: safe_node_failure_code(failure.code).to_owned(),
        },
    };
    send_update(updates, update).await;
    Ok(())
}

async fn c2_workspace_document_action(
    handle: &C2ControlHandle,
    route: NodeRoute,
    action: AppAction,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<(), C2ControlError> {
    if matches!(
        &action,
        AppAction::CreateWorkspaceFile { .. } | AppAction::CreateWorkspaceDirectory { .. }
    ) {
        return c2_workspace_entry_create_action(handle, route, action, updates).await;
    }
    let node_id = route.node_id.to_string();
    let (request, token, file_key, git_destination, save, git_diff) = match action {
        AppAction::ReadWorkspaceFile { workspace_id, path, token, .. } => {
            let key = WorkspaceFileTabKey {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            };
            let Ok(workspace_id_value) = WorkspaceId::new(workspace_id) else {
                send_update(
                    updates,
                    WorkerUpdate::WorkspaceFileFailed {
                        key,
                        token,
                        message: "workspace route is invalid".to_owned(),
                        save: false,
                    },
                )
                .await;
                return Ok(());
            };
            (
                NodeRequest::ReadWorkspaceFile {
                    workspace_id: workspace_id_value,
                    path,
                },
                token,
                Some(key),
                None,
                false,
                false,
            )
        }
        AppAction::WriteWorkspaceFile {
            workspace_id,
            path,
            expected_revision,
            text,
            token,
            ..
        } => {
            let key = WorkspaceFileTabKey {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            };
            let Ok(workspace_id) = WorkspaceId::new(workspace_id) else {
                send_update(
                    updates,
                    WorkerUpdate::WorkspaceFileFailed {
                        key,
                        token,
                        message: "workspace route is invalid".to_owned(),
                        save: true,
                    },
                )
                .await;
                return Ok(());
            };
            let Ok(expected_revision) = WorkspaceFileRevision::new(expected_revision) else {
                send_update(
                    updates,
                    WorkerUpdate::WorkspaceFileFailed {
                        key,
                        token,
                        message: "invalid saved file revision".to_owned(),
                        save: true,
                    },
                )
                .await;
                return Ok(());
            };
            (
                NodeRequest::WriteWorkspaceFile {
                    workspace_id,
                    path,
                    expected_revision,
                    text,
                },
                token,
                Some(key),
                None,
                true,
                false,
            )
        }
        AppAction::ReadGitHistory {
            workspace_id,
            path,
            before,
            limit,
            token,
            destination,
            ..
        } => {
            let Some(workspace_id) = WorkspaceId::new(workspace_id).ok() else {
                send_update(
                    updates,
                    WorkerUpdate::GitHistoryFailed {
                        destination,
                        token,
                        message: "workspace route is invalid".to_owned(),
                    },
                )
                .await;
                return Ok(());
            };
            let before = match before.map(GitObjectId::new).transpose() {
                Ok(before) => before,
                Err(_) => {
                    send_update(
                        updates,
                        WorkerUpdate::GitHistoryFailed {
                            destination,
                            token,
                            message: "Git history cursor is invalid".to_owned(),
                        },
                    )
                    .await;
                    return Ok(());
                }
            };
            (
                NodeRequest::ReadGitHistory {
                    workspace_id,
                    path,
                    before,
                    limit,
                },
                token,
                None,
                Some(destination),
                false,
                false,
            )
        }
        AppAction::ReadGitDiff {
            workspace_id,
            target,
            token,
            destination,
            ..
        } => {
            let Some(workspace_id) = WorkspaceId::new(workspace_id).ok() else {
                send_update(
                    updates,
                    WorkerUpdate::GitDiffFailed {
                        destination,
                        token,
                        message: "workspace route is invalid".to_owned(),
                    },
                )
                .await;
                return Ok(());
            };
            let Ok(request) = wire_git_diff_request(target) else {
                send_update(
                    updates,
                    WorkerUpdate::GitDiffFailed {
                        destination,
                        token,
                        message: "Git diff target is invalid".to_owned(),
                    },
                )
                .await;
                return Ok(());
            };
            (
                NodeRequest::ReadGitDiff { workspace_id, request },
                token,
                None,
                Some(destination),
                false,
                true,
            )
        }
        _ => return Ok(()),
    };
    let routed = match handle.request(route, request).await {
        Ok(routed) => routed,
        Err(error) => {
            let message = safe_c2_error(&error);
            let update = match (file_key, git_destination) {
                (Some(key), _) => WorkerUpdate::WorkspaceFileFailed {
                    key,
                    token,
                    message,
                    save,
                },
                (_, Some(destination)) if git_diff => WorkerUpdate::GitDiffFailed {
                    destination,
                    token,
                    message,
                },
                (_, Some(destination)) => WorkerUpdate::GitHistoryFailed {
                    destination,
                    token,
                    message,
                },
                _ => return Err(error),
            };
            send_update(updates, update).await;
            return Err(error);
        }
    };
    let update = match routed.response {
        Ok(C2NodeResponse::WorkspaceFileRead { file }) => WorkerUpdate::WorkspaceFileRead {
            node_id,
            token,
            file,
        },
        Ok(C2NodeResponse::WorkspaceFileWritten { file }) => {
            WorkerUpdate::WorkspaceFileWritten { node_id, token, file }
        }
        Ok(C2NodeResponse::GitHistoryRead { page, .. }) => git_history_read_update(
            git_destination.expect("git history destination"),
            token,
            page,
        ),
        Ok(C2NodeResponse::GitDiffRead { diff, .. }) => git_diff_read_update(
            git_destination.expect("git diff destination"),
            token,
            diff,
        ),
        Ok(_) => match (file_key, git_destination) {
            (Some(key), _) => WorkerUpdate::WorkspaceFileFailed {
                key,
                token,
                message: "C2 returned an invalid workspace file response".to_owned(),
                save,
            },
            (_, Some(destination)) if git_diff => WorkerUpdate::GitDiffFailed {
                destination,
                token,
                message: "C2 returned an invalid Git diff response".to_owned(),
            },
            (_, Some(destination)) => WorkerUpdate::GitHistoryFailed {
                destination,
                token,
                message: "C2 returned an invalid Git history response".to_owned(),
            },
            _ => return Ok(()),
        },
        Err(failure) => match (file_key, git_destination) {
            (Some(key), _) => WorkerUpdate::WorkspaceFileFailed {
                key,
                token,
                message: safe_node_failure_code(failure.code).to_owned(),
                save,
            },
            (_, Some(destination)) if git_diff => WorkerUpdate::GitDiffFailed {
                destination,
                token,
                message: safe_node_failure_code(failure.code).to_owned(),
            },
            (_, Some(destination)) => WorkerUpdate::GitHistoryFailed {
                destination,
                token,
                message: safe_node_failure_code(failure.code).to_owned(),
            },
            _ => return Ok(()),
        },
    };
    send_update(updates, update).await;
    Ok(())
}

async fn publish_c2_response(
    response: C2NodeResponse,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
    endpoint: String,
) {
    publish_c2_response_for_request(
        response,
        updates,
        node_id,
        incarnation_id,
        endpoint,
        None,
    )
    .await;
}

async fn publish_c2_response_for_request(
    response: C2NodeResponse,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
    endpoint: String,
    requested_after: Option<u64>,
) {
    match response {
        C2NodeResponse::Snapshot { event_sequence, snapshot, .. } => {
            send_update(
                updates,
                WorkerUpdate::C2Snapshot {
                    expected_node_id: node_id.to_owned(),
                    endpoint,
                    incarnation_id,
                    snapshot,
                    event_sequence,
                },
            )
            .await;
        }
        C2NodeResponse::Resync {
            event_sequence,
            oldest_available_sequence,
            snapshot,
            events,
        } => {
            let observation_batch = requested_after.map(|requested_after| {
                c2_observation_resync_batch(
                    node_id,
                    incarnation_id,
                    requested_after,
                    event_sequence,
                    oldest_available_sequence,
                    &events,
                )
            });
            for envelope in events {
                publish_c2_event(
                    envelope.event,
                    gate4agent_c2_protocol::NodeCursor {
                        incarnation_id,
                        sequence: envelope.sequence,
                    },
                    updates,
                    node_id,
                ).await;
            }
            if let Some(observation_batch) = observation_batch {
                match observation_batch {
                    Ok(batch) => {
                        send_update(updates, WorkerUpdate::ObservationResync(batch)).await;
                    }
                    Err(message) => {
                        send_update(updates, WorkerUpdate::Notice(message)).await;
                    }
                }
            }
            send_update(
                updates,
                WorkerUpdate::C2Snapshot {
                    expected_node_id: node_id.to_owned(),
                    endpoint,
                    incarnation_id,
                    snapshot,
                    event_sequence,
                },
            )
            .await;
        }
        C2NodeResponse::WorkspaceInspected { inspection } => {
            send_update(
                updates,
                WorkerUpdate::WorkspaceInspected {
                    node_id: node_id.to_owned(),
                    inspection: project_c2_workspace_inspection(inspection),
                },
            )
            .await;
        }
        C2NodeResponse::HostDirectoriesBrowsed { .. } => {}
        C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::WorkspaceFileWritten { .. }
        | C2NodeResponse::WorkspaceFileCreated { .. }
        | C2NodeResponse::WorkspaceDirectoryCreated { .. }
        | C2NodeResponse::GitHistoryRead { .. }
        | C2NodeResponse::GitDiffRead { .. } => {}
        C2NodeResponse::SpawnAccepted { session } => {
            send_update(updates, WorkerUpdate::OpenSession(project_wire_address(node_id, session))).await;
        }
        C2NodeResponse::SpawnSpecAccepted { receipt } => {
            let address = project_wire_address(node_id, receipt.session.clone());
            let opens_pty = receipt.mode == SessionMode::Pty;
            send_update(
                updates,
                WorkerUpdate::SpawnReceipt(receipt),
            )
            .await;
            if opens_pty {
                send_update(updates, WorkerUpdate::OpenSession(address)).await;
            }
        }
        C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt } => {
            let workspace_id = receipt.lease.workspace_id.to_string();
            let address = project_wire_address(node_id, receipt.spawn.session.clone());
            let opens_pty = receipt.spawn.mode == SessionMode::Pty;
            send_update(
                updates,
                WorkerUpdate::ManagedSpawnReceipt(receipt),
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::SelectWorkspace {
                    node_id: node_id.to_owned(),
                    workspace_id,
                },
            )
            .await;
            if opens_pty {
                send_update(
                    updates,
                    WorkerUpdate::OpenSession(address),
                )
                .await;
            }
        }
        C2NodeResponse::ManagedWorktreeCleanup { lease } => {
            send_update(
                updates,
                WorkerUpdate::ManagedWorktreeLease(lease),
            )
            .await;
        }
        C2NodeResponse::SessionRecordUpdated { record } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
            )
            .await;
        }
        C2NodeResponse::ProviderSessionIndexed { record } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
            )
            .await;
        }
        C2NodeResponse::SessionRecordResumed { record, session } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
            )
            .await;
            let _ = session;
        }
        C2NodeResponse::SessionRecordForgotten { record_id } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordRemoved {
                    node_id: node_id.to_owned(),
                    record_id: record_id.to_string(),
                },
            )
            .await;
        }
        C2NodeResponse::NativeSessionIndexed { .. }
        | C2NodeResponse::NativeSessionsCataloged { .. }
        | C2NodeResponse::NativeSessionsPaged { .. } => {}
        C2NodeResponse::HistoryDiscovered { session, candidates } => {
            send_update(
                updates,
                WorkerUpdate::HistoryDiscovered {
                    source: project_wire_address(node_id, session),
                    candidates,
                },
            )
            .await;
        }
        C2NodeResponse::HistoryLoaded {
            session,
            session_id,
            message_count,
            completed_turn_count,
        } => {
            send_update(
                updates,
                WorkerUpdate::HistoryLoaded {
                    source: project_wire_address(node_id, session),
                    session_id,
                    message_count,
                    completed_turn_count,
                },
            )
            .await;
        }
        C2NodeResponse::ContextPackExported { context }
        | C2NodeResponse::ContextPackForSessionRecordExported { context, .. } => {
            send_update(updates, WorkerUpdate::ContextExported(context)).await;
        }
        C2NodeResponse::ContextPackForgotten { context_id } => {
            send_update(updates, WorkerUpdate::ContextForgotten(context_id)).await;
        }
        C2NodeResponse::WorkspaceRegistered { workspace } => {
            publish_workspace_registered(
                updates,
                node_id,
                WorkspaceSnapshotUpdate::C2(workspace),
            )
            .await;
        }
        C2NodeResponse::StandaloneWorkspaceCreated { workspace } => {
            publish_workspace_registered(
                updates,
                node_id,
                WorkspaceSnapshotUpdate::C2(workspace),
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{node_id}: standalone repository created and ready to Launch"
                )),
            )
            .await;
        }
        C2NodeResponse::WorkspaceUnregistered { workspace_id } => {
            publish_workspace_removed(updates, node_id, workspace_id.to_string()).await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: space {workspace_id} unregistered")),
            )
            .await;
        }
        C2NodeResponse::WorktreeCreated { worktree, workspace } => {
            send_update(
                updates,
                WorkerUpdate::SelectWorkspace {
                    node_id: node_id.to_owned(),
                    workspace_id: workspace.workspace_id.to_string(),
                },
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: worktree {} created", host_path_display(&worktree.path))),
            )
            .await;
        }
        C2NodeResponse::WorktreeRemoved { target_root, .. } => {
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: worktree {} removed", host_path_display(&target_root))),
            )
            .await;
        }
        C2NodeResponse::Controller { .. }
        | C2NodeResponse::NativeSessionPreviewed { .. }
        | C2NodeResponse::SessionRecordPreviewed { .. }
        | C2NodeResponse::Armed { .. }
        | C2NodeResponse::Spawned { .. }
        | C2NodeResponse::Activated { .. }
        | C2NodeResponse::Aborted { .. }
        | C2NodeResponse::ReplyChunkAccepted { .. }
        | C2NodeResponse::CallRejected { .. }
        | C2NodeResponse::DeliveryStageBegun { .. }
        | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
        | C2NodeResponse::DeliveryCommitted { .. }
        | C2NodeResponse::DeliveryStageAborted { .. }
        | C2NodeResponse::DurableContextPackResolved { .. }
        | C2NodeResponse::Accepted => {}
        C2NodeResponse::ShuttingDown => {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: node_id.to_owned(),
                    endpoint,
                    state: ConnectionState::Disconnected("node shutdown".to_owned()),
                },
            )
            .await;
        }
    }
}

async fn publish_c2_event(
    event: C2NodeEvent,
    cursor: gate4agent_c2_protocol::NodeCursor,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
) {
    if matches!(event, C2NodeEvent::HarnessMcpReadCall { .. }) {
        return;
    }
    let event = match event {
        C2NodeEvent::Observation { address, observation } => {
            let update = observation_ingress_envelope(
                node_id,
                cursor,
                address,
                observation,
                ObservationTransport::C2,
            )
            .map(WorkerUpdate::ObservationIngress)
            .unwrap_or_else(WorkerUpdate::Notice);
            send_update(updates, update).await;
            return;
        }
        C2NodeEvent::ManagedObservation { record_id, observation } => {
            let update = managed_observation_ingress_envelope(
                node_id,
                cursor,
                record_id,
                observation,
                ObservationTransport::C2,
            )
            .map(WorkerUpdate::ObservationIngress)
            .unwrap_or_else(WorkerUpdate::Notice);
            send_update(updates, update).await;
            return;
        }
        event => event,
    };
    let event = match event {
        C2NodeEvent::Control { address, event } => {
            if let Some(notice) = safe_c2_control_notice(node_id, &address, &event) {
                Some(C2EventUpdate::Notice(notice))
            } else {
                None
            }
        }
        C2NodeEvent::TerminalFrame { address, frame } => Some(
            C2EventUpdate::TerminalFrame {
                address: project_wire_address(node_id, address),
                frame,
            },
        ),
        C2NodeEvent::SessionRecordUpserted { record } => Some(
            C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
        ),
        C2NodeEvent::SessionRecordRemoved { record_id } => Some(
            C2EventUpdate::SessionRecordRemoved { record_id: record_id.to_string() },
        ),
        C2NodeEvent::ManagedWorktreeUpserted { lease } => {
            Some(C2EventUpdate::ManagedWorktreeLease(lease))
        }
        C2NodeEvent::ManagedWorktreeRemoved { lease_id } => Some(
            C2EventUpdate::ManagedWorktreeRemoved { lease_id: lease_id.to_string() },
        ),
        C2NodeEvent::WorkspaceAdded { workspace } => {
            Some(C2EventUpdate::WorkspaceUpserted(workspace))
        }
        C2NodeEvent::WorkspaceRemoved { workspace_id } => Some(
            C2EventUpdate::WorkspaceRemoved { workspace_id: workspace_id.to_string() },
        ),
        C2NodeEvent::ResyncRequired { .. } => Some(C2EventUpdate::ResyncRequired),
        C2NodeEvent::ControllerChanged { .. } => Some(C2EventUpdate::Ignored),
        C2NodeEvent::HarnessMcpReadCall { .. } => {
            unreachable!("harness MCP event handled before projection")
        }
        C2NodeEvent::Observation { .. } | C2NodeEvent::ManagedObservation { .. } => {
            unreachable!("observation handled before projection")
        }
    };
    send_update(updates, WorkerUpdate::C2Event {
        node_id: node_id.to_owned(),
        cursor,
        event: event.unwrap_or(C2EventUpdate::Ignored),
    }).await;
}

fn project_wire_address(node_id: &str, address: WireSessionAddress) -> SessionAddress {
    SessionAddress {
        node_id: node_id.to_owned(),
        workspace_id: address.workspace_id.to_string(),
        instance_id: address.session.instance_id.0,
        generation: address.session.generation.0,
    }
}

fn observation_ingress_envelope(
    node_id: &str,
    cursor: gate4agent_c2_protocol::NodeCursor,
    address: WireSessionAddress,
    observation: ObservationV1,
    transport: ObservationTransport,
) -> Result<ObservationIngressEnvelope, String> {
    let node_id = ObservationNodeId::new(node_id)
        .map_err(|error| format!("observation route rejected: {error}"))?;
    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "observation receive clock is before the Unix epoch".to_owned())?
        .as_millis()
        .min(u64::MAX as u128) as u64;
    Ok(ObservationIngressEnvelope {
        node_id: node_id.clone(),
        cursor,
        received_at_ms: received_at_ms.max(1),
        transport,
        payload: ObservationIngressPayload::Observation {
            address: ObservationTarget::Runtime {
                key: RuntimeSessionKey {
                    node_id,
                    incarnation_id: cursor.incarnation_id,
                    workspace_id: address.workspace_id,
                    instance_id: address.session.instance_id,
                    generation: address.session.generation,
                },
            },
            observation,
        },
    })
}

fn managed_observation_ingress_envelope(
    node_id: &str,
    cursor: gate4agent_c2_protocol::NodeCursor,
    record_id: SessionRecordId,
    observation: ObservationV1,
    transport: ObservationTransport,
) -> Result<ObservationIngressEnvelope, String> {
    let node_id = ObservationNodeId::new(node_id)
        .map_err(|error| format!("managed observation route rejected: {error}"))?;
    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "observation receive clock is before the Unix epoch".to_owned())?
        .as_millis()
        .min(u64::MAX as u128) as u64;
    Ok(ObservationIngressEnvelope {
        node_id: node_id.clone(),
        cursor,
        received_at_ms: received_at_ms.max(1),
        transport,
        payload: ObservationIngressPayload::Observation {
            address: ObservationTarget::Managed {
                key: ManagedSessionKey {
                    node_id,
                    incarnation_id: cursor.incarnation_id,
                    record_id,
                },
            },
            observation,
        },
    })
}

fn observation_resync_gaps(
    requested_after: u64,
    high_watermark: u64,
    oldest_available_sequence: u64,
    sequences: &[u64],
) -> Result<Vec<ObservationGap>, String> {
    if high_watermark < requested_after {
        return Err("Session Monitor resync cursor regressed".to_owned());
    }
    let max_floor = high_watermark.checked_add(1).unwrap_or(u64::MAX);
    let requested_next = requested_after.checked_add(1).unwrap_or(u64::MAX);
    let minimum_event_sequence = requested_next.max(oldest_available_sequence);
    if oldest_available_sequence == 0
        || oldest_available_sequence > max_floor
        || (high_watermark == requested_after && !sequences.is_empty())
        || sequences.iter().any(|sequence| {
            *sequence < minimum_event_sequence || *sequence > high_watermark
        })
        || sequences.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err("Session Monitor resync event sequence is invalid".to_owned());
    }
    Ok((requested_next < oldest_available_sequence)
        .then(|| ObservationGap {
            first_sequence: requested_next,
            last_sequence: oldest_available_sequence - 1,
        })
        .into_iter()
        .collect())
}

fn direct_observation_resync_batch(
    node_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    requested_after: u64,
    high_watermark: u64,
    oldest_available_sequence: u64,
    events: &[gate4agent_node_protocol::NodeEventEnvelope],
) -> Result<ObservationResyncBatch, String> {
    let sequences = events.iter().map(|event| event.sequence).collect::<Vec<_>>();
    let gaps = observation_resync_gaps(
        requested_after,
        high_watermark,
        oldest_available_sequence,
        &sequences,
    )?;
    let mut observations = Vec::new();
    for envelope in events {
        let cursor = gate4agent_c2_protocol::NodeCursor {
            incarnation_id,
            sequence: envelope.sequence,
        };
        match &envelope.event {
            NodeEvent::Observation { address, observation } => observations.push(
                observation_ingress_envelope(
                    node_id,
                    cursor,
                    address.clone(),
                    observation.clone(),
                    ObservationTransport::DirectNode,
                )?,
            ),
            NodeEvent::ManagedObservation { record_id, observation } => observations.push(
                managed_observation_ingress_envelope(
                    node_id,
                    cursor,
                    record_id.clone(),
                    observation.clone(),
                    ObservationTransport::DirectNode,
                )?,
            ),
            _ => {}
        }
    }
    Ok(ObservationResyncBatch {
        node_id: ObservationNodeId::new(node_id)
            .map_err(|error| format!("Session Monitor resync route rejected: {error}"))?,
        incarnation_id,
        requested_after,
        high_watermark: gate4agent_c2_protocol::NodeCursor {
            incarnation_id,
            sequence: high_watermark,
        },
        oldest_available_sequence,
        records: Vec::new(),
        records_complete: false,
        gaps,
        events: observations,
    })
}

fn c2_observation_resync_batch(
    node_id: &str,
    incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
    requested_after: u64,
    high_watermark: u64,
    oldest_available_sequence: u64,
    events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
) -> Result<ObservationResyncBatch, String> {
    let sequences = events.iter().map(|event| event.sequence).collect::<Vec<_>>();
    let gaps = observation_resync_gaps(
        requested_after,
        high_watermark,
        oldest_available_sequence,
        &sequences,
    )?;
    let mut observations = Vec::new();
    for envelope in events {
        let cursor = gate4agent_c2_protocol::NodeCursor {
            incarnation_id,
            sequence: envelope.sequence,
        };
        match &envelope.event {
            C2NodeEvent::Observation { address, observation } => observations.push(
                observation_ingress_envelope(
                    node_id,
                    cursor,
                    address.clone(),
                    observation.clone(),
                    ObservationTransport::C2,
                )?,
            ),
            C2NodeEvent::ManagedObservation { record_id, observation } => observations.push(
                managed_observation_ingress_envelope(
                    node_id,
                    cursor,
                    record_id.clone(),
                    observation.clone(),
                    ObservationTransport::C2,
                )?,
            ),
            _ => {}
        }
    }
    Ok(ObservationResyncBatch {
        node_id: ObservationNodeId::new(node_id)
            .map_err(|error| format!("Session Monitor resync route rejected: {error}"))?,
        incarnation_id,
        requested_after,
        high_watermark: gate4agent_c2_protocol::NodeCursor {
            incarnation_id,
            sequence: high_watermark,
        },
        oldest_available_sequence,
        records: Vec::new(),
        records_complete: false,
        gaps,
        events: observations,
    })
}

fn safe_c2_error(error: &C2ControlError) -> String {
    match error {
        C2ControlError::InvalidEndpoint => "invalid C2 control endpoint".to_owned(),
        C2ControlError::InvalidToken | C2ControlError::Authentication(_) => {
            "C2 authentication unavailable".to_owned()
        }
        C2ControlError::AuthenticationTimedOut => "C2 authentication timed out".to_owned(),
        C2ControlError::Io(_) | C2ControlError::Closed => "C2 transport unavailable".to_owned(),
        C2ControlError::Frame(_) | C2ControlError::Protocol(_) => "C2 protocol invalid".to_owned(),
        C2ControlError::Relay(failure) => format!("C2 relay {:?}", failure.code),
        C2ControlError::QueueFull => "C2 control queue full".to_owned(),
        C2ControlError::RequestIdExhausted => "C2 request counter exhausted".to_owned(),
    }
}

fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.is_empty() || max_bytes == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

fn spawn_override<T>(value: Option<T>) -> SpawnOverride<T> {
    match value {
        Some(value) => SpawnOverride::Set { value },
        None => SpawnOverride::Clear,
    }
}

fn spawn_spec(
    node_id: String,
    workspace_id: String,
    provider: Provider,
    rows: u16,
    cols: u16,
    profile_id: String,
    profile_revision: String,
    prompt: Option<String>,
    bundle_id: Option<String>,
    context_id: Option<String>,
    idempotency_key: String,
) -> Option<SpawnSpec> {
    let mode = SessionMode::Pty;
    let required_capabilities = SpawnRequiredCapabilities::new([CapabilityId::new(
        SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
    ).ok()?]).ok()?;
    Some(SpawnSpec {
        target: SpawnTarget {
            node_id: NodeId::new(node_id).ok()?,
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            worktree_id: None,
        },
        profile_id: SpawnProfileId::new(profile_id).ok()?,
        expected_profile_revision: SpawnProfileRevision::new(profile_revision).ok()?,
        overrides: SpawnOverrides {
            provider: SpawnOverride::Set { value: map_provider(provider) },
            mode: SpawnOverride::Set { value: mode },
            terminal_size: SpawnOverride::Set {
                value: TerminalSize { rows, columns: cols },
            },
            prompt: spawn_override(prompt.map(SpawnPrompt::new).transpose().ok()?),
            bundle_id: spawn_override(bundle_id.map(SpawnBundleId::new).transpose().ok()?),
            context_id: spawn_override(context_id.map(SpawnContextId::new).transpose().ok()?),
            environment_profile_id: SpawnOverride::Clear,
        },
        deadline_ms: SpawnDeadlineMs::new(20_000).ok()?,
        idempotency_key: SpawnIdempotencyKey::new(idempotency_key).ok()?,
        required_capabilities,
    })
}

fn action_to_request(action: AppAction) -> Option<NodeRequest> {
    match action {
        AppAction::None | AppAction::Quit => None,
        AppAction::Spawn { workspace_id, provider, rows, cols, .. } => Some(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            provider: map_provider(provider),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows, columns: cols },
            initial_prompt: None,
        }),
        AppAction::SpawnSpec {
            node_id,
            workspace_id,
            provider,
            rows,
            cols,
            profile_id,
            profile_revision,
            prompt,
            bundle_id,
            context_id,
            idempotency_key,
        } => Some(NodeRequest::SpawnSpec {
            spec: spawn_spec(
                node_id,
                workspace_id,
                provider,
                rows,
                cols,
                profile_id,
                profile_revision,
                prompt,
                bundle_id,
                context_id,
                idempotency_key,
            )?,
        }),
        AppAction::SpawnManagedWorktree {
            node_id,
            workspace_id,
            provider,
            rows,
            cols,
            profile_id,
            profile_revision,
            worktree_profile_id,
            prompt,
            bundle_id,
            context_id,
            idempotency_key,
        } => Some(NodeRequest::SpawnManagedWorktree {
            request: ManagedWorktreeSpawnRequest {
                spawn_spec: spawn_spec(
                    node_id,
                    workspace_id,
                    provider,
                    rows,
                    cols,
                    profile_id,
                    profile_revision,
                    prompt,
                    bundle_id,
                    context_id,
                    idempotency_key,
                )?,
                worktree_profile_id: WorktreeProfileId::new(worktree_profile_id).ok()?,
            },
        }),
        AppAction::DiscoverHistory { address, limit } => Some(NodeRequest::DiscoverHistory {
            session: wire_address(&address).ok()?,
            limit,
        }),
        AppAction::LoadHistory { address, candidate_id } => Some(NodeRequest::LoadHistory {
            session: wire_address(&address).ok()?,
            candidate_id,
        }),
        AppAction::ExportContextPack { address } => Some(NodeRequest::ExportContextPack {
            session: wire_address(&address).ok()?,
        }),
        AppAction::ForgetContextPack { context_id, .. } => Some(NodeRequest::ForgetContextPack {
            context_id: SpawnContextId::new(context_id).ok()?,
        }),
        AppAction::Resume { address, rows, cols } => Some(NodeRequest::Resume {
            session: wire_address(&address).ok()?,
            terminal_size: TerminalSize { rows, columns: cols },
            initial_prompt: None,
        }),
        AppAction::ResumeSessionRecord { record_id, rows, cols, initial_prompt, .. } => {
            Some(NodeRequest::ResumeSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
                terminal_size: TerminalSize { rows, columns: cols },
                initial_prompt,
            })
        }
        AppAction::IndexProviderSession {
            workspace_id,
            provider,
            identity,
            display_name,
            ..
        } => Some(NodeRequest::IndexProviderSession {
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            provider: map_provider(provider),
            identity,
            display_name,
        }),
        AppAction::CatalogNativeSessions { .. }
        | AppAction::PageNativeSessions { .. }
        | AppAction::PreviewNativeSession { .. }
        | AppAction::IndexNativeSession { .. } => None,
        AppAction::PreviewSessionRecord { record_id, message_limit, .. } => {
            Some(NodeRequest::PreviewSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
                message_limit,
            })
        }
        AppAction::RefreshSessionRecordHistory { record_id, message_limit, .. } => {
            Some(NodeRequest::PreviewSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
                message_limit,
            })
        }
        AppAction::RenameSessionRecord { record_id, display_name, .. } => {
            Some(NodeRequest::RenameSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
                display_name,
            })
        }
        AppAction::SetSessionTask { record_id, expected_revision, target, .. } => {
            Some(NodeRequest::SetSessionTask {
                record_id: SessionRecordId::new(record_id).ok()?,
                expected_revision,
                target,
            })
        }
        AppAction::ForgetSessionRecord { record_id, .. } => {
            Some(NodeRequest::ForgetSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
            })
        }
        AppAction::Input { address, text } => Some(NodeRequest::Input {
            session: wire_address(&address).ok()?,
            text,
        }),
        AppAction::CreateWorkspaceFile { workspace_id, path, .. } => {
            Some(NodeRequest::CreateWorkspaceFile {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                path,
            })
        }
        AppAction::CreateWorkspaceDirectory { workspace_id, path, .. } => {
            Some(NodeRequest::CreateWorkspaceDirectory {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                path,
            })
        }
        AppAction::Paste { address, text } => Some(NodeRequest::Paste {
            session: wire_address(&address).ok()?,
            text,
        }),
        AppAction::TerminalControl { address, control } => Some(NodeRequest::TerminalControl {
            session: wire_address(&address).ok()?,
            control,
        }),
        AppAction::TerminalBytes { address, bytes } => Some(NodeRequest::TerminalBytes {
            session: wire_address(&address).ok()?,
            bytes,
        }),
        AppAction::Resize { address, rows, cols } => Some(NodeRequest::Resize {
            session: wire_address(&address).ok()?,
            size: TerminalSize { rows, columns: cols },
        }),
        AppAction::Stop { address, force } => Some(NodeRequest::Stop {
            session: wire_address(&address).ok()?,
            force,
        }),
        AppAction::Remove { address } => Some(NodeRequest::Remove {
            session: wire_address(&address).ok()?,
        }),
        AppAction::RegisterWorkspace { workspace_id, root, .. } => {
            Some(NodeRequest::RegisterWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                root,
            })
        }
        AppAction::BrowseHostDirectories {
            directory,
            after,
            ..
        } => Some(NodeRequest::BrowseHostDirectories { directory, after }),
        AppAction::UnregisterWorkspace { workspace_id, .. } => {
            Some(NodeRequest::UnregisterWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
            })
        }
        AppAction::CreateWorktree {
            source_workspace_id,
            workspace_id,
            target_root,
            branch,
            base,
            ..
        } => Some(NodeRequest::CreateWorktree {
            source_workspace_id: WorkspaceId::new(source_workspace_id).ok()?,
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            target_root,
            branch,
            base,
        }),
        AppAction::CreateStandaloneWorkspace {
            workspace_id,
            root,
            initial_branch,
            ..
        } => Some(NodeRequest::CreateStandaloneWorkspace {
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            root,
            initial_branch,
        }),
        AppAction::RemoveWorktree {
            source_workspace_id,
            target_root,
            ..
        } => Some(NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new(source_workspace_id).ok()?,
            target_root,
        }),
        AppAction::ReadWorkspaceFile { workspace_id, path, .. } => {
            Some(NodeRequest::ReadWorkspaceFile {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                path,
            })
        }
        AppAction::WriteWorkspaceFile {
            workspace_id,
            path,
            expected_revision,
            text,
            ..
        } => Some(NodeRequest::WriteWorkspaceFile {
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            path,
            expected_revision: WorkspaceFileRevision::new(expected_revision).ok()?,
            text,
        }),
        AppAction::ReadGitHistory { workspace_id, path, before, limit, .. } => {
            Some(NodeRequest::ReadGitHistory {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                path,
                before: before.map(GitObjectId::new).transpose().ok()?,
                limit,
            })
        }
        AppAction::ReadGitDiff { workspace_id, target, .. } => {
            Some(NodeRequest::ReadGitDiff {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                request: wire_git_diff_request(target).ok()?,
            })
        }
        AppAction::InspectWorkspace { workspace_id, .. } => {
            Some(NodeRequest::InspectWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
            })
        }
        AppAction::Resync { after_sequence, .. } => Some(NodeRequest::Resync { after_sequence }),
        AppAction::HarnessRefresh { .. }
        | AppAction::HarnessCreateTask { .. }
        | AppAction::HarnessMoveTask { .. }
        | AppAction::HarnessCancelTask { .. }
        | AppAction::HarnessRetryTask { .. }
        | AppAction::HarnessScheduleNext { .. }
        | AppAction::HarnessOpenMonitor { .. }
        | AppAction::HarnessOpenTerminal { .. }
        | AppAction::HarnessLoadTaskCorrelations { .. }
        | AppAction::HarnessLoadTaskLaunchOptions { .. }
        | AppAction::HarnessLoadRunTransfer { .. }
        | AppAction::HarnessObserveRunContextSource { .. }
        | AppAction::HarnessSaveTaskLaunchSpec { .. }
        | AppAction::HarnessStartTaskV2 { .. }
        | AppAction::HarnessInspectWorkspace { .. }
        | AppAction::HarnessReadWorkspaceFile { .. }
        | AppAction::HarnessReadGitHistory { .. }
        | AppAction::HarnessReadGitDiff { .. }
        | AppAction::HarnessLoadReverseAttribution { .. }
        | AppAction::HarnessInspectNodeWorkspace { .. }
        | AppAction::HarnessReadNodeWorkspaceFile { .. }
        | AppAction::HarnessReadNodeGitHistory { .. }
        | AppAction::HarnessReadNodeGitDiff { .. }
        | AppAction::HarnessSpawnSession { .. }
        | AppAction::HarnessWriteSessionInput { .. }
        | AppAction::HarnessResizeSession { .. }
        | AppAction::HarnessStopSession { .. } => None,
    }
}

fn wire_git_diff_request(target: WorkspaceGitDiffTarget) -> Result<GitDiffRequest, String> {
    let (mode, path) = match target {
        WorkspaceGitDiffTarget::Working { path } => (GitDiffMode::Working, path),
        WorkspaceGitDiffTarget::Staged { path } => (GitDiffMode::Staged, path),
        WorkspaceGitDiffTarget::Commit { revision, path } => (
            GitDiffMode::Commit {
                revision: GitObjectId::new(revision).map_err(|error| error.to_string())?,
            },
            path,
        ),
    };
    Ok(GitDiffRequest { mode, path })
}

fn project_git_commit(commit: GitCommitDetails) -> GitCommitView {
    GitCommitView {
        id: commit.id.as_str().to_owned(),
        parents: commit
            .parents
            .into_iter()
            .map(|value| value.as_str().to_owned())
            .collect(),
        subject: commit.subject,
        author_name: commit.author_name,
        author_email: commit.author_email,
        authored_at: commit.authored_at,
        committer_name: commit.committer_name,
        committer_email: commit.committer_email,
        committed_at: commit.committed_at,
        signature_status: format!("{:?}", commit.signature_status),
        signer: commit.signer,
    }
}

fn project_git_diff_target(diff: &GitDiff) -> WorkspaceGitDiffTarget {
    match &diff.mode {
        GitDiffMode::Working => WorkspaceGitDiffTarget::Working { path: diff.path.clone() },
        GitDiffMode::Staged => WorkspaceGitDiffTarget::Staged { path: diff.path.clone() },
        GitDiffMode::Commit { revision } => WorkspaceGitDiffTarget::Commit {
            revision: revision.as_str().to_owned(),
            path: diff.path.clone(),
        },
    }
}

async fn publish_workspace_registered(
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    workspace: WorkspaceSnapshotUpdate,
) {
    let workspace_id = match &workspace {
        WorkspaceSnapshotUpdate::Direct(workspace) => workspace.workspace_id.to_string(),
        WorkspaceSnapshotUpdate::C2(workspace) => workspace.workspace_id.to_string(),
    };
    send_update(
        updates,
        WorkerUpdate::WorkspaceUpserted {
            node_id: node_id.to_owned(),
            workspace,
        },
    )
    .await;
    send_update(
        updates,
        WorkerUpdate::SelectWorkspace {
            node_id: node_id.to_owned(),
            workspace_id,
        },
    )
    .await;
}

async fn publish_workspace_removed(
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    workspace_id: String,
) {
    send_update(
        updates,
        WorkerUpdate::WorkspaceRemoved {
            node_id: node_id.to_owned(),
            workspace_id,
        },
    )
    .await;
}

struct WorktreeResponseProjection {
    selected_workspace_id: Option<String>,
    refresh_workspace_id: Option<WorkspaceId>,
    notice: String,
}

fn project_worktree_response(
    response: &NodeResponse,
    node_id: &str,
    source_workspace_id: Option<&WorkspaceId>,
) -> Option<WorktreeResponseProjection> {
    match response {
        NodeResponse::WorktreeCreated { worktree, workspace } => Some(WorktreeResponseProjection {
            selected_workspace_id: Some(workspace.workspace_id.to_string()),
            refresh_workspace_id: Some(workspace.workspace_id.clone()),
            notice: format!("{node_id}: worktree {} created", host_path_display(&worktree.path)),
        }),
        NodeResponse::WorktreeRemoved { target_root, workspace_id } => Some(WorktreeResponseProjection {
            selected_workspace_id: None,
            refresh_workspace_id: source_workspace_id
                .filter(|source| workspace_id.as_ref() != Some(*source))
                .cloned(),
            notice: format!(
                "{node_id}: worktree {} removed{}",
                host_path_display(target_root),
                workspace_id
                    .as_ref()
                    .map(|workspace_id| format!(" ({workspace_id})"))
                    .unwrap_or_default()
            ),
        }),
        _ => None,
    }
}

async fn publish_node_event(
    event: NodeEvent,
    cursor: gate4agent_c2_protocol::NodeCursor,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    controller_owned: &mut bool,
    connection_id: u64,
) {
    if matches!(event, NodeEvent::HarnessMcpReadCall { .. }) {
        return;
    }
    if !matches!(event, NodeEvent::Observation { .. } | NodeEvent::ManagedObservation { .. }) {
        send_update(
            updates,
            WorkerUpdate::EventCursor {
                node_id: node_id.to_owned(),
                cursor,
            },
        )
        .await;
    }
    match event {
        NodeEvent::ControllerChanged { controller } => {
            *controller_owned = owns_controller(&controller, connection_id);
            if !*controller_owned {
                send_update(updates, WorkerUpdate::Notice(format!("{node_id}: controller held elsewhere"))).await;
            }
        }
        NodeEvent::ResyncRequired { .. } => {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: node_id.to_owned(),
                    endpoint: String::new(),
                    state: ConnectionState::Resyncing,
                },
            )
            .await;
        }
        NodeEvent::Control { address, event } => {
            if let Some(notice) = safe_control_notice(node_id, &address, &event) {
                send_update(updates, WorkerUpdate::Notice(notice)).await;
            }
        }
        NodeEvent::Observation { address, observation } => {
            let update = observation_ingress_envelope(
                node_id,
                cursor,
                address,
                observation,
                ObservationTransport::DirectNode,
            )
            .map(WorkerUpdate::ObservationIngress)
            .unwrap_or_else(WorkerUpdate::Notice);
            send_update(updates, update).await;
        }
        NodeEvent::ManagedObservation { record_id, observation } => {
            let update = managed_observation_ingress_envelope(
                node_id,
                cursor,
                record_id,
                observation,
                ObservationTransport::DirectNode,
            )
            .map(WorkerUpdate::ObservationIngress)
            .unwrap_or_else(WorkerUpdate::Notice);
            send_update(updates, update).await;
        }
        NodeEvent::TerminalFrame { .. } => {}
        NodeEvent::SessionRecordUpserted { record } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_managed_session(node_id, record)),
            )
            .await;
        }
        NodeEvent::SessionRecordRemoved { record_id } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordRemoved {
                    node_id: node_id.to_owned(),
                    record_id: record_id.to_string(),
                },
            )
            .await;
        }
        NodeEvent::ManagedWorktreeUpserted { lease } => {
            send_update(updates, WorkerUpdate::ManagedWorktreeLease(lease)).await;
        }
        NodeEvent::ManagedWorktreeRemoved { lease_id } => {
            send_update(
                updates,
                WorkerUpdate::ManagedWorktreeRemoved { lease_id: lease_id.to_string() },
            )
            .await;
        }
        NodeEvent::WorkspaceAdded { workspace } => {
            send_update(
                updates,
                WorkerUpdate::WorkspaceUpserted {
                    node_id: node_id.to_owned(),
                    workspace: WorkspaceSnapshotUpdate::Direct(workspace),
                },
            )
            .await;
        }
        NodeEvent::WorkspaceRemoved { workspace_id } => {
            send_update(
                updates,
                WorkerUpdate::WorkspaceRemoved {
                    node_id: node_id.to_owned(),
                    workspace_id: workspace_id.to_string(),
                },
            )
            .await;
        }
        NodeEvent::HarnessMcpReadCall { .. } => {
            unreachable!("harness MCP event handled before projection")
        }
    }
}

fn apply_observation_actor_update(app: &mut App, update: ObservationActorUpdate) -> AppAction {
    match update {
        ObservationActorUpdate::Committed { revision, route } => {
            app.apply_observation_committed_route(
                revision,
                route.projections,
                route.node_id,
                route.incarnation_id,
                route.durable_resume_cursor,
            );
            AppAction::None
        }
        ObservationActorUpdate::Rejected { message, node_id, incarnation_id } => {
            app.notice = Some(format!("Session Monitor observation rejected: {message}"));
            match (node_id, incarnation_id) {
                (Some(node_id), Some(incarnation_id)) => AppAction::Resync {
                    after_sequence: app.durable_observation_cursor(
                        node_id.as_str(),
                        incarnation_id,
                    ).unwrap_or(0),
                    node_id: node_id.to_string(),
                },
                _ => AppAction::None,
            }
        }
        ObservationActorUpdate::Unavailable(message) => {
            app.mark_observation_persistence_unavailable(message);
            AppAction::None
        }
    }
}

enum ObservationEnqueueOutcome {
    Queued,
    MissingWriter,
    Dropped,
    Recovery(AppAction),
}

fn enqueue_observation_command(
    app: &mut App,
    c2: &C2ApplyState,
    command: ObservationWriterCommand,
    node_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    recover_on_backpressure: bool,
) -> ObservationEnqueueOutcome {
    let Some(writer) = c2.observation_writer.as_ref() else {
        return ObservationEnqueueOutcome::MissingWriter;
    };
    match writer.try_send(command) {
        Ok(()) => ObservationEnqueueOutcome::Queued,
        Err(sync_mpsc::TrySendError::Full(_)) => {
            let message = "writer queue backpressure; committed state retained".to_owned();
            app.mark_observation_persistence_unavailable(message);
            if recover_on_backpressure {
                ObservationEnqueueOutcome::Recovery(AppAction::Resync {
                    node_id: node_id.to_owned(),
                    after_sequence: app.durable_observation_cursor(node_id, incarnation_id)
                        .unwrap_or(0),
                })
            } else {
                ObservationEnqueueOutcome::Dropped
            }
        }
        Err(sync_mpsc::TrySendError::Disconnected(_)) => {
            app.mark_observation_persistence_unavailable(
                "observation writer stopped; committed state retained".to_owned(),
            );
            if recover_on_backpressure {
                ObservationEnqueueOutcome::Recovery(AppAction::Resync {
                    node_id: node_id.to_owned(),
                    after_sequence: app.durable_observation_cursor(node_id, incarnation_id)
                        .unwrap_or(0),
                })
            } else {
                ObservationEnqueueOutcome::Dropped
            }
        }
    }
}

fn observation_gap_batch(
    node_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    after_sequence: u64,
    current_sequence: u64,
) -> Option<ObservationResyncBatch> {
    if current_sequence <= after_sequence.saturating_add(1) {
        return None;
    }
    let node_id = ObservationNodeId::new(node_id.to_owned()).ok()?;
    Some(ObservationResyncBatch {
        node_id,
        incarnation_id,
        requested_after: after_sequence,
        high_watermark: gate4agent_c2_protocol::NodeCursor {
            incarnation_id,
            sequence: current_sequence,
        },
        oldest_available_sequence: current_sequence,
        records: Vec::new(),
        records_complete: false,
        gaps: vec![gate4agent_observation_engine::gap(
            after_sequence.saturating_add(1),
            current_sequence.saturating_sub(1),
        )],
        events: Vec::new(),
    })
}

fn enqueue_observation_gap(
    app: &mut App,
    c2: &C2ApplyState,
    node_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    after_sequence: u64,
    current_sequence: u64,
) -> Option<AppAction> {
    let batch = observation_gap_batch(
        node_id,
        incarnation_id,
        after_sequence,
        current_sequence,
    )?;
    match enqueue_observation_command(
        app,
        c2,
        ObservationWriterCommand::Resync(batch),
        node_id,
        incarnation_id,
        true,
    ) {
        ObservationEnqueueOutcome::Queued | ObservationEnqueueOutcome::Dropped => None,
        ObservationEnqueueOutcome::Recovery(action) => Some(action),
        ObservationEnqueueOutcome::MissingWriter => {
            #[cfg(test)]
            {
                app.mark_session_monitor_transport_gap(
                    node_id,
                    incarnation_id,
                    after_sequence,
                    current_sequence,
                );
            }
            None
        }
    }
}

fn enqueue_managed_inventory(
    app: &mut App,
    c2: &mut C2ApplyState,
    node_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
) {
    let inventory = match app.observation_record_inventory(node_id, incarnation_id, true) {
        Ok(Some(inventory)) => inventory,
        Ok(None) => return,
        Err(error) => {
            app.notice = Some(format!("Session Monitor record inventory rejected: {error}"));
            return;
        }
    };
    let retry_inventory = inventory.clone();
    match enqueue_observation_command(
        app,
        c2,
        ObservationWriterCommand::Inventory(inventory),
        node_id,
        incarnation_id,
        false,
    ) {
        ObservationEnqueueOutcome::MissingWriter => {
            #[cfg(test)]
            {
                app.apply_managed_record_inventory(node_id, incarnation_id, true);
            }
        }
        ObservationEnqueueOutcome::Dropped => {
            c2.pending_observation_inventories.insert(
                (node_id.to_owned(), incarnation_id),
                retry_inventory,
            );
        }
        ObservationEnqueueOutcome::Queued | ObservationEnqueueOutcome::Recovery(_) => {
            c2.pending_observation_inventories
                .remove(&(node_id.to_owned(), incarnation_id));
        }
    }
}

fn retry_pending_observation_inventories(app: &mut App, c2: &mut C2ApplyState) {
    let pending = std::mem::take(&mut c2.pending_observation_inventories);
    for ((node_id, incarnation_id), inventory) in pending {
        let retry_inventory = inventory.clone();
        match enqueue_observation_command(
            app,
            c2,
            ObservationWriterCommand::Inventory(inventory),
            &node_id,
            incarnation_id,
            false,
        ) {
            ObservationEnqueueOutcome::Queued => {}
            ObservationEnqueueOutcome::MissingWriter
            | ObservationEnqueueOutcome::Dropped
            | ObservationEnqueueOutcome::Recovery(_) => {
                c2.pending_observation_inventories.insert(
                    (node_id, incarnation_id),
                    retry_inventory,
                );
            }
        }
    }
}

fn schedule_initial_observation_resync(
    app: &App,
    c2: &mut C2ApplyState,
    node_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    snapshot_high_watermark: u64,
    support: SessionMonitorSupport,
) -> Option<AppAction> {
    if !support.events {
        return None;
    }
    let key = (node_id.to_owned(), incarnation_id);
    if c2.initial_observation_resyncs.contains(&key) {
        return None;
    }
    let durable = app.durable_observation_cursor(node_id, incarnation_id);
    if durable.is_some_and(|sequence| sequence >= snapshot_high_watermark) {
        c2.initial_observation_resyncs.insert(key);
        return None;
    }
    c2.initial_observation_resyncs.insert(key);
    Some(AppAction::Resync {
        node_id: node_id.to_owned(),
        after_sequence: durable.unwrap_or(0),
    })
}

fn apply_update(app: &mut App, c2: &mut C2ApplyState, update: WorkerUpdate) -> AppAction {
    let mut follow_up = AppAction::None;
    match update {
        WorkerUpdate::State { node_id, endpoint, state } => {
            let endpoint = if endpoint.is_empty() {
                app.nodes
                    .iter()
                    .find(|node| node.node_id == node_id)
                    .map(|node| node.endpoint.clone())
                    .unwrap_or_default()
            } else {
                endpoint
            };
            app.set_node_connection(&node_id, &endpoint, state);
        }
        WorkerUpdate::RelayRoute { node_id, route } => {
            c2.relay_routes.insert(node_id.clone(), route);
            if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                node.relay_route = route;
            }
        }
        WorkerUpdate::TopologyNodeRemoved { node_id } => {
            c2.relay_routes.remove(&node_id);
            c2.snapshot_watermarks.remove(&node_id);
            c2.event_watermarks.remove(&node_id);
            c2.retired_incarnations.remove(&node_id);
            app.remove_topology_node(&node_id);
        }
        WorkerUpdate::Snapshot {
            expected_node_id,
            endpoint,
            incarnation_id,
            snapshot,
            controller_owned,
            event_sequence,
        } => {
            let cursor = gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: event_sequence,
            };
            let admission = c2.accept_snapshot(&expected_node_id, cursor);
            if admission != SnapshotAdmission::Rejected {
                let mut node = project_node(
                    expected_node_id,
                    endpoint,
                    incarnation_id,
                    snapshot,
                    controller_owned,
                    event_sequence,
                );
                if admission == SnapshotAdmission::InventoryOnly {
                    preserve_event_established_state(app, &mut node);
                }
                let node_id = node.node_id.clone();
                app.upsert_node(node);
                enqueue_managed_inventory(app, c2, &node_id, incarnation_id);
                let support = app.session_monitor_support_for_route(&node_id, incarnation_id);
                follow_up = schedule_initial_observation_resync(
                    app,
                    c2,
                    &node_id,
                    incarnation_id,
                    event_sequence,
                    support,
                ).unwrap_or_else(|| app.ensure_initial_agents_catalog());
            }
        }
        WorkerUpdate::C2Snapshot {
            expected_node_id,
            endpoint,
            incarnation_id,
            snapshot,
            event_sequence,
        } => {
            let cursor = gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: event_sequence,
            };
            let admission = c2.accept_snapshot(&expected_node_id, cursor);
            if admission != SnapshotAdmission::Rejected {
                let support = project_c2_observation_support(snapshot.observation_support.as_ref());
                if admission == SnapshotAdmission::Authoritative {
                    c2.rebuild_terminal_watermarks(&expected_node_id, &snapshot);
                }
                let mut node = project_c2_node(
                    expected_node_id.clone(),
                    endpoint,
                    incarnation_id,
                    snapshot,
                    event_sequence,
                    c2.relay_routes
                        .get(&expected_node_id)
                        .copied()
                        .unwrap_or(C2RelayRoute::Unknown),
                );
                if admission == SnapshotAdmission::InventoryOnly {
                    preserve_event_established_state(app, &mut node);
                }
                app.upsert_node(node);
                app.set_session_monitor_support(expected_node_id.clone(), incarnation_id, support);
                enqueue_managed_inventory(app, c2, &expected_node_id, incarnation_id);
                follow_up = schedule_initial_observation_resync(
                    app,
                    c2,
                    &expected_node_id,
                    incarnation_id,
                    event_sequence,
                    support,
                ).unwrap_or_else(|| app.ensure_initial_agents_catalog());
            }
        }
        WorkerUpdate::C2Event { node_id, cursor, event } => {
            if c2.event_is_stale_relative_to_snapshot(&node_id, cursor) {
                return AppAction::None;
            }
            let C2EventAdmission::Accepted { gap_after } = c2.accept_event(&node_id, cursor)
            else { return AppAction::None; };
            if let Some(after_sequence) = gap_after {
                let recovery = enqueue_observation_gap(
                    app,
                    c2,
                    &node_id,
                    cursor.incarnation_id,
                    after_sequence,
                    cursor.sequence,
                );
                follow_up = recovery.unwrap_or(AppAction::Resync {
                    node_id: node_id.clone(),
                    after_sequence,
                });
            }
            match event {
                C2EventUpdate::TerminalFrame { address, frame } => {
                    let frame_sequence = frame.sequence;
                    if c2.terminal_frame_is_new(&address, frame_sequence)
                        && app.apply_terminal_frame(&address, cursor.incarnation_id, frame)
                    {
                        c2.record_terminal_frame(address, frame_sequence);
                    }
                }
                C2EventUpdate::SessionRecordUpserted(record) => {
                    app.upsert_managed_session(record);
                    enqueue_managed_inventory(app, c2, &node_id, cursor.incarnation_id);
                }
                C2EventUpdate::SessionRecordRemoved { record_id } => {
                    app.remove_managed_session(&node_id, &record_id);
                    enqueue_managed_inventory(app, c2, &node_id, cursor.incarnation_id);
                }
                C2EventUpdate::ManagedWorktreeLease(lease) => {
                    let notice = format!(
                        "{node_id}: managed worktree {} is {}",
                        lease.workspace_id,
                        managed_worktree_state_label(lease.state),
                    );
                    app.apply_managed_worktree_lease(lease);
                    app.notice = Some(notice);
                }
                C2EventUpdate::ManagedWorktreeRemoved { lease_id } => {
                    app.apply_managed_worktree_removed(lease_id.clone());
                    app.notice = Some(format!("{node_id}: managed worktree {lease_id} removed"));
                }
                C2EventUpdate::WorkspaceUpserted(workspace) => {
                    upsert_workspace(
                        app,
                        &node_id,
                        WorkspaceSnapshotUpdate::C2(workspace),
                    );
                }
                C2EventUpdate::WorkspaceRemoved { workspace_id } => {
                    remove_workspace(app, &node_id, &workspace_id);
                }
                C2EventUpdate::Notice(notice) => app.notice = Some(notice),
                C2EventUpdate::ResyncRequired => {
                    c2.reset_terminal_watermarks(&node_id);
                    let endpoint = app.nodes.iter()
                        .find(|node| node.node_id == node_id)
                        .map(|node| node.endpoint.clone())
                        .unwrap_or_default();
                    app.set_node_connection(&node_id, &endpoint, ConnectionState::Resyncing);
                }
                C2EventUpdate::Ignored => {}
            }
            if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                node.event_sequence = cursor.sequence;
            }
        }
        WorkerUpdate::ObservationIngress(envelope) => {
            let node_id = envelope.node_id.to_string();
            let cursor = envelope.cursor;
            let C2EventAdmission::Accepted { gap_after } = c2.accept_observation(&node_id, cursor)
            else { return AppAction::None; };
            if let Some(after_sequence) = gap_after {
                let recovery = enqueue_observation_gap(
                    app,
                    c2,
                    &node_id,
                    cursor.incarnation_id,
                    after_sequence,
                    cursor.sequence,
                );
                follow_up = recovery.unwrap_or(AppAction::Resync {
                    node_id: node_id.clone(),
                    after_sequence,
                });
            }
            #[cfg(test)]
            let fallback_envelope = envelope.clone();
            match enqueue_observation_command(
                app,
                c2,
                ObservationWriterCommand::Ingress(envelope),
                &node_id,
                cursor.incarnation_id,
                true,
            ) {
                ObservationEnqueueOutcome::Queued => {}
                ObservationEnqueueOutcome::Dropped => {}
                ObservationEnqueueOutcome::Recovery(action) => follow_up = action,
                ObservationEnqueueOutcome::MissingWriter => {
                    #[cfg(test)]
                    {
                        let ingress_outcome = app.apply_observation_ingress(fallback_envelope);
                        if let SessionObservationIngressOutcome::Rejected {
                            resync_after: Some(after_sequence),
                            ..
                        } = ingress_outcome
                        {
                            follow_up = AppAction::Resync {
                                node_id: node_id.clone(),
                                after_sequence,
                            };
                        }
                    }
                }
            }
            if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                node.event_sequence = cursor.sequence;
            }
        }
        WorkerUpdate::ObservationResync(batch) => {
            let node_id = batch.node_id.to_string();
            let cursor = batch.high_watermark;
            if c2.observation_resync_is_admissible(&node_id, cursor) {
                #[cfg(test)]
                let fallback_batch = batch.clone();
                match enqueue_observation_command(
                    app,
                    c2,
                    ObservationWriterCommand::Resync(batch),
                    &node_id,
                    cursor.incarnation_id,
                    true,
                ) {
                    ObservationEnqueueOutcome::Queued => {
                        c2.record_observation_resync(node_id.clone(), cursor);
                    }
                    ObservationEnqueueOutcome::Dropped => {}
                    ObservationEnqueueOutcome::Recovery(action) => follow_up = action,
                    ObservationEnqueueOutcome::MissingWriter => {
                        #[cfg(test)]
                        if app.apply_observation_resync(fallback_batch) {
                            c2.record_observation_resync(node_id.clone(), cursor);
                        }
                    }
                }
                if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                    node.event_sequence = node.event_sequence.max(cursor.sequence);
                }
            }
        }
        #[cfg(test)]
        WorkerUpdate::Observation { node_id, cursor, address, observation } => {
            let Ok(target) = crate::app::observation_target(&address, cursor.incarnation_id) else {
                return AppAction::None;
            };
            let Ok(node_id_value) = ObservationNodeId::new(node_id.clone()) else {
                return AppAction::None;
            };
            let envelope = ObservationIngressEnvelope {
                node_id: node_id_value,
                cursor,
                received_at_ms: cursor.sequence.max(1),
                transport: ObservationTransport::DirectNode,
                payload: ObservationIngressPayload::Observation {
                    address: target,
                    observation,
                },
            };
            let C2EventAdmission::Accepted { gap_after } = c2.accept_observation(&node_id, cursor)
            else { return AppAction::None; };
            let ingress_outcome = app.apply_observation_ingress(envelope);
            if let Some(after_sequence) = gap_after {
                app.mark_session_monitor_transport_gap(
                    &node_id,
                    cursor.incarnation_id,
                    after_sequence,
                    cursor.sequence,
                );
                follow_up = AppAction::Resync { node_id: node_id.clone(), after_sequence };
            }
            if let SessionObservationIngressOutcome::Rejected {
                resync_after: Some(after_sequence),
                ..
            } = ingress_outcome
            {
                follow_up = AppAction::Resync { node_id: node_id.clone(), after_sequence };
            }
            if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                node.event_sequence = cursor.sequence;
            }
        }
        WorkerUpdate::EventCursor { node_id, cursor } => {
            let C2EventAdmission::Accepted { gap_after } = c2.accept_event(&node_id, cursor)
            else { return AppAction::None; };
            if let Some(after_sequence) = gap_after {
                let recovery = enqueue_observation_gap(
                    app,
                    c2,
                    &node_id,
                    cursor.incarnation_id,
                    after_sequence,
                    cursor.sequence,
                );
                follow_up = recovery.unwrap_or(AppAction::Resync {
                    node_id: node_id.clone(),
                    after_sequence,
                });
            }
            if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                node.event_sequence = cursor.sequence;
            }
        }
        WorkerUpdate::ObservationSupport { node_id, incarnation_id, support } => {
            app.set_session_monitor_support(node_id, incarnation_id, support);
        }
        WorkerUpdate::OpenSession(address) => app.request_open(address),
        WorkerUpdate::SelectWorkspace { node_id, workspace_id } => {
            app.request_space_selection(node_id, workspace_id)
        }
        WorkerUpdate::WorkspaceUpserted { node_id, workspace } => {
            upsert_workspace(app, &node_id, workspace)
        }
        WorkerUpdate::WorkspaceRemoved { node_id, workspace_id } => {
            remove_workspace(app, &node_id, &workspace_id)
        }
        WorkerUpdate::WorkspaceInspected { node_id, inspection } => {
            app.apply_workspace_inspection(node_id, inspection)
        }
        WorkerUpdate::WorkspaceFileRead { node_id, token, file } => {
            app.apply_workspace_file_read(node_id, token, file)
        }
        WorkerUpdate::WorkspaceFileWritten { node_id, token, file } => {
            app.apply_workspace_file_written(node_id, token, file)
        }
        WorkerUpdate::WorkspaceFileCreated { node_id, token, file } => {
            follow_up = app.apply_workspace_file_created(node_id, token, file)
        }
        WorkerUpdate::WorkspaceDirectoryCreated {
            node_id,
            workspace_id,
            token,
            entry,
        } => {
            follow_up = app.apply_workspace_directory_created(
                node_id,
                workspace_id,
                token,
                entry,
            )
        }
        WorkerUpdate::WorkspaceEntryCreateFailed {
            node_id,
            workspace_id,
            path,
            kind,
            token,
            message,
        } => app.fail_workspace_entry_create(
            node_id,
            workspace_id,
            path,
            kind,
            token,
            message,
        ),
        WorkerUpdate::WorkspaceFileFailed { key, token, message, save } => {
            app.fail_workspace_file(&key, token, message, save)
        }
        WorkerUpdate::GitHistoryRead { destination, token, page } => {
            let has_more = page.next_before.is_some();
            follow_up = app.apply_git_history(
                &destination,
                token,
                page.commits.into_iter().map(project_git_commit).collect(),
                page.next_before.map(|value| value.as_str().to_owned()),
                has_more,
            ).unwrap_or(AppAction::None);
        }
        WorkerUpdate::GitHistoryFailed { destination, token, message } => {
            app.fail_git_history(&destination, token, message)
        }
        WorkerUpdate::GitDiffRead { destination, token, diff } => {
            let target = project_git_diff_target(&diff);
            let byte_len = u32::try_from(diff.text.len()).unwrap_or(u32::MAX);
            app.apply_git_diff(
                &destination,
                token,
                WorkspaceGitDiffView {
                    target,
                    text: diff.text,
                    byte_len,
                    truncated: diff.truncated,
                },
            );
        }
        WorkerUpdate::GitDiffFailed { destination, token, message } => {
            app.fail_git_diff(&destination, token, message)
        }
        WorkerUpdate::HostDirectoriesBrowsed {
            node_id,
            token,
            append,
            listing,
        } => app.apply_host_directories(node_id, token, append, listing),
        WorkerUpdate::HostDirectoryBrowseFailed {
            node_id,
            token,
            message,
        } => app.fail_host_directory_browse(node_id, token, message),
        WorkerUpdate::SessionRecordUpserted(record) => {
            let node_id = record.node_id.clone();
            app.upsert_managed_session(record);
            if let Some(incarnation_id) = app.nodes.iter()
                .find(|node| node.node_id == node_id)
                .and_then(|node| node.incarnation_id)
            {
                enqueue_managed_inventory(app, c2, &node_id, incarnation_id);
            }
        }
        WorkerUpdate::ProviderSessionIndexed {
            record, node_id, workspace_id, provider, session_id, identity_matches, operation_token,
        } => {
            follow_up = app.complete_existing_session_import(
                record, operation_token, &node_id, &workspace_id, &provider, &session_id,
                identity_matches,
            );
        }
        WorkerUpdate::NativeSessionIndexed {
            node_id,
            route,
            catalog_revision,
            recent_cutoff_unix_ms,
            selection_id,
            record,
            operation_token,
        } => {
            follow_up = app.complete_native_session_index(
                &node_id,
                &route,
                catalog_revision,
                recent_cutoff_unix_ms,
                &selection_id,
                record,
                operation_token,
            );
        }
        WorkerUpdate::SessionRecordResumed { record, session, operation_token } => {
            match app.complete_existing_session_resume(&record, operation_token) {
                crate::app::SessionRecordResumeDisposition::RejectRecord => {}
                crate::app::SessionRecordResumeDisposition::UpsertRecord => {
                    app.upsert_managed_session(record);
                }
                crate::app::SessionRecordResumeDisposition::UpsertRecordAndOpen(address) => {
                    app.upsert_managed_session(record);
                    if address == session {
                        app.request_open(session);
                    }
                }
            }
        }
        WorkerUpdate::ExistingSessionOperationFailed {
            node_id, record_id, indexing, operation_token, message, stale_catalog,
        } => {
            follow_up = app.fail_existing_session_operation(
                &node_id,
                record_id.as_deref(),
                indexing,
                operation_token,
                message,
                stale_catalog,
            );
        }
        WorkerUpdate::NativeSessionsCataloged {
            node_id,
            route,
            token,
            entries,
            summary,
        } => app.apply_native_session_catalog_summary(
            node_id.clone(),
            route.clone(),
            token,
            entries.into_iter().map(|entry| {
                project_native_session_catalog_entry(
                    node_id.clone(),
                    route.clone(),
                    summary.as_ref().map_or(0, |summary| summary.catalog_revision),
                    summary.as_ref().map_or(0, |summary| summary.recent_cutoff_unix_ms),
                    entry,
                )
            }).collect(),
            summary,
        ),
        WorkerUpdate::NativeSessionsPaged {
            node_id,
            route,
            token,
            page,
        } => app.apply_native_session_page(
            node_id,
            route,
            token,
            page,
        ),
        WorkerUpdate::NativeSessionPageFailed {
            node_id,
            route,
            window,
            token,
            message,
            stale_catalog,
        } => {
            follow_up = app.fail_native_session_page(
                node_id,
                route,
                window,
                token,
                message,
                stale_catalog,
            );
        }
        WorkerUpdate::NativeSessionCatalogFailed {
            node_id,
            route,
            token,
            message,
            unavailable,
        } => app.fail_native_session_catalog(
            node_id,
            route,
            token,
            message,
            unavailable,
        ),
        WorkerUpdate::NativeSessionPreviewed {
            node_id, route, catalog_revision, recent_cutoff_unix_ms,
            selection_id, token, preview,
        } => app.apply_native_session_preview(
            node_id, route, catalog_revision, recent_cutoff_unix_ms,
            selection_id, token,
            project_native_session_preview(preview),
        ),
        WorkerUpdate::NativeSessionPreviewFailed {
            node_id, route, catalog_revision, recent_cutoff_unix_ms,
            selection_id, token, message, unavailable, stale_catalog,
        } => {
            follow_up = app.fail_native_session_preview(
                node_id,
                route,
                catalog_revision,
                recent_cutoff_unix_ms,
                selection_id,
                token,
                message,
                unavailable,
                stale_catalog,
            );
        }
        WorkerUpdate::SessionRecordPreviewed { node_id, record_id, token, preview } => {
            app.apply_session_record_preview(
                node_id, record_id, token, project_session_record_preview(preview),
            )
        }
        WorkerUpdate::SessionRecordPreviewFailed {
            node_id, record_id, token, message, unavailable,
        } => app.fail_session_record_preview(node_id, record_id, token, message, unavailable),
        WorkerUpdate::SessionRecordHistoryRefreshed {
            node_id, record_id, incarnation_id,
        } => app.complete_session_record_history_refresh(
            node_id,
            record_id,
            incarnation_id,
        ),
        WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id, record_id, incarnation_id, message,
        } => app.fail_session_record_history_refresh(
            node_id,
            record_id,
            incarnation_id,
            message,
        ),
        WorkerUpdate::SessionRecordRemoved { node_id, record_id } => {
            app.remove_managed_session(&node_id, &record_id);
            if let Some(incarnation_id) = app.nodes.iter()
                .find(|node| node.node_id == node_id)
                .and_then(|node| node.incarnation_id)
            {
                enqueue_managed_inventory(app, c2, &node_id, incarnation_id);
            }
        }
        WorkerUpdate::HistoryDiscovered { source, candidates } => {
            app.apply_history_discovered(source, candidates)
        }
        WorkerUpdate::HistoryLoaded {
            source,
            session_id,
            message_count,
            completed_turn_count,
        } => app.apply_history_loaded(
            source,
            session_id,
            message_count,
            completed_turn_count,
        ),
        WorkerUpdate::ContextExported(receipt) => app.apply_context_exported(receipt),
        WorkerUpdate::ContextForgotten(id) => app.apply_context_forgotten(id),
        WorkerUpdate::SpawnReceipt(receipt) => app.apply_spawn_receipt(receipt),
        WorkerUpdate::ManagedSpawnReceipt(receipt) => app.apply_managed_spawn_receipt(receipt),
        WorkerUpdate::ManagedWorktreeLease(lease) => {
            let notice = format!(
                "managed worktree {} is {}",
                lease.workspace_id,
                managed_worktree_state_label(lease.state),
            );
            app.apply_managed_worktree_lease(lease);
            app.notice = Some(notice);
        }
        WorkerUpdate::ManagedWorktreeRemoved { lease_id } => {
            app.apply_managed_worktree_removed(lease_id.clone());
            app.notice = Some(format!("managed worktree {lease_id} removed"));
        }
        WorkerUpdate::HarnessSessionSpawned { address } => {
            app.request_open(address);
        }
        WorkerUpdate::HarnessSnapshot { token, tasks, runs, nodes } => {
            let retained = nodes.iter().map(|node| node.node_id.clone()).collect::<BTreeSet<_>>();
            let removed = app.nodes.iter()
                .filter(|node| !retained.contains(&node.node_id))
                .map(|node| node.node_id.clone())
                .collect::<Vec<_>>();
            for node_id in removed {
                app.remove_topology_node(&node_id);
            }
            for mut node in nodes {
                app.preserve_known_session_terminal_state(&mut node);
                app.upsert_node(node);
            }
            app.apply_harness_snapshot(token, tasks, runs);
            follow_up = app.ensure_initial_agents_catalog();
        }
        WorkerUpdate::HarnessRefreshFailed { token, message } => {
            app.fail_harness_refresh(token, message);
        }
        WorkerUpdate::HarnessMonitor { run, monitor, timeline } => {
            app.apply_harness_monitor(run, monitor, timeline);
        }
        WorkerUpdate::HarnessMonitorFailed { run, message } => {
            app.fail_harness_monitor(&run, message);
        }
        WorkerUpdate::HarnessRunTransfer { run, token, summary } => {
            app.apply_harness_run_transfer(run, token, summary);
        }
        WorkerUpdate::HarnessRunTransferFailed { run, token, message } => {
            app.fail_harness_run_transfer(&run, token, message);
        }
        WorkerUpdate::HarnessRunContextSourceObserved {
            run,
            task,
            token,
            observation,
            launch_options,
        } => {
            follow_up = app.apply_harness_context_source_observation(
                run,
                task,
                token,
                observation,
                launch_options,
            );
        }
        WorkerUpdate::HarnessRunContextSourceObservationFailed {
            run,
            token,
            message,
        } => {
            app.fail_harness_context_source_observation(&run, token, message);
        }
        WorkerUpdate::HarnessTaskCorrelations { task_id, correlations, failures } => {
            app.apply_harness_task_correlations(&task_id, correlations, failures);
        }
        WorkerUpdate::HarnessTaskObservations { task_id, observations, failures } => {
            app.apply_harness_task_observations(&task_id, observations, failures);
        }
        WorkerUpdate::HarnessLaunchOptionsLoaded { task, token, options } => {
            app.apply_harness_launch_options(task, token, options);
        }
        WorkerUpdate::HarnessLaunchOptionsLoadFailed { task, token, message } => {
            app.fail_harness_launch_options(&task, token, message);
        }
        WorkerUpdate::HarnessLaunchSpecSaved {
            token,
            task,
            options,
            outcome,
        } => {
            app.apply_harness_launch_spec_saved(
                token,
                task,
                options,
                outcome,
            );
        }
        WorkerUpdate::HarnessTaskStartedV2 { token, task, outcome, options, transfer } => {
            app.apply_harness_task_started_v2(token, task, outcome, options, transfer);
        }
        WorkerUpdate::HarnessExecutionMutationFailed { token, task, message } => {
            app.fail_harness_execution_mutation(token, &task, message);
        }
        WorkerUpdate::HarnessWorkspaceInspected { run, token, inspection } => {
            let (origin, entries, tree_truncated, git) =
                project_harness_workspace_inspection(inspection);
            app.apply_harness_workspace_inspection(
                run,
                token,
                origin,
                entries,
                tree_truncated,
                git,
            );
        }
        WorkerUpdate::HarnessWorkspaceInspectionFailed { run, token, failure } => {
            app.fail_harness_workspace_inspection(&run, token, failure);
        }
        WorkerUpdate::HarnessWorkspaceFileRead { key, token, file } => {
            app.apply_harness_workspace_file_read(
                &key,
                token,
                project_harness_origin(file.origin),
                project_harness_path(file.path),
                project_harness_file_content(file.content),
                file.revision.map(|revision| revision.as_str().to_owned()),
            );
        }
        WorkerUpdate::HarnessWorkspaceFileFailed { key, token, failure } => {
            app.fail_harness_workspace_file(&key, token, failure);
        }
        WorkerUpdate::HarnessGitHistoryRead { destination, token, page } => {
            let has_more = harness_git_history_has_more(&page);
            let history_truncated = page.truncated;
            follow_up = app.apply_harness_git_history(
                &destination,
                token,
                project_harness_origin(page.origin),
                page.path.map(project_harness_path),
                page.commits.into_iter().map(project_harness_git_commit).collect(),
                page.next_before.map(|cursor| cursor.as_str().to_owned()),
                has_more,
                history_truncated,
            ).unwrap_or(AppAction::None);
        }
        WorkerUpdate::HarnessGitHistoryFailed { destination, token, failure } => {
            app.fail_harness_git_history(&destination, token, failure);
        }
        WorkerUpdate::HarnessGitDiffRead { destination, token, diff } => {
            let origin = project_harness_origin(diff.origin);
            let text_len = diff.text.len().min(u32::MAX as usize) as u32;
            let target = project_harness_git_diff_target_from_response(diff.mode, diff.path);
            app.apply_harness_git_diff(
                &destination,
                token,
                origin,
                WorkspaceGitDiffView {
                    target,
                    text: diff.text,
                    byte_len: text_len,
                    truncated: diff.truncated,
                },
            );
        }
        WorkerUpdate::HarnessGitDiffFailed { destination, token, failure } => {
            app.fail_harness_git_diff(&destination, token, failure);
        }
        WorkerUpdate::HarnessNodeWorkspaceInspected { node_id: expected_node_id, workspace_id: expected_workspace_id, inspection } => {
            match project_harness_node_workspace_inspection(inspection) {
                Ok((node_id, inspection)) if node_id == expected_node_id
                    && inspection.workspace_id.as_str() == expected_workspace_id =>
                {
                    app.apply_workspace_inspection(node_id, inspection);
                }
                Ok((node_id, inspection)) => {
                    app.fail_workspace_inspection(
                        expected_node_id,
                        expected_workspace_id,
                        format!(
                            "Harness replied for {node_id}/{} instead of the requested workspace",
                            inspection.workspace_id,
                        ),
                    );
                }
                Err(message) => {
                    app.fail_workspace_inspection(expected_node_id, expected_workspace_id, message);
                }
            }
        }
        WorkerUpdate::HarnessNodeWorkspaceInspectionFailed { node_id, workspace_id, message } => {
            app.fail_workspace_inspection(node_id, workspace_id, message);
        }
        WorkerUpdate::HarnessNodeWorkspaceFileRead { node_id: expected_node_id, token, file } => {
            match project_harness_node_workspace_file(file) {
                Ok((node_id, file)) => app.apply_workspace_file_read(node_id, token, file),
                Err(message) => app.notice = Some(format!("{expected_node_id}: {message}")),
            }
        }
        WorkerUpdate::HarnessNodeWorkspaceFileFailed { key, token, message } => {
            app.fail_workspace_file(&key, token, message, false);
        }
        WorkerUpdate::HarnessNodeGitHistoryRead { destination, token, page } => {
            let (commits, next_before, has_more) = project_harness_node_git_history(page);
            follow_up = app.apply_git_history(
                &destination,
                token,
                commits,
                next_before,
                has_more,
            ).unwrap_or(AppAction::None);
        }
        WorkerUpdate::HarnessNodeGitHistoryFailed { destination, token, message } => {
            app.fail_git_history(&destination, token, message);
        }
        WorkerUpdate::HarnessNodeGitDiffRead { destination, token, diff } => {
            app.apply_git_diff(&destination, token, project_harness_node_git_diff(diff));
        }
        WorkerUpdate::HarnessNodeGitDiffFailed { destination, token, message } => {
            app.fail_git_diff(&destination, token, message);
        }
        WorkerUpdate::HarnessReverseAttributionLoaded { subject, token, value } => {
            app.apply_harness_reverse_attribution(subject, token, value);
        }
        WorkerUpdate::HarnessReverseAttributionFailed { subject, token, message } => {
            app.fail_harness_reverse_attribution(&subject, token, message);
        }
        WorkerUpdate::HarnessTerminalRead(page) => {
            let HarnessRuntimeTerminalPageV1 { session, frames, .. } = page;
            if let Ok(incarnation_id) =
                session.incarnation_id.parse::<gate4agent_node_protocol::NodeIncarnationId>()
            {
                let address = SessionAddress {
                    node_id: session.node_id,
                    workspace_id: session.workspace_id,
                    instance_id: session.instance_id,
                    generation: session.generation,
                };
                for wire_frame in frames {
                    let frame_sequence = wire_frame.sequence;
                    let frame = terminal_frame_from_harness(wire_frame);
                    if c2.terminal_frame_is_new(&address, frame_sequence)
                        && app.apply_terminal_frame(&address, incarnation_id, frame)
                    {
                        c2.record_terminal_frame(address.clone(), frame_sequence);
                    }
                }
            }
        }
        WorkerUpdate::Notice(notice) => app.notice = Some(notice),
    }
    follow_up
}

fn project_native_session_catalog_entry(
    node_id: String,
    route: NativeSessionCatalogRoute,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    entry: NativeSessionCatalogEntry,
) -> NativeSessionCatalogRowView {
    NativeSessionCatalogRowView {
        node_id,
        route,
        catalog_revision,
        recent_cutoff_unix_ms,
        selection_id: entry.selection_id,
        title: entry.title,
        modified_at: entry.modified_at_unix_ms.map(|value| value.to_string()),
        model: entry.model,
        message_count: Some(entry.message_count),
        completed_turn_count: entry.completed_turn_count,
        external_group: entry.external_group,
        record_id: entry.record_id.map(|record_id| record_id.to_string()),
    }
}

fn harness_native_session_route(
    nodes: Option<&[NodeView]>,
    node_id: &str,
    route: &NativeSessionCatalogRoute,
) -> Result<HarnessNativeSessionRouteV1, String> {
    let incarnation_id = nodes
        .and_then(|nodes| nodes.iter().find(|node| node.node_id == node_id))
        .and_then(|node| node.incarnation_id)
        .ok_or_else(|| "Harness native history requires an exact runtime incarnation".to_owned())?;
    let scope = match route.scope {
        gate4agent_types::NativeSessionCatalogScope::Workspace => {
            HarnessNativeSessionCatalogScopeV1::Workspace
        }
        gate4agent_types::NativeSessionCatalogScope::Unregistered => {
            HarnessNativeSessionCatalogScopeV1::Unregistered
        }
    };
    Ok(HarnessNativeSessionRouteV1 {
        node_id: node_id.to_owned(),
        incarnation_id: incarnation_id.to_string(),
        scope,
        workspace_id: route.workspace_id.clone(),
        provider: route.provider.to_string(),
    })
}

fn project_harness_native_catalog_entry(
    entry: HarnessNativeSessionCatalogEntryV1,
) -> Result<NativeSessionCatalogEntry, String> {
    Ok(NativeSessionCatalogEntry {
        selection_id: entry.selection_id,
        title: entry.title,
        modified_at_unix_ms: entry.modified_at_unix_ms,
        model: entry.model,
        message_count: entry.message_count,
        completed_turn_count: entry.completed_turn_count,
        external_group: entry.external_group.map(|group| NativeSessionExternalGroup {
            group_id: group.group_id,
            kind: match group.kind {
                HarnessNativeSessionExternalGroupKindV1::Project => {
                    NativeSessionExternalGroupKind::Project
                }
                HarnessNativeSessionExternalGroupKindV1::Global => {
                    NativeSessionExternalGroupKind::Global
                }
            },
            display_name: group.display_name,
        }),
        record_id: entry.record_id.map(SessionRecordId::new).transpose()
            .map_err(|error| format!("invalid Harness native-history record ID: {error}"))?,
    })
}

fn project_harness_native_catalog_summary(
    summary: HarnessNativeSessionCatalogSummaryV1,
) -> NativeSessionCatalogSummary {
    NativeSessionCatalogSummary {
        catalog_revision: summary.catalog_revision,
        recent_cutoff_unix_ms: summary.recent_cutoff_unix_ms,
        recent_total_count: summary.recent_total_count,
        older_total_count: summary.older_total_count,
        recent_next_after_selection_id: summary.recent_next_after_selection_id,
        recent_has_more: summary.recent_has_more,
    }
}

fn harness_native_catalog_window(
    window: NativeSessionCatalogWindow,
) -> HarnessNativeSessionCatalogWindowV1 {
    match window {
        NativeSessionCatalogWindow::Recent => HarnessNativeSessionCatalogWindowV1::Recent,
        NativeSessionCatalogWindow::Older => HarnessNativeSessionCatalogWindowV1::Older,
    }
}

fn project_harness_native_catalog_page(
    page: gate4agent_harness_client::HarnessNativeSessionCatalogPageV1,
) -> Result<NativeSessionCatalogPage, String> {
    Ok(NativeSessionCatalogPage {
        window: match page.window {
            HarnessNativeSessionCatalogWindowV1::Recent => NativeSessionCatalogWindow::Recent,
            HarnessNativeSessionCatalogWindowV1::Older => NativeSessionCatalogWindow::Older,
        },
        revision: page.revision,
        entries: page.entries.into_iter()
            .map(project_harness_native_catalog_entry)
            .collect::<Result<Vec<_>, _>>()?,
        next_after_selection_id: page.next_after_selection_id,
        remaining_count: page.remaining_count,
        has_more: page.has_more,
    })
}

fn project_harness_native_preview(
    preview: HarnessNativeSessionPreviewV1,
) -> SessionRecordPreview {
    SessionRecordPreview {
        title: preview.title,
        modified_at_unix_ms: preview.modified_at_unix_ms,
        model: preview.model,
        message_count: preview.message_count,
        message_count_exact: preview.message_count_exact,
        completed_turn_count: preview.completed_turn_count,
        total_tokens: preview.total_tokens,
        truncated: preview.truncated,
        messages: preview.messages.into_iter().map(|message| NativeSessionPreviewMessage {
            role: match message.role {
                HarnessNativeSessionPreviewRoleV1::User => HistoryMessageRole::User,
                HarnessNativeSessionPreviewRoleV1::Assistant => HistoryMessageRole::Assistant,
            },
            text: message.text,
        }).collect(),
    }
}

fn native_session_catalog_request(
    route: &NativeSessionCatalogRoute,
    limit: u16,
) -> Result<NodeRequest, String> {
    Ok(NodeRequest::CatalogNativeSessions {
        route: wire_native_session_route(route)?,
        limit,
    })
}

fn wire_native_session_route(
    route: &NativeSessionCatalogRoute,
) -> Result<WireNativeSessionCatalogRoute, String> {
    match route.scope {
        gate4agent_types::NativeSessionCatalogScope::Workspace => {
            let workspace_id = route.workspace_id.as_ref()
                .ok_or_else(|| "workspace native route is missing workspace ID".to_owned())?;
            let workspace_id = WorkspaceId::new(workspace_id.clone())
                .map_err(|_| "invalid workspace ID".to_owned())?;
            Ok(WireNativeSessionCatalogRoute::workspace(
                workspace_id,
                map_provider(route.provider.clone()),
            ))
        }
        gate4agent_types::NativeSessionCatalogScope::Unregistered => {
            if route.workspace_id.is_some() {
                return Err("unregistered native route contains workspace ID".to_owned());
            }
            Ok(WireNativeSessionCatalogRoute::unregistered(
                map_provider(route.provider.clone()),
            ))
        }
    }
}

fn project_native_session_route(route: WireNativeSessionCatalogRoute) -> NativeSessionCatalogRoute {
    NativeSessionCatalogRoute {
        scope: route.scope,
        workspace_id: route.workspace_id.map(|workspace_id| workspace_id.to_string()),
        provider: project_provider(route.provider),
    }
}

fn wire_native_session_selection(
    route: &NativeSessionCatalogRoute,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    selection_id: &str,
) -> Result<NativeSessionSelection, String> {
    Ok(NativeSessionSelection {
        route: wire_native_session_route(route)?,
        catalog_revision,
        recent_cutoff_unix_ms,
        selection_id: selection_id.to_owned(),
    })
}

fn project_preview_messages(
    messages: Vec<gate4agent_types::NativeSessionPreviewMessage>,
) -> Vec<NativeSessionPreviewMessageView> {
    messages.into_iter().map(|message| NativeSessionPreviewMessageView {
        role: match message.role {
            gate4agent_types::HistoryMessageRole::User => "user",
            gate4agent_types::HistoryMessageRole::Assistant => "assistant",
        }.to_owned(),
        text: message.text,
    }).collect()
}

fn project_native_session_preview(preview: SessionRecordPreview) -> NativeSessionPreviewView {
    NativeSessionPreviewView {
        title: preview.title,
        modified_at: preview.modified_at_unix_ms.map(|value| value.to_string()),
        model: preview.model,
        message_count: preview.message_count,
        message_count_exact: preview.message_count_exact,
        completed_turn_count: preview.completed_turn_count,
        total_tokens: preview.total_tokens,
        truncated: preview.truncated,
        messages: project_preview_messages(preview.messages),
    }
}

fn project_session_record_preview(preview: SessionRecordPreview) -> NativeSessionPreviewView {
    NativeSessionPreviewView {
        title: preview.title,
        modified_at: preview.modified_at_unix_ms.map(|value| value.to_string()),
        model: preview.model,
        message_count: preview.message_count,
        message_count_exact: preview.message_count_exact,
        completed_turn_count: preview.completed_turn_count,
        total_tokens: preview.total_tokens,
        truncated: preview.truncated,
        messages: project_preview_messages(preview.messages),
    }
}

fn native_sessions_cataloged_update(
    node_id: String,
    token: u64,
    route: WireNativeSessionCatalogRoute,
    entries: Vec<NativeSessionCatalogEntry>,
    summary: Option<NativeSessionCatalogSummary>,
) -> WorkerUpdate {
    WorkerUpdate::NativeSessionsCataloged {
        node_id,
        route: project_native_session_route(route),
        token,
        entries,
        summary,
    }
}

fn upsert_workspace(app: &mut App, node_id: &str, workspace: WorkspaceSnapshotUpdate) {
    let Some(mut node) = app.nodes.iter().find(|node| node.node_id == node_id).cloned() else {
        return;
    };
    let workspace_id = match &workspace {
        WorkspaceSnapshotUpdate::Direct(workspace) => workspace.workspace_id.to_string(),
        WorkspaceSnapshotUpdate::C2(workspace) => workspace.workspace_id.to_string(),
    };
    let providers = node
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .or_else(|| node.workspaces.first())
        .map(|workspace| workspace.providers.clone())
        .unwrap_or_default();
    let retained_progress = node.workspaces.iter()
        .flat_map(|workspace| workspace.sessions.iter())
        .filter_map(|session| session.progress.clone().map(|progress| {
            ((
                session.address.workspace_id.clone(),
                session.address.instance_id,
                session.address.generation,
            ), progress)
        }))
        .collect::<BTreeMap<_, _>>();
    let workspace = match workspace {
        WorkspaceSnapshotUpdate::Direct(workspace) => {
            let workspace_id = workspace.workspace_id.to_string();
            let sessions = workspace
                .sessions
                .into_iter()
                .filter_map(|session| {
                    project_session(node_id, &workspace_id, session, &retained_progress)
                })
                .collect();
            WorkspaceView {
                label: workspace_id.clone(),
                workspace_id,
                canonical_root: workspace.canonical_root,
                providers,
                sessions,
                worktree_service_mode: workspace.worktree_service_mode,
                managed_worktree_profiles: workspace.managed_worktree_profiles,
            }
        }
        WorkspaceSnapshotUpdate::C2(workspace) => {
            let workspace_id = workspace.workspace_id.to_string();
            let sessions = workspace
                .sessions
                .into_iter()
                .filter_map(|session| {
                    project_c2_session(node_id, &workspace_id, session, &retained_progress)
                })
                .collect();
            WorkspaceView {
                label: workspace_id.clone(),
                workspace_id,
                canonical_root: workspace.canonical_root,
                providers,
                sessions,
                worktree_service_mode: workspace.worktree_service_mode,
                managed_worktree_profiles: workspace.managed_worktree_profiles,
            }
        }
    };
    if let Some(existing) = node
        .workspaces
        .iter_mut()
        .find(|existing| existing.workspace_id == workspace.workspace_id)
    {
        *existing = workspace;
    } else {
        node.workspaces.push(workspace);
    }
    app.upsert_node(node);
}

fn remove_workspace(app: &mut App, node_id: &str, workspace_id: &str) {
    let Some(mut node) = app.nodes.iter().find(|node| node.node_id == node_id).cloned() else {
        return;
    };
    let previous_len = node.workspaces.len();
    node.workspaces
        .retain(|workspace| workspace.workspace_id != workspace_id);
    if node.workspaces.len() != previous_len {
        if app.pending_space.as_ref().is_some_and(|(pending_node_id, pending_workspace_id)| {
            pending_node_id == node_id && pending_workspace_id == workspace_id
        }) {
            app.pending_space = None;
        }
        let replacement_workspace_id = node
            .workspaces
            .first()
            .map(|workspace| workspace.workspace_id.clone());
        let removed_spawn_target = app.spawn.as_ref().is_some_and(|spawn| {
            spawn.node_id == node_id && spawn.workspace_id == workspace_id
        });
        if removed_spawn_target {
            if let Some(replacement_workspace_id) = replacement_workspace_id {
                if let Some(spawn) = app.spawn.as_mut() {
                    spawn.workspace_id = replacement_workspace_id;
                }
            } else {
                app.spawn = None;
                if app.focus == crate::app::Focus::Spawn {
                    app.focus = crate::app::Focus::Agents;
                }
            }
        }
        app.upsert_node(node);
    }
}

/// Reconstructs the real node-protocol `LaunchInventory` from the harness
/// runtime inventory's redacted mirror (`HarnessRuntimeLaunchInventoryV1`
/// cannot itself be that type: `gate4agent-node-protocol` already depends on
/// `gate4agent-harness-api` for the shared Harness MCP wire types, so the
/// reverse edge would be a cyclic package dependency; see
/// `HarnessRuntimeInventoryV1::launch_inventory`'s doc comment).
fn project_harness_launch_inventory(
    inventory: HarnessRuntimeLaunchInventoryV1,
) -> Result<LaunchInventory, String> {
    let spawn_profiles = inventory.spawn_profiles.map(|profiles| {
        profiles.into_iter().map(|profile| {
            let environment_profile = profile.environment_profile.map(|receipt| -> Result<ResolvedEnvironmentProfileReceipt, String> {
                Ok(ResolvedEnvironmentProfileReceipt {
                    profile_id: receipt.profile_id.parse().map_err(|error| {
                        format!("invalid Harness runtime environment profile ID: {error}")
                    })?,
                    profile_revision: receipt.profile_revision.parse().map_err(|error| {
                        format!("invalid Harness runtime environment profile revision: {error}")
                    })?,
                })
            }).transpose()?;
            Ok(SpawnProfileSummary {
                id: profile.id.parse()
                    .map_err(|error| format!("invalid Harness runtime spawn profile ID: {error}"))?,
                revision: profile.revision.parse().map_err(|error| {
                    format!("invalid Harness runtime spawn profile revision: {error}")
                })?,
                environment_profile,
            })
        }).collect::<Result<Vec<_>, String>>()
    }).transpose()?;
    let bundles = inventory.bundles.map(|bundles| {
        bundles.into_iter().map(|bundle| {
            Ok(ResolvedBundleReceipt {
                id: bundle.id.parse()
                    .map_err(|error| format!("invalid Harness runtime bundle ID: {error}"))?,
                revision: bundle.revision.parse()
                    .map_err(|error| format!("invalid Harness runtime bundle revision: {error}"))?,
                digest: bundle.digest.parse()
                    .map_err(|error| format!("invalid Harness runtime bundle digest: {error}"))?,
            })
        }).collect::<Result<Vec<_>, String>>()
    }).transpose()?;
    if spawn_profiles.is_none() && bundles.is_none() {
        return Err("Harness runtime launch inventory carries no negotiated component".to_owned());
    }
    Ok(LaunchInventory { spawn_profiles, bundles })
}

fn project_harness_inventory_node(entry: HarnessRuntimeNodeInventoryV1) -> Result<NodeView, String> {
    let HarnessRuntimeNodeInventoryV1 {
        node_id,
        incarnation_id,
        observed_at_unix_ms,
        event_sequence,
        inventory,
    } = entry;
    let incarnation_id = incarnation_id.parse()
        .map_err(|error| format!("invalid Harness runtime incarnation ID: {error}"))?;
    let providers = inventory.enabled_providers.into_iter().map(|provider| {
        provider.parse().map(|provider| ProviderInventory {
            provider: project_provider(provider),
            enabled: true,
        }).map_err(|error| format!("invalid Harness runtime provider: {error}"))
    }).collect::<Result<Vec<_>, _>>()?;
    let launch_inventory = inventory.launch_inventory
        .map(project_harness_launch_inventory)
        .transpose()?;
    let workspaces = inventory.workspaces.into_values().map(|workspace| {
        let workspace_id = workspace.workspace_id;
        let canonical_root = OpaqueHostPath::utf8(workspace.display_root)
            .map_err(|error| format!("invalid Harness runtime display root: {error}"))?;
        let sessions = workspace.sessions.into_iter().filter_map(|session| {
            project_harness_inventory_session(&node_id, &workspace_id, session).transpose()
        }).collect::<Result<Vec<_>, String>>()?;
        Ok(WorkspaceView {
            label: workspace_id.clone(),
            workspace_id,
            canonical_root,
            providers: providers.clone(),
            worktree_service_mode: None,
            managed_worktree_profiles: None,
            sessions,
        })
    }).collect::<Result<Vec<_>, String>>()?;
    let session_records = inventory.managed_sessions.into_iter().map(|record| {
        project_harness_inventory_managed_session(&node_id, record)
    }).collect::<Result<Vec<_>, String>>()?;
    Ok(NodeView {
        endpoint: format!("harness://runtime-inventory/{node_id}@{observed_at_unix_ms}"),
        node_id,
        incarnation_id: Some(incarnation_id),
        relay_route: C2RelayRoute::Unknown,
        connection: ConnectionState::Connected,
        controller_owned: false,
        event_sequence,
        launch_inventory,
        providers,
        workspaces,
        session_records,
    })
}

fn harness_terminal_session_address(
    address: &SessionAddress,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
) -> HarnessRuntimeSessionAddressV1 {
    HarnessRuntimeSessionAddressV1 {
        node_id: address.node_id.clone(),
        incarnation_id: incarnation_id.to_string(),
        workspace_id: address.workspace_id.clone(),
        instance_id: address.instance_id,
        generation: address.generation,
    }
}

// Not a `From` impl: both `HarnessRuntimeTerminalFrameV1` (gate4agent-harness-api,
// re-exported by gate4agent-harness-client) and `TerminalFrame` (gate4agent-types)
// are foreign to this crate, so the orphan rule forbids implementing the foreign
// `From` trait for a foreign type here (mirrors the server-side
// `terminal_frame_to_wire` note in gate4agent-harness-service/src/terminal.rs).
// `contents` (plain-text render) has no wire counterpart: `apply_terminal_frame`
// (app.rs) never reads it, so it is left at the type default (empty string).
fn terminal_frame_from_harness(frame: HarnessRuntimeTerminalFrameV1) -> TerminalFrame {
    TerminalFrame {
        sequence: frame.sequence,
        size: TerminalSize {
            rows: frame.size.rows,
            columns: frame.size.columns,
        },
        cursor_row: frame.cursor_row,
        cursor_column: frame.cursor_column,
        contents: String::new(),
        formatted: frame.formatted,
        scrollback_formatted: frame.scrollback_formatted,
        alternate_screen: frame.alternate_screen,
        mouse_protocol_enabled: frame.mouse_protocol_enabled,
        mouse_protocol_encoding: match frame.mouse_protocol_encoding {
            HarnessRuntimeMouseProtocolEncodingV1::Default => TerminalMouseProtocolEncoding::Default,
            HarnessRuntimeMouseProtocolEncodingV1::Utf8 => TerminalMouseProtocolEncoding::Utf8,
            HarnessRuntimeMouseProtocolEncodingV1::Sgr => TerminalMouseProtocolEncoding::Sgr,
        },
    }
}

fn project_harness_inventory_session(
    node_id: &str,
    workspace_id: &str,
    session: HarnessRuntimeSessionV1,
) -> Result<Option<SessionView>, String> {
    if session.transport != HarnessRuntimeTransportV1::Pty {
        return Ok(None);
    }
    let provider = session.provider.parse()
        .map_err(|error| format!("invalid Harness runtime session provider: {error}"))?;
    let (status, running, stoppable, removable, restartable) = match session.status {
        HarnessRuntimeSessionStatusV1::Registered => ("registered", false, true, false, false),
        HarnessRuntimeSessionStatusV1::Starting => ("starting", true, true, false, false),
        HarnessRuntimeSessionStatusV1::Running => ("running", true, true, false, false),
        HarnessRuntimeSessionStatusV1::Stopping => ("stopping", false, true, false, false),
        HarnessRuntimeSessionStatusV1::Exited => ("exited", false, false, true, true),
        HarnessRuntimeSessionStatusV1::Failed => ("failed", false, false, true, true),
    };
    Ok(Some(SessionView {
        address: SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            instance_id: session.instance_id,
            generation: session.generation,
        },
        provider,
        status: status.to_owned(),
        running,
        stoppable,
        removable,
        restartable,
        attention: false,
        has_provider_session_identity: false,
        progress: None,
        terminal_formatted: Vec::new(),
        terminal_scrollback: Vec::new(),
        terminal_alternate_screen: false,
        terminal_mouse_protocol_enabled: false,
        terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
        terminal_cursor: None,
    }))
}

fn project_harness_inventory_managed_session(
    node_id: &str,
    record: HarnessRuntimeManagedSessionV1,
) -> Result<ManagedSessionView, String> {
    let provider = record.provider.parse()
        .map_err(|error| format!("invalid Harness managed-session provider: {error}"))?;
    let mode = match record.mode {
        HarnessRuntimeManagedModeV1::Pty => SessionMode::Pty,
        HarnessRuntimeManagedModeV1::Inline => SessionMode::Inline,
    };
    let state = match record.state {
        HarnessRuntimeManagedStateV1::IdentityPending => ManagedSessionState::IdentityPending,
        HarnessRuntimeManagedStateV1::Live => ManagedSessionState::Live,
        HarnessRuntimeManagedStateV1::Dormant => ManagedSessionState::Dormant,
        HarnessRuntimeManagedStateV1::Unavailable => ManagedSessionState::Unavailable,
    };
    Ok(ManagedSessionView {
        node_id: node_id.to_owned(),
        record_id: record.record_id,
        display_name: record.display_name,
        provider: project_provider(provider),
        mode,
        state,
        workspace_id: record.workspace_id,
        canonical_root: None,
        has_provider_session_identity: record.provider_identity_present,
        bundle: None,
        context_id: None,
        context: None,
        task_binding: None,
        active_session: record.active_binding.map(|address| SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: address.workspace_id,
            instance_id: address.instance_id,
            generation: address.generation,
        }),
        last_error: None,
    })
}

fn project_c2_node(
    expected_node_id: String,
    endpoint: String,
    incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
    snapshot: C2NodeSnapshot,
    event_sequence: u64,
    relay_route: C2RelayRoute,
) -> NodeView {
    let agent_progress = project_agent_progress(&snapshot.agent_progress);
    let providers = snapshot
        .enabled_providers
        .iter()
        .map(|provider| ProviderInventory {
            provider: project_provider(provider.clone()),
            enabled: true,
        })
        .collect::<Vec<_>>();
    let node_id = snapshot.node_id.to_string();
    debug_assert_eq!(node_id, expected_node_id);
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .map(|workspace| {
            let workspace_id = workspace.workspace_id.to_string();
            let label = workspace_id.clone();
            let sessions = workspace
                .sessions
                .into_iter()
                .filter_map(|session| {
                    project_c2_session(&node_id, &workspace_id, session, &agent_progress)
                })
                .collect();
            WorkspaceView {
                workspace_id,
                label,
                canonical_root: workspace.canonical_root,
                providers: providers.clone(),
                sessions,
                worktree_service_mode: workspace.worktree_service_mode,
                managed_worktree_profiles: workspace.managed_worktree_profiles,
            }
        })
        .collect();
    let session_records = snapshot
        .session_records
        .into_iter()
        .map(|record| project_c2_managed_session(&node_id, record))
        .collect();
    NodeView {
        node_id,
        incarnation_id: Some(incarnation_id),
        endpoint,
        relay_route,
        connection: ConnectionState::Connected,
        controller_owned: true,
        event_sequence,
        providers,
        workspaces,
        session_records,
        launch_inventory: snapshot.launch_inventory,
    }
}

/// A snapshot admitted as `InventoryOnly` lags the per-event stream: keep the
/// event-established session records and displayed event sequence from the
/// node view the events already built, applying only the inventory-shaped
/// remainder (launch inventory, workspaces, providers) the snapshot alone
/// carries. A node the events never built has nothing to preserve — the
/// projected snapshot then applies in full.
fn preserve_event_established_state(app: &App, node: &mut NodeView) {
    if let Some(existing) = app.nodes.iter().find(|existing| existing.node_id == node.node_id) {
        node.session_records = existing.session_records.clone();
        node.event_sequence = existing.event_sequence;
    }
}

fn project_node(
    expected_node_id: String,
    endpoint: String,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    snapshot: NodeSnapshot,
    controller_owned: bool,
    event_sequence: u64,
) -> NodeView {
    let agent_progress = project_agent_progress(&snapshot.agent_progress);
    let providers = snapshot
        .enabled_providers
        .iter()
        .map(|provider| ProviderInventory {
            provider: project_provider(provider.clone()),
            enabled: true,
        })
        .collect::<Vec<_>>();
    let node_id = snapshot.node_id.to_string();
    debug_assert_eq!(node_id, expected_node_id);
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .map(|workspace| {
            let workspace_id = workspace.workspace_id.to_string();
            let label = workspace_id.clone();
            let sessions = workspace
                .sessions
                .into_iter()
                .filter_map(|session| {
                    project_session(&node_id, &workspace_id, session, &agent_progress)
                })
                .collect();
            WorkspaceView {
                workspace_id,
                label,
                canonical_root: workspace.canonical_root,
                providers: providers.clone(),
                sessions,
                worktree_service_mode: workspace.worktree_service_mode,
                managed_worktree_profiles: workspace.managed_worktree_profiles,
            }
        })
        .collect();
    let session_records = snapshot
        .session_records
        .into_iter()
        .map(|record| project_managed_session(&node_id, record))
        .collect();
    NodeView {
        node_id,
        incarnation_id: Some(incarnation_id),
        endpoint,
        relay_route: C2RelayRoute::Unknown,
        connection: ConnectionState::Connected,
        controller_owned,
        event_sequence,
        providers,
        workspaces,
        session_records,
        launch_inventory: snapshot.launch_inventory,
    }
}

fn project_managed_session(node_id: &str, record: ManagedSessionRecord) -> ManagedSessionView {
    ManagedSessionView {
        node_id: node_id.to_owned(),
        record_id: record.record_id.to_string(),
        display_name: record.display_name,
        provider: project_provider(record.provider),
        mode: record.mode,
        state: record.state,
        workspace_id: record.workspace_id.to_string(),
        canonical_root: Some(record.canonical_root),
        has_provider_session_identity: record.provider_session.is_some(),
        bundle: record.bundle,
        context_id: record.context_id,
        context: record.context,
        task_binding: record.task_binding,
        active_session: record.active_session.map(|address| SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: address.workspace_id.to_string(),
            instance_id: address.session.instance_id.0,
            generation: address.session.generation.0,
        }),
        last_error: record.last_error,
    }
}

fn project_c2_managed_session(
    node_id: &str,
    record: C2ManagedSessionRecord,
) -> ManagedSessionView {
    ManagedSessionView {
        node_id: node_id.to_owned(),
        record_id: record.record_id.to_string(),
        display_name: record.display_name,
        provider: project_provider(record.provider),
        mode: record.mode,
        state: record.state,
        workspace_id: record.workspace_id.to_string(),
        canonical_root: None,
        has_provider_session_identity: record.provider_identity_present,
        bundle: record.bundle,
        context_id: record.context_id,
        context: record.context,
        task_binding: record.task_binding,
        active_session: record.active_session.map(|address| SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: address.workspace_id.to_string(),
            instance_id: address.session.instance_id.0,
            generation: address.session.generation.0,
        }),
        last_error: None,
    }
}

fn project_c2_workspace_inspection(
    inspection: C2WorkspaceInspection,
) -> gate4agent_node_protocol::WorkspaceInspection {
    gate4agent_node_protocol::WorkspaceInspection {
        workspace_id: inspection.workspace_id,
        entries: inspection.entries,
        tree_truncated: inspection.tree_truncated,
        git: gate4agent_node_protocol::GitSnapshot {
            is_repository: inspection.git.is_repository,
            branch: inspection.git.branch,
            status: inspection.git.status,
            recent_commits: inspection.git.recent_commits,
            worktrees: inspection.git.worktrees.into_iter().map(|worktree| {
                gate4agent_node_protocol::GitWorktreeSnapshot {
                    path: worktree.path,
                    head: worktree.head,
                    branch: worktree.branch,
                    is_bare: worktree.is_bare,
                    is_main: worktree.is_main,
                    locked: worktree.locked,
                    lock_reason: worktree.locked.then(|| "locked".to_owned()),
                    prunable: worktree.prunable,
                    prunable_reason: worktree.prunable.then(|| "prunable".to_owned()),
                    workspace_id: worktree.workspace_id,
                }
            }).collect(),
            managed_worktree: inspection.git.managed_worktree,
            truncated: inspection.git.truncated,
            diagnostic: inspection.git.diagnostic_present
                .then(|| "git inspection unavailable".to_owned()),
        },
        truncation: inspection.truncation,
    }
}

fn project_c2_session(
    node_id: &str,
    workspace_id: &str,
    session: C2SessionSnapshot,
    agent_progress: &BTreeMap<(String, u64, u64), AgentProgressV1>,
) -> Option<SessionView> {
    let provider = session.agent_id.as_str().parse().ok()?;
    if !supports_tui_transport(session.transport) {
        return None;
    }
    let lifecycle = project_c2_lifecycle(&session.status);
    let terminal_cursor = session.terminal_frame.as_ref()
        .map(|frame| (frame.cursor_row, frame.cursor_column));
    let terminal_formatted = session.terminal_frame.as_ref()
        .map(|frame| frame.formatted.clone())
        .unwrap_or_default();
    let terminal_scrollback = session.terminal_frame.as_ref()
        .map(|frame| frame.scrollback_formatted.clone())
        .unwrap_or_default();
    let terminal_alternate_screen = session.terminal_frame.as_ref()
        .is_some_and(|frame| frame.alternate_screen);
    let terminal_mouse_protocol_enabled = session.terminal_frame.as_ref()
        .is_some_and(|frame| frame.mouse_protocol_enabled);
    let terminal_mouse_protocol_encoding = session.terminal_frame.as_ref()
        .map(|frame| frame.mouse_protocol_encoding)
        .unwrap_or(TerminalMouseProtocolEncoding::Default);
    Some(SessionView {
        address: SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            instance_id: session.instance_id.0,
            generation: session.generation.0,
        },
        provider,
        status: c2_status_label(&session.status),
        running: lifecycle.running,
        stoppable: lifecycle.stoppable,
        removable: lifecycle.removable,
        restartable: lifecycle.restartable,
        attention: matches!(
            session.provider_activity,
            ProviderActivity::WaitingForInput | ProviderActivity::Blocked
        ) || session.provider_interaction_pending,
        has_provider_session_identity: session.provider_identity_present,
        progress: agent_progress
            .get(&(workspace_id.to_owned(), session.instance_id.0, session.generation.0))
            .cloned(),
        terminal_formatted,
        terminal_scrollback,
        terminal_alternate_screen,
        terminal_mouse_protocol_enabled,
        terminal_mouse_protocol_encoding,
        terminal_cursor,
    })
}

fn project_session(
    node_id: &str,
    workspace_id: &str,
    session: SessionSnapshot,
    agent_progress: &BTreeMap<(String, u64, u64), AgentProgressV1>,
) -> Option<SessionView> {
    let provider = session.agent_id.as_str().parse().ok()?;
    if !supports_tui_transport(session.transport) {
        return None;
    }
    let lifecycle = project_lifecycle(&session.status);
    let attention = matches!(
        session.provider.activity,
        ProviderActivity::WaitingForInput | ProviderActivity::Blocked
    ) || !session.provider.interactions.is_empty();
    let terminal_cursor = session
        .terminal_frame
        .as_ref()
        .map(|frame| (frame.cursor_row, frame.cursor_column));
    let terminal_formatted = session
        .terminal_frame
        .as_ref()
        .map(|frame| frame.formatted.clone())
        .unwrap_or_default();
    let terminal_scrollback = session
        .terminal_frame
        .as_ref()
        .map(|frame| frame.scrollback_formatted.clone())
        .unwrap_or_default();
    let terminal_alternate_screen = session
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame.alternate_screen);
    let terminal_mouse_protocol_enabled = session
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame.mouse_protocol_enabled);
    let terminal_mouse_protocol_encoding = session
        .terminal_frame
        .as_ref()
        .map(|frame| frame.mouse_protocol_encoding)
        .unwrap_or(TerminalMouseProtocolEncoding::Default);
    Some(SessionView {
        address: SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            instance_id: session.instance_id.0,
            generation: session.generation.0,
        },
        provider,
        status: status_label(&session.status),
        running: lifecycle.running,
        stoppable: lifecycle.stoppable,
        removable: lifecycle.removable,
        restartable: lifecycle.restartable,
        attention,
        has_provider_session_identity: session.provider.session.is_some(),
        progress: agent_progress
            .get(&(workspace_id.to_owned(), session.instance_id.0, session.generation.0))
            .cloned(),
        terminal_formatted,
        terminal_scrollback,
        terminal_alternate_screen,
        terminal_mouse_protocol_enabled,
        terminal_mouse_protocol_encoding,
        terminal_cursor,
    })
}

fn project_agent_progress(
    entries: &[SessionAgentProgress],
) -> BTreeMap<(String, u64, u64), AgentProgressV1> {
    entries
        .iter()
        .map(|entry| {
            (
                (
                    entry.address.workspace_id.to_string(),
                    entry.address.session.instance_id.0,
                    entry.address.session.generation.0,
                ),
                entry.progress.clone(),
            )
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProjectedLifecycle {
    running: bool,
    stoppable: bool,
    removable: bool,
    restartable: bool,
}

fn project_lifecycle(status: &SessionStatus) -> ProjectedLifecycle {
    match status {
        SessionStatus::Registered => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        SessionStatus::Starting | SessionStatus::Running => ProjectedLifecycle {
            running: true,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        SessionStatus::Stopping => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        SessionStatus::Exited { .. } | SessionStatus::Failed { .. } => ProjectedLifecycle {
            running: false,
            stoppable: false,
            removable: true,
            restartable: true,
        },
    }
}

fn project_c2_lifecycle(status: &C2SessionStatus) -> ProjectedLifecycle {
    match status {
        C2SessionStatus::Registered => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        C2SessionStatus::Starting | C2SessionStatus::Running => ProjectedLifecycle {
            running: true,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        C2SessionStatus::Stopping => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        C2SessionStatus::Exited { .. } | C2SessionStatus::Failed => ProjectedLifecycle {
            running: false,
            stoppable: false,
            removable: true,
            restartable: true,
        },
    }
}

fn supports_tui_transport(transport: TransportKind) -> bool {
    transport == TransportKind::Pty
}

fn safe_control_notice(
    node_id: &str,
    address: &WireSessionAddress,
    event: &ControlEvent,
) -> Option<String> {
    let class = match &event.event {
        ControlEventKind::CommandRejected { .. } => "command rejected",
        ControlEventKind::InputFailed { .. } => "input failed",
        ControlEventKind::ResizeFailed { .. } => "resize failed",
        ControlEventKind::ForegroundFailed { .. } => "foreground refresh failed",
        ControlEventKind::CapabilityProbeFailed { .. } => "capability probe failed",
        ControlEventKind::HistoryFailed { .. } => "history failed",
        ControlEventKind::ResumeDenied { .. } => "resume denied",
        ControlEventKind::ResumeFailed { .. } => "resume failed",
        ControlEventKind::TerminalStale { .. } => "terminal snapshot stale",
        ControlEventKind::InteractionResolutionFailed { .. } => "interaction resolution failed",
        ControlEventKind::Resumed { .. } => "session resumed",
        ControlEventKind::Exited { forced, .. } => {
            if *forced { "session force-stopped" } else { "session exited" }
        }
        ControlEventKind::Failed { .. } => "session failed",
        _ => return None,
    };
    Some(format!(
        "{node_id}/{} #{}:{}: {class}",
        address.workspace_id,
        address.session.instance_id.0,
        address.session.generation.0
    ))
}

fn safe_c2_control_notice(
    node_id: &str,
    address: &WireSessionAddress,
    event: &C2ControlEvent,
) -> Option<String> {
    let class = match &event.event {
        C2ControlEventKind::CommandRejected => "command rejected",
        C2ControlEventKind::InputFailed => "input failed",
        C2ControlEventKind::ResizeFailed => "resize failed",
        C2ControlEventKind::ForegroundFailed => "foreground refresh failed",
        C2ControlEventKind::CapabilityProbeFailed => "capability probe failed",
        C2ControlEventKind::HistoryFailed => "history failed",
        C2ControlEventKind::ResumeDenied => "resume denied",
        C2ControlEventKind::ResumeFailed => "resume failed",
        C2ControlEventKind::TerminalStale => "terminal snapshot stale",
        C2ControlEventKind::InteractionResolutionFailed => "interaction resolution failed",
        C2ControlEventKind::Resumed => "session resumed",
        C2ControlEventKind::Exited { forced, .. } => {
            if *forced { "session force-stopped" } else { "session exited" }
        }
        C2ControlEventKind::Failed => "session failed",
        _ => return None,
    };
    Some(format!(
        "{node_id}/{} #{}:{}: {class}",
        address.workspace_id,
        address.session.instance_id.0,
        address.session.generation.0
    ))
}

fn history_refresh_incarnation_matches(
    requested: gate4agent_node_protocol::NodeIncarnationId,
    connected: gate4agent_node_protocol::NodeIncarnationId,
) -> bool {
    requested == connected
}

fn c2_session_record_history_refresh_update(
    node_id: String,
    requested_record_id: &str,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    response: Result<C2NodeResponse, C2NodeFailure>,
) -> WorkerUpdate {
    match response {
        Ok(C2NodeResponse::SessionRecordPreviewed { record_id, .. })
            if record_id.as_str() == requested_record_id =>
        {
            WorkerUpdate::SessionRecordHistoryRefreshed {
                node_id,
                record_id: requested_record_id.to_owned(),
                incarnation_id,
            }
        }
        Ok(_) => WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id,
            record_id: requested_record_id.to_owned(),
            incarnation_id,
            message: "C2 returned an invalid history refresh response".to_owned(),
        },
        Err(failure) => WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id,
            record_id: requested_record_id.to_owned(),
            incarnation_id,
            message: safe_node_failure_code(failure.code).to_owned(),
        },
    }
}

fn catalog_failure_fields(code: NodeFailureCode, _message: &str) -> (String, bool) {
    if code == NodeFailureCode::InvalidRequest {
        return (
            "native session history is not configured on this Node".to_owned(),
            true,
        );
    }
    (
        safe_node_failure_code(code).to_owned(),
        code == NodeFailureCode::UnsupportedCapability,
    )
}

fn safe_node_failure_code(code: NodeFailureCode) -> &'static str {
    match code {
        NodeFailureCode::InvalidRequest => "invalid request",
        NodeFailureCode::HostDirectoryInvalid => "invalid host directory",
        NodeFailureCode::HostDirectoryReadFailed => "host directory read failed",
        NodeFailureCode::HostDirectoryReadTimedOut => "host directory read timed out",
        NodeFailureCode::UnsupportedCapability => "required capability unavailable",
        NodeFailureCode::HarnessMcpUnavailable
        | NodeFailureCode::ReservationNotFound
        | NodeFailureCode::ReservationExpired
        | NodeFailureCode::NotActivated
        | NodeFailureCode::CallNotFound => "internal harness service unavailable",
        NodeFailureCode::ReservationConflict
        | NodeFailureCode::BindingMismatch
        | NodeFailureCode::ChunkOutOfOrder => "internal harness request rejected",
        NodeFailureCode::ResponseTooLarge => "internal harness response rejected",
        NodeFailureCode::UnknownDeliveryStage
        | NodeFailureCode::DeliveryStageStorageFailed => {
            "internal harness delivery unavailable"
        }
        NodeFailureCode::DeliveryManifestInvalid
        | NodeFailureCode::DeliveryStageConflict
        | NodeFailureCode::DeliveryBlobUnexpected
        | NodeFailureCode::DeliveryChunkOutOfOrder
        | NodeFailureCode::DeliveryBlobDigestMismatch
        | NodeFailureCode::DeliveryBundleDigestMismatch
        | NodeFailureCode::DeliveryStageIncomplete => "internal harness delivery rejected",
        NodeFailureCode::Unauthorized => "authentication rejected",
        NodeFailureCode::ObserverReadOnly => "operator access required",
        NodeFailureCode::ControllerBusy => "controller busy",
        NodeFailureCode::ControllerRequired => "controller required",
        NodeFailureCode::WorkspaceRegistrationRequired => {
            "workspace registration required"
        }
        NodeFailureCode::UnknownWorkspace => "workspace unavailable",
        NodeFailureCode::InvalidRepositoryPath => "repository path invalid",
        NodeFailureCode::RepositoryFileNotFound => "repository file unavailable",
        NodeFailureCode::RepositoryFileNotRegular => "repository path is not a regular file",
        NodeFailureCode::RepositoryPathUnsafe => "repository path is unsafe",
        NodeFailureCode::RepositoryFileReadTimedOut => "repository file read timed out",
        NodeFailureCode::RepositoryFileReadFailed => "repository file read failed",
        NodeFailureCode::RepositoryFileWriteTimedOut => "repository file write timed out",
        NodeFailureCode::RepositoryFileWriteFailed => "repository file write failed",
        NodeFailureCode::RepositoryFileRevisionConflict => {
            "file changed outside the editor; reload before saving"
        }
        NodeFailureCode::RepositoryEntryAlreadyExists => "repository entry already exists",
        NodeFailureCode::RepositoryParentNotFound => "repository parent does not exist",
        NodeFailureCode::RepositoryParentNotDirectory => {
            "repository parent is not a directory"
        }
        NodeFailureCode::RepositoryEntryCreateTimedOut => "repository entry creation timed out",
        NodeFailureCode::RepositoryEntryCreateFailed => "repository entry creation failed",
        NodeFailureCode::GitReadTimedOut => "Git read timed out",
        NodeFailureCode::GitReadFailed => "Git read failed",
        NodeFailureCode::InvalidWorkspaceRoot => "workspace root invalid",
        NodeFailureCode::DuplicateWorkspaceId => "workspace ID already registered",
        NodeFailureCode::DuplicateWorkspaceRoot => "workspace root already registered",
        NodeFailureCode::WorkspaceBusy => "workspace has managed sessions",
        NodeFailureCode::LastWorkspace => "node must retain one workspace",
        NodeFailureCode::NotGitRepository => "workspace is not a git repository",
        NodeFailureCode::WorktreeConflict => "worktree path or branch conflicts",
        NodeFailureCode::WorktreeProtected => "protected worktree cannot be removed",
        NodeFailureCode::WorktreeDirty => "worktree has uncommitted changes",
        NodeFailureCode::WorktreeLocked => "worktree is locked",
        NodeFailureCode::UnknownManagedWorktreeLease => "managed worktree lease unavailable",
        NodeFailureCode::ManagedWorktreeBusy => "managed worktree is busy",
        NodeFailureCode::ManagedWorktreeOwnershipConflict => "managed worktree ownership conflicts",
        NodeFailureCode::ManagedWorktreeProfileRevisionMismatch => {
            "managed worktree profile revision mismatch"
        }
        NodeFailureCode::ManagedWorktreeRecoveryRequired => "managed worktree requires recovery",
        NodeFailureCode::UnknownSpawnProfile => "spawn profile unavailable",
        NodeFailureCode::SpawnProfileRevisionMismatch => "spawn profile revision mismatch",
        NodeFailureCode::UnknownBundle => "session bundle unavailable",
        NodeFailureCode::UnknownContextPack => "context pack unavailable",
        NodeFailureCode::ContextPackBusy => "context pack busy",
        NodeFailureCode::ContextPackMaterializationFailed => "context pack materialization failed",
        NodeFailureCode::UnknownEnvironmentProfile => "environment profile unavailable",
        NodeFailureCode::BundleBindingMismatch => "session bundle binding mismatch",
        NodeFailureCode::EnvironmentProfileBindingMismatch => "environment profile binding mismatch",
        NodeFailureCode::BundleMaterializationFailed => "session bundle materialization failed",
        NodeFailureCode::SpawnTargetMismatch => "spawn target mismatch",
        NodeFailureCode::SpawnIdempotencyConflict => "spawn request conflicts with an earlier request",
        NodeFailureCode::SpawnIdempotencyCapacity => "spawn request capacity exhausted",
        NodeFailureCode::SpawnDeadlineExceeded => "spawn deadline exceeded",
        NodeFailureCode::UnsupportedSpawnCapability => "required spawn capability unavailable",
        NodeFailureCode::UnknownSession => "session unavailable",
        NodeFailureCode::UnknownSessionRecord => "managed session record unavailable",
        NodeFailureCode::SessionRecordNotResumable => "managed session cannot resume",
        NodeFailureCode::SessionRecordBusy => "managed session is already live",
        NodeFailureCode::SessionRecordConflict => "managed session identity conflicts",
        NodeFailureCode::SessionWorkspaceMismatch => "session/workspace mismatch",
        NodeFailureCode::StaleNativeSessionCatalog => "native session catalog changed; refresh required",
        NodeFailureCode::StaleGeneration => "stale session generation",
        NodeFailureCode::BackendBusy => "node backend busy",
        NodeFailureCode::BackendDisconnected => "node backend disconnected",
        NodeFailureCode::BackendOperationFailed => "node backend operation failed",
        NodeFailureCode::ShuttingDown => "node shutting down",
        NodeFailureCode::StandaloneWorkspaceRecoveryRequired => {
            "standalone workspace recovery required"
        }
    }
}

fn managed_worktree_state_label(state: ManagedWorktreeLeaseState) -> &'static str {
    match state {
        ManagedWorktreeLeaseState::Allocating => "allocating",
        ManagedWorktreeLeaseState::Ready => "ready",
        ManagedWorktreeLeaseState::InUse => "in use",
        ManagedWorktreeLeaseState::Retained => "retained",
        ManagedWorktreeLeaseState::CleanupBlocked => "cleanup blocked",
        ManagedWorktreeLeaseState::RecoveryRequired => "awaiting recovery",
        ManagedWorktreeLeaseState::Removed => "removed",
    }
}

async fn reject_disconnected_commands(
    commands: &mut mpsc::Receiver<AppAction>,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    reason: &str,
) {
    let mut rejected = false;
    while let Ok(action) = commands.try_recv() {
        rejected = true;
        if let Some(update) = workspace_document_failed_update(&action, reason.to_owned()) {
            send_update(updates, update).await;
        }
        if let AppAction::RefreshSessionRecordHistory {
            node_id,
            node_incarnation_id,
            record_id,
            ..
        } = action {
            send_update(updates, WorkerUpdate::SessionRecordHistoryRefreshFailed {
                node_id,
                record_id,
                incarnation_id: node_incarnation_id,
                message: reason.to_owned(),
            }).await;
        }
    }
    if rejected {
        send_update(
            updates,
            WorkerUpdate::Notice(format!(
                "{node_id}: disconnected; queued actions cancelled"
            )),
        )
        .await;
    }
}

async fn send_update(updates: &mpsc::Sender<WorkerUpdate>, update: WorkerUpdate) {
    let _ = updates.send(update).await;
}

fn owns_controller(controller: &Option<ControllerState>, connection_id: u64) -> bool {
    controller
        .as_ref()
        .is_some_and(|state| state.connection_id == connection_id)
}

fn wire_address(address: &SessionAddress) -> Result<WireSessionAddress, String> {
    Ok(WireSessionAddress {
        workspace_id: WorkspaceId::new(address.workspace_id.clone()).map_err(|error| error.to_string())?,
        session: SessionKey {
            instance_id: gate4agent_types::AgentInstanceId(address.instance_id),
            generation: gate4agent_types::SessionGeneration(address.generation),
        },
    })
}

fn map_provider(provider: Provider) -> AgentId {
    provider
}

fn project_provider(provider: AgentId) -> Provider {
    provider
}

fn status_label(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Registered => "registered".to_owned(),
        SessionStatus::Starting => "starting".to_owned(),
        SessionStatus::Running => "running".to_owned(),
        SessionStatus::Stopping => "stopping".to_owned(),
        SessionStatus::Exited { exit_code } => format!(
            "exited({})",
            exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ),
        SessionStatus::Failed { .. } => "failed".to_owned(),
    }
}

fn c2_status_label(status: &C2SessionStatus) -> String {
    match status {
        C2SessionStatus::Registered => "registered".to_owned(),
        C2SessionStatus::Starting => "starting".to_owned(),
        C2SessionStatus::Running => "running".to_owned(),
        C2SessionStatus::Stopping => "stopping".to_owned(),
        C2SessionStatus::Exited { exit_code } => format!(
            "exited({})",
            exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ),
        C2SessionStatus::Failed => "failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_client::{
        HarnessExpectedExecutionSpecRevisionV1, HarnessExecutionModeV1,
        HarnessLaunchPlanRefV1, HarnessOrdinaryLaunchPlanOptionV1, HarnessRequestDigest,
        HarnessReviewedTaskLaunchSelectionV1, HarnessReviewedWorktreeSelectionV1,
        HarnessRevision, HarnessRunId, HarnessRunLifecycleV1,
        HarnessTaskLaunchIssuanceId, HarnessTaskLaunchIssuanceRefV1,
        HarnessTaskReviewPolicyV1,
        HarnessTaskId, HarnessTaskStateV1, RedactedRunIntentV1,
        RedactedWorktreeIntentV1, TaskCreatorCategoryV1,
        HarnessGitSummaryV1, HarnessNodeWorkspaceOriginV1, HarnessWorkspaceTreeEntryV1,
    };
    use gate4agent_node_protocol::{
        AgentProgressCurrentV1, AgentProgressUsageV1, LaunchInventory,
        ManagedWorktreeProfileSummary, ManagedWorktreeRetention, OpaqueHostPath,
        ObservationEvidenceV1,
        ResolvedBundleReceipt, SpawnBundleDigest, SpawnBundleRevision, SpawnProfileRevision,
        SpawnProfileSummary, WorktreeProfileInventory, WorktreeProfileRevision,
    };
    use crate::app::{
        ContextUsageSegment, ContextUsageSegmentHit, ControlSection, DragState, Focus, HitRegion, HitTarget, LaunchContextMode, LaunchField,
        HarnessTaskComposerField, HarnessTaskRef, LaunchTarget, MenuPlacement, PtyColorMode, RosterMode, SidebarPresentation, SpawnDialog,
        SurfacePaneLayout,
    };
    use crate::surface::PaneId;

    fn test_agent_progress(sequence: u64) -> AgentProgressV1 {
        AgentProgressV1 {
            provider_sequence: sequence,
            activity: ProviderActivity::Working,
            completed_turns: 3,
            usage: Some(AgentProgressUsageV1 {
                input_tokens: 10,
                output_tokens: 4,
                cache_read_tokens: 2,
                cache_write_tokens: 1,
                reasoning_tokens: 5,
            }),
            current: AgentProgressCurrentV1::Working,
            active_tool_labels: vec!["shell".to_owned()],
            active_tool_count: 1,
            attention: None,
            subagent_count: 2,
            last_event_kind: None,
            gap_count: 0,
            stale: false,
            truncated: false,
        }
    }

    fn host_path(value: impl Into<String>) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.into()).unwrap()
    }

    fn provider(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn incarnation(byte: u8) -> gate4agent_node_protocol::NodeIncarnationId {
        gate4agent_node_protocol::NodeIncarnationId::from_bytes([byte; 16])
    }

    fn paginated_harness_task(index: usize) -> RedactedTaskV1 {
        RedactedTaskV1 {
            task_id: HarnessTaskId::new(format!("htask_{index:024x}")).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            title: format!("task {index}"),
            body: String::new(),
            creator: TaskCreatorCategoryV1::User,
            parent_task_id: None,
            dependency_ids: Vec::new(),
            state: HarnessTaskStateV1::Ready,
            run_ids: Vec::new(),
            references_redacted: false,
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    fn harness_task_ref(task: &RedactedTaskV1) -> HarnessTaskRef {
        HarnessTaskRef {
            task_id: task.task_id.clone(),
            task_revision: task.revision,
        }
    }

    fn reviewed_launch_selection() -> HarnessReviewedTaskLaunchSelectionV1 {
        HarnessReviewedTaskLaunchSelectionV1 {
            plan: HarnessOrdinaryLaunchPlanOptionV1 {
                plan: HarnessLaunchPlanRefV1 {
                    plan_id: HarnessSelectorV1::new("ordinary-codex").unwrap(),
                    revision: HarnessRevision::new(2).unwrap(),
                    digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
                },
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
                provider_id: HarnessSelectorV1::new("codex").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
            },
            worktree: HarnessReviewedWorktreeSelectionV1::Existing,
            context_source: None,
            delivery: None,
            review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
        }
    }

    fn launch_issuance_ref() -> HarnessTaskLaunchIssuanceRefV1 {
        HarnessTaskLaunchIssuanceRefV1 {
            issuance_id: HarnessTaskLaunchIssuanceId::new(format!(
                "hissue_{}",
                "c".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            digest: HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        }
    }

    fn paginated_harness_run(index: usize) -> RedactedRunV1 {
        RedactedRunV1 {
            run_id: HarnessRunId::new(format!("hrun_{index:024x}")).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: Some(paginated_harness_task(1).task_id),
            operation_id: None,
            intent: RedactedRunIntentV1 {
                mode: HarnessExecutionModeV1::Pty,
                worktree: RedactedWorktreeIntentV1::Existing,
                has_delivery_bundle: false,
                has_continuation: false,
            },
            lifecycle: HarnessRunLifecycleV1::Requested,
            binding: RedactedBindingStateV1::None,
            result_disposition: None,
            failure_category: None,
            context_pack: None,
            git_facts: None,
            references_redacted: false,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    fn harness_runtime_inventory_node(node_id: &str, observed_at_unix_ms: u64)
        -> HarnessRuntimeNodeInventoryV1
    {
        let workspace_id = "workspace-a".to_owned();
        HarnessRuntimeNodeInventoryV1 {
            node_id: node_id.to_owned(),
            incarnation_id: "11".repeat(16),
            observed_at_unix_ms,
            event_sequence: 17,
            inventory: gate4agent_harness_client::HarnessRuntimeInventoryV1 {
                enabled_providers: vec!["codex".to_owned()],
                workspaces: BTreeMap::from([(
                    workspace_id.clone(),
                    gate4agent_harness_client::HarnessRuntimeWorkspaceV1 {
                        workspace_id: workspace_id.clone(),
                        display_root: r"C:\redacted\workspace-a".to_owned(),
                        display_root_truncated: false,
                        sessions: vec![
                            HarnessRuntimeSessionV1 {
                                instance_id: 7,
                                generation: 3,
                                provider: "codex".to_owned(),
                                transport: HarnessRuntimeTransportV1::Pty,
                                status: HarnessRuntimeSessionStatusV1::Running,
                                process_id: Some(700),
                                terminal_size: Some(
                                    gate4agent_harness_client::HarnessRuntimeTerminalSizeV1 {
                                        rows: 40,
                                        columns: 120,
                                    },
                                ),
                                operation_pending: false,
                                input_pending: false,
                            },
                            HarnessRuntimeSessionV1 {
                                instance_id: 8,
                                generation: 1,
                                provider: "codex".to_owned(),
                                transport: HarnessRuntimeTransportV1::Pipe,
                                status: HarnessRuntimeSessionStatusV1::Running,
                                process_id: None,
                                terminal_size: None,
                                operation_pending: false,
                                input_pending: false,
                            },
                        ],
                        session_count: 2,
                        sessions_truncated: false,
                    },
                )]),
                workspace_count: 1,
                workspaces_truncated: false,
                session_count: 2,
                sessions_truncated: false,
                managed_sessions: vec![
                    HarnessRuntimeManagedSessionV1 {
                        record_id: "record-a".to_owned(),
                        display_name: "Dormant review".to_owned(),
                        display_name_truncated: false,
                        provider: "codex".to_owned(),
                        mode: HarnessRuntimeManagedModeV1::Inline,
                        state: HarnessRuntimeManagedStateV1::Dormant,
                        workspace_id: workspace_id.clone(),
                        active_binding: None,
                        provider_identity_present: true,
                        updated_at_unix_ms: observed_at_unix_ms,
                    },
                    HarnessRuntimeManagedSessionV1 {
                        record_id: "record-b".to_owned(),
                        display_name: "Live review".to_owned(),
                        display_name_truncated: false,
                        provider: "codex".to_owned(),
                        mode: HarnessRuntimeManagedModeV1::Pty,
                        state: HarnessRuntimeManagedStateV1::Live,
                        workspace_id: workspace_id.clone(),
                        active_binding: Some(
                            gate4agent_harness_client::HarnessRuntimeSessionBindingV1 {
                                workspace_id,
                                instance_id: 7,
                                generation: 3,
                            },
                        ),
                        provider_identity_present: true,
                        updated_at_unix_ms: observed_at_unix_ms,
                    },
                ],
                managed_session_count: 2,
                managed_sessions_truncated: false,
                launch_inventory: None,
            },
        }
    }

    #[test]
    fn harness_runtime_inventory_projects_redacted_agent_board_without_control_ownership() {
        let node = project_harness_inventory_node(harness_runtime_inventory_node(
            "node-a",
            1_725_000_000_000,
        )).unwrap();

        assert_eq!(node.node_id, "node-a");
        assert_eq!(node.incarnation_id, Some(incarnation(0x11)));
        assert_eq!(
            node.endpoint,
            "harness://runtime-inventory/node-a@1725000000000",
        );
        assert_eq!(node.relay_route, C2RelayRoute::Unknown);
        assert!(!node.controller_owned);
        assert_eq!(node.event_sequence, 17);
        assert!(node.launch_inventory.is_none());
        assert_eq!(node.workspaces.len(), 1);
        assert_eq!(node.workspaces[0].sessions.len(), 1);
        let session = &node.workspaces[0].sessions[0];
        assert_eq!(session.address.instance_id, 7);
        assert!(session.running);
        assert!(session.terminal_formatted.is_empty());
        assert!(session.terminal_scrollback.is_empty());
        assert_eq!(node.session_records.len(), 2);
        assert_eq!(node.session_records[0].state, ManagedSessionState::Dormant);
        assert_eq!(node.session_records[0].mode, SessionMode::Inline);
        assert!(node.session_records[0].active_session.is_none());
        assert_eq!(node.session_records[1].state, ManagedSessionState::Live);
        assert_eq!(
            node.session_records[1].active_session.as_ref().unwrap().instance_id,
            7,
        );
        assert!(node.session_records.iter().all(|record| {
            record.bundle.is_none()
                && record.context.is_none()
                && record.task_binding.is_none()
                && record.last_error.is_none()
        }));
    }

    #[test]
    fn harness_runtime_inventory_paginates_and_preserves_last_exact_snapshot() {
        let mut cursors = Vec::new();
        let nodes = collect_harness_runtime_inventory_pages(|cursor| {
            cursors.push(cursor.clone());
            match cursor.as_deref() {
                None => Ok(gate4agent_harness_client::HarnessRuntimeInventoryPageV1 {
                    nodes: vec![harness_runtime_inventory_node("node-a", 1)],
                    next_cursor: Some("node-a".to_owned()),
                }),
                Some("node-a") => Ok(
                    gate4agent_harness_client::HarnessRuntimeInventoryPageV1 {
                        nodes: vec![harness_runtime_inventory_node("node-b", 2)],
                        next_cursor: None,
                    },
                ),
                Some(other) => Err(format!("unexpected cursor {other}")),
            }
        }).unwrap().into_iter().map(project_harness_inventory_node)
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(cursors, vec![None, Some("node-a".to_owned())]);
        assert_eq!(nodes.iter().map(|node| node.node_id.as_str()).collect::<Vec<_>>(), [
            "node-a", "node-b",
        ]);

        let mut last_exact = None;
        assert_eq!(
            retain_last_exact_harness_inventory(&mut last_exact, nodes.clone()),
            nodes,
        );
        assert!(harness_inventory_needs_retry(
            last_exact.as_deref(),
            &[],
            false,
        ));
        assert!(harness_inventory_needs_retry(
            last_exact.as_deref(),
            &nodes,
            true,
        ));
        assert!(!harness_inventory_needs_retry(
            last_exact.as_deref(),
            &nodes,
            false,
        ));
        assert_eq!(
            retain_last_exact_harness_inventory(&mut last_exact, Vec::new()),
            nodes,
        );
    }

    #[test]
    fn harness_snapshot_pagination_is_bounded_and_failure_keeps_previous_snapshot_stale() {
        let mut task_page_calls = 0usize;
        let task_page_error = collect_harness_task_pages(|_| {
            task_page_calls += 1;
            let task = paginated_harness_task(task_page_calls);
            Ok(TaskPageV1 {
                next_cursor: Some(task.task_id.clone()),
                tasks: vec![task],
            })
        }).unwrap_err();
        assert_eq!(task_page_calls, HARNESS_TASK_PAGE_BUDGET);
        assert_eq!(task_page_error, "Harness task pagination exceeded page budget");

        let page_size = usize::from(HARNESS_SNAPSHOT_PAGE_SIZE);
        let mut task_entity_calls = 0usize;
        let task_entity_error = collect_harness_task_pages(|_| {
            let first = task_entity_calls * page_size + 1;
            task_entity_calls += 1;
            let tasks = (first..first + page_size)
                .map(paginated_harness_task)
                .collect::<Vec<_>>();
            Ok(TaskPageV1 {
                next_cursor: tasks.last().map(|task| task.task_id.clone()),
                tasks,
            })
        }).unwrap_err();
        assert_eq!(task_entity_calls, HARNESS_TASK_ENTITY_BUDGET / page_size + 1);
        assert_eq!(task_entity_error, "Harness task pagination exceeded entity budget");

        let mut run_page_calls = 0usize;
        let run_page_error = collect_harness_run_pages(|_| {
            run_page_calls += 1;
            let run = paginated_harness_run(run_page_calls);
            Ok(RunPageV1 {
                next_cursor: Some(run.run_id.clone()),
                runs: vec![run],
            })
        }).unwrap_err();
        assert_eq!(run_page_calls, HARNESS_RUN_PAGE_BUDGET);
        assert_eq!(run_page_error, "Harness run pagination exceeded page budget");

        let mut run_entity_calls = 0usize;
        let run_entity_error = collect_harness_run_pages(|_| {
            let first = run_entity_calls * page_size + 1;
            run_entity_calls += 1;
            let runs = (first..first + page_size)
                .map(paginated_harness_run)
                .collect::<Vec<_>>();
            Ok(RunPageV1 {
                next_cursor: runs.last().map(|run| run.run_id.clone()),
                runs,
            })
        }).unwrap_err();
        assert_eq!(run_entity_calls, HARNESS_RUN_ENTITY_BUDGET / page_size + 1);
        assert_eq!(run_entity_error, "Harness run pagination exceeded entity budget");

        let retained = paginated_harness_task(HARNESS_TASK_ENTITY_BUDGET + 10);
        let mut app = App::default();
        app.begin_harness_refresh(1);
        app.apply_harness_snapshot(1, vec![retained.clone()], Vec::new());
        app.begin_harness_refresh(2);
        app.fail_harness_refresh(2, task_page_error.clone());
        assert_eq!(app.harness_kanban.tasks.len(), 1);
        assert_eq!(app.harness_kanban.tasks.get(&retained.task_id), Some(&retained));
        assert_eq!(app.harness_kanban.stale_reason.as_deref(), Some(task_page_error.as_str()));
    }

    fn test_launch_inventory() -> LaunchInventory {
        LaunchInventory {
            spawn_profiles: Some(vec![SpawnProfileSummary {
                id: SpawnProfileId::new("review-default").unwrap(),
                revision: SpawnProfileRevision::new("review-default.r3").unwrap(),
                environment_profile: None,
            }]),
            bundles: Some(vec![ResolvedBundleReceipt {
                id: SpawnBundleId::new("review-tools").unwrap(),
                revision: SpawnBundleRevision::new("review-tools.r2").unwrap(),
                digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            }]),
        }
    }

    fn test_worktree_profile_inventory() -> WorktreeProfileInventory {
        WorktreeProfileInventory {
            profiles: vec![ManagedWorktreeProfileSummary {
                id: WorktreeProfileId::new("review-tree").unwrap(),
                revision: WorktreeProfileRevision::new("review-tree.r4").unwrap(),
                retention: ManagedWorktreeRetention::Retain,
            }],
        }
    }

    fn direct_workspace(workspace_id: &str) -> gate4agent_node_protocol::WorkspaceSnapshot {
        gate4agent_node_protocol::WorkspaceSnapshot {
            workspace_id: WorkspaceId::new(workspace_id).unwrap(),
            canonical_root: host_path(format!(r"C:\work\{workspace_id}")),
            sessions: Vec::new(),
            worktree_service_mode: None,
            managed_worktree_profiles: None,
        }
    }

    #[test]
    fn c2_workspace_inspection_preserves_managed_worktree_scope() {
        let scope = gate4agent_node_protocol::ManagedWorktreeGitScope {
            lease_id: gate4agent_node_protocol::ManagedWorktreeLeaseId::new("lease-a").unwrap(),
            source_workspace_id: WorkspaceId::new("source-a").unwrap(),
            branch: "agent/record-a".to_owned(),
            base_commit: GitObjectId::new("a".repeat(40)).unwrap(),
            active_session_count: 1,
            managed_record_count: 1,
        };
        let projected = project_c2_workspace_inspection(C2WorkspaceInspection {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            entries: Vec::new(),
            tree_truncated: false,
            git: gate4agent_c2_protocol::C2GitSnapshot {
                is_repository: true,
                branch: Some("agent/record-a".to_owned()),
                status: Vec::new(),
                recent_commits: Vec::new(),
                worktrees: Vec::new(),
                managed_worktree: Some(scope.clone()),
                truncated: false,
                diagnostic_present: false,
            },
            truncation: None,
        });
        assert_eq!(projected.git.managed_worktree, Some(scope));
    }

    fn c2_workspace(workspace_id: &str) -> gate4agent_c2_protocol::C2WorkspaceSnapshot {
        gate4agent_c2_protocol::C2WorkspaceSnapshot {
            workspace_id: WorkspaceId::new(workspace_id).unwrap(),
            canonical_root: host_path(format!(r"C:\work\{workspace_id}")),
            sessions: Vec::new(),
            worktree_service_mode: None,
            managed_worktree_profiles: None,
        }
    }

    fn point_spawn_at_pending_workspace(app: &mut App, workspace_id: &str) {
        let spawn = app.spawn.as_mut().unwrap();
        spawn.workspace_id = workspace_id.to_owned();
        spawn.field = LaunchField::Workspace;
        app.pending_space = Some(("node-a".to_owned(), workspace_id.to_owned()));
        app.focus = Focus::Spawn;
    }

    fn assert_spawn_launches_workspace(app: &mut App, workspace_id: &str) {
        assert!(app.nodes[0]
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == workspace_id));
        assert_eq!(app.pending_space, None);
        assert_eq!(app.focus, Focus::Spawn);
        assert_eq!(app.spawn.as_ref().unwrap().workspace_id, workspace_id);
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::SpawnSpec {
                workspace_id: actual,
                prompt: None,
                ..
            } if actual == workspace_id
        ));
    }

    fn cursor_app() -> App {
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 1,
            generation: 1,
        };
        let mut app = App::default();
        app.nodes.push(NodeView {
            node_id: "node-a".to_owned(),
            incarnation_id: None,
            endpoint: r"\\.\pipe\node-a".to_owned(),
            relay_route: C2RelayRoute::Unknown,
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 1,
            session_records: Vec::new(),
            launch_inventory: None,
            providers: Vec::new(),
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "acme".to_owned(),
                canonical_root: host_path(r"C:\work\acme"),
                providers: Vec::new(),
                sessions: vec![SessionView {
                    address: address.clone(),
                    provider: provider("codex"),
                    status: "running".to_owned(),
                    running: true,
                    stoppable: true,
                    removable: false,
                    restartable: false,
                    attention: false,
                    has_provider_session_identity: true,
                    progress: None,
                    terminal_formatted: Vec::new(),
                    terminal_scrollback: Vec::new(),
                    terminal_alternate_screen: false,
                    terminal_mouse_protocol_enabled: false,
                    terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
                terminal_cursor: Some((99, 99)),
            }],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
        });
        app.surface.open_in_focused(SurfaceTab::Pty(address));
        app.focus = crate::app::Focus::Viewport;
        app.layout.viewport = uzor_tui::Rect::new(26, 1, 10, 5);
        app
    }

    fn cursor_address(app: &App) -> SessionAddress {
        app.surface
            .active_tab()
            .and_then(SurfaceTab::pty_address)
            .expect("cursor fixture has an active PTY tab")
            .clone()
    }

    fn spawn_modal_app() -> App {
        let mut app = cursor_app();
        app.nodes[0].launch_inventory = Some(test_launch_inventory());
        app.nodes[0].workspaces[0].providers.push(ProviderInventory {
            provider: provider("codex"),
            enabled: true,
        });
        app.focus = Focus::Spawn;
        app.terminal_cols = 120;
        app.terminal_rows = 40;
        app.spawn = Some(SpawnDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            target: LaunchTarget::ExistingWorkspace,
            // Must match `test_launch_inventory()`'s only advertised profile id.
            profile_id: "review-default".to_owned(),
            worktree_profile_id: "default".to_owned(),
            bundle_id: String::new(),
            context_mode: LaunchContextMode::None,
            field: LaunchField::Workspace,
        });
        app.layout.spawn_modal = uzor_tui::Rect::new(20, 5, 60, 18);
        app
    }

    #[test]
    fn initial_viewport_matches_fixed_sidebar_and_single_tab_row() {
        assert_eq!(initial_viewport_size(100, 24), TerminalSize { rows: 23, columns: 74 });
        assert_eq!(initial_viewport_size(48, 12), TerminalSize { rows: 11, columns: 24 });
    }

    #[test]
    fn c2_workspace_projection_uses_id_label_and_preserves_foreign_path_token() {
        let opaque = OpaqueHostPath::unix_bytes(vec![b'/', b's', b'r', b'v', b'/', 0xff, b'/', b'.', b'.']).unwrap();
        let view = project_c2_node(
            "node-a".to_owned(),
            "remote".to_owned(),
            incarnation(1),
            C2NodeSnapshot {
                node_id: NodeId::new("node-a").unwrap(),
                enabled_providers: Vec::new(),
                provider_runtime_statuses: Default::default(),
                observation_support: None,
                workspaces: vec![gate4agent_c2_protocol::C2WorkspaceSnapshot {
                    workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                    canonical_root: opaque.clone(),
                    sessions: Vec::new(),
                    worktree_service_mode: None,
                    managed_worktree_profiles: None,
                }],
                session_records: Vec::new(),
                agent_progress: Vec::new(),
                managed_worktrees: Vec::new(),
                launch_inventory: None,
            },
            7,
            C2RelayRoute::Unknown,
        );

        assert_eq!(view.workspaces[0].label, "workspace-a");
        assert_eq!(view.workspaces[0].canonical_root, opaque);
    }

    #[test]
    fn direct_and_c2_projection_preserve_authoritative_launch_inventories() {
        let launch_inventory = test_launch_inventory();
        let worktree_profiles = test_worktree_profile_inventory();
        let snapshot = NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![provider("codex")],
            provider_runtime_statuses: Default::default(),
            workspaces: vec![gate4agent_node_protocol::WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                canonical_root: host_path(r"C:\work\acme"),
                sessions: Vec::new(),
                worktree_service_mode: None,
                managed_worktree_profiles: Some(worktree_profiles.clone()),
            }],
            session_records: Vec::new(),
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: Some(launch_inventory.clone()),
        };

        let direct = project_node(
            "node-a".to_owned(),
            r"\\.\pipe\node-a".to_owned(),
            incarnation(2),
            snapshot.clone(),
            true,
            11,
        );
        let through_c2 = project_c2_node(
            "node-a".to_owned(),
            "c2".to_owned(),
            incarnation(3),
            C2NodeSnapshot::from(&snapshot),
            12,
            C2RelayRoute::Unknown,
        );

        assert_eq!(direct.launch_inventory, Some(launch_inventory.clone()));
        assert_eq!(through_c2.launch_inventory, Some(launch_inventory));
        assert_eq!(
            direct.workspaces[0].managed_worktree_profiles,
            Some(worktree_profiles.clone()),
        );
        assert_eq!(
            through_c2.workspaces[0].managed_worktree_profiles,
            Some(worktree_profiles),
        );
    }

    #[test]
    fn direct_and_c2_agent_progress_projection_has_exact_address_parity() {
        let address = WireSessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: gate4agent_types::AgentInstanceId(7),
                generation: gate4agent_types::SessionGeneration(2),
            },
        };
        let progress = test_agent_progress(11);
        let session = SessionSnapshot {
            instance_id: address.session.instance_id,
            agent_id: provider("codex"),
            transport: TransportKind::Pty,
            generation: address.session.generation,
            status: SessionStatus::Running,
            pending_operation: None,
            pending_input: None,
            process_id: Some(7),
            terminal_size: None,
            terminal_frame: None,
            terminal_stale: None,
            session_options: None,
            capabilities: Default::default(),
            history: Default::default(),
            resume: Default::default(),
            foreground: Default::default(),
            provider: gate4agent_types::ProviderSnapshot {
                sequence: 11,
                lead_activity: ProviderActivity::Working,
                activity: ProviderActivity::Working,
                ..Default::default()
            },
        };
        let snapshot = NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![provider("codex")],
            provider_runtime_statuses: Default::default(),
            workspaces: vec![gate4agent_node_protocol::WorkspaceSnapshot {
                workspace_id: address.workspace_id.clone(),
                canonical_root: host_path(r"C:\work\acme"),
                sessions: vec![session],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            agent_progress: vec![SessionAgentProgress {
                address,
                progress: progress.clone(),
            }],
        };
        let direct = project_node(
            "node-a".to_owned(),
            "direct".to_owned(),
            incarnation(2),
            snapshot.clone(),
            true,
            1,
        );
        let through_c2 = project_c2_node(
            "node-a".to_owned(),
            "c2".to_owned(),
            incarnation(2),
            C2NodeSnapshot::from(&snapshot),
            1,
            C2RelayRoute::Unknown,
        );
        assert_eq!(direct.workspaces[0].sessions[0].progress, Some(progress.clone()));
        assert_eq!(through_c2.workspaces[0].sessions[0].progress, Some(progress));
        assert_eq!(direct.workspaces[0].sessions[0], through_c2.workspaces[0].sessions[0]);
    }

    #[tokio::test]
    async fn direct_workspace_response_and_events_keep_spawn_authoritative_and_remove_stale_space() {
        let mut app = spawn_modal_app();
        point_spawn_at_pending_workspace(&mut app, "scratch");
        let (updates, mut receiver) = mpsc::channel(8);
        let mut controller_owned = true;

        publish_workspace_registered(
            &updates,
            "node-a",
            WorkspaceSnapshotUpdate::Direct(direct_workspace("scratch")),
        )
        .await;
        while let Ok(update) = receiver.try_recv() {
            apply_update(&mut app, &mut C2ApplyState::default(), update);
        }
        publish_node_event(
            NodeEvent::WorkspaceAdded { workspace: direct_workspace("scratch") },
            gate4agent_c2_protocol::NodeCursor { incarnation_id: incarnation(1), sequence: 1 },
            &updates,
            "node-a",
            &mut controller_owned,
            1,
        )
        .await;
        // A non-observation `NodeEvent` publishes its cursor advance and its
        // payload as two separate messages; drain both so nothing is left in
        // the channel to leak into the next section's `recv` below.
        apply_update(
            &mut app,
            &mut C2ApplyState::default(),
            receiver.recv().await.unwrap(),
        );
        apply_update(
            &mut app,
            &mut C2ApplyState::default(),
            receiver.recv().await.unwrap(),
        );
        assert_eq!(app.nodes[0].workspaces.len(), 2);
        assert_spawn_launches_workspace(&mut app, "scratch");

        let mut app = spawn_modal_app();
        upsert_workspace(
            &mut app,
            "node-a",
            WorkspaceSnapshotUpdate::Direct(direct_workspace("scratch")),
        );
        point_spawn_at_pending_workspace(&mut app, "scratch");
        publish_workspace_removed(&updates, "node-a", "scratch".to_owned()).await;
        apply_update(
            &mut app,
            &mut C2ApplyState::default(),
            receiver.recv().await.unwrap(),
        );
        assert!(app.nodes[0]
            .workspaces
            .iter()
            .all(|workspace| workspace.workspace_id != "scratch"));
        assert_eq!(app.spawn.as_ref().unwrap().workspace_id, "workspace-a");

        upsert_workspace(
            &mut app,
            "node-a",
            WorkspaceSnapshotUpdate::Direct(direct_workspace("scratch")),
        );
        publish_node_event(
            NodeEvent::WorkspaceRemoved {
                workspace_id: WorkspaceId::new("scratch").unwrap(),
            },
            gate4agent_c2_protocol::NodeCursor { incarnation_id: incarnation(1), sequence: 2 },
            &updates,
            "node-a",
            &mut controller_owned,
            1,
        )
        .await;
        // Same two-message shape as above: the cursor advance, then the
        // removal payload itself.
        apply_update(
            &mut app,
            &mut C2ApplyState::default(),
            receiver.recv().await.unwrap(),
        );
        apply_update(
            &mut app,
            &mut C2ApplyState::default(),
            receiver.recv().await.unwrap(),
        );
        assert!(app.nodes[0]
            .workspaces
            .iter()
            .all(|workspace| workspace.workspace_id != "scratch"));
    }

    #[tokio::test]
    async fn c2_workspace_response_and_events_keep_spawn_authoritative_and_remove_stale_space() {
        let mut app = spawn_modal_app();
        point_spawn_at_pending_workspace(&mut app, "scratch");
        let (updates, mut receiver) = mpsc::channel(12);
        let mut c2 = C2ApplyState::default();

        publish_c2_response(
            C2NodeResponse::WorkspaceRegistered { workspace: c2_workspace("scratch") },
            &updates,
            "node-a",
            incarnation(4),
            "c2".to_owned(),
        )
        .await;
        while let Ok(update) = receiver.try_recv() {
            apply_update(&mut app, &mut c2, update);
        }
        publish_c2_event(
            C2NodeEvent::WorkspaceAdded { workspace: c2_workspace("scratch") },
            gate4agent_c2_protocol::NodeCursor { incarnation_id: incarnation(4), sequence: 1 },
            &updates,
            "node-a",
        )
        .await;
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert_eq!(app.nodes[0].workspaces.len(), 2);
        assert_spawn_launches_workspace(&mut app, "scratch");

        let mut app = spawn_modal_app();
        upsert_workspace(
            &mut app,
            "node-a",
            WorkspaceSnapshotUpdate::C2(c2_workspace("scratch")),
        );
        point_spawn_at_pending_workspace(&mut app, "scratch");
        publish_c2_response(
            C2NodeResponse::WorkspaceUnregistered {
                workspace_id: WorkspaceId::new("scratch").unwrap(),
            },
            &updates,
            "node-a",
            incarnation(4),
            "c2".to_owned(),
        )
        .await;
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert!(app.nodes[0]
            .workspaces
            .iter()
            .all(|workspace| workspace.workspace_id != "scratch"));
        assert_eq!(app.spawn.as_ref().unwrap().workspace_id, "workspace-a");
        let _notice = receiver.recv().await.unwrap();

        upsert_workspace(
            &mut app,
            "node-a",
            WorkspaceSnapshotUpdate::C2(c2_workspace("scratch")),
        );
        publish_c2_event(
            C2NodeEvent::WorkspaceRemoved {
                workspace_id: WorkspaceId::new("scratch").unwrap(),
            },
            gate4agent_c2_protocol::NodeCursor { incarnation_id: incarnation(4), sequence: 2 },
            &updates,
            "node-a",
        )
        .await;
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert!(app.nodes[0]
            .workspaces
            .iter()
            .all(|workspace| workspace.workspace_id != "scratch"));
    }

    #[test]
    fn c2_shared_command_route_accepts_actions_for_discovered_nodes() {
        let mut app = cursor_app();
        let address = cursor_address(&app);
        let (sender, mut receiver) = mpsc::channel(1);
        let commands = BTreeMap::from([(C2_COMMAND_ROUTE.to_owned(), sender)]);

        send_operator_action(
            &mut app,
            &commands,
            AppAction::Resize {
                address: address.clone(),
                rows: 23,
                cols: 79,
            },
        );

        assert!(matches!(
            receiver.try_recv(),
            Ok(AppAction::Resize { address: routed, rows: 23, cols: 79 }) if routed == address
        ));
    }

    #[tokio::test]
    async fn c2_topology_reconciles_offline_parked_online_incarnation_and_removal() {
        use gate4agent_c2_protocol::{C2RelayRoute, C2TopologyNode};
        use gate4agent_node_protocol::NodeIncarnationId;

        let node_id = NodeId::new("node-a").unwrap();
        let endpoint = r"\\.\pipe\node-a".to_owned();
        let offline = C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                relay_route: C2RelayRoute::LocalIpc,
                transport: NodeTransportState::Offline,
                current_incarnation_id: None,
                observation_support: None,
                provider_contracts: Vec::new(),
                provider_adapter_contracts: Vec::new(),
                provider_runtime_statuses: Default::default(),
            }],
        };
        let mut routes = BTreeMap::new();
        let mut endpoints = BTreeMap::new();
        let mut relay_routes = BTreeMap::new();
        let (updates, mut receiver) = mpsc::channel(8);

        let snapshots = reconcile_c2_topology(
            &offline,
            &mut routes,
            &mut endpoints,
            &mut relay_routes,
            &updates,
        )
        .await;
        assert!(snapshots.is_empty());
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::State {
                node_id: ref observed,
                state: ConnectionState::Disconnected(_),
                ..
            }) if observed == "node-a"
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::RelayRoute {
                node_id,
                route: C2RelayRoute::LocalIpc,
            }) if node_id == "node-a"
        ));

        let parked = C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                relay_route: C2RelayRoute::LocalIpc,
                transport: NodeTransportState::Parked,
                current_incarnation_id: None,
                observation_support: None,
                provider_contracts: Vec::new(),
                provider_adapter_contracts: Vec::new(),
                provider_runtime_statuses: Default::default(),
            }],
        };
        let snapshots = reconcile_c2_topology(
            &parked,
            &mut routes,
            &mut endpoints,
            &mut relay_routes,
            &updates,
        )
        .await;
        assert!(snapshots.is_empty());
        assert!(routes.is_empty());
        assert_eq!(endpoints.get("node-a"), Some(&endpoint));
        assert!(matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let online = C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                relay_route: C2RelayRoute::LocalIpc,
                transport: NodeTransportState::Online,
                current_incarnation_id: Some(incarnation),
                observation_support: None,
                provider_contracts: Vec::new(),
                provider_adapter_contracts: Vec::new(),
                provider_runtime_statuses: Default::default(),
            }],
        };
        let snapshots = reconcile_c2_topology(
            &online,
            &mut routes,
            &mut endpoints,
            &mut relay_routes,
            &updates,
        )
        .await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].expected_incarnation_id, incarnation);
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::ObservationSupport {
                node_id,
                incarnation_id,
                support: SessionMonitorSupport {
                    known: true,
                    events: false,
                    managed_target: false,
                    workflow_detail: false,
                },
            }) if node_id == "node-a" && incarnation_id == incarnation
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::State {
                state: ConnectionState::Connecting,
                ..
            })
        ));

        let snapshots = reconcile_c2_topology(
            &C2Topology { nodes: Vec::new() },
            &mut routes,
            &mut endpoints,
            &mut relay_routes,
            &updates,
        )
        .await;
        assert!(snapshots.is_empty());
        assert!(routes.is_empty());
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::TopologyNodeRemoved { node_id }) if node_id == "node-a"
        ));
    }

    #[test]
    fn c2_relay_route_reducer_preserves_authoritative_topology_fact_without_endpoint_inference() {
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        let cases = [
            ("node-local", "ssh://misleading", C2RelayRoute::LocalIpc),
            ("node-ssh", r"\\.\pipe\misleading", C2RelayRoute::SshForwardedLoopback),
            ("node-unknown", "127.0.0.1:9000", C2RelayRoute::Unknown),
        ];

        for (node_id, endpoint, route) in cases {
            apply_update(
                &mut app,
                &mut c2,
                WorkerUpdate::State {
                    node_id: node_id.to_owned(),
                    endpoint: endpoint.to_owned(),
                    state: ConnectionState::Connecting,
                },
            );
            apply_update(
                &mut app,
                &mut c2,
                WorkerUpdate::RelayRoute {
                    node_id: node_id.to_owned(),
                    route,
                },
            );
        }

        assert_eq!(app.nodes[0].relay_route, C2RelayRoute::LocalIpc);
        assert_eq!(app.nodes[1].relay_route, C2RelayRoute::SshForwardedLoopback);
        assert_eq!(app.nodes[2].relay_route, C2RelayRoute::Unknown);
    }

    #[tokio::test]
    async fn direct_and_c2_observation_support_stays_exact_per_node() {
        use gate4agent_c2_protocol::{C2ObservationSupport, C2RelayRoute, C2TopologyNode};

        let direct = negotiated_observation_support(&[
            CapabilityId::new(NODE_OBSERVATION_EVENTS_CAPABILITY).unwrap(),
            CapabilityId::new(NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY).unwrap(),
            CapabilityId::new(NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY).unwrap(),
        ]);
        assert_eq!(
            direct,
            SessionMonitorSupport {
                known: true,
                events: true,
                managed_target: true,
                workflow_detail: true,
            },
        );

        let c2_incarnation = incarnation(8);
        let legacy_incarnation = incarnation(9);
        let topology = C2Topology {
            nodes: vec![
                C2TopologyNode {
                    node_id: NodeId::new("node-c2").unwrap(),
                    endpoint: "c2-node".to_owned(),
                    relay_route: C2RelayRoute::Unknown,
                    transport: NodeTransportState::Online,
                    current_incarnation_id: Some(c2_incarnation),
                    observation_support: Some(C2ObservationSupport {
                        events: true,
                        managed_target: true,
                        workflow_detail: false,
                    }),
                    provider_contracts: Vec::new(),
                    provider_adapter_contracts: Vec::new(),
                    provider_runtime_statuses: Default::default(),
                },
                C2TopologyNode {
                    node_id: NodeId::new("node-legacy").unwrap(),
                    endpoint: "legacy-node".to_owned(),
                    relay_route: C2RelayRoute::Unknown,
                    transport: NodeTransportState::Online,
                    current_incarnation_id: Some(legacy_incarnation),
                    observation_support: Some(C2ObservationSupport {
                        events: false,
                        managed_target: false,
                        workflow_detail: false,
                    }),
                    provider_contracts: Vec::new(),
                    provider_adapter_contracts: Vec::new(),
                    provider_runtime_statuses: Default::default(),
                },
            ],
        };
        let mut routes = BTreeMap::new();
        let mut endpoints = BTreeMap::new();
        let mut relay_routes = BTreeMap::new();
        let (updates, mut receiver) = mpsc::channel(8);
        let snapshots = reconcile_c2_topology(
            &topology,
            &mut routes,
            &mut endpoints,
            &mut relay_routes,
            &updates,
        )
        .await;
        assert_eq!(snapshots.len(), 2);

        let mut supports = BTreeMap::new();
        for _ in 0..2 {
            let Some(WorkerUpdate::ObservationSupport { node_id, support, .. }) = receiver.recv().await
            else { panic!("per-node observation support must precede state updates") };
            supports.insert(node_id, support);
        }
        assert_eq!(
            supports["node-c2"],
            SessionMonitorSupport {
                known: true,
                events: true,
                managed_target: true,
                workflow_detail: false,
            },
        );
        assert_eq!(
            supports["node-legacy"],
            SessionMonitorSupport {
                known: true,
                events: false,
                managed_target: false,
                workflow_detail: false,
            },
        );
    }

    #[test]
    fn provider_shortcuts_preserve_ctrl_g_alt_bytes_and_operator_escape() {
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(map_key(ctrl_g), Some(UiKey::Ctrl('g')));
        let operator_escape = KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(map_key(operator_escape), Some(UiKey::OperatorEscape));
        for ch in ['b', 'f', 'p', 't', 'y'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT);
            assert_eq!(map_key(key), Some(UiKey::TerminalBytes(vec![0x1b, ch as u8])));
        }
    }

    #[test]
    fn modified_enter_keys_map_to_the_scoped_editor_alias() {
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            Some(UiKey::ModifiedEnter),
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(UiKey::ModifiedEnter),
        );
        assert_eq!(
            map_key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(UiKey::ModifiedEnter),
        );
        assert_eq!(
            map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(UiKey::Enter),
        );
    }

    #[test]
    fn frame_scheduler_is_idle_without_a_visible_animation() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::new(now);

        assert!(scheduler.redraw_due(now, false));
        assert!(!scheduler.consume_redraw(now, false));
        assert!(!scheduler.redraw_due(now + Duration::from_secs(1), false));
        assert_eq!(
            scheduler.poll_timeout(now + Duration::from_secs(1), false, &[]),
            EVENT_POLL_INTERVAL
        );
    }

    #[test]
    fn frame_scheduler_coalesces_dirty_frames_to_sixty_hz_and_keeps_animation_bounded() {
        let now = Instant::now();
        let mut scheduler = FrameScheduler::new(now);
        let _ = scheduler.consume_redraw(now, false);
        scheduler.mark_dirty();

        assert!(!scheduler.redraw_due(now + Duration::from_millis(15), false));
        assert!(scheduler.redraw_due(now + DIRTY_FRAME_INTERVAL, false));
        assert!(!scheduler.consume_redraw(now + DIRTY_FRAME_INTERVAL, true));
        assert!(!scheduler.redraw_due(
            now + DIRTY_FRAME_INTERVAL + ANIMATION_FRAME_INTERVAL - Duration::from_millis(1),
            true,
        ));
        assert!(scheduler.redraw_due(
            now + DIRTY_FRAME_INTERVAL + ANIMATION_FRAME_INTERVAL,
            true,
        ));
        assert!(scheduler.consume_redraw(
            now + DIRTY_FRAME_INTERVAL + ANIMATION_FRAME_INTERVAL,
            true,
        ));
        let after = now + DIRTY_FRAME_INTERVAL + ANIMATION_FRAME_INTERVAL;
        assert!(!scheduler.redraw_due(after + Duration::from_secs(1), false));
    }

    #[test]
    fn inactive_inspection_does_not_turn_a_past_deadline_into_a_zero_poll() {
        let now = Instant::now();
        let past = now - Duration::from_secs(1);
        let mut scheduler = FrameScheduler::new(now);
        let _ = scheduler.consume_redraw(now, false);

        for (route, visible, pending) in [(false, true, false), (true, false, false), (true, true, true)] {
            let deadline = inspection_poll_deadline(route, visible, pending, past);
            assert!(deadline.is_none());
            assert_eq!(scheduler.poll_timeout(now, false, &[deadline]), EVENT_POLL_INTERVAL);
        }
        assert_eq!(
            inspection_poll_deadline(true, true, false, past),
            Some(past),
        );
    }

    #[test]
    fn mouse_drag_coalescing_preserves_down_up_and_event_order() {
        let point = |kind, column| TerminalEvent::Mouse(MouseEvent {
            kind,
            column,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        let key = TerminalEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let events = coalesce_mouse_drags(vec![
            point(MouseEventKind::Down(MouseButton::Left), 1),
            point(MouseEventKind::Drag(MouseButton::Left), 2),
            point(MouseEventKind::Drag(MouseButton::Left), 3),
            key.clone(),
            point(MouseEventKind::Drag(MouseButton::Left), 4),
            point(MouseEventKind::Drag(MouseButton::Left), 5),
            point(MouseEventKind::Up(MouseButton::Left), 6),
        ]);

        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left), column: 1, ..
        })));
        assert!(matches!(events[1], TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left), column: 3, ..
        })));
        assert!(matches!(events[2], TerminalEvent::Key(_)));
        assert!(matches!(events[3], TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left), column: 5, ..
        })));
        assert!(matches!(events[4], TerminalEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left), column: 6, ..
        })));
    }

    #[test]
    fn cursor_is_visible_only_for_focused_pty_and_clamped() {
        let mut app = cursor_app();
        assert_eq!(visible_cursor_position(&app), Some((35, 5)));
        app.nodes[0].workspaces[0].sessions[0].running = false;
        assert_eq!(visible_cursor_position(&app), None);
        app.nodes[0].workspaces[0].sessions[0].running = true;
        app.focus = crate::app::Focus::Tabs;
        assert_eq!(visible_cursor_position(&app), None);
        app.focus = crate::app::Focus::Viewport;
        let address = cursor_address(&app);
        app.terminal_scroll_offsets.insert(address, 1);
        assert_eq!(visible_cursor_position(&app), None);
    }

    #[test]
    fn cursor_uses_focused_grid_pane_viewport() {
        let mut app = cursor_app();
        app.layout.surface_panes.push(SurfacePaneLayout {
            pane_id: PaneId(0),
            frame: uzor_tui::Rect::new(39, 8, 10, 6),
            header: uzor_tui::Rect::new(40, 9, 8, 1),
            viewport: uzor_tui::Rect::new(40, 10, 7, 3),
        });

        assert_eq!(visible_cursor_position(&app), Some((46, 12)));
    }

    #[test]
    fn right_mouse_down_opens_agent_menu_without_activating_pty() {
        let mut app = cursor_app();
        app.focus = crate::app::Focus::Viewport;
        app.layout.hits.push(HitRegion {
            rect: uzor_tui::Rect::new(0, 3, 24, 2),
            target: HitTarget::Agent(0),
        });
        let focused = app.focused_address().cloned();
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 4,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(map_mouse(&mut app, event), AppAction::None);
        assert!(app.agent_menu.is_some());
        assert_eq!(app.focus, crate::app::Focus::Viewport);
        assert_eq!(app.focused_address(), focused.as_ref());
    }

    #[test]
    fn moved_mouse_enters_and_leaves_context_usage_hit_region() {
        let mut app = cursor_app();
        let hit = ContextUsageSegmentHit {
            segment: ContextUsageSegment::CacheRead,
            tokens: 25,
            context_window: 100,
            evidence: ObservationEvidenceV1::StructuredProvider,
        };
        app.layout.hits.push(HitRegion {
            rect: uzor_tui::Rect::new(10, 5, 20, 1),
            target: HitTarget::ContextUsageSegment(hit),
        });

        assert_eq!(map_mouse(&mut app, MouseEvent {
            kind: MouseEventKind::Moved,
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }), AppAction::None);
        assert_eq!(app.context_usage_hover.unwrap().hit, hit);

        assert_eq!(map_mouse(&mut app, MouseEvent {
            kind: MouseEventKind::Moved,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }), AppAction::None);
        assert!(app.context_usage_hover.is_none());
    }

    #[test]
    fn mouse_down_drag_up_keeps_a_surface_tab_unique() {
        let mut app = cursor_app();
        app.layout.hits.push(HitRegion {
            rect: uzor_tui::Rect::new(1, 0, 8, 1),
            target: HitTarget::SurfaceTab(PaneId(0), 0),
        });
        app.layout.surface_panes.push(SurfacePaneLayout {
            pane_id: PaneId(0),
            frame: uzor_tui::Rect::new(20, 1, 10, 8),
            header: uzor_tui::Rect::new(20, 1, 10, 1),
            viewport: uzor_tui::Rect::new(20, 2, 10, 7),
        });

        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, down), AppAction::None);
        assert!(matches!(app.drag_state, Some(DragState::SessionChip { moved: false, .. })));

        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 21,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, drag), AppAction::None);
        assert!(matches!(app.drag_state, Some(DragState::SessionChip { moved: true, .. })));

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 21,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, up), AppAction::None);
        assert!(app.drag_state.is_none());
        assert_eq!(app.surface.leaf_ids(), vec![PaneId(0)]);
        assert_eq!(app.surface.focused_pane().tabs.len(), 1);
    }

    #[test]
    fn mouse_down_routes_spawn_field_and_launch_button_through_app_click() {
        let mut app = spawn_modal_app();
        app.layout.hits.extend([
            HitRegion {
                rect: uzor_tui::Rect::new(24, 9, 30, 1),
                target: HitTarget::SpawnField(LaunchField::Provider),
            },
            HitRegion {
                rect: uzor_tui::Rect::new(62, 20, 12, 1),
                target: HitTarget::SpawnLaunch,
            },
        ]);

        let field_down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, field_down), AppAction::None);
        assert_eq!(app.spawn.as_ref().unwrap().field, LaunchField::Provider);

        let launch_down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 64,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            map_mouse(&mut app, launch_down),
            AppAction::SpawnSpec {
                prompt: None,
                ..
            }
        ));
        assert!(app.spawn.is_none());
    }

    #[test]
    fn mouse_down_drag_up_moves_spawn_modal_and_ends_drag() {
        let mut app = spawn_modal_app();
        app.layout.hits.push(HitRegion {
            rect: uzor_tui::Rect::new(20, 5, 60, 1),
            target: HitTarget::SpawnDrag,
        });

        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, down), AppAction::None);
        assert!(matches!(app.drag_state, Some(DragState::SpawnModal { .. })));

        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 35,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, drag), AppAction::None);
        assert_eq!(app.spawn_modal_position, Some((30, 10)));

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 35,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, up), AppAction::None);
        assert!(app.drag_state.is_none());
    }

    #[test]
    fn terminal_size_diff_tracks_visible_sessions_without_repeat_storms() {
        let first = cursor_address(&cursor_app());
        let mut second = first.clone();
        second.instance_id = 2;
        let mut third = first.clone();
        third.instance_id = 3;
        let mut last = BTreeMap::new();

        let initial = diff_terminal_sizes(
            vec![(first.clone(), 20, 80), (second.clone(), 20, 80)],
            &mut last,
        );
        assert_eq!(initial.len(), 2);
        assert!(diff_terminal_sizes(
            vec![(first.clone(), 20, 80), (second.clone(), 20, 80)],
            &mut last,
        )
        .is_empty());

        let changed = diff_terminal_sizes(
            vec![(second.clone(), 18, 60), (third.clone(), 18, 60)],
            &mut last,
        );
        assert_eq!(
            changed,
            vec![
                AppAction::Resize {
                    address: second.clone(),
                    rows: 18,
                    cols: 60,
                },
                AppAction::Resize {
                    address: third.clone(),
                    rows: 18,
                    cols: 60,
                },
            ]
        );
        assert_eq!(last.len(), 2);
        assert!(!last.contains_key(&first));
    }

    #[test]
    fn action_projection_is_pty_only_and_restart_has_no_prompt() {
        let address = cursor_address(&cursor_app());
        let spawn = action_to_request(AppAction::Spawn {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("kimi"),
            rows: 24,
            cols: 80,
        })
        .unwrap();
        assert!(matches!(spawn, NodeRequest::Spawn { mode: SessionMode::Pty, initial_prompt: None, .. }));
        let resume = action_to_request(AppAction::Resume { address, rows: 24, cols: 80 }).unwrap();
        assert!(matches!(resume, NodeRequest::Resume { initial_prompt: None, .. }));
    }

    #[test]
    fn folder_browser_action_projects_only_selected_node_directory_and_cursor() {
        let directory = host_path(r"D:\projects");
        let after = host_path(r"D:\projects\alpha");
        let request = action_to_request(AppAction::BrowseHostDirectories {
            node_id: "node-b".to_owned(),
            directory: Some(directory.clone()),
            after: Some(after.clone()),
            token: 41,
            append: true,
        })
        .unwrap();

        assert_eq!(action_node_id(&AppAction::BrowseHostDirectories {
            node_id: "node-b".to_owned(),
            directory: None,
            after: None,
            token: 42,
            append: false,
        }), Some("node-b"));
        assert_eq!(
            request,
            NodeRequest::BrowseHostDirectories {
                directory: Some(directory),
                after: Some(after),
            }
        );
    }

    #[test]
    fn session_lab_spawn_actions_project_strict_specs() {
        let request = action_to_request(AppAction::SpawnSpec {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            rows: 31,
            cols: 107,
            profile_id: "review".to_owned(),
            profile_revision: "review.r1".to_owned(),
            prompt: Some("inspect the diff".to_owned()),
            bundle_id: Some("review-bundle".to_owned()),
            context_id: None,
            idempotency_key: "launch-1".to_owned(),
        }).unwrap();
        let NodeRequest::SpawnSpec { spec } = request else {
            panic!("expected spawn spec request");
        };
        assert_eq!(spec.target.node_id.as_str(), "node-a");
        assert_eq!(spec.target.workspace_id.as_str(), "workspace-a");
        assert_eq!(spec.profile_id.as_str(), "review");
        assert_eq!(spec.expected_profile_revision.as_str(), "review.r1");
        assert_eq!(spec.deadline_ms.get(), 20_000);
        assert_eq!(spec.idempotency_key.as_str(), "launch-1");
        assert_eq!(
            spec.required_capabilities.as_slice(),
            &[CapabilityId::new(SPAWN_RUNTIME_RAW_PTY_LIFECYCLE).unwrap()],
        );
        assert!(matches!(
            spec.overrides.provider,
            SpawnOverride::Set { value } if value.as_str() == "codex"
        ));
        assert!(matches!(spec.overrides.mode, SpawnOverride::Set { value: SessionMode::Pty }));
        assert!(matches!(
            spec.overrides.terminal_size,
            SpawnOverride::Set { value: TerminalSize { rows: 31, columns: 107 } }
        ));
        assert!(matches!(
            spec.overrides.prompt,
            SpawnOverride::Set { value } if value.as_str() == "inspect the diff"
        ));
        assert!(matches!(
            spec.overrides.bundle_id,
            SpawnOverride::Set { value } if value.as_str() == "review-bundle"
        ));
        assert!(matches!(spec.overrides.context_id, SpawnOverride::Clear));
        assert!(matches!(spec.overrides.environment_profile_id, SpawnOverride::Clear));

        let managed = action_to_request(AppAction::SpawnManagedWorktree {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("claude"),
            rows: 24,
            cols: 80,
            profile_id: "review".to_owned(),
            profile_revision: "review.r1".to_owned(),
            worktree_profile_id: "isolated-review".to_owned(),
            prompt: None,
            bundle_id: None,
            context_id: Some("context-1".to_owned()),
            idempotency_key: "launch-2".to_owned(),
        }).unwrap();
        let NodeRequest::SpawnManagedWorktree { request } = managed else {
            panic!("expected managed worktree spawn request");
        };
        assert_eq!(request.worktree_profile_id.as_str(), "isolated-review");
        assert_eq!(
            request.spawn_spec.expected_profile_revision.as_str(),
            "review.r1",
        );
        assert_eq!(
            request.spawn_spec.required_capabilities.as_slice(),
            &[CapabilityId::new(SPAWN_RUNTIME_RAW_PTY_LIFECYCLE).unwrap()],
        );
        assert!(matches!(
            request.spawn_spec.overrides.mode,
            SpawnOverride::Set { value: SessionMode::Pty }
        ));
        assert!(matches!(request.spawn_spec.overrides.prompt, SpawnOverride::Clear));
        assert!(matches!(request.spawn_spec.overrides.bundle_id, SpawnOverride::Clear));
        assert!(matches!(
            request.spawn_spec.overrides.context_id,
            SpawnOverride::Set { value } if value.as_str() == "context-1"
        ));
    }

    #[test]
    fn session_lab_history_context_actions_map_without_message_payloads() {
        let address = cursor_address(&cursor_app());
        let discover = action_to_request(AppAction::DiscoverHistory {
            address: address.clone(),
            limit: 7,
        }).unwrap();
        assert!(matches!(discover, NodeRequest::DiscoverHistory { limit: 7, .. }));
        let load = action_to_request(AppAction::LoadHistory {
            address: address.clone(),
            candidate_id: "candidate-1".to_owned(),
        }).unwrap();
        assert!(matches!(
            load,
            NodeRequest::LoadHistory { candidate_id, .. } if candidate_id == "candidate-1"
        ));
        assert!(matches!(
            action_to_request(AppAction::ExportContextPack { address }).unwrap(),
            NodeRequest::ExportContextPack { .. }
        ));
        assert!(matches!(
            action_to_request(AppAction::ForgetContextPack {
                node_id: "node-a".to_owned(),
                context_id: "context-1".to_owned(),
            }).unwrap(),
            NodeRequest::ForgetContextPack { context_id } if context_id.as_str() == "context-1"
        ));
    }

    #[test]
    fn session_lab_worker_updates_apply_only_bounded_history_metadata() {
        let mut app = cursor_app();
        let source = cursor_address(&app);
        app.history = Some(crate::app::HistoryDialog {
            source: source.clone(),
            source_provider: provider("codex"),
            source_workspace_root: host_path(r"C:\work\acme"),
            candidates: Vec::new(),
            selected: 0,
            loaded: None,
            context: None,
            pending_label: Some("discovering".to_owned()),
        });
        let mut c2 = C2ApplyState::default();
        apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::HistoryDiscovered {
                source: source.clone(),
                candidates: vec![gate4agent_types::HistoryCandidateSummary {
                    id: "candidate-1".to_owned(),
                    session_id_hint: "session-1".to_owned(),
                    modified_at_unix_ms: Some(7),
                }],
            },
        );
        assert_eq!(app.history.as_ref().unwrap().candidates.len(), 1);
        apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::HistoryLoaded {
                source,
                session_id: "session-1".to_owned(),
                message_count: 12,
                completed_turn_count: Some(5),
            },
        );
        let loaded = app.history.as_ref().unwrap().loaded.as_ref().unwrap();
        assert_eq!(loaded.session_id, "session-1");
        assert_eq!(loaded.message_count, 12);
        assert_eq!(loaded.completed_turn_count, Some(5));
    }

    #[test]
    fn raw_terminal_bytes_project_without_text_or_paste_framing() {
        let address = cursor_address(&cursor_app());
        let request = action_to_request(AppAction::TerminalBytes {
            address,
            bytes: b"\x1bp".to_vec(),
        })
        .unwrap();
        assert!(matches!(
            request,
            NodeRequest::TerminalBytes { bytes, .. } if bytes == b"\x1bp"
        ));
    }

    #[test]
    fn workspace_actions_project_exact_id_and_root() {
        let register = action_to_request(AppAction::RegisterWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
            root: host_path(r"C:\tmp\scratch"),
        })
        .unwrap();
        assert!(matches!(register, NodeRequest::RegisterWorkspace { workspace_id, root } if workspace_id.as_str() == "scratch" && root == host_path(r"C:\tmp\scratch")));
        let unregister = action_to_request(AppAction::UnregisterWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
        })
        .unwrap();
        assert!(matches!(unregister, NodeRequest::UnregisterWorkspace { workspace_id } if workspace_id.as_str() == "scratch"));
        let inspect = action_to_request(AppAction::InspectWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
        })
        .unwrap();
        assert!(matches!(inspect, NodeRequest::InspectWorkspace { workspace_id } if workspace_id.as_str() == "scratch"));

        let standalone = action_to_request(AppAction::CreateStandaloneWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch-repo".to_owned(),
            root: host_path(r"C:\tmp\scratch-repo"),
            initial_branch: Some("trunk".to_owned()),
        })
        .unwrap();
        assert!(matches!(
            standalone,
            NodeRequest::CreateStandaloneWorkspace {
                workspace_id,
                root,
                initial_branch: Some(branch),
            } if workspace_id.as_str() == "scratch-repo"
                && root == host_path(r"C:\tmp\scratch-repo")
                && branch == "trunk"
        ));
        assert_eq!(
            action_node_id(&AppAction::CreateStandaloneWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "scratch-repo".to_owned(),
                root: host_path(r"C:\tmp\scratch-repo"),
                initial_branch: None,
            }),
            Some("node-a"),
        );
    }

    #[test]
    fn managed_session_actions_project_exact_node_protocol_requests() {
        let resume = action_to_request(AppAction::ResumeSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
            rows: 31,
            cols: 107,
            initial_prompt: None,
            operation_token: 0,
        })
        .unwrap();
        assert!(matches!(
            resume,
            NodeRequest::ResumeSessionRecord { record_id, terminal_size, initial_prompt: None }
                if record_id.as_str() == "record-1"
                    && terminal_size.rows == 31
                    && terminal_size.columns == 107
        ));
        let rename = action_to_request(AppAction::RenameSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
            display_name: "review".to_owned(),
        })
        .unwrap();
        assert!(matches!(
            rename,
            NodeRequest::RenameSessionRecord { record_id, display_name }
                if record_id.as_str() == "record-1" && display_name == "review"
        ));
        let task_id: gate4agent_node_protocol::TaskId =
            "task-0123456789abcdef01234567".parse().unwrap();
        let set_task = action_to_request(AppAction::SetSessionTask {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
            expected_revision: 11,
            target: gate4agent_node_protocol::SessionTaskTargetV1::Existing {
                task_id: task_id.clone(),
            },
        })
        .unwrap();
        assert!(matches!(
            set_task,
            NodeRequest::SetSessionTask {
                record_id,
                expected_revision: 11,
                target: gate4agent_node_protocol::SessionTaskTargetV1::Existing { task_id: actual },
            } if record_id.as_str() == "record-1" && actual == task_id
        ));
        let forget = action_to_request(AppAction::ForgetSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
        })
        .unwrap();
        assert!(matches!(
            forget,
            NodeRequest::ForgetSessionRecord { record_id }
                if record_id.as_str() == "record-1"
        ));
    }

    #[test]
    fn task_binding_projection_has_direct_and_c2_parity() {
        let binding = gate4agent_node_protocol::SessionTaskBindingV1 {
            revision: 5,
            task_id: Some("task-0123456789abcdef01234567".parse().unwrap()),
            changed_at_unix_ms: 7,
        };
        let direct = ManagedSessionRecord {
            record_id: SessionRecordId::new("record-task").unwrap(),
            display_name: "task run".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            canonical_root: host_path(r"C:\work\acme"),
            provider_session: None,
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: None,
            task_binding: Some(binding.clone()),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 7,
            last_error: None,
        };
        let c2 = C2ManagedSessionRecord::from(&direct);
        let direct_view = project_managed_session("node-a", direct);
        let c2_view = project_c2_managed_session("node-a", c2);
        assert_eq!(direct_view.task_binding, Some(binding.clone()));
        assert_eq!(c2_view.task_binding, Some(binding));
        assert_eq!(direct_view.task_binding, c2_view.task_binding);
    }

    #[test]
    fn index_provider_session_action_maps_exact_reference_request() {
        let request = action_to_request(AppAction::IndexProviderSession {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            identity: gate4agent_types::ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: "native-session-7".to_owned(),
                transcript_path: None,
            },
            display_name: "Recovered Codex".to_owned(),
            operation_token: 0,
        })
        .unwrap();

        assert!(matches!(
            request,
            NodeRequest::IndexProviderSession {
                workspace_id,
                provider,
                identity,
                display_name,
            } if workspace_id.as_str() == "workspace-a"
                && provider == AgentId::new("codex").unwrap()
                && identity.key == gate4agent_types::ProviderSessionKey::SessionId
                && identity.id == "native-session-7"
                && identity.transcript_path.is_none()
                && display_name == "Recovered Codex"
        ));
    }

    #[test]
    fn native_session_catalog_client_fans_out_one_bounded_request_per_workspace_provider_route() {
        let routes = vec![
            NativeSessionCatalogRoute::workspace("workspace-a".to_owned(), provider("claude")),
            NativeSessionCatalogRoute::workspace("workspace-a".to_owned(), provider("grok")),
            NativeSessionCatalogRoute::workspace("workspace-b".to_owned(), provider("qwen-code")),
        ];
        let requests = routes.iter().map(|route| {
            (route.clone(), native_session_catalog_request(route, 64).unwrap())
        }).collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        assert!(matches!(&requests[0].1, NodeRequest::CatalogNativeSessions {
            route, limit: 64,
        } if route.workspace_id.as_ref().map(WorkspaceId::as_str) == Some("workspace-a")
            && route.provider.as_str() == "claude"));
        assert!(matches!(&requests[1].1, NodeRequest::CatalogNativeSessions {
            route, limit: 64,
        } if route.workspace_id.as_ref().map(WorkspaceId::as_str) == Some("workspace-a")
            && route.provider.as_str() == "grok"));
        assert!(matches!(&requests[2].1, NodeRequest::CatalogNativeSessions {
            route, limit: 64,
        } if route.workspace_id.as_ref().map(WorkspaceId::as_str) == Some("workspace-b")
            && route.provider.as_str() == "qwen-code"));
    }

    #[test]
    fn native_preview_without_history_source_is_explicitly_unavailable() {
        let (c2_message, c2_unavailable) = catalog_failure_fields(
            NodeFailureCode::InvalidRequest,
            "invalid request",
        );
        assert!(c2_unavailable);
        assert_eq!(
            c2_message,
            "native session history is not configured on this Node",
        );

    }

    #[test]
    fn direct_and_c2_catalog_success_projection_preserves_local_request_token() {
        let update = native_sessions_cataloged_update(
            "node-a".to_owned(),
            41,
            WireNativeSessionCatalogRoute::workspace(
                WorkspaceId::new("workspace-a").unwrap(),
                AgentId::new("codex").unwrap(),
            ),
            vec![NativeSessionCatalogEntry {
                selection_id: "hist_019f".to_owned(),
                title: Some("Review".to_owned()),
                modified_at_unix_ms: Some(7),
                model: Some("gpt-5".to_owned()),
                message_count: 4,
                completed_turn_count: Some(2),
                external_group: None,
                record_id: None,
            }],
            Some(NativeSessionCatalogSummary {
                catalog_revision: 7,
                recent_cutoff_unix_ms: 9,
                recent_total_count: 1,
                older_total_count: 3,
                recent_next_after_selection_id: None,
                recent_has_more: false,
            }),
        );
        assert!(matches!(
            update,
            WorkerUpdate::NativeSessionsCataloged {
                node_id,
                route,
                token: 41,
                entries,
                summary,
            } if node_id == "node-a"
                && route.workspace_id.as_deref() == Some("workspace-a")
                && route.provider == AgentId::new("codex").unwrap()
                && entries[0].message_count == 4
                && summary.as_ref().map(|value| value.older_total_count) == Some(3)
        ));
    }

    #[test]
    fn legacy_catalog_projection_preserves_absent_paging_summary() {
        let update = native_sessions_cataloged_update(
            "node-a".to_owned(),
            42,
            WireNativeSessionCatalogRoute::workspace(
                WorkspaceId::new("workspace-a").unwrap(),
                AgentId::new("codex").unwrap(),
            ),
            vec![NativeSessionCatalogEntry {
                selection_id: "legacy-selection".to_owned(),
                title: None,
                modified_at_unix_ms: None,
                model: None,
                message_count: 1,
                completed_turn_count: None,
                external_group: None,
                record_id: None,
            }],
            None,
        );
        assert!(matches!(update,
            WorkerUpdate::NativeSessionsCataloged {
                token: 42,
                entries,
                summary: None,
                ..
            } if entries[0].selection_id == "legacy-selection"
        ));
    }

    #[test]
    fn native_and_record_preview_actions_keep_opaque_correlation_separate() {
        let route = NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(),
            provider("codex"),
        );
        let native = NodeRequest::PreviewNativeSession {
            selection: wire_native_session_selection(&route, 7, 9, "hist_019f").unwrap(),
            message_limit: 24,
        };
        assert!(matches!(&native, NodeRequest::PreviewNativeSession {
            selection, message_limit: 24,
        } if selection.route.workspace_id.as_ref().map(WorkspaceId::as_str) == Some("workspace-a")
            && selection.route.provider == AgentId::new("codex").unwrap()
            && selection.selection_id == "hist_019f"));
        let record = action_to_request(AppAction::PreviewSessionRecord {
            node_id: "node-a".to_owned(), record_id: "record-7".to_owned(),
            message_limit: 24, token: 74,
        }).unwrap();
        assert!(matches!(&record, NodeRequest::PreviewSessionRecord {
            record_id, message_limit: 24,
        } if record_id.as_str() == "record-7"));
        for request in [&native, &record] {
            assert!(!matches!(
                request,
                NodeRequest::Spawn { .. }
                    | NodeRequest::SpawnSpec { .. }
                    | NodeRequest::SpawnManagedWorktree { .. }
                    | NodeRequest::Resume { .. }
                    | NodeRequest::ResumeSessionRecord { .. }
            ));
        }

        let projected = project_session_record_preview(SessionRecordPreview {
            title: Some("Dormant record".to_owned()), modified_at_unix_ms: Some(9),
            model: Some("gpt-5".to_owned()), message_count: 1,
            message_count_exact: true,
            completed_turn_count: Some(0), total_tokens: None, truncated: false,
            messages: vec![gate4agent_types::NativeSessionPreviewMessage {
                role: gate4agent_types::HistoryMessageRole::Assistant,
                text: "bounded answer".to_owned(),
            }],
        });
        assert_eq!(projected.messages[0].text, "bounded answer");
    }

    #[test]
    fn history_refresh_via_c2_maps_exact_request_and_discards_preview_payload() {
        let incarnation_id = incarnation(91);
        let action = AppAction::RefreshSessionRecordHistory {
            node_id: "node-a".to_owned(),
            node_incarnation_id: incarnation_id,
            record_id: "record-history".to_owned(),
            message_limit: 1,
        };
        assert_eq!(action_node_id(&action), Some("node-a"));
        assert!(matches!(
            action_to_request(action),
            Some(NodeRequest::PreviewSessionRecord { record_id, message_limit: 1 })
                if record_id.as_str() == "record-history"
        ));

        let preview = SessionRecordPreview {
            title: Some("history".to_owned()),
            modified_at_unix_ms: Some(9),
            model: Some("model".to_owned()),
            message_count: 7,
            message_count_exact: true,
            completed_turn_count: Some(3),
            total_tokens: Some(144),
            truncated: true,
            messages: vec![gate4agent_types::NativeSessionPreviewMessage {
                role: gate4agent_types::HistoryMessageRole::Assistant,
                text: "RAW-PREVIEW-MUST-NOT-REACH-APP".to_owned(),
            }],
        };
        let record_id = SessionRecordId::new("record-history").unwrap();
        let through_c2 = c2_session_record_history_refresh_update(
            "node-a".to_owned(),
            "record-history",
            incarnation_id,
            Ok(C2NodeResponse::SessionRecordPreviewed { record_id, preview }),
        );
        assert!(matches!(
            &through_c2,
            WorkerUpdate::SessionRecordHistoryRefreshed {
                node_id,
                record_id,
                incarnation_id: actual,
            } if node_id == "node-a"
                && record_id == "record-history"
                && *actual == incarnation_id
        ));
    }

    #[test]
    fn history_refresh_failures_are_categorical_and_never_forward_raw_provider_error() {
        let incarnation_id = incarnation(92);
        let raw = "RAW-PROVIDER-HOME-AND-SESSION-CONTENT";
        let through_c2 = c2_session_record_history_refresh_update(
            "node-a".to_owned(),
            "record-history",
            incarnation_id,
            Err(C2NodeFailure {
                code: NodeFailureCode::BackendOperationFailed,
                message: raw.to_owned(),
            }),
        );
        assert!(matches!(
            &through_c2,
            WorkerUpdate::SessionRecordHistoryRefreshFailed {
                message,
                ..
            } if message == "node backend operation failed"
        ));
    }

    #[test]
    fn history_refresh_replacement_guard_rejects_old_c2_incarnations() {
        let requested = incarnation(93);
        let replacement = incarnation(94);
        assert_ne!(requested, replacement);
        let c2_route_incarnation = replacement;
        assert!(!history_refresh_incarnation_matches(
            requested,
            c2_route_incarnation,
        ));
        assert!(history_refresh_incarnation_matches(requested, requested));
    }

    #[test]
    fn history_refresh_queue_rejection_clears_pending_and_allows_explicit_retry() {
        let incarnation_id = incarnation(96);
        let mut app = cursor_app();
        app.nodes[0].incarnation_id = Some(incarnation_id);
        app.nodes[0].session_records.push(ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-queue".to_owned(),
            display_name: "Queue retry".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: None,
            has_provider_session_identity: true,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            active_session: None,
            last_error: None,
        });
        let agent = crate::app::AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-queue".to_owned(),
        };
        let action = app.open_session_monitor(agent.clone());
        assert!(matches!(action, AppAction::RefreshSessionRecordHistory { .. }));
        let (commands, _queued) = mpsc::channel(1);
        commands.try_send(AppAction::None).unwrap();
        let routes = BTreeMap::from([("node-a".to_owned(), commands)]);

        send_operator_action(&mut app, &routes, action);

        assert_eq!(
            app.notice.as_deref(),
            Some("Session Monitor history refresh failed: command queue busy"),
        );
        assert!(matches!(
            app.open_session_monitor(agent),
            AppAction::RefreshSessionRecordHistory { message_limit: 1, .. }
        ));
    }

    #[test]
    fn native_and_managed_preview_projection_preserves_total_token_tristate() {
        let preview = |total_tokens| SessionRecordPreview {
            title: Some("Bounded record".to_owned()),
            modified_at_unix_ms: Some(9),
            model: Some("gpt-5".to_owned()),
            message_count: 1,
            message_count_exact: true,
            completed_turn_count: Some(0),
            total_tokens,
            truncated: false,
            messages: vec![gate4agent_types::NativeSessionPreviewMessage {
                role: gate4agent_types::HistoryMessageRole::Assistant,
                text: "bounded answer".to_owned(),
            }],
        };

        for total_tokens in [None, Some(0), Some(4_321)] {
            let source = preview(total_tokens);
            let native = project_native_session_preview(source.clone());
            let managed = project_session_record_preview(source);

            assert_eq!(native.total_tokens, total_tokens);
            assert_eq!(managed.total_tokens, total_tokens);
            assert_eq!(native, managed);
        }
    }

    #[test]
    fn worktree_actions_project_exact_source_target_branch_and_base() {
        let create = action_to_request(AppAction::CreateWorktree {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            workspace_id: "feature-a".to_owned(),
            target_root: host_path(r"C:\work\feature-a"),
            branch: "feature/a".to_owned(),
            base: Some("origin/main".to_owned()),
        })
        .unwrap();
        assert!(matches!(create, NodeRequest::CreateWorktree {
            source_workspace_id,
            workspace_id,
            target_root,
            branch,
            base: Some(base),
        } if source_workspace_id.as_str() == "workspace-a"
            && workspace_id.as_str() == "feature-a"
            && target_root == host_path(r"C:\work\feature-a")
            && branch == "feature/a"
            && base == "origin/main"));
        let remove = action_to_request(AppAction::RemoveWorktree {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            target_root: host_path(r"C:\work\feature-a"),
        })
        .unwrap();
        assert!(matches!(remove, NodeRequest::RemoveWorktree { source_workspace_id, target_root }
            if source_workspace_id.as_str() == "workspace-a" && target_root == host_path(r"C:\work\feature-a")));
    }

    #[test]
    fn worktree_response_projection_selects_created_and_refreshes_authoritative_workspace() {
        let created_workspace_id = WorkspaceId::new("feature-a").unwrap();
        let created = NodeResponse::WorktreeCreated {
            worktree: gate4agent_node_protocol::GitWorktreeSnapshot {
                path: host_path(r"C:\work\feature-a"),
                head: "abc".to_owned(),
                branch: Some("feature/a".to_owned()),
                is_bare: false,
                is_main: false,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: Some(created_workspace_id.clone()),
            },
            workspace: gate4agent_node_protocol::WorkspaceSnapshot {
                workspace_id: created_workspace_id.clone(),
                canonical_root: host_path(r"C:\work\feature-a"),
                sessions: Vec::new(),
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            },
        };
        let projection = project_worktree_response(
            &created,
            "node-a",
            Some(&WorkspaceId::new("workspace-a").unwrap()),
        )
        .unwrap();
        assert_eq!(projection.selected_workspace_id.as_deref(), Some("feature-a"));
        assert_eq!(projection.refresh_workspace_id, Some(created_workspace_id));
        assert!(projection.notice.contains("created"));

        let source = WorkspaceId::new("workspace-a").unwrap();
        let removed = NodeResponse::WorktreeRemoved {
            target_root: host_path(r"C:\work\feature-a"),
            workspace_id: Some(WorkspaceId::new("feature-a").unwrap()),
        };
        let projection = project_worktree_response(&removed, "node-a", Some(&source)).unwrap();
        assert_eq!(projection.selected_workspace_id, None);
        assert_eq!(projection.refresh_workspace_id, Some(source.clone()));
        assert!(projection.notice.contains("removed"));

        let removed_source = NodeResponse::WorktreeRemoved {
            target_root: host_path(r"C:\work\workspace-a"),
            workspace_id: Some(source.clone()),
        };
        let projection = project_worktree_response(&removed_source, "node-a", Some(&source)).unwrap();
        assert_eq!(projection.selected_workspace_id, None);
        assert_eq!(projection.refresh_workspace_id, None);
        assert!(projection.notice.contains("removed"));
    }

    #[test]
    fn inspection_uses_observer_queue_not_operator_command_queue() {
        let mut app = cursor_app();
        let (operator_tx, mut operator_rx) = mpsc::channel(1);
        let mut operator = BTreeMap::new();
        operator.insert("node-a".to_owned(), operator_tx);
        let (inspection_tx, mut inspection_rx) = watch::channel(None);
        let mut inspections = BTreeMap::new();
        inspections.insert("node-a".to_owned(), inspection_tx);

        send_action(
            &mut app,
            &operator,
            &inspections,
            AppAction::InspectWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
            },
        );

        assert!(matches!(
            operator_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(inspection_rx.has_changed().unwrap());
        assert_eq!(
            inspection_rx.borrow_and_update().as_ref().unwrap().as_str(),
            "workspace-a"
        );
    }

    #[test]
    fn harness_only_backend_rejects_node_actions_without_transport_fallback() {
        let mut app = App::default();
        let (harness_tx, mut harness_rx) = mpsc::channel(1);
        let (history_tx, mut history_rx) = mpsc::channel(1);
        let (detail_tx, mut detail_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);

        // `UnregisterWorkspace` has neither a Harness route nor a specific
        // reject handler (unlike `WriteWorkspaceFile`/`CreateWorkspace*`,
        // covered by `harness_only_backend_clears_optimistic_write_state_instead_of_hanging`
        // below): the generic fallback notice is the only feedback.
        // `InspectWorkspace` and siblings are covered by
        // `harness_only_backend_routes_sidebar_workspace_reads_to_the_detail_lane`,
        // now that the dispatch layer rewrites them.
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::UnregisterWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
            },
        );

        assert!(matches!(
            harness_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(matches!(
            history_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(matches!(
            detail_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness-owned session action unavailable: no typed Harness intent exists"),
        );
    }

    #[test]
    fn harness_only_backend_clears_optimistic_write_state_instead_of_hanging() {
        let mut app = App::default();
        let (harness_tx, _harness_rx) = mpsc::channel(1);
        let (history_tx, _history_rx) = mpsc::channel(1);
        let (detail_tx, _detail_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);

        // Writes stay direct-mode-only (no Harness intent exists for them).
        // The optimistic "saving" state `Ctrl+s` already set on the editor
        // must clear to an honest error, not hang forever — see
        // `reject_workspace_write_action`.
        let path = RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        let file_key = WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: path.clone(),
        };
        let mut editor = crate::text_editor::TextEditor::new(
            "fn main() {}\n".to_owned(),
            Some("b".repeat(64)),
        ).unwrap();
        editor.mark_saving();
        app.file_tabs.insert(file_key.clone(), crate::app::WorkspaceFileTabView {
            editor,
            state: crate::app::WorkspaceFileState::Ready,
            edit_mode: false,
            request_token: 1,
            inline_history: None,
        });
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::WriteWorkspaceFile {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path,
                expected_revision: "b".repeat(64),
                text: "fn main() {}\n".to_owned(),
                token: 1,
            },
        );
        // No generic notice: `reject_workspace_write_action` already routed
        // the honest failure into the file tab's own error state (rendered
        // inline where the user tried to save), the same division of labor
        // `reject_history_refresh_action` uses for history refresh.
        assert_eq!(app.notice, None);
        assert!(matches!(
            app.file_tabs.get(&file_key).unwrap().editor.sync_state(),
            crate::text_editor::SyncState::Error(_),
        ), "write rejection must clear the optimistic saving state, not hang");

        // The "creating without overwrite..." modal spinner must clear too.
        app.create_workspace_entry = Some(crate::app::CreateWorkspaceEntryDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: "new/hello.txt".to_owned(),
            kind: WorkspaceEntryKind::File,
            pending: true,
            error: None,
            token: 2,
        });
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::CreateWorkspaceFile {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: RepositoryPath::utf8("new/hello.txt".to_owned()).unwrap(),
                token: 2,
            },
        );
        // Unlike `fail_workspace_file`, `fail_workspace_entry_create` also
        // surfaces a notice (the create dialog is a modal, not a background
        // save) — both channels carry the same honest, non-generic message.
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness-owned session action unavailable"),
        );
        let dialog = app.create_workspace_entry.as_ref().unwrap();
        assert!(!dialog.pending, "create rejection must clear the pending spinner, not hang");
        assert!(dialog.error.is_some());
    }

    #[test]
    fn harness_only_backend_routes_sidebar_workspace_reads_to_the_detail_lane() {
        let mut app = App::default();
        let (harness_tx, mut harness_rx) = mpsc::channel(4);
        let (history_tx, mut history_rx) = mpsc::channel(4);
        let (detail_tx, mut detail_rx) = mpsc::channel(4);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);

        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::InspectWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessInspectNodeWorkspace { node_id, workspace_id })
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));

        let path = RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::ReadWorkspaceFile {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: path.clone(),
                token: 7,
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessReadNodeWorkspaceFile { node_id, workspace_id, token: 7, .. })
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));

        let destination = WorkspaceGitRequestDestination::Surface(WorkspaceGitTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            history_path: None,
        });
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::ReadGitHistory {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: None,
                before: None,
                limit: 20,
                token: 9,
                destination: destination.clone(),
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessReadNodeGitHistory { node_id, workspace_id, token: 9, .. })
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));

        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::ReadGitDiff {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                target: WorkspaceGitDiffTarget::Working { path: Some(path) },
                token: 11,
                destination,
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessReadNodeGitDiff { node_id, workspace_id, token: 11, .. })
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));

        assert!(matches!(
            harness_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(matches!(
            history_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert_eq!(app.notice, None);
    }

    #[test]
    fn harness_node_workspace_inspected_response_populates_the_direct_state() {
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        let inspection = HarnessNodeWorkspaceInspectionV1 {
            origin: HarnessNodeWorkspaceOriginV1 {
                node_id: "node-a".to_owned(),
                node_incarnation_id: "07".repeat(16),
                workspace_id: "workspace-a".to_owned(),
            },
            entries: vec![HarnessWorkspaceTreeEntryV1 {
                relative_path: HarnessRepositoryPathV1::new("src/lib.rs").unwrap(),
                kind: HarnessWorkspaceEntryKindV1::File,
            }],
            tree_truncated: false,
            git: HarnessGitSummaryV1 {
                is_repository: true,
                branch: Some("main".to_owned()),
                status: Vec::new(),
                recent_commits: Vec::new(),
                truncated: false,
            },
            truncation: None,
        };
        let follow_up = apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::HarnessNodeWorkspaceInspected {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                inspection,
            },
        );
        assert_eq!(follow_up, AppAction::None);
        let stored = app.workspace_inspections
            .get(&("node-a".to_owned(), "workspace-a".to_owned()))
            .expect("harness-mode InspectWorkspace response lands in workspace_inspections");
        assert_eq!(stored.workspace_id.as_str(), "workspace-a");
        assert_eq!(stored.entries.len(), 1);
        assert!(stored.git.is_repository);
        assert_eq!(stored.git.branch.as_deref(), Some("main"));

        // A mismatched-workspace reply is rejected rather than silently
        // adopted into the requested slot.
        let mismatched = HarnessNodeWorkspaceInspectionV1 {
            origin: HarnessNodeWorkspaceOriginV1 {
                node_id: "node-a".to_owned(),
                node_incarnation_id: "07".repeat(16),
                workspace_id: "workspace-b".to_owned(),
            },
            entries: Vec::new(),
            tree_truncated: false,
            git: HarnessGitSummaryV1 {
                is_repository: false,
                branch: None,
                status: Vec::new(),
                recent_commits: Vec::new(),
                truncated: false,
            },
            truncation: None,
        };
        apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::HarnessNodeWorkspaceInspected {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                inspection: mismatched,
            },
        );
        assert!(app.notice.as_deref().unwrap_or_default().contains("workspace-b"));
        let still_valid = app.workspace_inspections
            .get(&("node-a".to_owned(), "workspace-a".to_owned()))
            .expect("the earlier valid inspection is not clobbered by a mismatched reply");
        assert_eq!(still_valid.entries.len(), 1);
    }

    #[test]
    fn harness_actions_route_only_to_the_harness_backend() {
        let mut app = App::default();
        let (c2_tx, mut c2_rx) = mpsc::channel(1);
        let (harness_tx, mut harness_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([
            (C2_COMMAND_ROUTE.to_owned(), c2_tx),
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
        ]);

        send_operator_action(
            &mut app,
            &commands,
            AppAction::HarnessRefresh { token: 41 },
        );

        assert!(matches!(
            harness_rx.try_recv(),
            Ok(AppAction::HarnessRefresh { token: 41 }),
        ));
        assert!(matches!(
            c2_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
    }

    #[test]
    fn missing_harness_sender_never_falls_back_to_c2_and_rolls_back_pending() {
        let mut app = App::default();
        let task = paginated_harness_task(2);
        app.begin_harness_refresh(81);
        app.harness_kanban.execution_mutation = Some(crate::app::HarnessExecutionMutationState {
            token: 81,
            task: harness_task_ref(&task),
            kind: crate::app::HarnessExecutionMutationKind::StartTask,
            refresh_run: None,
        });
        let (c2_tx, mut c2_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([(C2_COMMAND_ROUTE.to_owned(), c2_tx)]);

        send_operator_action(
            &mut app,
            &commands,
            AppAction::HarnessStartTaskV2 {
                token: 81,
                task: harness_task_ref(&task),
                expected_execution_spec_revision: HarnessRevision::new(1).unwrap(),
                expected_launch_issuance: launch_issuance_ref(),
            },
        );

        assert!(matches!(c2_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(app.harness_kanban.execution_mutation.is_none());
        assert_eq!(app.harness_kanban.pending_refresh, None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness launch action failed: Harness operator unavailable: execution mutation queue is closed"),
        );
    }

    #[test]
    fn harness_v6_launch_mutations_use_serial_lane_without_node_fallback() {
        let mut app = App::default();
        let (mutation_tx, mut mutation_rx) = mpsc::channel(2);
        let (history_tx, mut history_rx) = mpsc::channel(1);
        let (detail_tx, mut detail_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), mutation_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        let task = HarnessTaskRef {
            task_id: HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap(),
            task_revision: HarnessRevision::new(4).unwrap(),
        };
        let save = AppAction::HarnessSaveTaskLaunchSpec {
            token: 70,
            task: task.clone(),
            expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            selection: reviewed_launch_selection(),
            refresh_run: None,
        };
        let start = AppAction::HarnessStartTaskV2 {
            token: 71,
            task,
            expected_execution_spec_revision: HarnessRevision::new(1).unwrap(),
            expected_launch_issuance: launch_issuance_ref(),
        };

        send_operator_action(&mut app, &commands, save.clone());
        send_operator_action(&mut app, &commands, start.clone());
        assert_eq!(mutation_rx.try_recv().unwrap(), save);
        assert_eq!(mutation_rx.try_recv().unwrap(), start);
        assert!(matches!(history_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(matches!(detail_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(action_to_request(save).is_none());
        assert!(action_to_request(start).is_none());
    }

    #[test]
    fn rejected_execution_mutation_clears_pending_state() {
        let mut app = App::default();
        let task = paginated_harness_task(3);
        app.begin_harness_refresh(1);
        app.apply_harness_snapshot(1, vec![task.clone()], Vec::new());
        app.harness_kanban.execution_mutation = Some(crate::app::HarnessExecutionMutationState {
            token: 72,
            task: harness_task_ref(&task),
            kind: crate::app::HarnessExecutionMutationKind::StartTask,
            refresh_run: None,
        });
        app.begin_harness_refresh(72);
        let (mutation_tx, _mutation_rx) = mpsc::channel(1);
        mutation_tx.try_send(AppAction::None).unwrap();
        let (history_tx, _history_rx) = mpsc::channel(1);
        let (detail_tx, _detail_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), mutation_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        send_operator_action(
            &mut app,
            &commands,
            AppAction::HarnessStartTaskV2 {
                token: 72,
                task: harness_task_ref(&task),
                expected_execution_spec_revision: HarnessRevision::new(1).unwrap(),
                expected_launch_issuance: launch_issuance_ref(),
            },
        );

        assert!(app.harness_kanban.execution_mutation.is_none());
        assert_eq!(app.harness_kanban.pending_refresh, None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness launch action failed: Harness operator busy: execution mutation queue is full"),
        );
    }

    #[test]
    fn harness_intent_factory_emits_valid_unique_request_refs_without_panicking() {
        let mut factory = HarnessIntentFactory::default();
        let generated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (0..64).map(|index| {
                factory.next_intent(HarnessOperatorActionV1::CreateTask {
                    title: format!("task {index}"),
                    body: String::new(),
                    parent_task_id: None,
                    dependencies: Vec::new(),
                    initial_state: HarnessTaskStateV1::Backlog,
                })
            }).collect::<Result<Vec<_>, _>>()
        }));
        let intents = generated.expect("Harness intent generation must not panic")
            .expect("Harness intent generation must produce valid request references");
        let mut request_refs = BTreeSet::new();
        for intent in intents {
            let request_ref = intent.request_ref.as_str();
            let payload = request_ref.strip_prefix("hireq_").unwrap();
            assert_eq!(payload.len(), 24);
            assert!(payload.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }));
            assert!(request_refs.insert(request_ref.to_owned()));
            assert!(intent.validate().is_ok());
        }
    }

    #[test]
    fn harness_native_history_reads_route_to_history_but_resume_mutation_fails_closed() {
        let mut app = App::default();
        let (harness_tx, mut harness_rx) = mpsc::channel(2);
        let (history_tx, mut history_rx) = mpsc::channel(2);
        let (detail_tx, mut detail_rx) = mpsc::channel(2);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        let route = NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(),
            provider("codex"),
        );

        send_operator_action(
            &mut app,
            &commands,
            AppAction::CatalogNativeSessions {
                node_id: "node-a".to_owned(),
                routes: vec![route.clone()],
                limit: 64,
                token: 11,
            },
        );
        assert!(matches!(
            history_rx.try_recv(),
            Ok(AppAction::CatalogNativeSessions { node_id, token: 11, .. })
                if node_id == "node-a",
        ));
        assert!(matches!(
            harness_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(matches!(detail_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        send_operator_action(
            &mut app,
            &commands,
            AppAction::IndexNativeSession {
                node_id: "node-a".to_owned(),
                route,
                catalog_revision: 3,
                recent_cutoff_unix_ms: 7,
                selection_id: "selection-a".to_owned(),
                display_name: "Native session".to_owned(),
                operation_token: 12,
            },
        );
        assert!(matches!(
            harness_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert!(matches!(
            history_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
        ));
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness-owned session action unavailable: no typed Harness intent exists"),
        );
    }

    #[test]
    fn harness_queue_rejection_rolls_back_pending_refresh_and_restores_composer() {
        let mut app = App::default();
        app.begin_harness_refresh(51);
        let (harness_tx, mut harness_rx) = mpsc::channel(1);
        harness_tx.try_send(AppAction::None).unwrap();
        let (history_tx, _history_rx) = mpsc::channel(1);
        let (detail_tx, _detail_rx) = mpsc::channel(1);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);

        send_operator_action(
            &mut app,
            &commands,
            AppAction::HarnessCreateTask {
                token: 51,
                title: "Keep this title".to_owned(),
                body: "Keep this body".to_owned(),
                initial_state: HarnessTaskStateV1::Backlog,
            },
        );

        assert_eq!(app.harness_kanban.pending_refresh, None);
        let composer = app.harness_kanban.composer.as_ref().unwrap();
        assert_eq!(composer.title, "Keep this title");
        assert_eq!(composer.body, "Keep this body");
        assert_eq!(composer.field, HarnessTaskComposerField::Body);
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness operator busy: command queue is full"),
        );
        assert!(matches!(harness_rx.try_recv(), Ok(AppAction::None)));

        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let (history_tx, _history_rx) = mpsc::channel(1);
        let (detail_tx, _detail_rx) = mpsc::channel(1);
        let closed_commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), closed_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        app.begin_harness_refresh(52);
        send_operator_action(
            &mut app,
            &closed_commands,
            AppAction::HarnessRefresh { token: 52 },
        );
        assert_eq!(app.harness_kanban.pending_refresh, None);
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness operator unavailable: command queue is closed"),
        );
    }

    #[test]
    fn harness_detail_queue_rejection_clears_every_pending_correlation() {
        let mut app = App::default();
        let mut task = paginated_harness_task(1);
        let run = paginated_harness_run(1);
        task.run_ids.push(run.run_id.clone());
        app.begin_harness_refresh(1);
        app.apply_harness_snapshot(1, vec![task.clone()], vec![run.clone()]);
        app.harness_kanban.correlation_pending.insert(run.run_id.clone());

        let (harness_tx, _harness_rx) = mpsc::channel(1);
        let (history_tx, _history_rx) = mpsc::channel(1);
        let (detail_tx, mut detail_rx) = mpsc::channel(1);
        detail_tx.try_send(AppAction::None).unwrap();
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        send_operator_action(
            &mut app,
            &commands,
            AppAction::HarnessLoadTaskCorrelations {
                task: harness_task_ref(&task),
                launch_token: 91,
                runs: vec![HarnessRunRef {
                    run_id: run.run_id.clone(),
                    run_revision: run.revision,
                }],
            },
        );

        assert!(!app.harness_kanban.correlation_pending.contains(&run.run_id));
        assert!(app.harness_kanban.correlation_failures.contains_key(&run.run_id));
        assert_eq!(
            app.notice.as_deref(),
            Some("Harness operator busy: correlation command queue is full"),
        );
        assert!(matches!(detail_rx.try_recv(), Ok(AppAction::None)));
    }

    #[test]
    fn harness_read_lane_isolated_from_refresh_mutations_and_scheduler() {
        let mut app = App::default();
        let (harness_tx, mut harness_rx) = mpsc::channel(1);
        let (history_tx, mut history_rx) = mpsc::channel(1);
        let (detail_tx, mut detail_rx) = mpsc::channel(2);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        let route = NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(),
            provider("codex"),
        );
        send_operator_action(
            &mut app,
            &commands,
            AppAction::CatalogNativeSessions {
                node_id: "node-a".to_owned(),
                routes: vec![route],
                limit: 64,
                token: 61,
            },
        );

        let task = paginated_harness_task(1);
        let mutation_actions = vec![
            AppAction::HarnessRefresh { token: 62 },
            AppAction::HarnessCreateTask {
                token: 63,
                title: "Created while history is busy".to_owned(),
                body: String::new(),
                initial_state: HarnessTaskStateV1::Backlog,
            },
            AppAction::HarnessMoveTask {
                token: 64,
                task_id: task.task_id.clone(),
                expected_revision: task.revision,
                state: HarnessTaskStateV1::Running,
            },
            AppAction::HarnessCancelTask {
                token: 65,
                task_id: task.task_id.clone(),
                expected_revision: task.revision,
            },
            AppAction::HarnessRetryTask {
                token: 66,
                task_id: task.task_id.clone(),
                expected_revision: task.revision,
            },
            AppAction::HarnessScheduleNext {
                token: 67,
                plan_id: None,
            },
        ];
        for action in mutation_actions {
            send_operator_action(&mut app, &commands, action.clone());
            assert_eq!(harness_rx.try_recv().unwrap(), action);
        }
        let run = paginated_harness_run(1);
        let monitor = AppAction::HarnessOpenMonitor {
            run: HarnessRunRef {
                run_id: run.run_id.clone(),
                run_revision: run.revision,
            },
        };
        let correlations = AppAction::HarnessLoadTaskCorrelations {
            task: harness_task_ref(&task),
            launch_token: 92,
            runs: vec![HarnessRunRef {
                run_id: run.run_id,
                run_revision: run.revision,
            }],
        };
        send_operator_action(&mut app, &commands, monitor.clone());
        send_operator_action(&mut app, &commands, correlations.clone());
        assert!(matches!(harness_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(matches!(
            history_rx.try_recv(),
            Ok(AppAction::CatalogNativeSessions { token: 61, .. }),
        ));
        assert!(matches!(history_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert_eq!(detail_rx.try_recv().unwrap(), monitor);
        assert_eq!(detail_rx.try_recv().unwrap(), correlations);
    }

    #[test]
    fn harness_snapshot_bootstraps_native_history_catalog_once() {
        let node = project_harness_inventory_node(harness_runtime_inventory_node(
            "node-a",
            1_725_000_000_000,
        )).unwrap();
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();

        let first = apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::HarnessSnapshot {
                token: 1,
                tasks: Vec::new(),
                runs: Vec::new(),
                nodes: vec![node.clone()],
            },
        );
        assert!(matches!(
            first,
            AppAction::CatalogNativeSessions { ref node_id, ref routes, .. }
                if node_id == "node-a"
                    && routes.iter().any(|route| {
                        route.workspace_id.as_deref() == Some("workspace-a")
                            && route.provider == provider("codex")
                    })
        ));
        assert!(app.existing_session.is_some());

        let repeated = apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::HarnessSnapshot {
                token: 2,
                tasks: Vec::new(),
                runs: Vec::new(),
                nodes: vec![node],
            },
        );
        assert_eq!(repeated, AppAction::None);
    }

    #[test]
    fn harness_native_history_pins_incarnation_and_projects_into_native_sessions_surface() {
        let node = project_harness_inventory_node(harness_runtime_inventory_node(
            "node-a",
            1_725_000_000_000,
        )).unwrap();
        let mut app = App::default();
        app.nodes = vec![node.clone()];
        let action = app.ensure_initial_agents_catalog();
        let AppAction::CatalogNativeSessions { node_id, routes, token, .. } = action else {
            panic!("Harness inventory should open the native sessions catalog");
        };
        let route = routes.into_iter().find(|route| {
            route.workspace_id.as_deref() == Some("workspace-a")
                && route.provider == provider("codex")
        }).unwrap();
        let pinned = harness_native_session_route(Some(&[node]), &node_id, &route).unwrap();
        assert_eq!(pinned.node_id, "node-a");
        assert_eq!(pinned.incarnation_id, "11".repeat(16));
        assert_eq!(pinned.scope, HarnessNativeSessionCatalogScopeV1::Workspace);
        assert_eq!(pinned.workspace_id.as_deref(), Some("workspace-a"));

        let entry = project_harness_native_catalog_entry(
            HarnessNativeSessionCatalogEntryV1 {
                selection_id: "selection-a".to_owned(),
                title: Some("Persisted native review".to_owned()),
                modified_at_unix_ms: Some(17),
                model: Some("gpt-5".to_owned()),
                message_count: 9,
                completed_turn_count: Some(4),
                external_group: None,
                record_id: None,
            },
        ).unwrap();
        let mut c2 = C2ApplyState::default();
        apply_update(
            &mut app,
            &mut c2,
            WorkerUpdate::NativeSessionsCataloged {
                node_id,
                route: route.clone(),
                token,
                entries: vec![entry],
                summary: Some(NativeSessionCatalogSummary {
                    catalog_revision: 3,
                    recent_cutoff_unix_ms: 7,
                    recent_total_count: 1,
                    older_total_count: 0,
                    recent_next_after_selection_id: None,
                    recent_has_more: false,
                }),
            },
        );
        let dialog = app.existing_session.as_ref().unwrap();
        assert_eq!(dialog.rows.len(), 1);
        assert_eq!(dialog.rows[0].title.as_deref(), Some("Persisted native review"));
        assert_eq!(dialog.rows[0].route, route);
        // The provider branch carrying this catalog result starts collapsed on
        // first open of the native/live tree (see
        // `initial_native_provider_branches_are_collapsed_and_keep_grok_header_visible`);
        // expand it so its projected Session item is reachable.
        app.collapsed_native_providers.remove(&(
            "node-a".to_owned(),
            crate::app::NativeSessionGroupKey::Workspace(route.workspace_id.clone().unwrap()),
            route.provider.clone(),
        ));
        assert!(app.native_session_tree_items().iter().any(|item| {
            matches!(item, crate::app::NativeSessionTreeItem::Session { .. })
        }));
    }

    #[test]
    fn harness_native_preview_projection_keeps_only_bounded_redacted_message_fields() {
        let preview = project_harness_native_preview(HarnessNativeSessionPreviewV1 {
            title: Some("Review".to_owned()),
            modified_at_unix_ms: Some(17),
            model: Some("gpt-5".to_owned()),
            message_count: 2,
            message_count_exact: true,
            completed_turn_count: Some(1),
            total_tokens: Some(42),
            truncated: false,
            messages: vec![
                gate4agent_harness_client::HarnessNativeSessionPreviewMessageV1 {
                    role: HarnessNativeSessionPreviewRoleV1::User,
                    text: "bounded question".to_owned(),
                },
                gate4agent_harness_client::HarnessNativeSessionPreviewMessageV1 {
                    role: HarnessNativeSessionPreviewRoleV1::Assistant,
                    text: "bounded answer".to_owned(),
                },
            ],
        });
        assert_eq!(preview.message_count, 2);
        assert!(preview.message_count_exact);
        assert_eq!(preview.messages[0].role, HistoryMessageRole::User);
        assert_eq!(preview.messages[1].role, HistoryMessageRole::Assistant);
        assert_eq!(preview.messages[1].text, "bounded answer");
    }

    #[test]
    fn projection_accepts_only_pty_sessions() {
        assert!(supports_tui_transport(TransportKind::Pty));
        assert!(!supports_tui_transport(TransportKind::Pipe));
        assert!(!supports_tui_transport(TransportKind::Acp));
    }

    #[test]
    fn lifecycle_projection_never_removes_registered_or_stopping_sessions() {
        for status in [SessionStatus::Registered, SessionStatus::Starting, SessionStatus::Running, SessionStatus::Stopping] {
            let lifecycle = project_lifecycle(&status);
            assert!(lifecycle.stoppable);
            assert!(!lifecycle.removable);
            assert!(!lifecycle.restartable);
        }
        for status in [
            SessionStatus::Exited { exit_code: Some(0) },
            SessionStatus::Failed { message: "failed".to_owned() },
        ] {
            let lifecycle = project_lifecycle(&status);
            assert!(!lifecycle.stoppable);
            assert!(lifecycle.removable);
            assert!(lifecycle.restartable);
        }
    }

    #[test]
    fn raw_paste_chunks_preserve_utf8_boundaries() {
        let text = format!("{}é", "a".repeat(MAX_NODE_TEXT_BYTES - 1));
        let chunks = utf8_chunks(&text, MAX_NODE_TEXT_BYTES);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn run_options_can_carry_an_explicit_style_override() {
        let options = RunOptions {
            mode: StartupMode::ManualC2(C2Endpoint {
                endpoint: "test-c2".to_owned(),
                token: "test-token".to_owned(),
            }),
            startup: None,
            color_mode_override: Some(PtyColorMode::Inherited),
        };
        assert_eq!(options.color_mode_override, Some(PtyColorMode::Inherited));
    }

    #[test]
    fn startup_modes_select_exactly_one_backend_without_direct_node_fallback() {
        let manual = StartupMode::ManualC2(C2Endpoint {
            endpoint: "test-c2".to_owned(),
            token: "c2-secret".to_owned(),
        });
        assert_eq!(
            backend_selection(&manual),
            BackendSelection {
                c2_worker: true,
                harness_worker: false,
                observation_writer: true,
                direct_node_worker: false,
            },
        );

        let harness = StartupMode::Harness(HarnessOperatorEndpoint {
            endpoint: "127.0.0.1:18080".parse().unwrap(),
            credential: HarnessOperatorCredential::parse(format!(
                "g4aho_{}",
                "0".repeat(64),
            )).unwrap(),
            launch_plan_id: None,
        });
        assert_eq!(
            backend_selection(&harness),
            BackendSelection {
                c2_worker: false,
                harness_worker: true,
                observation_writer: false,
                direct_node_worker: false,
            },
        );
    }

    #[test]
    fn backend_credentials_are_not_persisted_with_ui_preferences() {
        let secret = "c2-secret-sentinel";
        let _options = RunOptions {
            mode: StartupMode::ManualC2(C2Endpoint {
                endpoint: "test-c2".to_owned(),
                token: secret.to_owned(),
            }),
            startup: None,
            color_mode_override: None,
        };
        let preferences = preferences_for_save(&App::default(), PtyColorMode::Inherited);
        let path = std::env::temp_dir().join(format!(
            "gate4agent-tui-mode-secret-{}-{}.json",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        ));

        preferences.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.windows(secret.len()).any(|window| window == secret.as_bytes()));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invocation_style_override_is_not_persisted_without_a_user_change() {
        let mut app = App::default();
        app.color_mode = PtyColorMode::GateOverride;

        let preferences = preferences_for_save(&app, PtyColorMode::Inherited);

        assert_eq!(app.color_mode, PtyColorMode::GateOverride);
        assert_eq!(preferences.color_mode, PtyColorMode::Inherited);
    }

    #[test]
    fn c2_control_notice_uses_only_redacted_event_category_and_address() {
        let address = WireSessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: gate4agent_types::AgentInstanceId(7),
                generation: gate4agent_types::SessionGeneration(2),
            },
        };
        let event = C2ControlEvent {
            protocol_version: gate4agent_types::CONTROL_PROTOCOL_VERSION,
            sequence: 4,
            command_id: None,
            instance_id: gate4agent_types::AgentInstanceId(7),
            generation: gate4agent_types::SessionGeneration(2),
            event: C2ControlEventKind::Resumed,
        };
        assert_eq!(
            safe_c2_control_notice("node-a", &address, &event).as_deref(),
            Some("node-a/workspace-a #7:2: session resumed"),
        );

        let provider_event = C2ControlEvent {
            event: C2ControlEventKind::ProviderEvent {
                event: gate4agent_c2_protocol::C2ProviderEventKind::SessionIdentityObserved,
            },
            ..event
        };
        assert_eq!(safe_c2_control_notice("node-a", &address, &provider_event), None);
    }

    fn c2_test_record(display_name: &str) -> C2ManagedSessionRecord {
        C2ManagedSessionRecord {
            record_id: SessionRecordId::new("record-1").unwrap(),
            display_name: display_name.to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            active_session: None,
            provider_identity_present: true,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: None,
            task_binding: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    fn c2_test_snapshot(display_name: &str) -> C2NodeSnapshot {
        C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![provider("codex")],
            provider_runtime_statuses: Default::default(),
            observation_support: None,
            workspaces: Vec::new(),
            session_records: vec![c2_test_record(display_name)],
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
        }
    }

    fn terminal_frame(sequence: u64, formatted: &[u8]) -> TerminalFrame {
        TerminalFrame {
            sequence,
            size: TerminalSize { rows: 24, columns: 80 },
            cursor_row: 2,
            cursor_column: formatted.len() as u16,
            contents: String::from_utf8_lossy(formatted).into_owned(),
            formatted: formatted.to_vec(),
            scrollback_formatted: vec![b"previous".to_vec()],
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
        }
    }

    fn c2_terminal_snapshot(frame: TerminalFrame) -> C2NodeSnapshot {
        C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![provider("codex")],
            provider_runtime_statuses: Default::default(),
            observation_support: None,
            workspaces: vec![gate4agent_c2_protocol::C2WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                canonical_root: host_path(r"C:\repo"),
                sessions: vec![C2SessionSnapshot {
                    instance_id: gate4agent_types::AgentInstanceId(7),
                    agent_id: provider("codex"),
                    transport: TransportKind::Pty,
                    generation: gate4agent_types::SessionGeneration(2),
                    status: C2SessionStatus::Running,
                    pending_operation: None,
                    pending_input: None,
                    process_id: Some(7),
                    terminal_size: Some(frame.size),
                    terminal_frame: Some(frame),
                    provider_activity: ProviderActivity::Idle,
                    provider_interaction_pending: false,
                    provider_identity_present: true,
                }],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
            session_records: Vec::new(),
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
        }
    }

    fn direct_catalog_snapshot() -> NodeSnapshot {
        NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![provider("codex")],
            provider_runtime_statuses: Default::default(),
            workspaces: vec![direct_workspace("workspace-a")],
            session_records: Vec::new(),
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
        }
    }

    #[test]
    fn split_files_with_visible_agents_autoloads_direct_catalog() {
        let incarnation_id = incarnation(3);
        let mut app = App::default();
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.sidebar_presentation, SidebarPresentation::Split);
        assert_eq!(app.roster_mode, RosterMode::Agents);
        let mut c2 = C2ApplyState::default();

        let first = apply_update(&mut app, &mut c2, WorkerUpdate::Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: direct_catalog_snapshot(),
            controller_owned: true,
            event_sequence: 4,
        });
        match first {
            AppAction::CatalogNativeSessions {
                node_id,
                routes,
                limit,
                token: _,
            } => {
                assert_eq!(node_id, "node-a");
                assert_eq!(limit, 64);
                assert_eq!(
                    routes,
                    vec![
                        NativeSessionCatalogRoute::workspace(
                            "workspace-a".to_owned(),
                            provider("codex"),
                        ),
                        NativeSessionCatalogRoute::unregistered(provider("codex")),
                    ],
                );
            }
            other => panic!("expected initial catalog action, got {other:?}"),
        }
    }

    #[test]
    fn second_authoritative_split_snapshot_does_not_reset_catalog() {
        let incarnation_id = incarnation(3);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();

        let first = apply_update(&mut app, &mut c2, WorkerUpdate::Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: direct_catalog_snapshot(),
            controller_owned: true,
            event_sequence: 4,
        });
        let token = match first {
            AppAction::CatalogNativeSessions { token, .. } => token,
            other => panic!("expected initial catalog action, got {other:?}"),
        };
        app.existing_session.as_mut().unwrap().rows.push(
            crate::app::NativeSessionCatalogRowView {
                node_id: "node-a".to_owned(),
                route: NativeSessionCatalogRoute::workspace(
                    "workspace-a".to_owned(),
                    provider("codex"),
                ),
                catalog_revision: 7,
                recent_cutoff_unix_ms: 8,
                selection_id: "selection-a".to_owned(),
                title: Some("Preserved".to_owned()),
                modified_at: None,
                model: None,
                message_count: Some(1),
                completed_turn_count: Some(1),
                external_group: None,
                record_id: None,
            },
        );

        let repeated = apply_update(&mut app, &mut c2, WorkerUpdate::Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: direct_catalog_snapshot(),
            controller_owned: true,
            event_sequence: 5,
        });
        assert!(matches!(repeated, AppAction::None));
        let dialog = app.existing_session.as_ref().unwrap();
        assert_eq!(dialog.request_token, token);
        assert_eq!(dialog.rows.len(), 1);
        assert_eq!(dialog.rows[0].title.as_deref(), Some("Preserved"));
    }

    #[test]
    fn authoritative_inventory_does_not_catalog_when_agents_roster_is_hidden() {
        let mut hidden_apps = Vec::new();

        let mut activity_files = App::default();
        activity_files.sidebar_presentation = SidebarPresentation::Activity;
        hidden_apps.push(activity_files);

        let mut collapsed_agents = App::default();
        collapsed_agents.sidebar_presentation = SidebarPresentation::Activity;
        collapsed_agents.control_section = ControlSection::Agents;
        collapsed_agents.sidebar_collapsed = true;
        hidden_apps.push(collapsed_agents);

        let mut closed_modal = App::default();
        closed_modal.menu_placement = MenuPlacement::Modal;
        closed_modal.control_section = ControlSection::Agents;
        closed_modal.focus = Focus::Spaces;
        hidden_apps.push(closed_modal);

        let mut workspaces_roster = App::default();
        workspaces_roster.roster_mode = RosterMode::Workspaces;
        hidden_apps.push(workspaces_roster);

        for mut app in hidden_apps {
            let action = apply_update(
                &mut app,
                &mut C2ApplyState::default(),
                WorkerUpdate::Snapshot {
                    expected_node_id: "node-a".to_owned(),
                    endpoint: r"\\.\pipe\node-a".to_owned(),
                    incarnation_id: incarnation(3),
                    snapshot: direct_catalog_snapshot(),
                    controller_owned: true,
                    event_sequence: 4,
                },
            );
            assert!(matches!(action, AppAction::None));
            assert!(app.existing_session.is_none());
        }
    }

    #[test]
    fn first_authoritative_c2_inventory_autoloads_agents_catalog_once() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([4; 16]);
        let mut app = App::default();
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.sidebar_presentation, SidebarPresentation::Split);
        let mut c2 = C2ApplyState::default();

        let first = apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_terminal_snapshot(terminal_frame(1, b"snapshot")),
            event_sequence: 4,
        });
        let (token, routes) = match first {
            AppAction::CatalogNativeSessions {
                node_id,
                routes,
                limit,
                token,
            } => {
                assert_eq!(node_id, "node-a");
                assert_eq!(limit, 64);
                assert!(token > 0);
                (token, routes)
            }
            other => panic!("expected initial catalog action, got {other:?}"),
        };
        assert_eq!(
            routes,
            vec![
                NativeSessionCatalogRoute::workspace(
                    "workspace-a".to_owned(),
                    provider("codex"),
                ),
                NativeSessionCatalogRoute::unregistered(provider("codex")),
            ],
        );
        let dialog = app.existing_session.as_ref().unwrap();
        assert_eq!(dialog.request_token, token);
        assert_eq!(dialog.pending_routes.len(), 2);
        assert_eq!(app.focus, Focus::Spaces);

        app.existing_session.as_mut().unwrap().rows.push(
            crate::app::NativeSessionCatalogRowView {
                node_id: "node-a".to_owned(),
                route: routes[0].clone(),
                catalog_revision: 7,
                recent_cutoff_unix_ms: 8,
                selection_id: "selection-a".to_owned(),
                title: Some("Preserved".to_owned()),
                modified_at: None,
                model: None,
                message_count: Some(1),
                completed_turn_count: Some(1),
                external_group: None,
                record_id: None,
            },
        );

        let repeated = apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_terminal_snapshot(terminal_frame(2, b"snapshot-newer")),
            event_sequence: 5,
        });
        assert!(matches!(repeated, AppAction::None));
        let dialog = app.existing_session.as_ref().unwrap();
        assert_eq!(dialog.request_token, token);
        assert_eq!(dialog.rows.len(), 1);
        assert_eq!(dialog.rows[0].selection_id, "selection-a");
    }

    #[test]
    fn c2_terminal_frames_require_exact_address_incarnation_and_monotonic_sequence() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([5; 16]);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_terminal_snapshot(terminal_frame(10, b"snapshot")),
            event_sequence: 4,
        });
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let push = |cursor_sequence, incarnation_id, address, frame| WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: cursor_sequence,
            },
            event: C2EventUpdate::TerminalFrame { address, frame },
        };

        apply_update(&mut app, &mut c2, push(
            5,
            incarnation_id,
            address.clone(),
            terminal_frame(11, b"push-11"),
        ));
        apply_update(&mut app, &mut c2, push(
            6,
            incarnation_id,
            address.clone(),
            terminal_frame(11, b"duplicate"),
        ));
        assert_eq!(app.find_session(&address).unwrap().terminal_formatted, b"push-11");

        let mut wrong_address = address.clone();
        wrong_address.workspace_id = "other-workspace".to_owned();
        apply_update(&mut app, &mut c2, push(
            7,
            incarnation_id,
            wrong_address,
            terminal_frame(12, b"wrong-address"),
        ));
        apply_update(&mut app, &mut c2, push(
            8,
            gate4agent_c2_protocol::NodeIncarnationId::from_bytes([6; 16]),
            address.clone(),
            terminal_frame(12, b"wrong-incarnation"),
        ));
        apply_update(&mut app, &mut c2, push(
            9,
            incarnation_id,
            address.clone(),
            terminal_frame(12, b"push-12"),
        ));
        assert_eq!(app.find_session(&address).unwrap().terminal_formatted, b"push-12");

        apply_update(&mut app, &mut c2, push(
            10,
            incarnation_id,
            address.clone(),
            terminal_frame(20, b"push-20"),
        ));
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_terminal_snapshot(terminal_frame(2, b"resynced")),
            event_sequence: 10,
        });
        apply_update(&mut app, &mut c2, push(
            11,
            incarnation_id,
            address.clone(),
            terminal_frame(3, b"post-resync"),
        ));
        assert_eq!(
            app.find_session(&address).unwrap().terminal_formatted,
            b"post-resync",
        );
    }

    #[test]
    fn c2_event_before_snapshot_stays_monotonic_and_snapshot_is_authoritative_at_watermark() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([5; 16]);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("baseline"),
            event_sequence: 4,
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 6,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("event-newer"),
            )),
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("snapshot-stale"),
            event_sequence: 5,
        });

        assert_eq!(app.nodes[0].session_records[0].display_name, "event-newer");
        assert_eq!(app.nodes[0].event_sequence, 6);

        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("snapshot-authoritative"),
            event_sequence: 6,
        });
        assert_eq!(
            app.nodes[0].session_records[0].display_name,
            "snapshot-authoritative"
        );
        assert_eq!(app.nodes[0].event_sequence, 6);
    }

    #[test]
    fn c2_lagging_snapshot_still_delivers_inventory_without_regressing_event_state() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([7; 16]);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("baseline"),
            event_sequence: 1,
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 50,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("event-live"),
            )),
        });
        // A live terminal stream outruns every periodic snapshot: the next
        // snapshot lags the event watermark yet is the ONLY carrier of
        // inventory-shaped state. It must land that state (the pre-fix
        // wholesale rejection starved launch inventory and workspaces
        // forever) without regressing what events established.
        let mut lagging = c2_test_snapshot("snapshot-lagging");
        lagging.launch_inventory = Some(test_launch_inventory());
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: lagging,
            event_sequence: 2,
        });

        assert!(app.nodes[0].launch_inventory.is_some(), "inventory starved");
        assert_eq!(app.nodes[0].session_records[0].display_name, "event-live");
        assert_eq!(app.nodes[0].event_sequence, 50);
    }

    #[test]
    fn c2_duplicate_and_stale_events_cannot_regress_snapshot_record() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([6; 16]);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("snapshot-current"),
            event_sequence: 5,
        });
        for (sequence, display_name) in [(5, "duplicate"), (4, "stale")] {
            apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence,
                },
                event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                    "node-a",
                    c2_test_record(display_name),
                )),
            });
        }
        assert_eq!(app.nodes[0].session_records[0].display_name, "snapshot-current");
        assert_eq!(app.nodes[0].event_sequence, 5);

        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 6,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("fresh"),
            )),
        });
        assert_eq!(app.nodes[0].session_records[0].display_name, "fresh");
        assert_eq!(app.nodes[0].event_sequence, 6);

        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 7,
            },
            event: C2EventUpdate::Ignored,
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 6,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("stale-after-ignored-event"),
            )),
        });
        assert_eq!(app.nodes[0].session_records[0].display_name, "fresh");
        assert_eq!(app.nodes[0].event_sequence, 7);
    }

    #[tokio::test]
    async fn c2_snapshot_and_record_event_feed_the_same_authoritative_app_projection() {
        let record = C2ManagedSessionRecord {
            record_id: SessionRecordId::new("record-1").unwrap(),
            display_name: "review".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::IdentityPending,
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            active_session: None,
            provider_identity_present: true,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            exported_context: None,
            task_binding: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let snapshot = C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![provider("codex")],
            provider_runtime_statuses: Default::default(),
            observation_support: None,
            workspaces: Vec::new(),
            session_records: vec![record.clone()],
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
        };
        let (updates, mut receiver) = mpsc::channel(8);
        publish_c2_response(
            C2NodeResponse::Snapshot {
                event_sequence: 4,
                controller: None,
                snapshot,
            },
            &updates,
            "node-a",
            gate4agent_c2_protocol::NodeIncarnationId::from_bytes([4; 16]),
            r"\\.\pipe\node-a".to_owned(),
        )
        .await;
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert_eq!(app.nodes[0].session_records[0].display_name, "review");
        assert!(app.nodes[0].controller_owned);
        assert!(app.nodes[0].session_records[0].has_provider_session_identity);

        let mut dormant = record;
        dormant.state = gate4agent_node_protocol::ManagedSessionState::Dormant;
        dormant.display_name = "renamed".to_owned();
        publish_c2_event(
            C2NodeEvent::SessionRecordUpserted { record: dormant },
            gate4agent_c2_protocol::NodeCursor {
                incarnation_id: gate4agent_c2_protocol::NodeIncarnationId::from_bytes([4; 16]),
                sequence: 5,
            },
            &updates,
            "node-a",
        )
        .await;
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert_eq!(app.nodes[0].session_records[0].display_name, "renamed");
        assert_eq!(
            app.nodes[0].session_records[0].state,
            gate4agent_node_protocol::ManagedSessionState::Dormant
        );
        assert!(app.nodes[0].session_records[0].has_provider_session_identity);
    }

    #[tokio::test]
    async fn generic_c2_native_index_response_does_not_publish_record_upsert() {
        let (updates, mut receiver) = mpsc::channel(2);
        publish_c2_response(
            C2NodeResponse::NativeSessionIndexed {
                selection: NativeSessionSelection {
                    route: WireNativeSessionCatalogRoute::workspace(
                        WorkspaceId::new("workspace-a").unwrap(),
                        provider("codex"),
                    ),
                    catalog_revision: 1,
                    recent_cutoff_unix_ms: 0,
                    selection_id: "selection-a".to_owned(),
                },
                record: c2_test_record("native index"),
            },
            &updates,
            "node-a",
            gate4agent_c2_protocol::NodeIncarnationId::from_bytes([4; 16]),
            "c2".to_owned(),
        )
        .await;

        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn workspace_file_and_git_actions_map_to_bounded_node_requests() {
        let path = gate4agent_node_protocol::RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        let save = action_to_request(AppAction::WriteWorkspaceFile {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: path.clone(),
            expected_revision: "a".repeat(64),
            text: "pub fn saved() {}\n".to_owned(),
            token: 9,
        });
        assert!(matches!(
            save,
            Some(NodeRequest::WriteWorkspaceFile {
                workspace_id,
                path: request_path,
                expected_revision,
                ..
            }) if workspace_id.as_str() == "workspace-a"
                && request_path == path
                && expected_revision.as_str() == "a".repeat(64)
        ));

        let diff = action_to_request(AppAction::ReadGitDiff {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            target: WorkspaceGitDiffTarget::Commit {
                revision: "b".repeat(40),
                path: Some(path.clone()),
            },
            token: 10,
            destination: WorkspaceGitRequestDestination::Surface(WorkspaceGitTabKey {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                history_path: Some(path.clone()),
            }),
        });
        assert!(matches!(
            diff,
            Some(NodeRequest::ReadGitDiff {
                request: GitDiffRequest {
                    mode: GitDiffMode::Commit { revision },
                    path: Some(request_path),
                },
                ..
            }) if revision.as_str() == "b".repeat(40) && request_path == path
        ));

        let history = action_to_request(AppAction::ReadGitHistory {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: Some(path.clone()),
            before: None,
            limit: 20,
            token: 11,
            destination: WorkspaceGitRequestDestination::Surface(WorkspaceGitTabKey {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                history_path: Some(path.clone()),
            }),
        });
        assert!(matches!(
            history,
            Some(NodeRequest::ReadGitHistory {
                workspace_id,
                path: Some(request_path),
                before: None,
                limit: 20,
            }) if workspace_id.as_str() == "workspace-a" && request_path == path
        ));
    }

    #[test]
    fn workspace_entry_create_actions_map_to_exact_requests_and_failures() {
        let file_path = RepositoryPath::utf8("notes/new.md".to_owned()).unwrap();
        let directory_path = RepositoryPath::utf8("notes/assets".to_owned()).unwrap();
        let file_action = AppAction::CreateWorkspaceFile {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: file_path.clone(),
            token: 41,
        };
        let directory_action = AppAction::CreateWorkspaceDirectory {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: directory_path.clone(),
            token: 42,
        };

        assert!(matches!(
            action_to_request(file_action.clone()),
            Some(NodeRequest::CreateWorkspaceFile { workspace_id, path })
                if workspace_id.as_str() == "workspace-a" && path == file_path
        ));
        assert!(matches!(
            action_to_request(directory_action.clone()),
            Some(NodeRequest::CreateWorkspaceDirectory { workspace_id, path })
                if workspace_id.as_str() == "workspace-a" && path == directory_path
        ));
        assert!(matches!(
            workspace_document_failed_update(
                &file_action,
                "repository entry already exists".to_owned(),
            ),
            Some(WorkerUpdate::WorkspaceEntryCreateFailed {
                node_id,
                workspace_id,
                path,
                kind: WorkspaceEntryKind::File,
                token: 41,
                ..
            }) if node_id == "node-a"
                && workspace_id == "workspace-a"
                && path == file_path
        ));
        assert!(matches!(
            workspace_document_failed_update(
                &directory_action,
                "controller required".to_owned(),
            ),
            Some(WorkerUpdate::WorkspaceEntryCreateFailed {
                node_id,
                workspace_id,
                path,
                kind: WorkspaceEntryKind::Directory,
                token: 42,
                ..
            }) if node_id == "node-a"
                && workspace_id == "workspace-a"
                && path == directory_path
        ));
    }

    #[test]
    fn git_success_updates_preserve_exact_inline_file_destination() {
        let path = gate4agent_node_protocol::RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        let destination = WorkspaceGitRequestDestination::InlineFile(WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: path.clone(),
        });

        assert!(matches!(
            git_history_read_update(
                destination.clone(),
                12,
                GitHistoryPage { commits: Vec::new(), next_before: None, truncated: false },
            ),
            WorkerUpdate::GitHistoryRead {
                destination: actual,
                token: 12,
                ..
            } if actual == destination
        ));
        assert!(matches!(
            git_diff_read_update(
                destination.clone(),
                13,
                GitDiff {
                    mode: GitDiffMode::Working,
                    path: Some(path),
                    text: "diff".to_owned(),
                    truncated: false,
                },
            ),
            WorkerUpdate::GitDiffRead {
                destination: actual,
                token: 13,
                ..
            } if actual == destination
        ));
    }

    #[test]
    fn c2_transport_failure_preserves_exact_inline_file_destination() {
        let path =
            gate4agent_node_protocol::RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        let destination = WorkspaceGitRequestDestination::InlineFile(WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: path.clone(),
        });
        let action = AppAction::ReadGitHistory {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: Some(path),
            before: None,
            limit: 20,
            token: 14,
            destination: destination.clone(),
        };

        assert!(matches!(
            workspace_document_failed_update(&action, "C2 transport unavailable".to_owned()),
            Some(WorkerUpdate::GitHistoryFailed {
                destination: actual,
                token: 14,
                message,
            }) if actual == destination
                && message == "C2 transport unavailable"
        ));
    }

    #[tokio::test]
    async fn disconnected_queue_preserves_exact_inline_file_destination() {
        let (commands, mut queued) = mpsc::channel(2);
        let (updates, mut received) = mpsc::channel(4);
        let path =
            gate4agent_node_protocol::RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        let destination = WorkspaceGitRequestDestination::InlineFile(WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: path.clone(),
        });
        commands
            .try_send(AppAction::ReadGitHistory {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: Some(path),
                before: None,
                limit: 20,
                token: 15,
                destination: destination.clone(),
            })
            .unwrap();

        reject_disconnected_commands(
            &mut queued,
            &updates,
            "node-a",
            "node disconnected",
        )
        .await;

        assert!(matches!(
            received.try_recv(),
            Ok(WorkerUpdate::GitHistoryFailed {
                destination: actual,
                token: 15,
                message,
            }) if actual == destination
                && message == "node disconnected"
        ));
        assert!(matches!(received.try_recv(), Ok(WorkerUpdate::Notice(_))));
    }

    fn client_monitor_observation(source_sequence: u64) -> ObservationV1 {
        ObservationV1 {
            source_sequence,
            observed_at_unix_ms: Some(1_000 + source_sequence),
            evidence: gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
            kind: gate4agent_node_protocol::ObservationKindV1::Working,
            truncated: false,
        }
    }

    fn monitor_wire_address(generation: u64) -> WireSessionAddress {
        WireSessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: gate4agent_types::AgentInstanceId(7),
                generation: gate4agent_types::SessionGeneration(generation),
            },
        }
    }

    fn client_runtime_monitor_target(
        address: &SessionAddress,
        incarnation: gate4agent_node_protocol::NodeIncarnationId,
    ) -> SessionMonitorTarget {
        SessionMonitorTarget::Runtime {
            address: address.clone(),
            incarnation,
        }
    }

    fn authoritative_monitor_snapshot(
        app: &mut App,
        apply: &mut C2ApplyState,
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
        event_sequence: u64,
    ) {
        apply_update(
            app,
            apply,
            WorkerUpdate::C2Snapshot {
                expected_node_id: "node-a".to_owned(),
                endpoint: "c2".to_owned(),
                incarnation_id,
                snapshot: c2_terminal_snapshot(terminal_frame(1, b"snapshot")),
                event_sequence,
            },
        );
    }

    fn monitor_ingress(
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
        sequence: u64,
        transport: ObservationTransport,
        observation: ObservationV1,
    ) -> ObservationIngressEnvelope {
        let cursor = gate4agent_c2_protocol::NodeCursor { incarnation_id, sequence };
        observation_ingress_envelope(
            "node-a",
            cursor,
            monitor_wire_address(2),
            observation,
            transport,
        )
        .unwrap()
    }

    fn gapped_session_monitor(
        incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
        transport: ObservationTransport,
    ) -> (App, C2ApplyState, ObservationV1) {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        authoritative_monitor_snapshot(&mut app, &mut apply, incarnation_id, 0);
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::ObservationIngress(monitor_ingress(
                incarnation_id,
                1,
                transport,
                client_monitor_observation(1),
            )),
        );
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::EventCursor {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 2,
                },
            },
        );
        let recovered = client_monitor_observation(2);
        assert_eq!(
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::ObservationIngress(monitor_ingress(
                    incarnation_id,
                    4,
                    transport,
                    recovered.clone(),
                )),
            ),
            AppAction::Resync {
                node_id: "node-a".to_owned(),
                after_sequence: 2,
            },
        );
        let monitor = app.session_monitors.values().next().unwrap();
        assert!(app
            .session_monitor_projection(&monitor.key)
            .unwrap()
            .transport_incomplete);
        (app, apply, recovered)
    }

    #[test]
    fn session_monitor_uses_engine_canonical_dedupe_across_receive_metadata() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let incarnation_id = incarnation(28);
        authoritative_monitor_snapshot(&mut app, &mut apply, incarnation_id, 0);
        let direct = monitor_ingress(
            incarnation_id,
            1,
            ObservationTransport::DirectNode,
            client_monitor_observation(1),
        );
        let mut relayed = direct.clone();
        relayed.received_at_ms = direct.received_at_ms.saturating_add(9);
        relayed.transport = ObservationTransport::C2;

        assert_eq!(
            apply_update(&mut app, &mut apply, WorkerUpdate::ObservationIngress(direct)),
            AppAction::None,
        );
        assert_eq!(
            apply_update(&mut app, &mut apply, WorkerUpdate::ObservationIngress(relayed)),
            AppAction::None,
        );
        let monitor = app.session_monitors.values().next().unwrap();
        assert_eq!(app.session_monitor_projection(&monitor.key).unwrap().timeline.len(), 1);
    }

    #[test]
    fn session_monitor_rejects_conflicting_payload_for_same_node_cursor() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let incarnation_id = incarnation(29);
        authoritative_monitor_snapshot(&mut app, &mut apply, incarnation_id, 0);
        let first = monitor_ingress(
            incarnation_id,
            1,
            ObservationTransport::DirectNode,
            client_monitor_observation(1),
        );
        let mut conflicting = first.clone();
        let ObservationIngressPayload::Observation { observation, .. } = &mut conflicting.payload
        else { unreachable!("runtime observation ingress") };
        observation.kind = gate4agent_node_protocol::ObservationKindV1::TurnCompleted;

        apply_update(&mut app, &mut apply, WorkerUpdate::ObservationIngress(first));
        apply_update(&mut app, &mut apply, WorkerUpdate::ObservationIngress(conflicting));

        assert!(matches!(app.notice.as_deref(), Some(notice) if notice.contains("reused with different canonical ingress")));
        let monitor = app.session_monitors.values().next().unwrap();
        let projection = app.session_monitor_projection(&monitor.key).unwrap();
        assert_eq!(projection.timeline.len(), 1);
        assert_eq!(projection.timeline[0].kind, gate4agent_node_protocol::ObservationKindV1::Working);
    }

    #[test]
    fn session_monitor_accepts_late_generation_for_current_incarnation_without_rebind() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let incarnation_id = incarnation(30);
        authoritative_monitor_snapshot(&mut app, &mut apply, incarnation_id, 7);
        let mut late = monitor_ingress(
            incarnation_id,
            8,
            ObservationTransport::C2,
            client_monitor_observation(1),
        );
        let ObservationIngressPayload::Observation {
            address: ObservationTarget::Runtime { key },
            ..
        } = &mut late.payload else { unreachable!("runtime observation ingress") };
        key.generation = gate4agent_types::SessionGeneration(1);

        assert_eq!(
            apply_update(&mut app, &mut apply, WorkerUpdate::ObservationIngress(late)),
            AppAction::None,
        );
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 1,
        };
        assert!(app.session_monitors.contains_key(
            &client_runtime_monitor_target(&address, incarnation_id)
        ));
    }

    #[test]
    fn managed_inventory_snapshot_does_not_clear_gap() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let incarnation_id = incarnation(40);
        authoritative_monitor_snapshot(&mut app, &mut apply, incarnation_id, 0);
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::ObservationIngress(monitor_ingress(
                incarnation_id,
                1,
                ObservationTransport::C2,
                client_monitor_observation(1),
            )),
        );
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::EventCursor {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 2,
                },
            },
        );
        assert_eq!(
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::ObservationIngress(monitor_ingress(
                    incarnation_id,
                    4,
                    ObservationTransport::C2,
                    client_monitor_observation(2),
                )),
            ),
            AppAction::Resync {
                node_id: "node-a".to_owned(),
                after_sequence: 2,
            },
        );
        let mut snapshot = c2_terminal_snapshot(terminal_frame(1, b"snapshot"));
        snapshot.session_records = vec![c2_test_record("dormant inventory")];
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::C2Snapshot {
                expected_node_id: "node-a".to_owned(),
                endpoint: "c2".to_owned(),
                incarnation_id,
                snapshot,
                event_sequence: 4,
            },
        );

        let monitor = app.session_monitors.values().next().unwrap();
        let projection = app.session_monitor_projection(&monitor.key).unwrap();
        assert!(projection.transport_incomplete);
        assert_eq!(
            projection.freshness,
            gate4agent_observation_api::ProjectionFreshness::IncompleteAfterGap,
        );
        let managed = SessionMonitorTarget::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
            incarnation: incarnation_id,
        };
        assert!(app.session_monitors.get(&managed).is_none());
        app.open_session_monitor(crate::app::AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
        });
        assert!(app.session_monitors.get(&managed).is_some());
    }

    #[test]
    fn terminal_only_sequence_holes_do_not_mark_observation_partial_with_direct_c2_parity() {
        let incarnation_id = incarnation(41);
        let (mut direct_app, mut direct_apply, recovered) =
            gapped_session_monitor(incarnation_id, ObservationTransport::DirectNode);
        let direct_events = vec![gate4agent_node_protocol::NodeEventEnvelope {
            sequence: 4,
            event: NodeEvent::Observation {
                address: monitor_wire_address(2),
                observation: recovered.clone(),
            },
        }];
        let direct_batch = direct_observation_resync_batch(
            "node-a",
            incarnation_id,
            2,
            6,
            1,
            &direct_events,
        )
        .unwrap();
        assert!(direct_batch.gaps.is_empty());
        assert_eq!(direct_batch.events.len(), 1);
        assert_eq!(
            apply_update(
                &mut direct_app,
                &mut direct_apply,
                WorkerUpdate::ObservationResync(direct_batch.clone()),
            ),
            AppAction::None,
        );

        let (mut c2_app, mut c2_apply, _) =
            gapped_session_monitor(incarnation_id, ObservationTransport::C2);
        let c2_events = direct_events
            .iter()
            .map(gate4agent_c2_protocol::C2NodeEventEnvelope::from)
            .collect::<Vec<_>>();
        let c2_batch = c2_observation_resync_batch(
            "node-a",
            incarnation_id,
            2,
            6,
            1,
            &c2_events,
        )
        .unwrap();
        assert!(c2_batch.gaps.is_empty());
        assert_eq!(c2_batch.events.len(), 1);
        assert!(direct_batch.events[0].canonical_eq(&c2_batch.events[0]));
        assert_eq!(
            apply_update(
                &mut c2_app,
                &mut c2_apply,
                WorkerUpdate::ObservationResync(c2_batch),
            ),
            AppAction::None,
        );

        for app in [&direct_app, &c2_app] {
            let monitor = app.session_monitors.values().next().unwrap();
            let projection = app.session_monitor_projection(&monitor.key).unwrap();
            assert!(!projection.transport_incomplete);
            assert_ne!(
                projection.freshness,
                gate4agent_observation_api::ProjectionFreshness::IncompleteAfterGap,
            );
            assert_eq!(projection.timeline.len(), 2);
        }
    }

    #[test]
    fn real_durable_eviction_marks_observation_partial() {
        let incarnation_id = incarnation(42);
        let (mut app, mut apply, recovered) =
            gapped_session_monitor(incarnation_id, ObservationTransport::DirectNode);
        let incomplete_events = vec![gate4agent_node_protocol::NodeEventEnvelope {
            sequence: 4,
            event: NodeEvent::Observation {
                address: monitor_wire_address(2),
                observation: recovered,
            },
        }];
        let batch = direct_observation_resync_batch(
            "node-a",
            incarnation_id,
            2,
            4,
            4,
            &incomplete_events,
        )
        .unwrap();
        assert_eq!(
            batch.gaps,
            vec![ObservationGap {
                first_sequence: 3,
                last_sequence: 3,
            }],
        );
        let c2_events = incomplete_events
            .iter()
            .map(gate4agent_c2_protocol::C2NodeEventEnvelope::from)
            .collect::<Vec<_>>();
        let c2_batch = c2_observation_resync_batch(
            "node-a",
            incarnation_id,
            2,
            4,
            4,
            &c2_events,
        )
        .unwrap();
        assert_eq!(c2_batch.gaps, batch.gaps);
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::ObservationResync(batch),
        );

        let monitor = app.session_monitors.values().next().unwrap();
        let projection = app.session_monitor_projection(&monitor.key).unwrap();
        assert!(projection.transport_incomplete);
        assert_eq!(
            projection.freshness,
            gate4agent_observation_api::ProjectionFreshness::IncompleteAfterGap,
        );
    }

    #[tokio::test]
    async fn routed_c2_observation_reaches_exact_session_monitor() {
        let (updates, mut received) = mpsc::channel(2);
        let cursor = gate4agent_c2_protocol::NodeCursor {
            incarnation_id: incarnation(31),
            sequence: 44,
        };
        publish_c2_event(
            C2NodeEvent::Observation {
                address: monitor_wire_address(2),
                observation: client_monitor_observation(9),
            },
            cursor,
            &updates,
            "node-a",
        ).await;
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::C2Snapshot {
                expected_node_id: "node-a".to_owned(),
                endpoint: "c2".to_owned(),
                incarnation_id: cursor.incarnation_id,
                snapshot: c2_terminal_snapshot(terminal_frame(1, b"snapshot")),
                event_sequence: 43,
            },
        );
        let action = apply_update(&mut app, &mut apply, received.recv().await.unwrap());
        assert_eq!(action, AppAction::None);
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let monitor = app.session_monitors
            .get(&client_runtime_monitor_target(&address, cursor.incarnation_id))
            .unwrap();
        let projection = app.session_monitor_projection(&monitor.key).unwrap();
        assert_eq!(projection.timeline.back().unwrap().cursor.sequence, 44);
        assert_eq!(projection.timeline.back().unwrap().kind, gate4agent_node_protocol::ObservationKindV1::Working);
    }

    #[tokio::test]
    async fn harness_mcp_transient_events_do_not_publish_ui_updates_or_cursors() {
        let direct_event = NodeEvent::HarnessMcpReadCall {
            reservation_id: gate4agent_node_protocol::HarnessMcpReservationId::new(format!(
                "hmcpres_{}", "a".repeat(24),
            )).unwrap(),
            activation_digest: gate4agent_node_protocol::HarnessMcpActivationDigest::new(format!(
                "sha256:{}", "b".repeat(64),
            )).unwrap(),
            record_id: SessionRecordId::new("record-a").unwrap(),
            session: monitor_wire_address(2),
            call_id: gate4agent_node_protocol::HarnessMcpCallId::new(format!(
                "hmcpcall_{}", "c".repeat(24),
            )).unwrap(),
            request: gate4agent_node_protocol::HarnessReadRequestV1::ContextGet,
            deadline_unix_ms: 4_000,
        };
        let c2_event = C2NodeEvent::from(&direct_event);
        let cursor = gate4agent_c2_protocol::NodeCursor {
            incarnation_id: incarnation(61),
            sequence: 0,
        };

        let (direct_updates, mut direct_received) = mpsc::channel(2);
        let mut controller_owned = true;
        publish_node_event(
            direct_event,
            cursor,
            &direct_updates,
            "node-a",
            &mut controller_owned,
            1,
        ).await;
        assert!(matches!(
            direct_received.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        let (c2_updates, mut c2_received) = mpsc::channel(2);
        publish_c2_event(c2_event, cursor, &c2_updates, "node-a").await;
        assert!(matches!(
            c2_received.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn snapshot_cursor_cannot_drop_queued_observation() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let incarnation_id = incarnation(32);
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::C2Snapshot {
                expected_node_id: "node-a".to_owned(),
                endpoint: "c2".to_owned(),
                incarnation_id,
                snapshot: c2_terminal_snapshot(terminal_frame(1, b"snapshot")),
                event_sequence: 100,
            },
        );
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let action = apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::Observation {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 99,
                },
                address: address.clone(),
                observation: client_monitor_observation(1),
            },
        );
        assert_eq!(action, AppAction::None);
        let monitor = app.session_monitors
            .get(&client_runtime_monitor_target(&address, incarnation_id))
            .unwrap();
        assert_eq!(
            app.session_monitor_projection(&monitor.key).unwrap().timeline.back().unwrap().cursor.sequence,
            99,
        );
        assert_eq!(apply.snapshot_watermarks["node-a"].sequence, 100);
        assert_eq!(apply.event_watermarks["node-a"].sequence, 99);
    }

    #[tokio::test]
    async fn direct_observation_preserves_envelope_cursor_and_incarnation() {
        let (updates, mut received) = mpsc::channel(2);
        let mut controller_owned = true;
        let cursor = gate4agent_c2_protocol::NodeCursor {
            incarnation_id: incarnation(33),
            sequence: 712,
        };
        let observation = client_monitor_observation(17);
        publish_node_event(
            NodeEvent::Observation {
                address: monitor_wire_address(4),
                observation: observation.clone(),
            },
            cursor,
            &updates,
            "node-a",
            &mut controller_owned,
            1,
        ).await;
        assert!(matches!(
            received.recv().await,
            Some(WorkerUpdate::ObservationIngress(ObservationIngressEnvelope {
                node_id,
                cursor: actual_cursor,
                transport: ObservationTransport::DirectNode,
                payload: ObservationIngressPayload::Observation {
                    address: ObservationTarget::Runtime { key },
                    observation: actual_observation,
                },
                ..
            })) if node_id.as_str() == "node-a"
                && actual_cursor == cursor
                && key.generation.0 == 4
                && actual_observation == observation
        ));
    }

    #[tokio::test]
    async fn managed_observation_maps_exact_record_and_incarnation_direct_and_c2() {
        let cursor = gate4agent_c2_protocol::NodeCursor {
            incarnation_id: incarnation(53),
            sequence: 19,
        };
        let record_id = SessionRecordId::new("record-managed").unwrap();
        let observation = client_monitor_observation(4);

        let (direct_updates, mut direct_received) = mpsc::channel(2);
        let mut controller_owned = true;
        publish_node_event(
            NodeEvent::ManagedObservation {
                record_id: record_id.clone(),
                observation: observation.clone(),
            },
            cursor,
            &direct_updates,
            "node-a",
            &mut controller_owned,
            1,
        ).await;
        let direct = direct_received.recv().await.unwrap();

        let (c2_updates, mut c2_received) = mpsc::channel(2);
        publish_c2_event(
            C2NodeEvent::ManagedObservation {
                record_id: record_id.clone(),
                observation: observation.clone(),
            },
            cursor,
            &c2_updates,
            "node-a",
        ).await;
        let c2 = c2_received.recv().await.unwrap();

        for (update, transport) in [
            (direct, ObservationTransport::DirectNode),
            (c2, ObservationTransport::C2),
        ] {
            let WorkerUpdate::ObservationIngress(envelope) = update else {
                panic!("expected managed observation ingress")
            };
            let ObservationIngressPayload::Observation {
                address: ObservationTarget::Managed { key },
                observation: actual,
            } = envelope.payload else {
                panic!("expected exact managed observation target")
            };
            assert_eq!(envelope.cursor, cursor);
            assert_eq!(envelope.transport, transport);
            assert_eq!(key.node_id.as_str(), "node-a");
            assert_eq!(key.incarnation_id, cursor.incarnation_id);
            assert_eq!(key.record_id, record_id);
            assert_eq!(actual, observation);
        }
    }

    #[tokio::test]
    async fn direct_observation_before_any_snapshot_requests_bootstrap_resync() {
        let (updates, mut received) = mpsc::channel(4);
        let mut controller_owned = true;
        let incarnation_id = incarnation(34);
        publish_node_event(
            NodeEvent::WorkspaceAdded { workspace: direct_workspace("scratch") },
            gate4agent_c2_protocol::NodeCursor { incarnation_id, sequence: 20 },
            &updates,
            "node-a",
            &mut controller_owned,
            1,
        ).await;
        publish_node_event(
            NodeEvent::Observation {
                address: monitor_wire_address(1),
                observation: client_monitor_observation(1),
            },
            gate4agent_c2_protocol::NodeCursor { incarnation_id, sequence: 21 },
            &updates,
            "node-a",
            &mut controller_owned,
            1,
        ).await;

        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let mut actions = Vec::new();
        for _ in 0..3 {
            actions.push(apply_update(&mut app, &mut apply, received.recv().await.unwrap()));
        }
        // An observation racing ahead of the first topology snapshot must
        // request a resync — that request IS the bootstrap. Rejecting it
        // locally (tried once) starved the session roster forever in both
        // direct and light mode.
        assert!(
            actions.iter().any(|action| matches!(
                action,
                AppAction::Resync { node_id, after_sequence: 0 } if node_id == "node-a"
            )),
            "expected a bootstrap resync request, got {actions:?}",
        );
        assert_eq!(apply.event_watermarks["node-a"].sequence, 21);
        assert!(matches!(app.notice.as_deref(), Some(notice) if notice.contains("resync requested")));
    }

    #[test]
    fn new_snapshot_incarnation_rejects_late_old_event_cursor() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let old_incarnation = incarnation(35);
        let new_incarnation = incarnation(36);
        for (incarnation_id, event_sequence) in [(old_incarnation, 1), (new_incarnation, 1)] {
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::C2Snapshot {
                    expected_node_id: "node-a".to_owned(),
                    endpoint: "c2".to_owned(),
                    incarnation_id,
                    snapshot: c2_terminal_snapshot(terminal_frame(event_sequence, b"snapshot")),
                    event_sequence,
                },
            );
            if incarnation_id == old_incarnation {
                let address = SessionAddress {
                    node_id: "node-a".to_owned(),
                    workspace_id: "workspace-a".to_owned(),
                    instance_id: 7,
                    generation: 2,
                };
                apply_update(
                    &mut app,
                    &mut apply,
                    WorkerUpdate::Observation {
                        node_id: "node-a".to_owned(),
                        cursor: gate4agent_c2_protocol::NodeCursor {
                            incarnation_id,
                            sequence: 2,
                        },
                        address,
                        observation: client_monitor_observation(1),
                    },
                );
            }
        }
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        assert!(app.session_monitors
            .get(&client_runtime_monitor_target(&address, old_incarnation))
            .unwrap()
            .node_restarted);
        assert_eq!(
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::Observation {
                    node_id: "node-a".to_owned(),
                    cursor: gate4agent_c2_protocol::NodeCursor {
                        incarnation_id: old_incarnation,
                        sequence: 3,
                    },
                    address: address.clone(),
                    observation: client_monitor_observation(2),
                },
            ),
            AppAction::None,
        );
        let monitor = app.session_monitors
            .get(&client_runtime_monitor_target(&address, old_incarnation))
            .unwrap();
        assert_eq!(
            app.session_monitor_projection(&monitor.key).unwrap().timeline.back().unwrap().cursor.sequence,
            2,
        );
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::Observation {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id: new_incarnation,
                    sequence: 2,
                },
                address: address.clone(),
                observation: client_monitor_observation(1),
            },
        );
        assert!(app.session_monitors
            .contains_key(&client_runtime_monitor_target(&address, new_incarnation)));
    }

    #[test]
    fn direct_snapshot_replacement_rejects_late_old_incarnation_event() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let old_incarnation = incarnation(37);
        let new_incarnation = incarnation(38);
        for (incarnation_id, event_sequence, endpoint) in [
            (old_incarnation, 8, "direct-a"),
            (new_incarnation, 1, "direct-b"),
        ] {
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::Snapshot {
                    expected_node_id: "node-a".to_owned(),
                    endpoint: endpoint.to_owned(),
                    incarnation_id,
                    snapshot: direct_catalog_snapshot(),
                    controller_owned: true,
                    event_sequence,
                },
            );
        }
        assert_eq!(app.nodes[0].incarnation_id, Some(new_incarnation));
        assert_eq!(
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::EventCursor {
                    node_id: "node-a".to_owned(),
                    cursor: gate4agent_c2_protocol::NodeCursor {
                        incarnation_id: old_incarnation,
                        sequence: 9,
                    },
                },
            ),
            AppAction::None,
        );
        assert!(!apply.event_watermarks.contains_key("node-a"));
        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::EventCursor {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id: new_incarnation,
                    sequence: 2,
                },
            },
        );
        assert_eq!(apply.event_watermarks["node-a"].incarnation_id, new_incarnation);
        assert_eq!(apply.event_watermarks["node-a"].sequence, 2);
    }

    #[test]
    fn retired_direct_snapshot_cannot_reclaim_incarnation_authority() {
        let mut app = App::default();
        let mut apply = C2ApplyState::default();
        let old_incarnation = incarnation(39);
        let new_incarnation = incarnation(40);
        for (incarnation_id, event_sequence, endpoint) in [
            (old_incarnation, 5, "direct-a"),
            (new_incarnation, 1, "direct-b"),
            (old_incarnation, 6, "late-direct-a"),
        ] {
            apply_update(
                &mut app,
                &mut apply,
                WorkerUpdate::Snapshot {
                    expected_node_id: "node-a".to_owned(),
                    endpoint: endpoint.to_owned(),
                    incarnation_id,
                    snapshot: direct_catalog_snapshot(),
                    controller_owned: true,
                    event_sequence,
                },
            );
        }
        assert_eq!(app.nodes[0].incarnation_id, Some(new_incarnation));
        assert_eq!(app.nodes[0].endpoint, "direct-b");
        assert_eq!(apply.snapshot_watermarks["node-a"].incarnation_id, new_incarnation);
        assert!(apply.retired_incarnations["node-a"].contains(&old_incarnation));

        apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::EventCursor {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id: new_incarnation,
                    sequence: 2,
                },
            },
        );
        assert_eq!(apply.event_watermarks["node-a"].incarnation_id, new_incarnation);
        assert_eq!(apply.event_watermarks["node-a"].sequence, 2);
    }

    fn observation_test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "gate4agent-tui-observation-{label}-{}-{}.sqlite3",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        ))
    }

    fn cleanup_observation_test_path(path: &Path) {
        for candidate in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn observation_actor_exposes_only_committed_projection_and_visible_unavailable_state() {
        let path = observation_test_path("committed-only");
        let (writer, initial, updates) = start_observation_writer(&path).unwrap();
        let incarnation_id = incarnation(71);
        let mut app = App::default();
        app.apply_observation_open_snapshot(initial.projections, initial.durable_resume_cursors);
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let key = crate::app::SessionMonitorKey {
            agent: crate::app::AgentRowKey::Legacy(address.clone()),
            target: client_runtime_monitor_target(&address, incarnation_id),
        };
        let envelope = monitor_ingress(
            incarnation_id,
            1,
            ObservationTransport::DirectNode,
            client_monitor_observation(1),
        );

        writer.try_send(ObservationWriterCommand::Ingress(envelope)).unwrap();
        assert!(app.session_monitor_projection(&key).is_none());
        let committed = updates.recv_timeout(Duration::from_secs(2)).unwrap();
        apply_observation_actor_update(&mut app, committed);
        assert_eq!(app.session_monitor_projection(&key).unwrap().timeline.len(), 1);

        apply_observation_actor_update(
            &mut app,
            ObservationActorUpdate::Unavailable("injected commit failure".to_owned()),
        );
        assert_eq!(app.session_monitor_projection(&key).unwrap().timeline.len(), 1);
        assert!(matches!(
            app.observation_persistence,
            crate::app::ObservationPersistenceState::Unavailable(ref reason)
                if reason == "injected commit failure"
        ));
        writer.shutdown(&updates).unwrap();
        cleanup_observation_test_path(&path);
    }

    #[test]
    fn open_monitor_history_refresh_composes_exact_request_and_separate_committed_history_observation() {
        let path = observation_test_path("managed-history-refresh");
        let (writer, initial, updates) = start_observation_writer(&path).unwrap();
        let incarnation_id = incarnation(95);
        let mut app = cursor_app();
        app.nodes[0].incarnation_id = Some(incarnation_id);
        app.nodes[0].session_records.push(ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-history".to_owned(),
            display_name: "Dormant history".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: None,
            has_provider_session_identity: true,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            active_session: None,
            last_error: None,
        });
        app.apply_observation_open_snapshot(initial.projections, initial.durable_resume_cursors);
        let agent = crate::app::AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-history".to_owned(),
        };
        let action = app.open_session_monitor(agent.clone());
        assert!(matches!(
            &action,
            AppAction::RefreshSessionRecordHistory {
                node_id,
                node_incarnation_id,
                record_id,
                message_limit: 1,
            } if node_id == "node-a"
                && *node_incarnation_id == incarnation_id
                && record_id == "record-history"
        ));
        assert!(matches!(
            action_to_request(action),
            Some(NodeRequest::PreviewSessionRecord { record_id, message_limit: 1 })
                if record_id.as_str() == "record-history"
        ));
        assert!(app.preview_tabs.is_empty());

        let node_id = ObservationNodeId::new("node-a").unwrap();
        let managed_key = ManagedSessionKey {
            node_id: node_id.clone(),
            incarnation_id,
            record_id: SessionRecordId::new("record-history").unwrap(),
        };
        writer.try_send(ObservationWriterCommand::Inventory(ObservationRecordInventory {
            node_id: node_id.clone(),
            incarnation_id,
            records: vec![gate4agent_observation_api::ManagedRecordLink {
                managed: managed_key.clone(),
                runtime: None,
            }],
            complete: true,
        })).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        let history_observation = ObservationV1 {
            source_sequence: 1,
            observed_at_unix_ms: Some(1),
            evidence: gate4agent_node_protocol::ObservationEvidenceV1::HistoryProjection,
            kind: gate4agent_node_protocol::ObservationKindV1::HistorySnapshot {
                message_count: 7,
                message_count_exact: true,
                completed_turn_count: Some(3),
                total_tokens: Some(144),
            },
            truncated: false,
        };
        writer.try_send(ObservationWriterCommand::Ingress(
            managed_observation_ingress_envelope(
                "node-a",
                gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 1,
                },
                SessionRecordId::new("record-history").unwrap(),
                history_observation,
                ObservationTransport::DirectNode,
            ).unwrap(),
        )).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        let monitor_key = crate::app::SessionMonitorKey {
            agent,
            target: SessionMonitorTarget::Managed {
                node_id: "node-a".to_owned(),
                record_id: "record-history".to_owned(),
                incarnation: incarnation_id,
            },
        };
        let history = app.session_monitor_projection(&monitor_key)
            .and_then(|projection| projection.history.as_ref())
            .expect("committed managed history projection");
        assert_eq!(history.message_count, 7);
        assert_eq!(history.completed_turn_count, Some(3));
        assert_eq!(history.total_tokens, Some(144));
        assert!(matches!(
            app.surface.active_tab(),
            Some(SurfaceTab::SessionMonitor(key)) if key == &monitor_key
        ));
        assert!(app.preview_tabs.is_empty());

        writer.shutdown(&updates).unwrap();
        cleanup_observation_test_path(&path);
    }

    #[test]
    fn full_observation_writer_queue_never_blocks_event_handler_and_recovers_from_durable_cursor() {
        let (commands, held_commands) = sync_mpsc::sync_channel(1);
        commands.send(ObservationWriterCommand::Flush).unwrap();
        let mut apply = C2ApplyState::default();
        apply.observation_writer = Some(ObservationWriterHandle {
            commands,
            join: None,
        });
        let incarnation_id = incarnation(72);
        let mut app = App::default();
        app.apply_observation_open_snapshot(
            Vec::new(),
            vec![(
                ObservationNodeId::new("node-a").unwrap(),
                gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 7,
                },
            )],
        );
        let started = Instant::now();
        let action = apply_update(
            &mut app,
            &mut apply,
            WorkerUpdate::ObservationIngress(monitor_ingress(
                incarnation_id,
                8,
                ObservationTransport::DirectNode,
                client_monitor_observation(1),
            )),
        );
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_millis(100), "event handler blocked for {elapsed:?}");
        assert_eq!(action, AppAction::Resync {
            node_id: "node-a".to_owned(),
            after_sequence: 7,
        });
        assert!(matches!(
            app.observation_persistence,
            crate::app::ObservationPersistenceState::Unavailable(ref reason)
                if reason.contains("backpressure")
        ));
        assert!(matches!(held_commands.try_recv(), Ok(ObservationWriterCommand::Flush)));
        apply.observation_writer = None;
    }

    #[test]
    fn full_inventory_queue_is_visible_and_retries_exact_inventory() {
        let (commands, held_commands) = sync_mpsc::sync_channel(1);
        commands.send(ObservationWriterCommand::Flush).unwrap();
        let mut apply = C2ApplyState::default();
        apply.observation_writer = Some(ObservationWriterHandle {
            commands,
            join: None,
        });
        let incarnation_id = incarnation(75);
        let inventory = ObservationRecordInventory {
            node_id: ObservationNodeId::new("node-a").unwrap(),
            incarnation_id,
            records: Vec::new(),
            complete: true,
        };
        let mut app = App::default();
        assert!(matches!(
            enqueue_observation_command(
                &mut app,
                &apply,
                ObservationWriterCommand::Inventory(inventory.clone()),
                "node-a",
                incarnation_id,
                false,
            ),
            ObservationEnqueueOutcome::Dropped
        ));
        assert!(matches!(
            app.observation_persistence,
            crate::app::ObservationPersistenceState::Unavailable(ref reason)
                if reason.contains("backpressure")
        ));
        apply.pending_observation_inventories.insert(
            ("node-a".to_owned(), incarnation_id),
            inventory.clone(),
        );
        assert!(matches!(held_commands.try_recv(), Ok(ObservationWriterCommand::Flush)));
        retry_pending_observation_inventories(&mut app, &mut apply);
        let retried = held_commands.try_recv().unwrap();
        let ObservationWriterCommand::Inventory(actual) = retried else {
            panic!("expected retried inventory")
        };
        assert_eq!(actual, inventory);
        assert!(apply.pending_observation_inventories.is_empty());
        apply.observation_writer = None;
    }

    #[test]
    fn committed_route_replacement_removes_stale_cached_projection() {
        let path = observation_test_path("route-replacement");
        let (writer, initial, updates) = start_observation_writer(&path).unwrap();
        let incarnation_id = incarnation(76);
        let mut app = App::default();
        app.apply_observation_open_snapshot(initial.projections, initial.durable_resume_cursors);
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let key = crate::app::SessionMonitorKey {
            agent: crate::app::AgentRowKey::Legacy(address.clone()),
            target: client_runtime_monitor_target(&address, incarnation_id),
        };
        writer.try_send(ObservationWriterCommand::Ingress(monitor_ingress(
            incarnation_id,
            1,
            ObservationTransport::DirectNode,
            client_monitor_observation(1),
        ))).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        assert!(app.session_monitor_projection(&key).is_some());

        app.apply_observation_committed_route(
            2,
            Vec::new(),
            ObservationNodeId::new("node-a").unwrap(),
            incarnation_id,
            None,
        );
        assert!(app.session_monitor_projection(&key).is_none());
        writer.shutdown(&updates).unwrap();
        cleanup_observation_test_path(&path);
    }

    #[test]
    fn committed_new_incarnation_delta_freezes_cached_old_incarnation() {
        let path = observation_test_path("incarnation-replacement");
        let (writer, initial, updates) = start_observation_writer(&path).unwrap();
        let old_incarnation = incarnation(77);
        let new_incarnation = incarnation(78);
        let mut app = App::default();
        app.apply_observation_open_snapshot(initial.projections, initial.durable_resume_cursors);
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let old_key = crate::app::SessionMonitorKey {
            agent: crate::app::AgentRowKey::Legacy(address.clone()),
            target: client_runtime_monitor_target(&address, old_incarnation),
        };
        let new_key = crate::app::SessionMonitorKey {
            agent: crate::app::AgentRowKey::Legacy(address.clone()),
            target: client_runtime_monitor_target(&address, new_incarnation),
        };
        writer.try_send(ObservationWriterCommand::Ingress(monitor_ingress(
            old_incarnation,
            1,
            ObservationTransport::DirectNode,
            client_monitor_observation(1),
        ))).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        assert!(app.session_monitor_projection(&old_key).is_some());

        writer.try_send(ObservationWriterCommand::Ingress(monitor_ingress(
            new_incarnation,
            1,
            ObservationTransport::C2,
            client_monitor_observation(1),
        ))).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        let old = app.session_monitor_projection(&old_key).unwrap();
        assert_eq!(
            old.availability,
            gate4agent_observation_api::ProjectionAvailability::Frozen,
        );
        assert_eq!(
            old.freshness,
            gate4agent_observation_api::ProjectionFreshness::ReplacedIncarnation,
        );
        assert!(app.session_monitor_projection(&new_key).is_some());
        writer.shutdown(&updates).unwrap();
        cleanup_observation_test_path(&path);
    }

    #[test]
    fn actor_inventory_snapshot_does_not_clear_committed_gap() {
        let path = observation_test_path("inventory-gap");
        let (writer, initial, updates) = start_observation_writer(&path).unwrap();
        let incarnation_id = incarnation(79);
        let mut app = App::default();
        app.apply_observation_open_snapshot(initial.projections, initial.durable_resume_cursors);
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let key = crate::app::SessionMonitorKey {
            agent: crate::app::AgentRowKey::Legacy(address.clone()),
            target: client_runtime_monitor_target(&address, incarnation_id),
        };
        writer.try_send(ObservationWriterCommand::Ingress(monitor_ingress(
            incarnation_id,
            1,
            ObservationTransport::DirectNode,
            client_monitor_observation(1),
        ))).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        writer.try_send(ObservationWriterCommand::Resync(
            observation_gap_batch("node-a", incarnation_id, 1, 4).unwrap(),
        )).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        assert!(app.session_monitor_projection(&key).unwrap().transport_incomplete);

        writer.try_send(ObservationWriterCommand::Inventory(ObservationRecordInventory {
            node_id: ObservationNodeId::new("node-a").unwrap(),
            incarnation_id,
            records: Vec::new(),
            complete: true,
        })).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        assert!(app.session_monitor_projection(&key).unwrap().transport_incomplete);

        writer.try_send(ObservationWriterCommand::Resync(ObservationResyncBatch {
            node_id: ObservationNodeId::new("node-a").unwrap(),
            incarnation_id,
            requested_after: 1,
            high_watermark: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 4,
            },
            oldest_available_sequence: 2,
            records: Vec::new(),
            records_complete: false,
            gaps: Vec::new(),
            events: Vec::new(),
        })).unwrap();
        apply_observation_actor_update(
            &mut app,
            updates.recv_timeout(Duration::from_secs(2)).unwrap(),
        );
        assert!(!app.session_monitor_projection(&key).unwrap().transport_incomplete);
        assert_eq!(app.durable_observation_cursor("node-a", incarnation_id), Some(4));
        writer.shutdown(&updates).unwrap();
        cleanup_observation_test_path(&path);
    }

    #[test]
    fn bounded_actor_update_backlog_shutdown_drains_without_deadlock() {
        let path = observation_test_path("bounded-shutdown");
        let (writer, _, updates) = start_observation_writer(&path).unwrap();
        let incarnation_id = incarnation(81);
        for _ in 0..=OBSERVATION_COMMAND_QUEUE {
            writer.commands.send(ObservationWriterCommand::Inventory(
                ObservationRecordInventory {
                    node_id: ObservationNodeId::new("node-a").unwrap(),
                    incarnation_id,
                    records: Vec::new(),
                    complete: true,
                },
            )).unwrap();
        }
        let drained = writer.shutdown(&updates).unwrap();
        assert_eq!(drained.len(), OBSERVATION_COMMAND_QUEUE + 1);
        assert!(drained.iter().all(|update| matches!(
            update,
            ObservationActorUpdate::Committed { .. }
        )));
        cleanup_observation_test_path(&path);
    }

    #[test]
    fn durable_cursor_schedules_exact_initial_direct_and_c2_resync_once() {
        let incarnation_id = incarnation(73);
        let mut app = App::default();
        app.apply_observation_open_snapshot(
            Vec::new(),
            vec![(
                ObservationNodeId::new("node-a").unwrap(),
                gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 5,
                },
            )],
        );
        let support = SessionMonitorSupport {
            known: true,
            events: true,
            managed_target: true,
            workflow_detail: true,
        };
        for _transport in [ObservationTransport::DirectNode, ObservationTransport::C2] {
            let mut apply = C2ApplyState::default();
            assert_eq!(
                schedule_initial_observation_resync(
                    &app,
                    &mut apply,
                    "node-a",
                    incarnation_id,
                    9,
                    support,
                ),
                Some(AppAction::Resync {
                    node_id: "node-a".to_owned(),
                    after_sequence: 5,
                }),
            );
            assert_eq!(
                schedule_initial_observation_resync(
                    &app,
                    &mut apply,
                    "node-a",
                    incarnation_id,
                    9,
                    support,
                ),
                None,
            );
        }

        let mut equal = C2ApplyState::default();
        assert_eq!(
            schedule_initial_observation_resync(
                &app,
                &mut equal,
                "node-a",
                incarnation_id,
                5,
                support,
            ),
            None,
        );
        let mut replacement = C2ApplyState::default();
        assert_eq!(
            schedule_initial_observation_resync(
                &app,
                &mut replacement,
                "node-a",
                incarnation(74),
                0,
                support,
            ),
            Some(AppAction::Resync {
                node_id: "node-a".to_owned(),
                after_sequence: 0,
            }),
        );
    }

    #[test]
    fn rejected_observation_command_requests_recovery_from_durable_cursor() {
        let incarnation_id = incarnation(80);
        let mut app = App::default();
        app.apply_observation_open_snapshot(
            Vec::new(),
            vec![(
                ObservationNodeId::new("node-a").unwrap(),
                gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence: 13,
                },
            )],
        );
        let action = apply_observation_actor_update(
            &mut app,
            ObservationActorUpdate::Rejected {
                message: "cursor collision".to_owned(),
                node_id: Some(ObservationNodeId::new("node-a").unwrap()),
                incarnation_id: Some(incarnation_id),
            },
        );
        assert_eq!(action, AppAction::Resync {
            node_id: "node-a".to_owned(),
            after_sequence: 13,
        });
        assert!(matches!(app.notice.as_deref(), Some(notice) if notice.contains("cursor collision")));
    }

    #[test]
    fn harness_workspace_reads_have_only_the_harness_detail_route_and_no_node_fallback() {
        let origin = HarnessRunOrigin {
            run: HarnessRunRef {
                run_id: HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap(),
                run_revision: HarnessRevision::new(5).unwrap(),
            },
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: gate4agent_harness_client::HarnessNodeIncarnationV1::new(
                "cd".repeat(16),
            ).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
        };
        let action = AppAction::HarnessReadWorkspaceFile {
            origin,
            path: RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap(),
            token: 41,
        };
        assert!(harness_detail_read_action(&action));
        assert_eq!(action_node_id(&action), Some(HARNESS_DETAIL_COMMAND_ROUTE));
        assert!(action_to_request(action).is_none());
    }

    #[test]
    fn harness_phase5_observation_reads_have_no_node_request_fallback() {
        let run = HarnessRunRef {
            run_id: HarnessRunId::new(format!("hrun_{}", "6".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(3).unwrap(),
        };
        let monitor = AppAction::HarnessOpenMonitor { run: run.clone() };
        assert!(harness_detail_read_action(&monitor));
        assert!(action_to_request(monitor).is_none());

        let task = HarnessTaskId::new(format!("htask_{}", "7".repeat(24))).unwrap();
        let task_observations = AppAction::HarnessLoadTaskCorrelations {
            task: HarnessTaskRef {
                task_id: task,
                task_revision: HarnessRevision::new(5).unwrap(),
            },
            launch_token: 93,
            runs: vec![run],
        };
        assert!(harness_detail_read_action(&task_observations));
        assert!(action_to_request(task_observations).is_none());
    }

    #[test]
    fn harness_run_transfer_reads_use_only_detail_route_and_queue_failure_is_terminal() {
        let run = HarnessRunRef {
            run_id: HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(4).unwrap(),
        };
        let action = AppAction::HarnessLoadRunTransfer {
            run: run.clone(),
            token: 73,
        };
        assert!(harness_detail_read_action(&action));
        assert_eq!(action_node_id(&action), Some(HARNESS_DETAIL_COMMAND_ROUTE));
        assert!(action_to_request(action.clone()).is_none());

        let mut app = App::default();
        app.harness_kanban.run_transfers.insert(
            run.clone(),
            crate::app::HarnessRunTransferState::Loading { token: 73 },
        );
        assert!(reject_harness_queue_action(
            &mut app,
            &action,
            HarnessQueueRejection::Unavailable,
        ));
        assert!(matches!(
            app.harness_kanban.run_transfers.get(&run),
            Some(crate::app::HarnessRunTransferState::Error { token: 73, message })
                if message.contains("queue is closed"),
        ));
    }

    #[test]
    fn harness_launch_options_read_uses_detail_lane_and_queue_failure_is_terminal() {
        let task = paginated_harness_task(17);
        let task_ref = harness_task_ref(&task);
        let action = AppAction::HarnessLoadTaskLaunchOptions {
            task: task_ref.clone(),
            token: 94,
        };
        assert!(harness_detail_read_action(&action));
        assert_eq!(action_node_id(&action), Some(HARNESS_DETAIL_COMMAND_ROUTE));
        assert!(action_to_request(action.clone()).is_none());

        let mut app = App::default();
        app.harness_kanban.launch_options.insert(
            task_ref.clone(),
            crate::app::HarnessLaunchOptionsState::Loading { token: 94 },
        );
        assert!(reject_harness_queue_action(
            &mut app,
            &action,
            HarnessQueueRejection::Unavailable,
        ));
        assert!(matches!(
            app.harness_kanban.launch_options.get(&task_ref),
            Some(crate::app::HarnessLaunchOptionsState::Error { token: 94, message })
                if message.contains("queue is closed"),
        ));
    }

    #[test]
    fn harness_context_observation_uses_only_detail_lane_and_queue_failure_is_terminal() {
        let task = paginated_harness_task(18);
        let task_ref = harness_task_ref(&task);
        let run = HarnessRunRef {
            run_id: HarnessRunId::new(format!("hrun_{}", "9".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(6).unwrap(),
        };
        let action = AppAction::HarnessObserveRunContextSource {
            run: run.clone(),
            task: task_ref,
            token: 96,
        };
        assert!(harness_detail_read_action(&action));
        assert_eq!(action_node_id(&action), Some(HARNESS_DETAIL_COMMAND_ROUTE));
        assert!(action_to_request(action.clone()).is_none());

        let mut app = App::default();
        app.harness_kanban.run_context_sources.insert(
            run.clone(),
            crate::app::HarnessRunContextSourceState::Loading { token: 96 },
        );
        assert!(reject_harness_queue_action(
            &mut app,
            &action,
            HarnessQueueRejection::Unavailable,
        ));
        assert!(matches!(
            app.harness_kanban.run_context_sources.get(&run),
            Some(crate::app::HarnessRunContextSourceState::Error {
                token: 96,
                message,
            }) if message.contains("queue is closed"),
        ));
    }

    #[test]
    fn reverse_attribution_uses_only_harness_detail_lane_and_queue_failure_is_terminal() {
        let subject = HarnessReverseAttributionSubjectV1::RuntimeSession {
            workspace: gate4agent_harness_client::HarnessReverseAttributionWorkspaceV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation_id:
                    gate4agent_harness_client::HarnessNodeIncarnationV1::new(
                        "ab".repeat(16),
                    ).unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            },
            instance_id: 7,
            generation: 2,
        };
        let action = AppAction::HarnessLoadReverseAttribution {
            subject: subject.clone(),
            token: 95,
        };
        assert!(harness_detail_read_action(&action));
        assert_eq!(action_node_id(&action), Some(HARNESS_DETAIL_COMMAND_ROUTE));
        assert!(action_to_request(action.clone()).is_none());

        let mut app = App::default();
        app.harness_kanban.reverse_attribution = Some(
            crate::app::HarnessReverseAttributionDetail {
                subject: subject.clone(),
                state: crate::app::HarnessReverseAttributionState::Loading { token: 95 },
            },
        );
        assert!(reject_harness_queue_action(
            &mut app,
            &action,
            HarnessQueueRejection::Unavailable,
        ));
        assert!(matches!(
            app.harness_kanban.reverse_attribution.as_ref().map(|detail| &detail.state),
            Some(crate::app::HarnessReverseAttributionState::Error { token: 95, message })
                if message.contains("queue is closed"),
        ));
    }

    #[test]
    fn harness_git_load_more_depends_only_on_next_cursor() {
        let origin = HarnessRunWorkspaceOriginV1 {
            run_id: HarnessRunId::new(format!("hrun_{}", "e".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(1).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: gate4agent_harness_client::HarnessNodeIncarnationV1::new(
                "ef".repeat(16),
            ).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
        };
        let truncated_without_cursor = HarnessRunGitHistoryPageV1 {
            origin: origin.clone(),
            path: None,
            commits: Vec::new(),
            next_before: None,
            truncated: true,
        };
        assert!(!harness_git_history_has_more(&truncated_without_cursor));

        let cursor_without_truncation = HarnessRunGitHistoryPageV1 {
            origin,
            path: None,
            commits: Vec::new(),
            next_before: Some(HarnessGitObjectIdV1::new("a".repeat(40)).unwrap()),
            truncated: false,
        };
        assert!(harness_git_history_has_more(&cursor_without_truncation));
    }
}
