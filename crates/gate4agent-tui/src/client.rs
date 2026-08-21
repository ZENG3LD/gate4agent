use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, stdout};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::{
    cursor::{Hide, MoveTo, RestorePosition, SavePosition, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use gate4agent_c2_protocol::{
    C2RelayRoute, C2SessionSnapshot, C2SessionStatus, C2WorkspaceSnapshot,
};
use gate4agent_node_protocol::{
    AgentProgressV1, ContextPackLineageReceipt,
    GitCommitSummary,
    GitSnapshot, GitStatusEntry,
    HostDirectoryEntry, HostDirectoryListing,
    NativeSessionCatalogEntry, NativeSessionCatalogPage,
    NativeSessionCatalogSummary, NativeSessionCatalogWindow,
    SessionRecordPreview, NodeId,
    ResolvedContextPackReceipt,
    LaunchInventory, ResolvedBundleReceipt, ResolvedEnvironmentProfileReceipt, SpawnProfileSummary,
    ManagedSessionState, OpaqueHostPath, SessionAddress as WireSessionAddress, SessionKey,
    SessionMode, SessionRecordId,
    SpawnContextDigest, SpawnContextId,
    RepositoryPath, WorkspaceEntry, WorkspaceEntryKind, WorkspaceFileContent,
    WorkspaceFileRevision, WorkspaceId, WorktreeServiceMode,
};
use gate4agent_harness_client::{
    HarnessNativeSessionCatalogEntryV1, HarnessNativeSessionCatalogScopeV1,
    HarnessNativeSessionCatalogSummaryV1, HarnessNativeSessionCatalogWindowV1,
    HarnessNativeSessionExternalGroupKindV1, HarnessNativeSessionPreviewRoleV1,
    HarnessNativeSessionPreviewV1, HarnessNativeSessionRouteV1,
    HarnessNativeSessionSelectionV1,
    HarnessOperatorActionV1, HarnessOperatorClient,
    HarnessOperatorCredential, HarnessOperatorEventV1,
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
    HarnessProviderSessionIdentityV1, HarnessProviderSessionKeyV1, HarnessSessionTaskTargetV1,
    HarnessRuntimeManagedModeV1, HarnessRuntimeManagedSessionV1,
    HarnessRuntimeManagedStateV1, HarnessRuntimeNodeInventoryV1,
    HarnessRuntimeMouseProtocolEncodingV1, HarnessRuntimeSessionAddressV1,
    HarnessRuntimeSessionStatusV1, HarnessRuntimeSessionV1, HarnessRuntimeTerminalFrameV1,
    HarnessRuntimeTerminalPageV1, HarnessRuntimeTerminalSizeV1, HarnessRuntimeTransportV1,
    HarnessRuntimeLaunchInventoryV1,
    HarnessNodeWorkspaceFileV1, HarnessNodeWorkspaceInspectionV1, HarnessNodeWorkspaceDirectoryV1,
    HarnessWorkspaceFileRevisionV1,
    HarnessNodeGitDiffV1, HarnessNodeGitHistoryPageV1,
    HarnessGitWorktreeSnapshotV1, HarnessHostDirectoryListingV1, HarnessHostPathV1,
    HarnessResolvedContextPackReceiptV1, HarnessWorkspaceSnapshotV1, HarnessWorktreeServiceModeV1,
    RedactedTaskV1, RunPageV1, TaskPageV1,
    SessionMonitorV1 as HarnessSessionMonitorV1, TimelineEntryV1,
    HARNESS_TERMINAL_PAGE_LIMIT_MAX,
};
use gate4agent_harness_protocol::HarnessSelectorV1;
use gate4agent_types::{
    AgentId, AgentInstanceId, ProviderActivity, ProviderSessionIdentity,
    HistoryMessageRole, NativeSessionExternalGroup, NativeSessionExternalGroupKind,
    NativeSessionPreviewMessage, SessionGeneration,
    TerminalFrame, TerminalSize,
    TerminalMouseProtocolEncoding, TransportKind,
};
use tokio::sync::{mpsc, watch};
use uzor_tui::{Backend, CrosstermBackend, Screen};

use crate::app::{
    App, AppAction, ConnectionState, NodeView, Provider, ProviderInventory, PtyColorMode,
    ManagedSessionView, NativeSessionCatalogRoute, NativeSessionCatalogRowView, NativeSessionPreviewMessageView,
    NativeSessionPreviewView, SessionAddress, SessionView, UiKey, GitCommitView,
    HarnessReadFailure, HarnessRunOrigin, HarnessRunRef, HarnessWorkspaceFileTabKey,
    HarnessWorkspaceGitRequestDestination, SixelIconPlacement, WorkspaceFileTabKey, WorkspaceGitDiffTarget, WorkspaceGitDiffView,
    WorkspaceGitRequestDestination, WorkspaceView,
};
use crate::diagnostics::RuntimeDiagnostic;
use crate::icons;
use crate::preferences::{self, UiPreferences};
use crate::render;

#[cfg(test)]
use crate::app::{SurfaceTab, WorkspaceGitTabKey};

const COMMAND_QUEUE: usize = 64;
const UPDATE_QUEUE: usize = 256;
const HARNESS_COMMAND_ROUTE: &str = "\0harness";
const HARNESS_HISTORY_COMMAND_ROUTE: &str = "\0harness-history";
const HARNESS_DETAIL_COMMAND_ROUTE: &str = "\0harness-detail";
/// The harness read surface is poll-only (no event push yet): without a
/// periodic snapshot cadence the roster/kanban only converge after an
/// operator mutation, so sessions spawned moments ago (or by any other
/// client of the same harness) never appear until a manual refresh.
const HARNESS_SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Reconnect backoff for `harness_event_subscription_worker`: 1s, 2s, 4s,
/// capped at 8s. Doubles on every failed `subscribe_events` call or ended
/// subscription, resets to the initial value the moment a subscription
/// goes live again.
const HARNESS_SUBSCRIPTION_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const HARNESS_SUBSCRIPTION_BACKOFF_MAX: Duration = Duration::from_secs(8);
const RAW_INPUT_COALESCE: Duration = Duration::from_millis(12);
const INSPECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const HARNESS_TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PREFERENCES_SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DIRTY_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(80);
const ANIMATION_TICKS_PER_FRAME: u8 = 4;
const MAX_TERMINAL_EVENT_BATCH: usize = 256;
const HARNESS_SNAPSHOT_PAGE_SIZE: u16 = 64;
const HARNESS_TASK_PAGE_BUDGET: usize = 16;
const HARNESS_TASK_ENTITY_BUDGET: usize = 512;
const HARNESS_RUN_PAGE_BUDGET: usize = 32;
const HARNESS_RUN_ENTITY_BUDGET: usize = 1_024;
const HARNESS_RUNTIME_INVENTORY_PAGE_BUDGET: usize = 64;
const HARNESS_RUNTIME_INVENTORY_ENTITY_BUDGET: usize = 4_096;
const HARNESS_RUNTIME_INVENTORY_RETRY_BUDGET: usize = 20;
const HARNESS_RUNTIME_INVENTORY_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct HarnessOperatorEndpoint {
    pub endpoint: SocketAddr,
    pub credential: HarnessOperatorCredential,
    pub launch_plan_id: Option<HarnessSelectorV1>,
}

/// The app now speaks exactly one dialect (the harness operator wire, V9-V11
/// -- see `docs/gate4agent/plans/gate4agent-app-harness-protocol-contract-
/// 2026-08-20.md`): `gate4agent-tui` hosts the full harness and
/// `gate4agent-tui-light` hosts `gate4agent-harness-light` in-process, but
/// both drive `run()` through this same, single struct.
#[derive(Clone)]
pub struct RunOptions {
    pub operator: HarnessOperatorEndpoint,
    /// Whether the kanban (task/run board) starts enabled -- `true` for the
    /// full harness (`gate4agent-tui`'s own default), `false` for
    /// `gate4agent-tui-light` (the session board stays the default view,
    /// matching light's pre-cutover UX; light-harness tasks/runs are always
    /// empty by canon anyway, see `gate4agent-harness-light`'s crate doc).
    pub kanban_default: bool,
    pub color_mode_override: Option<PtyColorMode>,
}

enum WorkerUpdate {
    /// A typed harness SpawnSession succeeded: the app opens (or queues via
    /// `pending_open`) the PTY tab for the returned address — light-mode
    /// spawn parity, where the session panel appears without any manual
    /// roster interaction.
    HarnessSessionSpawned {
        address: SessionAddress,
    },
    SelectWorkspace { node_id: String, workspace_id: String },
    WorkspaceUpserted {
        node_id: String,
        workspace: WorkspaceSnapshotUpdate,
    },
    WorkspaceRemoved { node_id: String, workspace_id: String },
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
    ContextExported(ResolvedContextPackReceipt),
    ContextForgotten(SpawnContextId),
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
    /// Pushed by `harness_event_subscription_worker` for a
    /// `HarnessOperatorEventV1::SnapshotBaseline` -- the subscription's
    /// mandatory first frame, and its recovery frame after a `Lagged` (see
    /// `HarnessEventLagged`). Token-free unlike `HarnessSnapshot`: applied
    /// via `App::apply_harness_snapshot_pushed`, not the token-gated
    /// `apply_harness_snapshot`.
    HarnessEventSnapshotBaseline {
        tasks: Vec<RedactedTaskV1>,
        runs: Vec<RedactedRunV1>,
        nodes: Vec<NodeView>,
    },
    HarnessTaskChanged(RedactedTaskV1),
    HarnessRunChanged(RedactedRunV1),
    HarnessRuntimeInventoryNodeChanged(NodeView),
    HarnessRuntimeInventoryNodeRemoved { node_id: String },
    /// The subscription fell behind and the host dropped one or more
    /// events for it; a `HarnessEventSnapshotBaseline` always follows once
    /// the host can build one. No-op on its own -- the recovery frame is
    /// what actually resynchronizes state -- kept as its own variant purely
    /// so a future diagnostic surface has something to hook.
    HarnessEventLagged,
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
    // Write/create siblings of the node-workspace read pair above: the
    // editor save and create-file/create-directory dialog, still landing in
    // the same direct-mode state (`file_tabs`, `create_workspace_entry`) the
    // direct-C2 replies fill -- see `harness_route_workspace_action`.
    HarnessNodeWorkspaceFileWritten {
        node_id: String,
        token: u64,
        file: HarnessNodeWorkspaceFileV1,
    },
    HarnessNodeWorkspaceFileWriteFailed {
        key: WorkspaceFileTabKey,
        token: u64,
        message: String,
    },
    HarnessNodeWorkspaceFileCreated {
        node_id: String,
        token: u64,
        file: HarnessNodeWorkspaceFileV1,
    },
    HarnessNodeWorkspaceDirectoryCreated {
        token: u64,
        directory: HarnessNodeWorkspaceDirectoryV1,
    },
    HarnessNodeWorkspaceEntryCreateFailed {
        node_id: String,
        workspace_id: String,
        path: RepositoryPath,
        kind: WorkspaceEntryKind,
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

enum WorkspaceSnapshotUpdate {
    C2(gate4agent_c2_protocol::C2WorkspaceSnapshot),
}

/// The one piece of `C2ApplyState` (pre-cutover: c2 snapshot/event admission
/// plus this) that survives the direct-C2 dialect's removal: the harness
/// terminal-tail path (`WorkerUpdate::HarnessTerminalRead`, `run()`'s own
/// periodic `HarnessOpenTerminal` poll) shares this exact per-session
/// high-watermark tracking, so it gets its own small, focused home instead of
/// dying with the rest of that struct.
#[derive(Default)]
struct TerminalWatermarks {
    watermarks: BTreeMap<SessionAddress, u64>,
}

impl TerminalWatermarks {
    fn terminal_frame_is_new(&self, address: &SessionAddress, sequence: u64) -> bool {
        self.watermarks
            .get(address)
            .is_none_or(|current| sequence > *current)
    }

    fn record_terminal_frame(&mut self, address: SessionAddress, sequence: u64) {
        self.watermarks.insert(address, sequence);
    }

    fn terminal_watermark(&self, address: &SessionAddress) -> Option<u64> {
        self.watermarks.get(address).copied()
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
    let RunOptions { operator, kanban_default, color_mode_override } = options;
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
    let mut terminal_watermarks = TerminalWatermarks::default();
    initial_preferences.apply_to(&mut app);
    if let Some(color_mode) = color_mode_override {
        app.color_mode = color_mode;
    }
    app.terminal_cols = cols;
    app.terminal_rows = rows;
    // Set by `harness_event_subscription_worker` the moment a subscription
    // goes live; read by the run loop below to skip the
    // `HARNESS_SNAPSHOT_REFRESH_INTERVAL` poll while a subscription is
    // carrying updates -- see that gate's call site.
    let harness_subscription_active = Arc::new(AtomicBool::new(false));
    app.enable_harness_schedule(operator.launch_plan_id.clone());
    let client = HarnessOperatorClient::new(operator.endpoint, operator.credential)?;
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
    let subscription_client = client.clone();
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
    let subscription_updates = updates_tx.clone();
    let subscription_active = harness_subscription_active.clone();
    tokio::task::spawn_blocking(move || {
        harness_event_subscription_worker(
            subscription_client,
            subscription_updates,
            subscription_active,
        )
    });
    if kanban_default {
        let action = app.enable_harness_kanban();
        if command_tx.try_send(action).is_err() {
            return Err("Harness operator command queue rejected initial refresh".into());
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
    let mut sixel_emit_state = SixelEmitState::default();
    while !app.should_quit {
        let mut state_changed = false;
        while let Ok(update) = updates_rx.try_recv() {
            let action = apply_update(&mut app, &mut terminal_watermarks, update);
            queue_action(
                &mut app,
                &commands,
                &inspection_commands,
                &mut pending_raw,
                action,
            );
            state_changed = true;
        }
        let selected_route = app.selected_workspace_route();
        if selected_route != auto_inspected_route {
            auto_inspected_route = selected_route.clone();
            next_auto_inspection = Instant::now();
        }
        let now = Instant::now();
        // The harness backend serves workspace reads through the V9
        // node-scoped operator family — the periodic auto-inspection must
        // fire there too, or the Files/Git sidebar never leaves
        // "loading workspace...".
        if selected_route.is_some()
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
        // The poll is the fallback once a push subscription is live: while
        // `harness_subscription_active` is true, `next_harness_snapshot_
        // refresh` deliberately stays un-rearmed (stale, in the past), so
        // the instant the subscription drops this fires on the very next
        // loop pass instead of waiting up to `HARNESS_SNAPSHOT_REFRESH_
        // INTERVAL` for a stale re-arm to catch up.
        if now >= next_harness_snapshot_refresh
            && !harness_subscription_active.load(Ordering::Relaxed)
        {
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
        let focused_harness_terminal = app.focused_address().cloned();
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
                    after_sequence: terminal_watermarks.terminal_watermark(&address),
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
            flush_sixel_icon(&app, &mut sixel_emit_state)?;
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
        // Workspace-inspection polling has no dedicated deadline of its own:
        // the harness backend serves it through the auto-inspection action
        // above (`next_auto_inspection`), not a separate poll cadence -- the
        // direct-C2 poll cadence this used to gate died with the C2 dialect.
        let inspection_deadline: Option<Instant> = None;
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
                    // `Screen::resize` itself debounces (a no-op unless
                    // the size actually changed) and, on a real change,
                    // clears the WHOLE terminal on its own next `flush()`
                    // (`force_full_redraw`) -- wiping any sixel pixels
                    // already drawn there regardless of whether this
                    // frame's rail layout comes out byte-identical to the
                    // last one. Mirror that exact condition here so the
                    // gate in `flush_sixel_icon` cannot skip the one
                    // frame that actually needs a fresh emission.
                    if screen.size() != (cols, rows) {
                        screen.resize(cols, rows);
                        sixel_emit_state.force_next = true;
                    }
                    app.terminal_cols = cols;
                    app.terminal_rows = rows;
                    frames.mark_dirty();
                }
                _ => {}
                }
            }
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

/// Rewrites a direct-shaped workspace-read or workspace-write action into
/// its harness-routed, node-scoped sibling -- the app speaks only the
/// harness operator wire now, so this rewrite is unconditional. The App
/// methods that build `InspectWorkspace`/`ReadWorkspaceFile`/`ReadGitHistory`/
/// `ReadGitDiff`/`WriteWorkspaceFile`/`CreateWorkspaceFile`/
/// `CreateWorkspaceDirectory` stay dialect-agnostic and keep populating the
/// same direct-shaped state (`inspection_pending`, `file_tabs`, `git_tabs`,
/// `create_workspace_entry`) either way — only the wire-level action this
/// dispatch boundary sends onward changes. Applied once, here, rather than
/// at each of the several call sites that build these seven actions
/// (sidebar open, pagination, diff selection, editor save, create-entry
/// dialog).
fn harness_route_workspace_action(action: AppAction) -> AppAction {
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
        AppAction::WriteWorkspaceFile {
            node_id, workspace_id, path, expected_revision, text, token,
        } => AppAction::HarnessWriteNodeWorkspaceFile {
            node_id, workspace_id, path, expected_revision, text, token,
        },
        AppAction::CreateWorkspaceFile { node_id, workspace_id, path, token } => {
            AppAction::HarnessCreateNodeWorkspaceFile { node_id, workspace_id, path, token }
        }
        AppAction::CreateWorkspaceDirectory { node_id, workspace_id, path, token } => {
            AppAction::HarnessCreateNodeWorkspaceDirectory { node_id, workspace_id, path, token }
        }
        other => other,
    }
}

fn send_operator_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    action: AppAction,
) {
    let action = harness_route_workspace_action(action);
    let action = app.route_harness_session_verb(action);
    let Some(node_id) = action_node_id(&action).map(str::to_owned) else {
        return;
    };
    let harness_native_history_read = harness_native_history_read_action(&action);
    let harness_detail_read = harness_detail_read_action(&action);
    let harness_session_record_mutation = harness_session_record_mutation_action(&action);
    let harness_resource_mutation = harness_resource_mutation_action(&action);
    if node_id != HARNESS_COMMAND_ROUTE
        && !harness_native_history_read
        && !harness_detail_read
        && !harness_session_record_mutation
        && !harness_resource_mutation
    {
        if !reject_history_refresh_action(app, &action, "Harness-owned session action unavailable") {
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
    } else {
        // The early-return guard above already established that one of
        // `harness_session_record_mutation`/`harness_resource_mutation`/
        // `node_id == HARNESS_COMMAND_ROUTE` holds whenever execution reaches
        // here -- all three ride the same mutation lane.
        commands.get(HARNESS_COMMAND_ROUTE)
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
        // Write/create siblings of the four node-workspace reads above: the
        // sidebar's editor save and create-file/create-directory dialog
        // would otherwise hang in the "saving"/"creating" state a direct-
        // mode key handler already set (`TextEditor::mark_saving`,
        // `CreateWorkspaceEntryDialog::pending`) if this queue is busy or
        // closed before `harness_detail_worker` ever sees the action.
        AppAction::HarnessWriteNodeWorkspaceFile { node_id, workspace_id, path, token, .. } => {
            app.fail_workspace_file(
                &WorkspaceFileTabKey {
                    node_id: node_id.clone(),
                    workspace_id: workspace_id.clone(),
                    path: path.clone(),
                },
                *token,
                detail_failure().display(),
                true,
            );
            return true;
        }
        AppAction::HarnessCreateNodeWorkspaceFile { node_id, workspace_id, path, token }
        | AppAction::HarnessCreateNodeWorkspaceDirectory { node_id, workspace_id, path, token } => {
            let kind = if matches!(action, AppAction::HarnessCreateNodeWorkspaceFile { .. }) {
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
                detail_failure().display(),
            );
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
            // `PreviewSessionRecord`/`RefreshSessionRecordHistory` both send
            // the exact same node request (`NodeRequest::PreviewSessionRecord`
            // -- see `c2_preview_session_record`/`c2_refresh_session_record_
            // history` above, which relay the same `NodeRequest::
            // PreviewSessionRecord`), so they ride this same harness-mode
            // read lane the same way the light-mode C2 path reuses one node
            // request for both.
            | AppAction::PreviewSessionRecord { .. }
            | AppAction::RefreshSessionRecordHistory { .. }
    )
}

/// The session-record mutation family: `ResumeSessionRecord`/
/// `RenameSessionRecord`/`SetSessionTask`/`ForgetSessionRecord`/
/// `IndexProviderSession`/`IndexNativeSession`. Unlike the eight direct
/// session-control verbs `route_harness_session_verb` rewrites into typed
/// `Harness*` actions (they need `SessionAddress` -> `HarnessRuntimeSessionAddressV1`
/// translation via `harness_session_address`), these six already carry a
/// bare `node_id: String` field -- no translation needed, only a routing
/// decision -- so they ride the harness-operator mutation lane
/// (`HARNESS_COMMAND_ROUTE`) unmodified, the same way `harness_native_
/// history_read_action`'s four verbs ride the history lane unmodified.
fn harness_session_record_mutation_action(action: &AppAction) -> bool {
    matches!(
        action,
        AppAction::ResumeSessionRecord { .. }
            | AppAction::RenameSessionRecord { .. }
            | AppAction::SetSessionTask { .. }
            | AppAction::ForgetSessionRecord { .. }
            | AppAction::IndexProviderSession { .. }
            | AppAction::IndexNativeSession { .. }
    )
}

/// The resource-mutation family: `BrowseHostDirectories`/`RegisterWorkspace`/
/// `UnregisterWorkspace`/`CreateStandaloneWorkspace`/`CreateWorktree`/
/// `RemoveWorktree`/`ForgetContextPack`. Same shape as `harness_session_
/// record_mutation_action`'s own six verbs: each of these seven already
/// carries a bare `node_id: String` field, no `SessionAddress` ->
/// `HarnessRuntimeSessionAddressV1` translation needed, so they ride the
/// harness-operator mutation lane (`HARNESS_COMMAND_ROUTE`) unmodified.
/// `ExportContextPack` is this family's eighth verb but is not listed here:
/// it needs that translation (its light shape carries a bare `SessionAddress`
/// with no `incarnation_id`), so `route_harness_session_verb` rewrites it
/// into `HarnessExportContextPack` instead -- already routed to
/// `HARNESS_COMMAND_ROUTE` via `action_node_id`'s `Harness*Session*` cluster,
/// the same way the eight session-control verbs are.
fn harness_resource_mutation_action(action: &AppAction) -> bool {
    matches!(
        action,
        AppAction::BrowseHostDirectories { .. }
            | AppAction::RegisterWorkspace { .. }
            | AppAction::UnregisterWorkspace { .. }
            | AppAction::CreateStandaloneWorkspace { .. }
            | AppAction::CreateWorktree { .. }
            | AppAction::RemoveWorktree { .. }
            | AppAction::ForgetContextPack { .. }
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
            // Writes/creates: same one-shot synchronous `HarnessOperatorClient`
            // worker route as the four node-workspace reads above, not a
            // "read" in the strict sense but the same dispatch shape.
            | AppAction::HarnessWriteNodeWorkspaceFile { .. }
            | AppAction::HarnessCreateNodeWorkspaceFile { .. }
            | AppAction::HarnessCreateNodeWorkspaceDirectory { .. }
    )
}

/// What remains legitimately unroutable in harness-only mode now that the
/// session-record read (`PreviewSessionRecord`/`RefreshSessionRecordHistory`),
/// mutation (`ResumeSessionRecord`/`RenameSessionRecord`/`SetSessionTask`/
/// `ForgetSessionRecord`/`IndexProviderSession`/`IndexNativeSession`), and
/// resource-mutation (`BrowseHostDirectories`/`RegisterWorkspace`/
/// `UnregisterWorkspace`/`CreateStandaloneWorkspace`/`CreateWorktree`/
/// `RemoveWorktree`/`ExportContextPack`/`ForgetContextPack`) families each
/// have a typed harness route: `DiscoverHistory`/`LoadHistory` and
/// `SpawnManagedWorktree`.
///
/// `DiscoverHistory`/`LoadHistory` operate against an already-open
/// `SessionAddress`'s own native-history discovery (feeding a plain
/// `Resume`, not a managed `SessionRecordId`) -- a direct-C2/light-mode-only
/// concept from before the managed `SessionRecord` family existed, with no
/// harness-operator wire mapping and no live per-node connection in harness
/// mode to relay it through.
///
/// `SpawnManagedWorktree` is genuinely dead too, not merely unimplemented:
/// its `NodeRequest::SpawnManagedWorktree`/`V2` payload is a full
/// `ManagedWorktreeSpawnRequest` (a `SpawnSpec` -- profile id/revision,
/// bundle/context/environment-profile overrides, deadline, idempotency key,
/// required capabilities -- plus a `WorktreeProfileId`), the same profile-
/// and-catalog-resolved shape `StartTaskV2` builds internally through the
/// task-execution-spec/launch-catalog pipeline. The harness operator wire's
/// own `SpawnSession` (the closest sibling this task considered folding it
/// into) is deliberately the opposite: raw provider/profile-string/mode/
/// terminal-size fields, no `SpawnSpec` involved at all -- see that request
/// variant's own doc comment ("no Task/Run/plan"). Building a typed harness
/// verb for `SpawnManagedWorktree` would mean re-exposing the entire
/// `SpawnSpec` construction surface on the operator wire, a materially
/// different and separate scope from this slice's workspace/worktree CRUD
/// and host-directory-browse/context-pack verbs.
///
/// Ignores the generic `reason` string the other three call sites pass
/// (busy/unavailable/queue-full framings would misdescribe a verb that can
/// never succeed in this mode regardless of queue state) in favor of naming
/// the actual cause.
fn reject_history_refresh_action(app: &mut App, action: &AppAction, reason: &str) -> bool {
    // A record-history refresh that cannot be delivered (queue busy/closed,
    // node unavailable) must still clear its pending marker with a
    // descriptive reason, or the Session Monitor spins forever.
    if let AppAction::RefreshSessionRecordHistory {
        node_id,
        node_incarnation_id,
        record_id,
        ..
    } = action
    {
        app.fail_session_record_history_refresh(
            node_id.clone(),
            record_id.clone(),
            *node_incarnation_id,
            reason.to_owned(),
        );
        return true;
    }
    if matches!(action, AppAction::SpawnManagedWorktree { .. }) {
        app.notice = Some(
            "Harness-owned session action unavailable: SpawnManagedWorktree has no typed \
             harness-operator verb -- it requires a full SpawnSpec construction (profile/\
             bundle/context resolution) this wire does not expose outside a Task"
                .to_owned(),
        );
        return true;
    }
    if !matches!(action, AppAction::DiscoverHistory { .. } | AppAction::LoadHistory { .. }) {
        return false;
    }
    app.notice = Some(
        "Harness-owned session action unavailable: native session-history discovery has no \
         harness-operator wire mapping (light-mode direct-C2 only)"
            .to_owned(),
    );
    true
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
        | AppAction::HarnessStopSession { .. }
        | AppAction::HarnessControlSession { .. }
        | AppAction::HarnessWriteSessionBytes { .. }
        | AppAction::HarnessPasteSession { .. }
        | AppAction::HarnessRemoveSession { .. }
        | AppAction::HarnessResumeSession { .. }
        | AppAction::HarnessExportContextPack { .. } => Some(HARNESS_COMMAND_ROUTE),
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
        | AppAction::HarnessReadNodeGitDiff { .. }
        | AppAction::HarnessWriteNodeWorkspaceFile { .. }
        | AppAction::HarnessCreateNodeWorkspaceFile { .. }
        | AppAction::HarnessCreateNodeWorkspaceDirectory { .. } => Some(HARNESS_DETAIL_COMMAND_ROUTE),
        AppAction::None | AppAction::Quit => None,
    }
}

/// What `flush_sixel_icon` wrote to the terminal on its last actual
/// emission, plus a one-shot override -- see `flush_sixel_icon`'s own
/// doc comment for exactly what this gates.
#[derive(Default)]
struct SixelEmitState {
    last: Vec<SixelIconPlacement>,
    /// Set by the run loop's own `TerminalEvent::Resize` handling
    /// whenever `Screen::resize` is about to actually redraw (its own
    /// debounced size-changed condition, mirrored there) -- consumed
    /// (reset to `false`) by the very next `flush_sixel_icon` call
    /// regardless of whether the signature also changed.
    force_next: bool,
}

/// Writes every activity-rail button's own baked sixel icon
/// (`app.layout.sixel_icons`, populated by `render::render_rail_button`
/// only in `RailIcons::Sixel` mode) directly to the terminal, each at its
/// own absolute cell coordinates -- a plain terminal cell cannot hold a
/// raster image, so `render::render` (already run for this frame, via
/// `screen.buffer_mut()`) only leaves each such button's own body blank
/// in the cell buffer; this is the step that actually draws pixels
/// there. Called every frame right after `screen.flush()` writes the
/// diffed cell buffer to the SAME `stdout()` writer this uses (a fresh
/// handle each call, same pattern as `sync_cursor` below -- every
/// `Stdout` handle shares one underlying, already-flushed buffer, so
/// there is no interleaving risk running strictly after `screen.flush()`
/// returns in this single-threaded run loop). Saves/restores the real
/// cursor around each write since a terminal's own post-sixel cursor
/// placement is inconsistent across implementations (DEC spec leaves it
/// underspecified) -- `sync_cursor` re-authoritatively repositions it
/// right after this returns anyway.
///
/// GATED: sixel encoding itself is already cached for the process
/// lifetime (`icons::sixel`'s own `LazyLock`s), but writing already-
/// encoded bytes to the terminal every single frame still costs a
/// syscall plus the terminal's own decode/rasterize work on the far end,
/// for a rail that visually changes on a small minority of frames. `state`
/// tracks the placements actually written last time (icon identity,
/// absolute rect, and selection -- see `SixelIconPlacement`'s own doc
/// comment for why `selected` has to be part of this) and this function
/// is a no-op whenever `app.layout.sixel_icons` is unchanged from that
/// AND no resize forced a redraw since. An empty `sixel_icons` (`Braille`/
/// `Ascii` mode, or a terminal too short for the tall rail) still updates
/// `state.last` to empty and simply writes nothing -- correct either way,
/// since there is nothing this function itself needs to erase (the
/// cell-buffer diff already repainted those cells as plain background
/// text, which is what actually erases stale sixel pixels on a real
/// terminal).
fn flush_sixel_icon(app: &App, state: &mut SixelEmitState) -> io::Result<()> {
    flush_sixel_icon_into(&mut stdout(), app, state)
}

/// The actual gating + write logic behind [`flush_sixel_icon`], generic
/// over the writer purely so this module's own tests can assert on the
/// real bytes (a `Vec<u8>` sink) without a live terminal attached -- the
/// production call site above always plugs in the real `stdout()`.
fn flush_sixel_icon_into<W: io::Write>(writer: &mut W, app: &App, state: &mut SixelEmitState) -> io::Result<()> {
    let unchanged = !state.force_next && app.layout.sixel_icons == state.last;
    state.force_next = false;
    if unchanged {
        return Ok(());
    }
    state.last = app.layout.sixel_icons.clone();
    for placement in &app.layout.sixel_icons {
        if placement.rect.width == 0 || placement.rect.height == 0 {
            continue;
        }
        execute!(
            writer,
            SavePosition,
            MoveTo(placement.rect.x, placement.rect.y),
            Print(icons::sixel(placement.icon)),
            RestorePosition,
        )?;
    }
    Ok(())
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
            AppAction::HarnessControlSession { session, control } => {
                if let Err(error) = client.control_session(session, control) {
                    let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                        "harness session control failed: {error}"
                    )));
                }
            }
            AppAction::HarnessWriteSessionBytes { session, bytes } => {
                if let Err(error) = client.write_session_bytes(session, bytes) {
                    let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                        "harness session byte send failed: {error}"
                    )));
                }
            }
            AppAction::HarnessPasteSession { session, text } => {
                if let Err(error) = client.paste_session(session, text) {
                    let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                        "harness session paste failed: {error}"
                    )));
                }
            }
            AppAction::HarnessRemoveSession { token, session } => {
                match client.remove_session(session) {
                    // `require_inventory_change: true` mirrors
                    // `HarnessStopSession` above: the mutation just
                    // happened, so a retry-until-changed snapshot is worth
                    // the extra round trips (the runtime inventory should
                    // no longer include the removed session).
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
            AppAction::HarnessResumeSession { token, session, rows, cols } => {
                match client.resume_session(
                    session,
                    HarnessRuntimeTerminalSizeV1 { rows, columns: cols },
                ) {
                    // Same reasoning as `HarnessRemoveSession` above: the
                    // resumed session's status change (and bumped
                    // generation) is only visible through a fresh runtime-
                    // inventory read, never through this reply -- see
                    // `HarnessOperatorClient::resume_session`'s doc comment.
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
            // The six session-record mutation verbs below never touch the
            // harness kanban refresh-token gate (`publish_harness_snapshot`/
            // `publish_harness_failure`): unlike the task-CAS/session-control
            // mutations above, none of them originate from
            // `route_harness_session_verb`'s `begin_harness_mutation_refresh`
            // call, so there is no pending-refresh token to roll back on
            // failure. Runtime-inventory convergence for a successful
            // mutation happens server-side instead (`invalidate_runtime_
            // inventory_for_route` in `HostCommand::SessionRecordMutationFinished`,
            // gate4agent-harness-service/runtime.rs), which pushes an event
            // this session's own `harness_event_subscription_worker`
            // receives -- no client-side follow-up call needed here.
            AppAction::ResumeSessionRecord {
                node_id, record_id, rows, cols, initial_prompt, operation_token,
            } => {
                let update = match client.resume_session_record(
                    node_id.clone(),
                    record_id.clone(),
                    HarnessRuntimeTerminalSizeV1 { rows, columns: cols },
                    initial_prompt,
                ) {
                    Ok(resumed) => match project_harness_inventory_managed_session(&node_id, resumed.record) {
                        Ok(record) => WorkerUpdate::SessionRecordResumed {
                            record,
                            session: SessionAddress {
                                node_id: resumed.session.node_id,
                                workspace_id: resumed.session.workspace_id,
                                instance_id: resumed.session.instance_id,
                                generation: resumed.session.generation,
                            },
                            operation_token,
                        },
                        Err(message) => WorkerUpdate::ExistingSessionOperationFailed {
                            node_id, record_id: Some(record_id), indexing: false, operation_token,
                            message, stale_catalog: false,
                        },
                    },
                    Err(error) => WorkerUpdate::ExistingSessionOperationFailed {
                        node_id, record_id: Some(record_id), indexing: false, operation_token,
                        message: error.to_string(), stale_catalog: false,
                    },
                };
                let _ = updates.blocking_send(update);
            }
            AppAction::RenameSessionRecord { node_id, record_id, display_name } => {
                match client.rename_session_record(node_id.clone(), record_id, display_name) {
                    Ok(record) => match project_harness_inventory_managed_session(&node_id, record) {
                        Ok(record) => {
                            let _ = updates.blocking_send(WorkerUpdate::SessionRecordUpserted(record));
                        }
                        Err(message) => {
                            let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                        }
                    },
                    Err(error) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {error}")));
                    }
                }
            }
            AppAction::SetSessionTask { node_id, record_id, expected_revision, target } => {
                match client.set_session_task(
                    node_id.clone(),
                    record_id,
                    expected_revision,
                    harness_session_task_target(&target),
                ) {
                    Ok(record) => match project_harness_inventory_managed_session(&node_id, record) {
                        Ok(record) => {
                            let _ = updates.blocking_send(WorkerUpdate::SessionRecordUpserted(record));
                        }
                        Err(message) => {
                            let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                        }
                    },
                    Err(error) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {error}")));
                    }
                }
            }
            AppAction::ForgetSessionRecord { node_id, record_id } => {
                match client.forget_session_record(node_id.clone(), record_id) {
                    Ok(record_id) => {
                        let _ = updates.blocking_send(WorkerUpdate::SessionRecordRemoved { node_id, record_id });
                    }
                    Err(error) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {error}")));
                    }
                }
            }
            AppAction::IndexProviderSession {
                node_id, workspace_id, provider, identity, display_name, operation_token,
            } => {
                let source_session_id = identity.id.clone();
                let update = match client.index_provider_session(
                    node_id.clone(),
                    workspace_id.clone(),
                    provider.as_str().to_owned(),
                    harness_provider_session_identity(&identity),
                    display_name,
                ) {
                    Ok(record) => {
                        let identity_matches = record.provider_identity_present;
                        match project_harness_inventory_managed_session(&node_id, record) {
                            Ok(record) => WorkerUpdate::ProviderSessionIndexed {
                                record, identity_matches, node_id,
                                workspace_id, provider, session_id: source_session_id, operation_token,
                            },
                            Err(message) => WorkerUpdate::ExistingSessionOperationFailed {
                                node_id, record_id: None, indexing: true, operation_token,
                                message, stale_catalog: false,
                            },
                        }
                    }
                    Err(error) => WorkerUpdate::ExistingSessionOperationFailed {
                        node_id, record_id: None, indexing: true, operation_token,
                        message: error.to_string(), stale_catalog: false,
                    },
                };
                let _ = updates.blocking_send(update);
            }
            AppAction::IndexNativeSession {
                node_id, route, catalog_revision, recent_cutoff_unix_ms, selection_id,
                display_name, operation_token,
            } => {
                let nodes = harness_runtime_inventory_snapshot(&runtime_inventory);
                let update = (|| {
                    let selection = HarnessNativeSessionSelectionV1 {
                        route: harness_native_session_route(nodes.as_deref(), &node_id, &route)
                            .map_err(|message| WorkerUpdate::ExistingSessionOperationFailed {
                                node_id: node_id.clone(), record_id: None, indexing: true, operation_token,
                                message, stale_catalog: false,
                            })?,
                        catalog_revision,
                        recent_cutoff_unix_ms,
                        selection_id: selection_id.clone(),
                    };
                    let indexed = client.index_native_session(selection.clone(), display_name)
                        .map_err(|error| WorkerUpdate::ExistingSessionOperationFailed {
                            node_id: node_id.clone(), record_id: None, indexing: true, operation_token,
                            message: error.to_string(), stale_catalog: false,
                        })?;
                    if indexed.selection != selection {
                        return Err(WorkerUpdate::ExistingSessionOperationFailed {
                            node_id: node_id.clone(), record_id: None, indexing: true, operation_token,
                            message: "Harness operator returned a mismatched native session selection".to_owned(),
                            stale_catalog: false,
                        });
                    }
                    let record = project_harness_inventory_managed_session(&node_id, indexed.record)
                        .map_err(|message| WorkerUpdate::ExistingSessionOperationFailed {
                            node_id: node_id.clone(), record_id: None, indexing: true, operation_token,
                            message, stale_catalog: false,
                        })?;
                    Ok(WorkerUpdate::NativeSessionIndexed {
                        node_id, route, catalog_revision, recent_cutoff_unix_ms, selection_id,
                        record, operation_token,
                    })
                })();
                let _ = updates.blocking_send(update.unwrap_or_else(|failure| failure));
            }
            // The resource-mutation family below (host-directory browse plus
            // workspace/worktree lifecycle and context-pack export/forget)
            // shares the session-record family's own shape immediately
            // above: no `publish_harness_snapshot`/`publish_harness_failure`
            // refresh-token gate (none of these eight originate from
            // `route_harness_session_verb`'s `begin_harness_mutation_refresh`
            // call -- `HarnessExportContextPack` is the one rewritten action
            // here, and it carries no token either, see its own doc comment
            // in `app.rs`). Results land through the exact same mode-
            // agnostic `WorkerUpdate`/`App::apply_*` pairs the light-mode C2
            // response handler above (`WorkerUpdate::HostDirectoriesBrowsed`
            // -> `app.apply_host_directories`, `publish_workspace_registered`/
            // `publish_workspace_removed`, `WorkerUpdate::ContextExported`/
            // `ContextForgotten`) already fills -- never a Harness-only
            // parallel state.
            AppAction::BrowseHostDirectories { node_id, directory, after, token, append } => {
                let update = (|| -> Result<WorkerUpdate, WorkerUpdate> {
                    let harness_directory = directory.map(harness_host_path)
                        .transpose()
                        .map_err(|message| WorkerUpdate::HostDirectoryBrowseFailed {
                            node_id: node_id.clone(), token, message,
                        })?;
                    let harness_after = after.map(harness_host_path)
                        .transpose()
                        .map_err(|message| WorkerUpdate::HostDirectoryBrowseFailed {
                            node_id: node_id.clone(), token, message,
                        })?;
                    let listing = client.browse_host_directories(
                        node_id.clone(), harness_directory, harness_after,
                    ).map_err(|error| WorkerUpdate::HostDirectoryBrowseFailed {
                        node_id: node_id.clone(), token, message: error.to_string(),
                    })?;
                    let listing = project_harness_host_directory_listing(listing)
                        .map_err(|message| WorkerUpdate::HostDirectoryBrowseFailed {
                            node_id: node_id.clone(), token, message,
                        })?;
                    Ok(WorkerUpdate::HostDirectoriesBrowsed { node_id, token, append, listing })
                })();
                let _ = updates.blocking_send(update.unwrap_or_else(|failure| failure));
            }
            AppAction::RegisterWorkspace { node_id, workspace_id, root } => {
                let update = (|| -> Result<WorkspaceSnapshotUpdate, String> {
                    let root = harness_host_path(root)?;
                    let snapshot = client.register_workspace(
                        node_id.clone(), workspace_id, root,
                    ).map_err(|error| error.to_string())?;
                    project_harness_workspace_snapshot(snapshot)
                })();
                match update {
                    Ok(update) => publish_harness_workspace_registered(&updates, &node_id, update),
                    Err(message) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                    }
                }
            }
            AppAction::UnregisterWorkspace { node_id, workspace_id } => {
                match client.unregister_workspace(node_id.clone(), workspace_id) {
                    Ok(workspace_id) => {
                        let _ = updates.blocking_send(WorkerUpdate::WorkspaceRemoved {
                            node_id: node_id.clone(), workspace_id: workspace_id.clone(),
                        });
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                            "{node_id}: space {workspace_id} unregistered"
                        )));
                    }
                    Err(error) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {error}")));
                    }
                }
            }
            AppAction::CreateStandaloneWorkspace { node_id, workspace_id, root, initial_branch } => {
                let update = (|| -> Result<WorkspaceSnapshotUpdate, String> {
                    let root = harness_host_path(root)?;
                    let snapshot = client.create_standalone_workspace(
                        node_id.clone(), workspace_id, root, initial_branch,
                    ).map_err(|error| error.to_string())?;
                    project_harness_workspace_snapshot(snapshot)
                })();
                match update {
                    Ok(update) => {
                        publish_harness_workspace_registered(&updates, &node_id, update);
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                            "{node_id}: standalone repository created and ready to Launch"
                        )));
                    }
                    Err(message) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                    }
                }
            }
            // Unlike `RegisterWorkspace`/`CreateStandaloneWorkspace` above,
            // this does not upsert the new worktree's own workspace into
            // `app.workspaces` -- it just selects it, the same way the
            // light-mode `C2NodeResponse::WorktreeCreated` handler does
            // (`SelectWorkspace` only, no `WorkspaceUpserted`); the sidebar's
            // own auto-inspection picks the freshly selected workspace up.
            AppAction::CreateWorktree {
                node_id, source_workspace_id, workspace_id, target_root, branch, base,
            } => {
                let update = (|| -> Result<(HarnessGitWorktreeSnapshotV1, HarnessWorkspaceSnapshotV1), String> {
                    let target_root = harness_host_path(target_root)?;
                    client.create_worktree(
                        node_id.clone(), source_workspace_id, workspace_id, target_root, branch, base,
                    ).map_err(|error| error.to_string())
                })();
                match update {
                    Ok((worktree, workspace)) => {
                        let _ = updates.blocking_send(WorkerUpdate::SelectWorkspace {
                            node_id: node_id.clone(), workspace_id: workspace.workspace_id,
                        });
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                            "{node_id}: worktree {} created", worktree.path.as_str(),
                        )));
                    }
                    Err(message) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                    }
                }
            }
            AppAction::RemoveWorktree { node_id, source_workspace_id, target_root } => {
                let update = (|| -> Result<String, String> {
                    let target_root = harness_host_path(target_root)?;
                    let (removed_path, _workspace_id) = client.remove_worktree(
                        node_id.clone(), source_workspace_id, target_root,
                    ).map_err(|error| error.to_string())?;
                    Ok(removed_path)
                })();
                match update {
                    Ok(removed_path) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!(
                            "{node_id}: worktree {removed_path} removed"
                        )));
                    }
                    Err(message) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                    }
                }
            }
            AppAction::HarnessExportContextPack { session } => {
                let node_id = session.node_id.clone();
                let update = client.export_context_pack(session)
                    .map_err(|error| error.to_string())
                    .and_then(project_harness_context_pack_receipt);
                match update {
                    Ok(receipt) => {
                        let _ = updates.blocking_send(WorkerUpdate::ContextExported(receipt));
                    }
                    Err(message) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {message}")));
                    }
                }
            }
            AppAction::ForgetContextPack { node_id, context_id } => {
                match client.forget_context_pack(node_id.clone(), context_id) {
                    Ok(context_id) => match SpawnContextId::new(context_id) {
                        Ok(context_id) => {
                            let _ = updates.blocking_send(WorkerUpdate::ContextForgotten(context_id));
                        }
                        Err(error) => {
                            let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {error}")));
                        }
                    },
                    Err(error) => {
                        let _ = updates.blocking_send(WorkerUpdate::Notice(format!("{node_id}: {error}")));
                    }
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
            AppAction::PreviewSessionRecord { node_id, record_id, message_limit, token } => {
                publish_harness_session_record_preview(
                    &client, &updates, node_id, record_id, message_limit, token,
                )
            }
            AppAction::RefreshSessionRecordHistory {
                node_id, node_incarnation_id, record_id, message_limit,
            } => publish_harness_session_record_history_refresh(
                &client, &updates, nodes.as_deref(), node_id, node_incarnation_id, record_id,
                message_limit,
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
            AppAction::HarnessWriteNodeWorkspaceFile {
                node_id,
                workspace_id,
                path,
                expected_revision,
                text,
                token,
            } => {
                let result = (|| {
                    let wire_path = project_harness_repository_path(&path)?;
                    let wire_revision = HarnessWorkspaceFileRevisionV1::new(expected_revision)
                        .map_err(HarnessOperatorClientError::Api)?;
                    client.write_node_workspace_file(
                        node_id.clone(),
                        workspace_id.clone(),
                        wire_path,
                        text,
                        wire_revision,
                    )
                })();
                let update = match result {
                    Ok(file) => WorkerUpdate::HarnessNodeWorkspaceFileWritten { node_id, token, file },
                    Err(error) => WorkerUpdate::HarnessNodeWorkspaceFileWriteFailed {
                        key: WorkspaceFileTabKey { node_id, workspace_id, path },
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessCreateNodeWorkspaceFile { node_id, workspace_id, path, token } => {
                let result = project_harness_repository_path(&path)
                    .and_then(|wire_path| {
                        client.create_node_workspace_file(
                            node_id.clone(),
                            workspace_id.clone(),
                            wire_path,
                        )
                    });
                let update = match result {
                    Ok(file) => WorkerUpdate::HarnessNodeWorkspaceFileCreated { node_id, token, file },
                    Err(error) => WorkerUpdate::HarnessNodeWorkspaceEntryCreateFailed {
                        node_id,
                        workspace_id,
                        path,
                        kind: WorkspaceEntryKind::File,
                        token,
                        message: error.to_string(),
                    },
                };
                if updates.blocking_send(update).is_err() {
                    return;
                }
            }
            AppAction::HarnessCreateNodeWorkspaceDirectory { node_id, workspace_id, path, token } => {
                let result = project_harness_repository_path(&path)
                    .and_then(|wire_path| {
                        client.create_node_workspace_directory(
                            node_id.clone(),
                            workspace_id.clone(),
                            wire_path,
                        )
                    });
                let update = match result {
                    Ok(directory) => {
                        WorkerUpdate::HarnessNodeWorkspaceDirectoryCreated { token, directory }
                    }
                    Err(error) => WorkerUpdate::HarnessNodeWorkspaceEntryCreateFailed {
                        node_id,
                        workspace_id,
                        path,
                        kind: WorkspaceEntryKind::Directory,
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
            // Added alongside `gate4agent-harness-light` (see
            // `HarnessOperatorHostErrorV1::Unsupported`'s own doc comment):
            // the full harness this TUI's harness mode talks to never
            // returns it, but the match must stay exhaustive.
            HarnessOperatorHostErrorV1::Unsupported => "unsupported",
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

/// Node-scoped sibling projecting straight to `WorkspaceEntry` for
/// `apply_workspace_directory_created`, the same direct-mode apply function
/// the direct-C2 `CreateWorkspaceDirectory` reply uses. Unlike
/// `project_harness_node_workspace_file`, this is infallible: `origin.
/// workspace_id` stays a plain `String` on `apply_workspace_directory_
/// created`'s own signature (it never needs a typed `WorkspaceId`), and
/// `project_harness_path` already is.
fn project_harness_node_workspace_directory(
    directory: HarnessNodeWorkspaceDirectoryV1,
) -> (String, String, gate4agent_node_protocol::WorkspaceEntry) {
    let node_id = directory.origin.node_id;
    let workspace_id = directory.origin.workspace_id;
    let entry = gate4agent_node_protocol::WorkspaceEntry {
        relative_path: project_harness_path(directory.entry.relative_path),
        kind: match directory.entry.kind {
            HarnessWorkspaceEntryKindV1::File => WorkspaceEntryKind::File,
            HarnessWorkspaceEntryKindV1::Directory => WorkspaceEntryKind::Directory,
        },
    };
    (node_id, workspace_id, entry)
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

/// Harness-mode sibling of `c2_preview_session_record`: same node request
/// (`NodeRequest::PreviewSessionRecord`, relayed via `HarnessOperatorRequestV1::
/// PreviewSessionRecord`), same success/failure `WorkerUpdate` shapes, so
/// `apply_worker_update` applies either mode's result through the identical
/// `App::apply_session_record_preview`/`fail_session_record_preview` pair.
fn publish_harness_session_record_preview(
    client: &HarnessOperatorClient,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: String,
    record_id: String,
    message_limit: u16,
    token: u64,
) {
    let update = match client.preview_session_record(node_id.clone(), record_id.clone(), message_limit) {
        Ok(previewed) if previewed.record_id == record_id => WorkerUpdate::SessionRecordPreviewed {
            node_id,
            record_id,
            token,
            preview: project_harness_native_preview(previewed.preview),
        },
        Ok(_) => WorkerUpdate::SessionRecordPreviewFailed {
            node_id, record_id, token,
            message: "Harness operator returned a mismatched session record preview".to_owned(),
            unavailable: false,
        },
        Err(error) => {
            let error = HarnessNativeHistoryError::from_client(&error);
            WorkerUpdate::SessionRecordPreviewFailed {
                node_id, record_id, token, message: error.message, unavailable: error.unavailable,
            }
        }
    };
    let _ = updates.blocking_send(update);
}

/// Harness-mode sibling of `c2_refresh_session_record_history`: same
/// `history_refresh_incarnation_matches` pre-flight guard the light TUI's
/// per-node C2 connection performs against its own `route.expected_incarnation_id`
/// -- here against the harness runtime-inventory snapshot's cached
/// incarnation for the node, since a harness operator connection is
/// stateless per call rather than pinned to one node's connection.
fn publish_harness_session_record_history_refresh(
    client: &HarnessOperatorClient,
    updates: &mpsc::Sender<WorkerUpdate>,
    nodes: Option<&[NodeView]>,
    node_id: String,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    record_id: String,
    message_limit: u16,
) {
    let current_incarnation = nodes.and_then(|nodes| {
        nodes.iter().find(|node| node.node_id == node_id).and_then(|node| node.incarnation_id)
    });
    if current_incarnation != Some(incarnation_id) {
        let _ = updates.blocking_send(WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id, record_id, incarnation_id,
            message: "node incarnation changed".to_owned(),
        });
        return;
    }
    let update = match client.preview_session_record(node_id.clone(), record_id.clone(), message_limit) {
        Ok(previewed) if previewed.record_id == record_id => {
            WorkerUpdate::SessionRecordHistoryRefreshed { node_id, record_id, incarnation_id }
        }
        Ok(_) => WorkerUpdate::SessionRecordHistoryRefreshFailed {
            node_id, record_id, incarnation_id,
            message: "Harness operator returned a mismatched session record preview".to_owned(),
        },
        Err(error) => {
            let error = HarnessNativeHistoryError::from_client(&error);
            WorkerUpdate::SessionRecordHistoryRefreshFailed {
                node_id, record_id, incarnation_id, message: error.message,
            }
        }
    };
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


fn apply_update(app: &mut App, terminal: &mut TerminalWatermarks, update: WorkerUpdate) -> AppAction {
    let mut follow_up = AppAction::None;
    match update {
        WorkerUpdate::SelectWorkspace { node_id, workspace_id } => {
            app.request_space_selection(node_id, workspace_id)
        }
        WorkerUpdate::WorkspaceUpserted { node_id, workspace } => {
            upsert_workspace(app, &node_id, workspace)
        }
        WorkerUpdate::WorkspaceRemoved { node_id, workspace_id } => {
            remove_workspace(app, &node_id, &workspace_id)
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
            app.upsert_managed_session(record);
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
        }
        WorkerUpdate::ContextExported(receipt) => app.apply_context_exported(receipt),
        WorkerUpdate::ContextForgotten(id) => app.apply_context_forgotten(id),
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
        WorkerUpdate::HarnessEventSnapshotBaseline { tasks, runs, nodes } => {
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
            app.apply_harness_snapshot_pushed(tasks, runs);
            follow_up = app.ensure_initial_agents_catalog();
        }
        WorkerUpdate::HarnessTaskChanged(task) => {
            app.apply_harness_task_changed(task);
        }
        WorkerUpdate::HarnessRunChanged(run) => {
            app.apply_harness_run_changed(run);
        }
        WorkerUpdate::HarnessRuntimeInventoryNodeChanged(mut node) => {
            app.preserve_known_session_terminal_state(&mut node);
            app.upsert_node(node);
        }
        WorkerUpdate::HarnessRuntimeInventoryNodeRemoved { node_id } => {
            app.remove_topology_node(&node_id);
        }
        WorkerUpdate::HarnessEventLagged => {}
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
        WorkerUpdate::HarnessNodeWorkspaceFileWritten { node_id: expected_node_id, token, file } => {
            match project_harness_node_workspace_file(file) {
                Ok((node_id, file)) => app.apply_workspace_file_written(node_id, token, file),
                Err(message) => app.notice = Some(format!("{expected_node_id}: {message}")),
            }
        }
        WorkerUpdate::HarnessNodeWorkspaceFileWriteFailed { key, token, message } => {
            app.fail_workspace_file(&key, token, message, true);
        }
        WorkerUpdate::HarnessNodeWorkspaceFileCreated { node_id: expected_node_id, token, file } => {
            match project_harness_node_workspace_file(file) {
                Ok((node_id, file)) => {
                    follow_up = app.apply_workspace_file_created(node_id, token, file);
                }
                Err(message) => app.notice = Some(format!("{expected_node_id}: {message}")),
            }
        }
        WorkerUpdate::HarnessNodeWorkspaceDirectoryCreated { token, directory } => {
            let (node_id, workspace_id, entry) = project_harness_node_workspace_directory(directory);
            follow_up = app.apply_workspace_directory_created(node_id, workspace_id, token, entry);
        }
        WorkerUpdate::HarnessNodeWorkspaceEntryCreateFailed {
            node_id,
            workspace_id,
            path,
            kind,
            token,
            message,
        } => {
            app.fail_workspace_entry_create(node_id, workspace_id, path, kind, token, message);
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
                    if terminal.terminal_frame_is_new(&address, frame_sequence)
                        && app.apply_terminal_frame(&address, incarnation_id, frame)
                    {
                        terminal.record_terminal_frame(address.clone(), frame_sequence);
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

/// Mirrors `gate4agent_node_protocol::SessionTaskTargetV1` into its wire
/// twin, the same duplication `map_terminal_control` (app.rs) uses for
/// `TerminalControl` and for the same reason -- see
/// `HarnessSessionTaskTargetV1`'s own doc comment.
fn harness_session_task_target(
    target: &gate4agent_node_protocol::SessionTaskTargetV1,
) -> HarnessSessionTaskTargetV1 {
    match target {
        gate4agent_node_protocol::SessionTaskTargetV1::New => HarnessSessionTaskTargetV1::New,
        gate4agent_node_protocol::SessionTaskTargetV1::Existing { task_id } => {
            HarnessSessionTaskTargetV1::Existing { task_id: task_id.to_string() }
        }
        gate4agent_node_protocol::SessionTaskTargetV1::Clear => HarnessSessionTaskTargetV1::Clear,
    }
}

fn harness_provider_session_identity(
    identity: &ProviderSessionIdentity,
) -> HarnessProviderSessionIdentityV1 {
    HarnessProviderSessionIdentityV1 {
        key: match identity.key {
            gate4agent_types::ProviderSessionKey::SessionId => HarnessProviderSessionKeyV1::SessionId,
            gate4agent_types::ProviderSessionKey::ConversationId => {
                HarnessProviderSessionKeyV1::ConversationId
            }
        },
        id: identity.id.clone(),
        transcript_path: identity.transcript_path.clone(),
    }
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

fn upsert_workspace(app: &mut App, node_id: &str, workspace: WorkspaceSnapshotUpdate) {
    let Some(mut node) = app.nodes.iter().find(|node| node.node_id == node_id).cloned() else {
        return;
    };
    let WorkspaceSnapshotUpdate::C2(workspace_ref) = &workspace;
    let workspace_id = workspace_ref.workspace_id.to_string();
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
    let WorkspaceSnapshotUpdate::C2(workspace) = workspace;
    let workspace = {
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

/// Subscription-reader thread: opens `SubscribeEvents` on its own dedicated
/// connection (separate from `harness_operator_worker`'s poll connection)
/// and forwards every pushed frame into `updates` as a `WorkerUpdate`,
/// reconnecting with capped exponential backoff
/// (`HARNESS_SUBSCRIPTION_BACKOFF_INITIAL`..`HARNESS_SUBSCRIPTION_BACKOFF_MAX`)
/// whenever the connection ends. `active` flips true the moment a
/// subscription goes live -- gating the run loop's
/// `HARNESS_SNAPSHOT_REFRESH_INTERVAL` poll into a fallback, see its call
/// site -- and false the moment the subscription ends, so the poll resumes
/// as a fallback during any reconnect gap.
fn harness_event_subscription_worker(
    client: HarnessOperatorClient,
    updates: mpsc::Sender<WorkerUpdate>,
    active: Arc<AtomicBool>,
) {
    let mut backoff = HARNESS_SUBSCRIPTION_BACKOFF_INITIAL;
    loop {
        let mut subscription = match client.subscribe_events() {
            Ok(subscription) => subscription,
            Err(_) => {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(HARNESS_SUBSCRIPTION_BACKOFF_MAX);
                continue;
            }
        };
        active.store(true, Ordering::Relaxed);
        backoff = HARNESS_SUBSCRIPTION_BACKOFF_INITIAL;
        loop {
            let event = match subscription.next_event() {
                Ok(event) => event,
                Err(_) => break,
            };
            let Some(update) = project_harness_operator_event(event) else { continue; };
            if updates.blocking_send(update).is_err() {
                active.store(false, Ordering::Relaxed);
                return;
            }
        }
        active.store(false, Ordering::Relaxed);
        thread::sleep(backoff);
        backoff = (backoff * 2).min(HARNESS_SUBSCRIPTION_BACKOFF_MAX);
    }
}

/// Projects one pushed `HarnessOperatorEventV1` into its `WorkerUpdate`,
/// mirroring the wire<->app-view split `publish_harness_snapshot`/
/// `project_harness_inventory_node` already keep for the poll path. `None`
/// when the event carries nothing the app needs to apply: a runtime
/// inventory node this build's `project_harness_inventory_node` cannot
/// project is dropped silently, the same tolerance the poll path already
/// has for a stale/partial view over a broken one (`load_harness_runtime_
/// inventory`'s own per-node projection failure only fails that one page,
/// never crashes the worker).
fn project_harness_operator_event(event: HarnessOperatorEventV1) -> Option<WorkerUpdate> {
    match event {
        HarnessOperatorEventV1::SnapshotBaseline { tasks, runs, nodes, .. } => {
            let nodes = nodes.into_iter()
                .filter_map(|node| project_harness_inventory_node(node).ok())
                .collect();
            Some(WorkerUpdate::HarnessEventSnapshotBaseline { tasks, runs, nodes })
        }
        HarnessOperatorEventV1::TaskChanged { task, .. } => {
            Some(WorkerUpdate::HarnessTaskChanged(task))
        }
        HarnessOperatorEventV1::RunChanged { run, .. } => {
            Some(WorkerUpdate::HarnessRunChanged(run))
        }
        HarnessOperatorEventV1::RuntimeInventoryChanged { node, .. } => {
            project_harness_inventory_node(node).ok()
                .map(WorkerUpdate::HarnessRuntimeInventoryNodeChanged)
        }
        HarnessOperatorEventV1::RuntimeInventoryRemoved { node_id, .. } => {
            Some(WorkerUpdate::HarnessRuntimeInventoryNodeRemoved { node_id })
        }
        HarnessOperatorEventV1::Lagged { .. } => Some(WorkerUpdate::HarnessEventLagged),
    }
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

/// Narrows a light-shaped `OpaqueHostPath` (Windows/Unix filesystem path --
/// may carry the non-UTF-8 `unix-bytes` representation) into the harness
/// operator wire's `HarnessHostPathV1` (UTF-8 only), the same narrowing
/// `gate4agent-harness-service`'s `repository_path_from_api` already applies
/// to repository-relative paths on this same wire.
fn harness_host_path(path: OpaqueHostPath) -> Result<HarnessHostPathV1, String> {
    let text = path.as_utf8()
        .ok_or_else(|| "host path is not representable as UTF-8".to_owned())?;
    HarnessHostPathV1::new(text).map_err(|error| error.to_string())
}

/// Reverse of `harness_host_path`: every harness host path is already
/// UTF-8-bounded, so this never fails the way `OpaqueHostPath::utf8` could
/// for an arbitrary caller-supplied string -- kept fallible only to mirror
/// `OpaqueHostPath::utf8`'s own signature at the one call site
/// (`project_harness_host_directory_listing`) that walks a whole page of
/// them.
fn opaque_host_path_from_harness(path: HarnessHostPathV1) -> Result<OpaqueHostPath, String> {
    OpaqueHostPath::utf8(path.as_str().to_owned()).map_err(|error| error.to_string())
}

/// `BrowseHostDirectories`'s reply, projected back into the light-shaped
/// `HostDirectoryListing` `App::apply_host_directories` already consumes --
/// see the doc comment on the resource-mutation match arms above for why
/// this reuses that mode-agnostic apply path rather than a Harness-only one.
fn project_harness_host_directory_listing(
    listing: HarnessHostDirectoryListingV1,
) -> Result<HostDirectoryListing, String> {
    Ok(HostDirectoryListing {
        directory: listing.directory.map(opaque_host_path_from_harness).transpose()?,
        parent: listing.parent.map(opaque_host_path_from_harness).transpose()?,
        entries: listing.entries.into_iter().map(|entry| Ok(HostDirectoryEntry {
            path: opaque_host_path_from_harness(entry.path)?,
            display_name: entry.display_name,
            is_link: entry.is_link,
        })).collect::<Result<Vec<_>, String>>()?,
        next_after: listing.next_after.map(opaque_host_path_from_harness).transpose()?,
        incomplete: listing.incomplete,
    })
}

/// `RegisterWorkspace`/`CreateStandaloneWorkspace`'s reply, projected into
/// `WorkspaceSnapshotUpdate::C2` -- the same privacy-thinned shape the
/// light-mode C2 relay already carries this data in (see that enum's own
/// doc comment). `sessions`/`managed_worktree_profiles` are always empty/
/// `None`: the harness wire's `HarnessWorkspaceSnapshotV1` deliberately drops
/// both (see that type's own doc comment in `gate4agent-harness-api`), and
/// the roster-affecting mutation that produced this snapshot already
/// invalidated the runtime inventory server-side, so the sidebar converges
/// on the real session list through that route instead.
fn project_harness_workspace_snapshot(
    snapshot: HarnessWorkspaceSnapshotV1,
) -> Result<WorkspaceSnapshotUpdate, String> {
    Ok(WorkspaceSnapshotUpdate::C2(C2WorkspaceSnapshot {
        workspace_id: WorkspaceId::new(snapshot.workspace_id)
            .map_err(|error| error.to_string())?,
        canonical_root: opaque_host_path_from_harness(snapshot.canonical_root)?,
        sessions: Vec::new(),
        worktree_service_mode: snapshot.worktree_service_mode.map(|mode| match mode {
            HarnessWorktreeServiceModeV1::Manual => WorktreeServiceMode::Manual,
            HarnessWorktreeServiceModeV1::Managed => WorktreeServiceMode::Managed,
            HarnessWorktreeServiceModeV1::Off => WorktreeServiceMode::Off,
        }),
        managed_worktree_profiles: None,
    }))
}

/// Sync counterpart of the async `publish_workspace_registered` above (the
/// light-mode C2 worker's own helper): `harness_operator_worker` runs inside
/// `tokio::task::spawn_blocking` and drives `updates` with `blocking_send`
/// throughout, never `.await`, so it cannot call that `async fn` directly.
/// Sends the identical `WorkerUpdate` pair in the identical order.
fn publish_harness_workspace_registered(
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    workspace: WorkspaceSnapshotUpdate,
) {
    let WorkspaceSnapshotUpdate::C2(workspace_ref) = &workspace;
    let workspace_id = workspace_ref.workspace_id.to_string();
    let _ = updates.blocking_send(WorkerUpdate::WorkspaceUpserted {
        node_id: node_id.to_owned(),
        workspace,
    });
    let _ = updates.blocking_send(WorkerUpdate::SelectWorkspace {
        node_id: node_id.to_owned(),
        workspace_id,
    });
}

/// `ExportContextPack`'s reply, projected back into the light-shaped
/// `ResolvedContextPackReceipt` `WorkerUpdate::ContextExported`/
/// `App::apply_context_exported` already consume -- the harness-service-side
/// mirror of this exact conversion is `harness_context_to_node` in
/// `gate4agent-harness-service`'s `c2.rs`; this is the TUI's own copy across
/// the crate boundary, same field-for-field shape.
fn project_harness_context_pack_receipt(
    context: HarnessResolvedContextPackReceiptV1,
) -> Result<ResolvedContextPackReceipt, String> {
    Ok(ResolvedContextPackReceipt {
        id: SpawnContextId::new(context.id.as_str()).map_err(|error| error.to_string())?,
        digest: SpawnContextDigest::new(context.digest.as_str()).map_err(|error| error.to_string())?,
        lineage: ContextPackLineageReceipt {
            source_node_id: NodeId::new(context.lineage.source_node_id.as_str())
                .map_err(|error| error.to_string())?,
            source_session: WireSessionAddress {
                workspace_id: WorkspaceId::new(context.lineage.source_workspace_id.as_str())
                    .map_err(|error| error.to_string())?,
                session: SessionKey {
                    instance_id: AgentInstanceId(context.lineage.source_instance_id),
                    generation: SessionGeneration(context.lineage.source_generation),
                },
            },
            source_provider: AgentId::new(context.lineage.source_provider.as_str())
                .map_err(|error| error.to_string())?,
        },
        source_message_count: context.source_message_count,
        retained_message_count: context.retained_message_count,
        byte_len: context.byte_len,
        truncated: context.truncated,
    })
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProjectedLifecycle {
    running: bool,
    stoppable: bool,
    removable: bool,
    restartable: bool,
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

fn project_provider(provider: AgentId) -> Provider {
    provider
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
        LaunchInventory, OpaqueHostPath,
        ObservationEvidenceV1,
        ResolvedBundleReceipt, SpawnBundleDigest, SpawnBundleId, SpawnBundleRevision,
        SpawnProfileId, SpawnProfileRevision,
        SpawnProfileSummary,
    };
    use crate::app::{
        ContextUsageSegment, ContextUsageSegmentHit, DragState, Focus, HitRegion, HitTarget, LaunchContextMode, LaunchField,
        HarnessTaskComposerField, HarnessTaskRef, LaunchTarget, PtyColorMode, SpawnDialog,
        SurfacePaneLayout,
    };
    use crate::surface::PaneId;

    fn host_path(value: impl Into<String>) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.into()).unwrap()
    }

    fn provider(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn incarnation(byte: u8) -> gate4agent_node_protocol::NodeIncarnationId {
        gate4agent_node_protocol::NodeIncarnationId::from_bytes([byte; 16])
    }

    fn sixel_placement(icon: icons::IconId, x: u16, selected: bool) -> SixelIconPlacement {
        SixelIconPlacement { icon, rect: uzor_tui::Rect::new(x, 1, 4, 2), selected }
    }

    #[test]
    fn flush_sixel_icon_skips_a_second_emission_when_the_signature_is_unchanged() {
        let mut app = App::default();
        app.layout.sixel_icons = vec![sixel_placement(icons::IconId::Files, 2, false)];
        let mut state = SixelEmitState::default();
        let mut written = Vec::new();

        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        assert!(!written.is_empty(), "the first emission for a non-empty sixel_icons must write real bytes");
        let after_first = written.len();

        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        assert_eq!(written.len(), after_first, "an unchanged signature must not write anything on the second call");
    }

    #[test]
    fn flush_sixel_icon_reemits_when_the_rect_or_selection_changes() {
        let mut app = App::default();
        app.layout.sixel_icons = vec![sixel_placement(icons::IconId::Files, 2, false)];
        let mut state = SixelEmitState::default();
        let mut written = Vec::new();
        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        let after_first = written.len();

        // Moved: same icon, different rect.
        app.layout.sixel_icons = vec![sixel_placement(icons::IconId::Files, 9, false)];
        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        assert!(written.len() > after_first, "a moved icon must trigger a fresh emission");
        let after_move = written.len();

        // Selection flips: same icon, same rect, only `selected` differs.
        app.layout.sixel_icons = vec![sixel_placement(icons::IconId::Files, 9, true)];
        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        assert!(written.len() > after_move, "a selection change must trigger a fresh emission even at the same rect");
    }

    #[test]
    fn flush_sixel_icon_force_next_reemits_even_with_an_unchanged_signature() {
        let mut app = App::default();
        app.layout.sixel_icons = vec![sixel_placement(icons::IconId::Files, 2, false)];
        let mut state = SixelEmitState::default();
        let mut written = Vec::new();
        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        let after_first = written.len();

        // Same signature, but a resize (or any other forced-redraw path)
        // set `force_next` -- the next call must still write bytes.
        state.force_next = true;
        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        assert!(written.len() > after_first, "force_next must force a real emission even when nothing in sixel_icons changed");
    }

    #[test]
    fn flush_sixel_icon_writes_nothing_for_an_empty_sixel_icons() {
        // Braille/Ascii tier, or a terminal too short for the tall rail:
        // `layout.sixel_icons` is empty and there is nothing to draw.
        let app = App::default();
        assert!(app.layout.sixel_icons.is_empty());
        let mut state = SixelEmitState::default();
        let mut written = Vec::new();
        flush_sixel_icon_into(&mut written, &app, &mut state).unwrap();
        assert!(written.is_empty(), "an empty sixel_icons must never write terminal bytes");
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
        // `RefreshSessionRecordHistory` rides the harness history lane (see
        // `harness_native_history_read_action`), not a bare node-id route.
        let routes = BTreeMap::from([(HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), commands)]);

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

        // After the management-verb family landed, every routable node
        // action has a typed harness path — the only remaining rejects are
        // the deliberately dead actions with their own precise messages.
        // `SpawnManagedWorktree` is that case: a full SpawnSpec construction
        // the operator wire does not expose outside a Task.
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::SpawnManagedWorktree {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                provider: "claude".parse().unwrap(),
                rows: 24,
                cols: 80,
                profile_id: "profile-a".to_owned(),
                profile_revision: "r1".to_owned(),
                worktree_profile_id: "worktree-a".to_owned(),
                prompt: None,
                bundle_id: None,
                context_id: None,
                idempotency_key: "idem-a".to_owned(),
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
            Some(
                "Harness-owned session action unavailable: SpawnManagedWorktree has no typed \
                 harness-operator verb -- it requires a full SpawnSpec construction (profile/\
                 bundle/context resolution) this wire does not expose outside a Task"
            ),
        );
    }

    #[test]
    fn harness_only_backend_routes_workspace_writes_and_creates_to_the_detail_lane() {
        let mut app = App::default();
        let (harness_tx, mut harness_rx) = mpsc::channel(4);
        let (history_tx, mut history_rx) = mpsc::channel(4);
        let (detail_tx, mut detail_rx) = mpsc::channel(4);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);

        let path = RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap();
        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::WriteWorkspaceFile {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: path.clone(),
                expected_revision: "b".repeat(64),
                text: "fn main() {}\n".to_owned(),
                token: 1,
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessWriteNodeWorkspaceFile {
                node_id, workspace_id, path: routed_path, token: 1, ..
            }) if node_id == "node-a" && workspace_id == "workspace-a" && routed_path == path
        ));

        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::CreateWorkspaceFile {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: path.clone(),
                token: 2,
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessCreateNodeWorkspaceFile {
                node_id, workspace_id, path: routed_path, token: 2,
            }) if node_id == "node-a" && workspace_id == "workspace-a" && routed_path == path
        ));

        send_action(
            &mut app,
            &commands,
            &BTreeMap::new(),
            AppAction::CreateWorkspaceDirectory {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                path: path.clone(),
                token: 3,
            },
        );
        assert!(matches!(
            detail_rx.try_recv(),
            Ok(AppAction::HarnessCreateNodeWorkspaceDirectory {
                node_id, workspace_id, path: routed_path, token: 3,
            }) if node_id == "node-a" && workspace_id == "workspace-a" && routed_path == path
        ));

        assert!(matches!(harness_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(matches!(history_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert_eq!(app.notice, None);
    }

    #[test]
    fn harness_only_backend_clears_optimistic_write_state_when_the_detail_lane_is_busy() {
        let mut app = App::default();
        let (harness_tx, _harness_rx) = mpsc::channel(1);
        let (history_tx, _history_rx) = mpsc::channel(1);
        let (detail_tx, _detail_rx) = mpsc::channel(1);
        // Fill the detail lane so the routed write/create below observes
        // `TrySendError::Full` -- the same busy path a live harness detail
        // worker under load would hit.
        detail_tx.try_send(AppAction::None).unwrap();
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);

        // The optimistic "saving" state `Ctrl+s` already set on the editor
        // must clear to an honest error, not hang forever, when the harness
        // detail lane is busy -- see `reject_harness_queue_action`'s
        // `HarnessWriteNodeWorkspaceFile` arm.
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
        // No generic notice: the busy-lane rejection already routed the
        // honest failure into the file tab's own error state (rendered
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
            Some("busy: Harness detail/read command queue is full"),
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
        let mut terminal = TerminalWatermarks::default();
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
            &mut terminal,
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
            &mut terminal,
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
    fn missing_harness_sender_rolls_back_pending_execution_mutation() {
        let mut app = App::default();
        let task = paginated_harness_task(2);
        app.begin_harness_refresh(81);
        app.harness_kanban.execution_mutation = Some(crate::app::HarnessExecutionMutationState {
            token: 81,
            task: harness_task_ref(&task),
            kind: crate::app::HarnessExecutionMutationKind::StartTask,
            refresh_run: None,
        });
        // No `HARNESS_COMMAND_ROUTE` entry at all: the same "operator
        // connection never came up" shape a live harness worker's own
        // startup failure leaves behind.
        let commands = BTreeMap::new();

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
    fn harness_native_history_and_session_record_families_route_in_harness_mode() {
        let mut app = App::default();
        let (harness_tx, mut harness_rx) = mpsc::channel(4);
        let (history_tx, mut history_rx) = mpsc::channel(4);
        let (detail_tx, mut detail_rx) = mpsc::channel(4);
        let commands = BTreeMap::from([
            (HARNESS_COMMAND_ROUTE.to_owned(), harness_tx),
            (HARNESS_HISTORY_COMMAND_ROUTE.to_owned(), history_tx),
            (HARNESS_DETAIL_COMMAND_ROUTE.to_owned(), detail_tx),
        ]);
        let route = NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(),
            provider("codex"),
        );

        // Native-session-catalog reads keep riding the history lane.
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
        assert!(matches!(harness_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(matches!(detail_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        // Session-record preview/refresh: same node request as native
        // session preview, so they ride the same history lane unmodified.
        send_operator_action(
            &mut app,
            &commands,
            AppAction::PreviewSessionRecord {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
                message_limit: 8,
                token: 13,
            },
        );
        assert!(matches!(
            history_rx.try_recv(),
            Ok(AppAction::PreviewSessionRecord { node_id, record_id, token: 13, .. })
                if node_id == "node-a" && record_id == "record-a",
        ));
        assert!(matches!(harness_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        send_operator_action(
            &mut app,
            &commands,
            AppAction::RefreshSessionRecordHistory {
                node_id: "node-a".to_owned(),
                node_incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([9; 16]),
                record_id: "record-a".to_owned(),
                message_limit: 8,
            },
        );
        assert!(matches!(
            history_rx.try_recv(),
            Ok(AppAction::RefreshSessionRecordHistory { node_id, record_id, .. })
                if node_id == "node-a" && record_id == "record-a",
        ));
        assert!(matches!(harness_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        // Session-record mutations: bare `node_id`, no `SessionAddress`
        // translation needed, so `route_harness_session_verb` leaves them
        // unmodified and they ride the operator mutation lane like every
        // other typed Harness verb -- the dispatch gap this test used to
        // document (`IndexNativeSession` failing closed) is now closed.
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
            Ok(AppAction::IndexNativeSession { node_id, operation_token: 12, .. })
                if node_id == "node-a",
        ));
        assert!(matches!(history_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(matches!(detail_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        send_operator_action(
            &mut app,
            &commands,
            AppAction::RenameSessionRecord {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
                display_name: "Renamed".to_owned(),
            },
        );
        assert!(matches!(
            harness_rx.try_recv(),
            Ok(AppAction::RenameSessionRecord { node_id, record_id, display_name })
                if node_id == "node-a" && record_id == "record-a" && display_name == "Renamed",
        ));

        // `DiscoverHistory`/`LoadHistory` stay legitimately unroutable: no
        // harness-operator wire mapping exists for them (see the doc
        // comment on `reject_history_refresh_action`), and the rejection
        // now names that reason precisely instead of the generic notice.
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 1,
            generation: 1,
        };
        send_operator_action(
            &mut app,
            &commands,
            AppAction::DiscoverHistory { address, limit: 16 },
        );
        assert!(matches!(harness_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert!(matches!(history_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
        assert_eq!(
            app.notice.as_deref(),
            Some(
                "Harness-owned session action unavailable: native session-history discovery has no harness-operator wire mapping (light-mode direct-C2 only)",
            ),
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
        let mut terminal = TerminalWatermarks::default();

        let first = apply_update(
            &mut app,
            &mut terminal,
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
            &mut terminal,
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
        let mut terminal = TerminalWatermarks::default();
        apply_update(
            &mut app,
            &mut terminal,
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
        for status in [
            C2SessionStatus::Registered,
            C2SessionStatus::Starting,
            C2SessionStatus::Running,
            C2SessionStatus::Stopping,
        ] {
            let lifecycle = project_c2_lifecycle(&status);
            assert!(lifecycle.stoppable);
            assert!(!lifecycle.removable);
            assert!(!lifecycle.restartable);
        }
        for status in [
            C2SessionStatus::Exited { exit_code: Some(0) },
            C2SessionStatus::Failed,
        ] {
            let lifecycle = project_c2_lifecycle(&status);
            assert!(!lifecycle.stoppable);
            assert!(lifecycle.removable);
            assert!(lifecycle.restartable);
        }
    }

    #[test]
    fn run_options_can_carry_an_explicit_style_override() {
        let options = RunOptions {
            operator: HarnessOperatorEndpoint {
                endpoint: "127.0.0.1:18080".parse().unwrap(),
                credential: HarnessOperatorCredential::parse(format!(
                    "g4aho_{}",
                    "0".repeat(64),
                )).unwrap(),
                launch_plan_id: None,
            },
            kanban_default: false,
            color_mode_override: Some(PtyColorMode::Inherited),
        };
        assert_eq!(options.color_mode_override, Some(PtyColorMode::Inherited));
    }

    #[test]
    fn backend_credentials_are_not_persisted_with_ui_preferences() {
        let secret = "1".repeat(64);
        let _options = RunOptions {
            operator: HarnessOperatorEndpoint {
                endpoint: "127.0.0.1:18080".parse().unwrap(),
                credential: HarnessOperatorCredential::parse(format!("g4aho_{secret}")).unwrap(),
                launch_plan_id: None,
            },
            kanban_default: false,
            color_mode_override: None,
        };
        let preferences = preferences_for_save(&App::default(), PtyColorMode::Inherited);
        let path = std::env::temp_dir().join(format!(
            "gate4agent-tui-mode-secret-{}-{}.json",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        ));

        preferences.save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.windows(secret.len()).any(|window| window == secret.as_bytes()));
        std::fs::remove_file(path).unwrap();
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
    }

    #[test]
    fn harness_phase5_observation_reads_have_no_node_request_fallback() {
        let run = HarnessRunRef {
            run_id: HarnessRunId::new(format!("hrun_{}", "6".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(3).unwrap(),
        };
        let monitor = AppAction::HarnessOpenMonitor { run: run.clone() };
        assert!(harness_detail_read_action(&monitor));

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

    #[test]
    fn harness_terminal_frames_dedupe_by_watermark_and_ignore_stale_replays() {
        // `TerminalWatermarks` is the one piece of the deleted `C2ApplyState`
        // that survived the direct-C2 dialect's removal (see that struct's
        // own doc comment): this exercises it through its sole surviving
        // producer, `WorkerUpdate::HarnessTerminalRead`, which is now the
        // app's only terminal-tail ingest path.
        let incarnation_id = incarnation(0x11);
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 3,
        };
        let mut app = App::default();
        app.nodes.push(NodeView {
            node_id: "node-a".to_owned(),
            incarnation_id: Some(incarnation_id),
            endpoint: "harness://runtime-inventory/node-a@1".to_owned(),
            relay_route: C2RelayRoute::Unknown,
            connection: ConnectionState::Connected,
            controller_owned: false,
            event_sequence: 1,
            session_records: Vec::new(),
            launch_inventory: None,
            providers: Vec::new(),
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "workspace-a".to_owned(),
                canonical_root: host_path(r"C:\work\workspace-a"),
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
                    terminal_cursor: None,
                }],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
        });
        let mut terminal = TerminalWatermarks::default();

        let harness_frame = |sequence: u64, bytes: &[u8]| HarnessRuntimeTerminalFrameV1 {
            sequence,
            size: HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
            cursor_row: 1,
            cursor_column: bytes.len() as u16,
            formatted: bytes.to_vec(),
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: HarnessRuntimeMouseProtocolEncodingV1::Default,
        };
        let page = |frames: Vec<HarnessRuntimeTerminalFrameV1>| WorkerUpdate::HarnessTerminalRead(
            HarnessRuntimeTerminalPageV1 {
                session: harness_terminal_session_address(&address, incarnation_id),
                frames,
                dropped: 0,
                transport_incomplete: false,
                next_cursor: None,
            },
        );

        apply_update(&mut app, &mut terminal, page(vec![harness_frame(1, b"first")]));
        assert_eq!(app.find_session(&address).unwrap().terminal_formatted, b"first");
        assert_eq!(terminal.terminal_watermark(&address), Some(1));

        // A page carrying only an already-seen sequence must not regress the
        // terminal contents -- the exact dedup the direct-C2 dialect's own
        // `C2EventUpdate::TerminalFrame` handling used to provide.
        apply_update(&mut app, &mut terminal, page(vec![harness_frame(1, b"stale-replay")]));
        assert_eq!(app.find_session(&address).unwrap().terminal_formatted, b"first");
        assert_eq!(terminal.terminal_watermark(&address), Some(1));

        apply_update(&mut app, &mut terminal, page(vec![harness_frame(2, b"second")]));
        assert_eq!(app.find_session(&address).unwrap().terminal_formatted, b"second");
        assert_eq!(terminal.terminal_watermark(&address), Some(2));
    }
}
