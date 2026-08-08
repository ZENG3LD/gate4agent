use crate::git_worktree::{
    create_worktree as create_git_worktree, list_worktrees as list_git_worktrees,
    paths_equal as worktree_paths_equal, remove_worktree as remove_git_worktree,
    removal_lookup_path as normalize_worktree_removal_target, run_git_read_bounded,
    GitCommandOutput, GitWorktreeError, GitWorktreeErrorKind, NativeGitWorktreeSnapshot,
};
use crate::session_registry::{
    self, validate_display_name, LoadedNodeState, MAX_MANAGED_SESSION_RECORDS,
};
use crate::protocol::{
    read_json_frame_limited_body_timeout, write_json_frame, write_json_frame_limited,
    AgentProvider, ArchitectureId, CapabilityId, ClientFrame, ClientRole, ControllerState,
    FrameError, GitCommitSummary, GitSnapshot, GitStatusEntry,
    GitWorktreeSnapshot, HostDescriptor, LocalTransportKind, NodeCompatibilitySupport, NodeEvent,
    NodeEventEnvelope, NodeFailure, NodeFailureCode, NodeHello, NodeId, NodeIncarnationId,
    NodeRequest, NodeResponse, NodeSnapshot, OpaqueHostPath, OperatingSystemId, PathEncoding,
    PathSemantics, PathStyle, ProtocolRange, RepositoryPath, RequestEnvelope, ResponseEnvelope,
    ServerChallenge, ServerFrame, SessionAddress, SessionKey, SessionMode, ManagedSessionRecord,
    ManagedSessionState, SessionRecordId, StateSchemaSupport, WorkspaceEntry, WorkspaceEntryKind,
    WorkspaceId, WorkspaceInspection, WorkspaceSnapshot, DEFAULT_CONTROLLER_LEASE_MS,
    MAX_CONTROLLER_LEASE_MS, MIN_CONTROLLER_LEASE_MS, NODE_COMPATIBILITY_METADATA_CAPABILITY,
    NODE_PROTOCOL_VERSION, MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    MAX_NODE_TERMINAL_BYTES, MAX_NODE_TEXT_BYTES, MAX_REPOSITORY_PATH_BYTES,
    MAX_WORKSPACE_ROOT_BYTES, NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V2,
};
use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_handle::{EventSubscription, Gate4AgentHandle, PortDispatchError};
use gate4agent_runtime_native::{HookIngressConfig, NativeRuntime, NativeRuntimeConfig};
use gate4agent_node_wire::{
    auth_proof, negotiated_auth_proof, proofs_match, random_incarnation_id, random_nonce,
    AuthDirection,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, InputAction, PromptFraming, PromptPayload, ResumeLaunchRequest,
    ResumeTarget, SessionGeneration, StartRequest, TerminalControl, TerminalText,
    TransportKind, CONTROL_PROTOCOL_VERSION, CONTROL_SESSIONS_MAX, WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{sleep, timeout};

mod http_api;

pub const DEFAULT_NODE_ENDPOINT: &str = r"\\.\pipe\gate4agent-node";

const NODE_EVENT_HISTORY_MAX: usize = 4_096;
const NODE_BROADCAST_CAPACITY: usize = 1_024;
const CONTROL_EVENT_SUBSCRIPTION_CAPACITY: usize = 1_024;
const MAX_PREAUTH_CONNECTIONS: usize = 32;
const MAX_AUTHENTICATED_CONNECTIONS: usize = 16;
const AUTH_FRAME_TIMEOUT_MS: u64 = 5_000;
const FRAME_BODY_TIMEOUT_MS: u64 = 5_000;
const CONNECTION_SHUTDOWN_GRACE_MS: u64 = 250;
const SPAWN_DISPATCH_TIMEOUT_MS: u64 = 2_000;
const MUTATION_SETTLE_TIMEOUT_MS: u64 = 5_000;
const READINESS_SETTLE_HEADROOM_MS: u64 = 2_000;
const MANAGED_RESUME_SETTLE_TIMEOUT_MS: u64 = 30_000;
const WORKSPACE_INSPECTION_MAX_CONCURRENCY: usize = 4;
const WORKSPACE_TREE_MAX_DEPTH: usize = 6;
const WORKSPACE_TREE_MAX_ENTRIES: usize = 512;
const GIT_STATUS_MAX_ENTRIES: usize = 128;
const GIT_COMMIT_MAX_ENTRIES: usize = 12;
const GIT_OUTPUT_MAX_BYTES: usize = 64 * 1_024;
const GIT_DIAGNOSTIC_MAX_BYTES: usize = 1_024;
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

fn windows_path_text(path: &OpaqueHostPath) -> &str {
    path.as_utf8()
        .expect("Windows node only creates UTF-8 opaque host paths")
}

fn require_windows_path(path: OpaqueHostPath) -> Result<String, NodeFailure> {
    path.as_utf8()
        .map(str::to_owned)
        .ok_or_else(|| failure(
            NodeFailureCode::InvalidRequest,
            "Windows node rejects non-UTF-8 host paths",
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
        let canonical_root = normalize_windows_verbatim_path(canonical_root);
        validate_workspace_root(&workspace_id, &canonical_root)?;
        Ok(Self {
            workspace_id,
            canonical_root,
        })
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn canonical_root(&self) -> &str {
        &self.canonical_root
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct NodeServerConfig {
    pub endpoint: String,
    api_listen: Option<std::net::SocketAddr>,
    pub node_id: NodeId,
    pub workspaces: Vec<WorkspaceConfig>,
    access_token: String,
    pub runtime: NativeRuntimeConfig,
    state_path: Option<PathBuf>,
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
        if !endpoint.starts_with(r"\\.\pipe\") || endpoint.len() <= r"\\.\pipe\".len() {
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
            if workspace_ids
                .insert(workspace.workspace_id.clone(), ())
                .is_some()
            {
                return Err(NodeServerError::DuplicateWorkspaceId(
                    workspace.workspace_id.clone(),
                ));
            }
            let root_key = workspace.canonical_root.to_lowercase();
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
}

pub fn default_state_path(node_id: &NodeId) -> Result<PathBuf, NodeServerError> {
    let root = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(NodeServerError::LocalAppDataUnavailable)?;
    Ok(root
        .join("Gate4Agent")
        .join("nodes")
        .join(node_id.as_str())
        .join("state-v1.json"))
}

pub struct NodeServer {
    config: NodeServerConfig,
    runtime: NativeRuntime,
    shared: Arc<NodeShared>,
    events: EventSubscription,
    state_path_lock: Option<session_registry::StatePathLock>,
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
        let input_settle_timeout_ms = catalog
            .iter()
            .map(|spec| spec.readiness.timeout_ms)
            .max()
            .unwrap_or(MUTATION_SETTLE_TIMEOUT_MS)
            .saturating_add(READINESS_SETTLE_HEADROOM_MS)
            .max(MUTATION_SETTLE_TIMEOUT_MS);
        let enabled_providers = catalog
            .iter()
            .filter_map(|spec| match spec.id.as_str() {
                "claude" => Some(AgentProvider::Claude),
                "codex" => Some(AgentProvider::Codex),
                "kimi" => Some(AgentProvider::Kimi),
                _ => None,
            })
            .collect();
        let state_path_lock =
            session_registry::StatePathLock::acquire(config.state_path.as_deref())
                .map_err(|error| {
                    durable_state_server_error(error, DURABLE_STATE_LOCK_FAILED_ERROR)
                })?;
        let (handle, runtime) = NativeRuntime::new(catalog, config.runtime);
        let events = handle.subscribe(CONTROL_EVENT_SUBSCRIPTION_CAPACITY);
        let incarnation_id = random_incarnation_id()
            .map_err(NodeServerError::IncarnationIdentity)?;
        let loaded = session_registry::load(config.state_path.as_deref(), &config.node_id)
            .map_err(durable_state_load_error)?;
        let (workspaces, records, persistence_warning) =
            merge_durable_state(&config.workspaces, loaded)?;
        let shared = Arc::new(NodeShared::new_with_incarnation(
            handle,
            config.access_token.clone(),
            config.node_id.clone(),
            incarnation_id,
            workspaces,
            enabled_providers,
            config.state_path.clone(),
            records,
            persistence_warning,
            input_settle_timeout_ms,
        ));
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
            shared.publish_control(event);
        }
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
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    tokio::select! {
        signal = ctrl_c.recv() => signal.ok_or(NodeServerError::SignalStreamClosed),
        signal = ctrl_break.recv() => signal.ok_or(NodeServerError::SignalStreamClosed),
    }
}

fn active_registry() -> Result<AgentRegistry, NodeServerError> {
    let specs = builtin_registry()
        .iter()
        .filter(|spec| matches!(spec.id.as_str(), "claude" | "codex" | "kimi"))
        .cloned()
        .collect::<Vec<_>>();
    AgentRegistry::new(specs).map_err(|error| NodeServerError::Registry(error.to_string()))
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
                workspace.canonical_root.to_lowercase(),
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
            if !existing.canonical_root.eq_ignore_ascii_case(&canonical_root) {
                return Err(NodeServerError::DurableState(io::Error::new(
                    io::ErrorKind::InvalidData,
                    DURABLE_STATE_CONFLICT_ERROR,
                )));
            }
            continue;
        }
        if roots.get(&canonical_root.to_lowercase()).is_some() {
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
            workspace.canonical_root.to_lowercase(),
            workspace.workspace_id.clone(),
        );
        workspaces.insert(workspace_id, workspace);
    }

    for record in &mut loaded.records {
        match workspaces.get(&record.workspace_id) {
            Some(workspace)
                if workspace
                    .canonical_root
                    .eq_ignore_ascii_case(windows_path_text(&record.canonical_root)) => {}
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
    enabled_providers: Vec<AgentProvider>,
    controller: Mutex<Option<ControllerLease>>,
    history: Mutex<NodeEventHistory>,
    event_tx: broadcast::Sender<NodeEventEnvelope>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionBinding {
    workspace_id: WorkspaceId,
    generation: SessionGeneration,
    pending_resume: Option<(SessionGeneration, CommandId)>,
    record_id: Option<SessionRecordId>,
}

struct SessionRecords {
    records: BTreeMap<SessionRecordId, ManagedSessionRecord>,
}

#[derive(Clone, Copy)]
struct ControllerLease {
    connection_id: u64,
    expires_at: Instant,
}

struct NodeEventHistory {
    last_sequence: u64,
    events: VecDeque<NodeEventEnvelope>,
}

impl NodeEventHistory {
    fn new() -> Self {
        Self {
            last_sequence: 0,
            events: VecDeque::with_capacity(NODE_EVENT_HISTORY_MAX),
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
        enabled_providers: Vec<AgentProvider>,
        state_path: Option<PathBuf>,
        records: Vec<ManagedSessionRecord>,
        persistence_warning: Option<String>,
        input_settle_timeout_ms: u64,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(NODE_BROADCAST_CAPACITY);
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
            enabled_providers,
            controller: Mutex::new(None),
            history: Mutex::new(NodeEventHistory::new()),
            event_tx,
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
        }
    }

    #[cfg(test)]
    fn new(
        handle: Gate4AgentHandle,
        access_token: String,
        node_id: NodeId,
        workspaces: Vec<WorkspaceConfig>,
        enabled_providers: Vec<AgentProvider>,
    ) -> Self {
        Self::new_with_incarnation(
            handle,
            access_token,
            node_id,
            NodeIncarnationId::from_bytes([0; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            workspaces,
            enabled_providers,
            None,
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
        let result = session_registry::save(
            self.state_path.as_deref(),
            &self.node_id,
            &workspaces,
            &records,
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
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(bindings.len() < CONTROL_SESSIONS_MAX);
        debug_assert!(!bindings.contains_key(&instance_id));
        bindings.insert(instance_id, SessionBinding {
            workspace_id,
            generation,
            pending_resume: None,
            record_id: None,
        });
    }

    fn bind_managed_session(
        &self,
        address: &SessionAddress,
        record_id: SessionRecordId,
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
                pending_resume: None,
                record_id: Some(record_id),
            },
        );
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

    fn default_record_name(&self, provider: AgentProvider) -> String {
        let records = self
            .session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ordinal = records
            .records
            .values()
            .filter(|record| record.provider == provider)
            .count()
            .saturating_add(1);
        let provider_name = match provider {
            AgentProvider::Claude => "claude",
            AgentProvider::Codex => "codex",
            AgentProvider::Kimi => "kimi",
        };
        format!("{provider_name} #{ordinal}")
    }

    fn new_record(
        &self,
        address: &SessionAddress,
        provider: AgentProvider,
        mode: SessionMode,
        state: ManagedSessionState,
        provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
    ) -> Result<ManagedSessionRecord, NodeFailure> {
        let canonical_root = self.workspace_root(&address.workspace_id)?;
        let now = unix_time_ms();
        Ok(ManagedSessionRecord {
            record_id: self.allocate_record_id()?,
            display_name: self.default_record_name(provider),
            provider,
            mode,
            state,
            workspace_id: address.workspace_id.clone(),
            canonical_root: opaque_windows_path(canonical_root),
            provider_session,
            active_session: Some(address.clone()),
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
                        candidate.provider,
                        candidate_identity,
                        record.provider,
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

    fn remove_record_memory(&self, record_id: &SessionRecordId) -> Option<ManagedSessionRecord> {
        self.session_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .remove(record_id)
    }

    fn discard_record(&self, record_id: &SessionRecordId) -> Result<(), NodeFailure> {
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
        self.publish(NodeEvent::SessionRecordRemoved {
            record_id: record_id.clone(),
        });
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
        if !working_directory.eq_ignore_ascii_case(windows_path_text(&record.canonical_root)) {
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
                            candidate.provider,
                            candidate_identity,
                            record.provider,
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
        if let Some(stale_address) = self.bound_address_for_record(record_id) {
            self.remove_session(&stale_address).await?;
        }
        self.ensure_binding_capacity()?;

        let request = ResumeLaunchRequest {
            working_directory,
            terminal_size,
            initial_prompt,
        };
        request
            .validate()
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
        let instance_id = AgentInstanceId(self.next_instance_id.fetch_add(1, Ordering::AcqRel));
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
        self.bind_managed_session(&address, record_id.clone());
        drop(transaction);

        let transport = match record.mode {
            SessionMode::Pty => TransportKind::Pty,
            SessionMode::Inline => TransportKind::Pipe,
        };
        let agent_id = AgentId::new(record.provider.agent_id())
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
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
            self.remove_binding(&address);
            return Err(error);
        }
        let command = self.prepare_command(ControlCommand::Resume {
            instance_id,
            target: ResumeTarget::ProviderSession { identity },
            request,
        });
        let started_after = self.current_sequence();
        if let Err(error) = self.arm_resume(&address, command.id) {
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
            Ok(settled_address) => Ok((self.record(record_id)?, settled_address)),
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

    async fn inspect_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceInspection, NodeFailure> {
        let _permit = self
            .inspection_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                failure(
                    NodeFailureCode::BackendBusy,
                    "workspace inspection capacity is busy",
                )
            })?;
        let canonical_root = self.workspace_root(&workspace_id)?;
        let tree_root = canonical_root.clone();
        let (entries, tree_truncated) = tokio::task::spawn_blocking(move || {
            collect_workspace_entries(Path::new(&tree_root))
        })
        .await
        .map_err(|error| {
            failure(
                NodeFailureCode::BackendOperationFailed,
                &format!("workspace inspection task failed: {error}"),
            )
        })?;
        let mut git = inspect_git_workspace(&canonical_root).await;
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
        Ok(WorkspaceInspection {
            workspace_id,
            entries,
            tree_truncated,
            git,
        })
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
            existing_root.eq_ignore_ascii_case(workspace.canonical_root())
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
                        .is_some_and(|root| root.eq_ignore_ascii_case(workspace.canonical_root()))
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
                        .is_some_and(|root| root.eq_ignore_ascii_case(workspace.canonical_root()))
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
        let workspace = self
            .register_workspace(workspace_id, worktree.path.clone())
            .await
            .map_err(|error| failure(error.code, &error.message))?;
        worktree.workspace_id = Some(workspace.workspace_id.clone());
        Ok((protocol_worktree(worktree), workspace))
    }

    async fn remove_worktree(
        &self,
        source_workspace_id: WorkspaceId,
        target_root: String,
    ) -> Result<Option<WorkspaceId>, NodeFailure> {
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

    fn bound_workspace_root(&self, address: &SessionAddress) -> Result<String, NodeFailure> {
        self.validate_address(address)?;
        self.workspace_root(&address.workspace_id)
    }

    fn remove_binding(&self, address: &SessionAddress) {
        let mut bindings = self
            .session_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bindings.get(&address.session.instance_id).is_some_and(|binding| {
            binding.workspace_id == address.workspace_id
                && binding.generation == address.session.generation
        }) {
            bindings.remove(&address.session.instance_id);
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
        binding.pending_resume = Some((binding.generation, command_id));
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
        let (address, record_id, resume_command_rejected) = {
            let mut bindings = self
                .session_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(binding) = bindings.get_mut(&event.instance_id) else {
                return;
            };
            let mut resume_command_rejected = false;
            if binding.generation == event.generation {
                if let Some((pending_generation, pending_command_id)) = binding.pending_resume {
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
                if !binding.pending_resume.is_some_and(|(generation, _)| {
                    generation == binding.generation
                })
                    || expected != Some(event.generation.0)
                    || !authorized
                {
                    return;
                }
                binding.generation = event.generation;
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
                binding.record_id.clone(),
                resume_command_rejected,
            )
        };
        if let Some(record_id) = record_id {
            self.reconcile_managed_record(
                &record_id,
                &address,
                &event,
                resume_command_rejected,
            );
        }
        self.publish(NodeEvent::Control { address, event });
    }

    fn reconcile_managed_record(
        &self,
        record_id: &SessionRecordId,
        address: &SessionAddress,
        event: &ControlEvent,
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
            } => Some(identity.clone()),
            _ => None,
        };
        let replacement_id = observed_identity
            .as_ref()
            .and_then(|_| self.allocate_record_id().ok());
        let mut upserts = Vec::new();
        let mut removals = Vec::new();
        let mut rebind = None;
        let original_records;
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
                                    record.provider,
                                    record_identity,
                                    current_snapshot.provider,
                                    &identity,
                                )
                            })
                    })
                    .map(|record| record.record_id.clone());
                if let Some(existing_id) = existing_id {
                    let scope_matches = records.records.get(&existing_id).is_some_and(|record| {
                        record.mode == current_snapshot.mode
                            && record.workspace_id == current_snapshot.workspace_id
                            && record
                                .canonical_root
                                .as_utf8()
                                .zip(current_snapshot.canonical_root.as_utf8())
                                .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
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
                            current_snapshot.provider,
                            current,
                            current_snapshot.provider,
                            &identity,
                        )
                    })
                {
                    if let Some(current) = records.records.get_mut(record_id) {
                        current.active_session = None;
                        current.state = ManagedSessionState::Dormant;
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
                                current_snapshot.provider.agent_id(),
                                records
                                    .records
                                    .values()
                                    .filter(|record| {
                                        record.provider == current_snapshot.provider
                                    })
                                    .count()
                                    .saturating_add(1),
                            ),
                            provider: current_snapshot.provider,
                            mode: current_snapshot.mode,
                            state: ManagedSessionState::Live,
                            workspace_id: current_snapshot.workspace_id,
                            canonical_root: current_snapshot.canonical_root,
                            provider_session: Some(identity),
                            active_session: Some(address.clone()),
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
                        current.state = if current.provider_session.is_some() {
                            ManagedSessionState::Live
                        } else {
                            ManagedSessionState::IdentityPending
                        };
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
        if let Some(new_record_id) = rebind {
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
        let mut workspaces = workspace_roots
            .iter()
            .map(|(workspace_id, canonical_root)| {
                (
                    workspace_id.clone(),
                    WorkspaceSnapshot {
                        workspace_id: workspace_id.clone(),
                        canonical_root: opaque_windows_path(canonical_root.clone()),
                        sessions: Vec::new(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        for session in &control.sessions {
            let Some(binding) = bindings.get(&session.instance_id) else {
                continue;
            };
            if binding.generation != session.generation {
                continue;
            }
            if let Some(workspace) = workspaces.get_mut(&binding.workspace_id) {
                workspace.sessions.push(session.clone());
            }
        }
        NodeSnapshot {
            node_id: self.node_id.clone(),
            enabled_providers: self.enabled_providers.clone(),
            workspaces: workspaces.into_values().collect(),
            session_records: self
                .session_records
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .records
                .values()
                .cloned()
                .collect(),
        }
    }

    fn publish(&self, event: NodeEvent) -> NodeEventEnvelope {
        let mut history = self.history.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = history
            .last_sequence
            .checked_add(1)
            .expect("node event sequence exhausted");
        let envelope = NodeEventEnvelope { sequence, event };
        if history.events.len() == NODE_EVENT_HISTORY_MAX {
            history.events.pop_front();
        }
        history.events.push_back(envelope.clone());
        history.last_sequence = sequence;
        drop(history);
        let _ = self.event_tx.send(envelope.clone());
        envelope
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
        provider: AgentProvider,
        mode: SessionMode,
        terminal_size: gate4agent_types::TerminalSize,
        initial_prompt: Option<String>,
    ) -> Result<SessionAddress, NodeFailure> {
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
        let record = self.new_record(
            &address,
            provider,
            mode,
            ManagedSessionState::IdentityPending,
            None,
        )?;
        let record_id = record.record_id.clone();
        let transaction = self
            .state_transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.insert_record(record.clone())?;
        self.bind_managed_session(&address, record_id.clone());
        if let Err(error) = self.persist_state_locked() {
            self.remove_binding(&address);
            self.remove_record_memory(&record_id);
            return Err(persistence_failure(error));
        }
        drop(transaction);
        self.publish_record(record);
        let transport = match mode {
            SessionMode::Pty => TransportKind::Pty,
            SessionMode::Inline => TransportKind::Pipe,
        };
        let agent_id = AgentId::new(provider.agent_id())
            .map_err(|error| failure(NodeFailureCode::InvalidRequest, &error.to_string()))?;
        let dispatch_timeout = Duration::from_millis(SPAWN_DISPATCH_TIMEOUT_MS);
        if let Err(error) = self.dispatch_bounded(
            ControlCommand::Register {
                instance_id,
                agent_id,
                transport,
            },
            dispatch_timeout,
        )
        .await {
            self.remove_binding(&address);
            self.discard_record(&record_id)?;
            return Err(error);
        }
        let start_result = self
            .dispatch_bounded(
                ControlCommand::Start {
                    instance_id,
                    request: StartRequest {
                        working_directory,
                        terminal_size,
                        initial_prompt,
                        session_options: None,
                    },
                },
                dispatch_timeout,
            )
            .await;
        if let Err(start_error) = start_result {
            let recovery = self.rollback_spawn(&address, dispatch_timeout).await;
            return match recovery {
                Ok(()) => {
                    self.discard_record(&record_id)?;
                    Err(start_error)
                }
                Err(recovery_error) => {
                    let diagnostic = format!(
                        "start failed and registration recovery failed: {}",
                        recovery_error.message,
                    );
                    self.mark_record_error(&record_id, &diagnostic)?;
                    Err(failure(recovery_error.code, &diagnostic))
                }
            };
        }

        let deadline = Instant::now() + dispatch_timeout;
        loop {
            if let Some(current) = self
                .handle
                .snapshot()
                .sessions
                .iter()
                .find(|current| current.instance_id == instance_id)
            {
                if current.generation != session.generation {
                    return Err(failure(NodeFailureCode::StaleGeneration, "spawn generation diverged"));
                }
                if !matches!(current.status, gate4agent_types::SessionStatus::Registered) {
                    return Ok(address);
                }
            }
            if Instant::now() >= deadline {
                self.rollback_spawn(&address, dispatch_timeout).await.map_err(|error| {
                        failure(
                            error.code,
                            &format!(
                                "start did not commit and registration recovery failed: {}",
                                error.message
                            ),
                        )
                    })?;
                self.discard_record(&record_id)?;
                return Err(failure(
                    NodeFailureCode::BackendBusy,
                    "start did not commit before the bounded deadline; registration was removed",
                ));
            }
            sleep(Duration::from_millis(2)).await;
        }
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
        self.remove_binding(address);
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
        self.remove_binding(address);
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
        let events = history
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect();
        drop(history);
        NodeResponse::Resync {
            event_sequence,
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
    let mut first = true;
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
        let server = create_pipe(endpoint, first)?;
        tokio::select! {
            result = server.connect() => result?,
            _ = shared.shutdown_notify.notified() => return Ok(()),
        }
        first = false;
        let connection_shared = Arc::clone(&shared);
        connections.spawn(async move {
            serve_connection(server, connection_shared, preauth_permit).await
        });
        if shared.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
    }
}

fn create_pipe(endpoint: &str, first: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    options.create(endpoint)
}

async fn serve_connection(
    mut pipe: NamedPipeServer,
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
        Some(offer) => Some(
            node_compatibility_support()?
                .negotiate(NODE_PROTOCOL_VERSION, offer)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        ),
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
    write_json_frame(
        &mut pipe,
        &ServerFrame::Hello(NodeHello {
            protocol_version: NODE_PROTOCOL_VERSION,
            incarnation_id: shared.incarnation_id,
            connection_id,
            role: hello.role,
            event_sequence: shared.current_sequence(),
            controller: shared.controller_state(),
            snapshot: shared.snapshot(),
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
    loop {
        tokio::select! {
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
                let reply = process_request(&shared, connection_id, hello.role, request).await;
                write_json_frame(&mut writer, &ServerFrame::Reply(reply)).await?;
            }
            event = event_rx.recv() => {
                match event {
                    Ok(event) => write_json_frame(&mut writer, &ServerFrame::Event(event)).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let (sequence, oldest) = {
                            let history = shared.history.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            let sequence = history.last_sequence;
                            let oldest = history.events.front().map_or(sequence, |event| event.sequence);
                            (sequence, oldest)
                        };
                        let event = NodeEventEnvelope {
                            sequence,
                            event: NodeEvent::ResyncRequired { oldest_available_sequence: oldest },
                        };
                        write_json_frame(&mut writer, &ServerFrame::Event(event)).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = shared.shutdown_notify.notified() => break,
        }
    }
    Ok(())
}

fn node_compatibility_support() -> Result<NodeCompatibilitySupport, NodeServerError> {
    Ok(NodeCompatibilitySupport {
        protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION)
            .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        capabilities: baseline_capabilities()?,
        host: HostDescriptor {
            operating_system: OperatingSystemId::new("windows")
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
            architecture: ArchitectureId::new(std::env::consts::ARCH)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        },
        path_semantics: PathSemantics {
            style: PathStyle::Windows,
            encoding: PathEncoding::Utf8,
        },
        local_transport: LocalTransportKind::WindowsNamedPipe,
        state_schema: StateSchemaSupport {
            versions: ProtocolRange::new(NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V2)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))?,
        },
        provider_contracts: Vec::new(),
    })
}

fn baseline_capabilities() -> Result<Vec<CapabilityId>, NodeServerError> {
    [NODE_COMPATIBILITY_METADATA_CAPABILITY]
        .into_iter()
        .map(|capability| {
            CapabilityId::new(capability)
                .map_err(|error| NodeServerError::Handshake(error.to_string()))
        })
        .collect()
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

fn collect_workspace_entries(root: &Path) -> (Vec<WorkspaceEntry>, bool) {
    let mut entries = Vec::new();
    let mut truncated = false;
    walk_workspace_directory(root, None, 0, &mut entries, &mut truncated);
    (entries, truncated)
}

fn walk_workspace_directory(
    directory: &Path,
    relative_directory: Option<&RepositoryPath>,
    depth: usize,
    entries: &mut Vec<WorkspaceEntry>,
    truncated: &mut bool,
) {
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
        if file_type.is_dir() && is_skipped_workspace_directory(&name) {
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

fn is_skipped_workspace_directory(name: &str) -> bool {
    name.eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case("target")
        || name.eq_ignore_ascii_case("node_modules")
}

async fn inspect_git_workspace(root: &str) -> GitSnapshot {
    let mut snapshot = GitSnapshot {
        is_repository: false,
        branch: None,
        status: Vec::new(),
        recent_commits: Vec::new(),
        worktrees: Vec::new(),
        truncated: false,
        diagnostic: None,
    };
    let repository = match run_git_bounded(
        root,
        &["rev-parse", "--is-inside-work-tree"],
        4 * 1_024,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            append_git_diagnostic(
                &mut snapshot,
                &format!("git inspection unavailable: {error}"),
            );
            return snapshot;
        }
    };
    snapshot.truncated |= repository.truncated;
    if repository.timed_out {
        append_git_diagnostic(&mut snapshot, "git repository probe timed out");
        return snapshot;
    }
    if !repository.success
        || String::from_utf8_lossy(&repository.stdout).trim() != "true"
    {
        let stderr = String::from_utf8_lossy(&repository.stderr);
        if !stderr.to_ascii_lowercase().contains("not a git repository") {
            append_git_diagnostic(&mut snapshot, stderr.trim());
        }
        return snapshot;
    }
    snapshot.is_repository = true;

    match run_git_bounded(root, &["branch", "--show-current"], 4 * 1_024).await {
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
    if snapshot.branch.is_none() {
        if let Ok(output) = run_git_bounded(root, &["rev-parse", "--short", "HEAD"], 4 * 1_024).await {
            snapshot.truncated |= output.truncated;
            if output.success && !output.timed_out {
                let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !head.is_empty() {
                    snapshot.branch = Some(format!("detached:{head}"));
                }
            }
        }
    }

    match run_git_bounded(
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

    let log_limit = (GIT_COMMIT_MAX_ENTRIES + 1).to_string();
    match run_git_bounded(
        root,
        &["log", "-n", &log_limit, "--pretty=format:%h%x1f%s", "--", "."],
        16 * 1_024,
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
    match list_git_worktrees(root).await {
        Ok(worktrees) => {
            snapshot.worktrees = worktrees.into_iter().map(protocol_worktree).collect()
        }
        Err(error) => append_git_diagnostic(
            &mut snapshot,
            &format!("git worktree list failed: {}", error.message),
        ),
    }
    snapshot
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

async fn process_request(shared: &NodeShared, connection_id: u64, role: ClientRole, envelope: RequestEnvelope) -> ResponseEnvelope {
    let result = process_request_inner(shared, connection_id, role, envelope.request).await;
    ResponseEnvelope { request_id: envelope.request_id, result }
}

async fn process_request_inner(shared: &NodeShared, connection_id: u64, role: ClientRole, request: NodeRequest) -> Result<NodeResponse, NodeFailure> {
    let read_only = matches!(
        &request,
        NodeRequest::Snapshot
            | NodeRequest::Resync { .. }
            | NodeRequest::InspectWorkspace { .. }
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
        NodeRequest::InspectWorkspace { workspace_id } => {
            let inspection = shared.inspect_workspace(workspace_id).await?;
            Ok(NodeResponse::WorkspaceInspected { inspection })
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
            let workspace = shared.register_workspace(workspace_id, root).await?;
            Ok(NodeResponse::WorkspaceRegistered { workspace })
        }
        NodeRequest::UnregisterWorkspace { workspace_id } => {
            shared.require_controller(connection_id, role)?;
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
        NodeRequest::Resume { session, terminal_size, initial_prompt } => {
            controlled_session(shared, connection_id, role, &session)?;
            if let Some(prompt) = initial_prompt.as_deref() {
                validate_node_text("resume initial prompt", prompt)?;
            }
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
                request,
            });
            shared.arm_resume(&session, command.id)?;
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

fn normalize_windows_verbatim_path(path: String) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path
    }
}

fn validate_workspace_root(
    workspace_id: &WorkspaceId,
    canonical_root: &str,
) -> Result<(), NodeServerError> {
    if canonical_root.starts_with(r"\\") {
        return Err(NodeServerError::InvalidWorkspaceRoot {
            workspace_id: workspace_id.clone(),
            path: canonical_root.to_owned(),
            message: "UNC workspace roots are unsupported by Windows PTY providers".to_owned(),
        });
    }
    if canonical_root.is_empty()
        || canonical_root.len() > WORKING_DIRECTORY_MAX_BYTES
        || canonical_root.chars().any(char::is_control)
        || !Path::new(canonical_root).is_absolute()
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

fn persistence_failure(_error: io::Error) -> NodeFailure {
    NodeFailure {
        code: NodeFailureCode::BackendOperationFailed,
        message: DURABLE_STATE_COMMIT_FAILED_ERROR.to_owned(),
    }
}

fn node_failure_category(code: NodeFailureCode) -> &'static str {
    match code {
        NodeFailureCode::InvalidRequest => "invalid-request",
        NodeFailureCode::Unauthorized => "unauthorized",
        NodeFailureCode::ObserverReadOnly => "observer-read-only",
        NodeFailureCode::ControllerBusy => "controller-busy",
        NodeFailureCode::ControllerRequired => "controller-required",
        NodeFailureCode::UnknownWorkspace => "unknown-workspace",
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
        NodeFailureCode::UnknownSession => "unknown-session",
        NodeFailureCode::UnknownSessionRecord => "unknown-session-record",
        NodeFailureCode::SessionRecordNotResumable => "session-record-not-resumable",
        NodeFailureCode::SessionRecordBusy => "session-record-busy",
        NodeFailureCode::SessionRecordConflict => "session-record-conflict",
        NodeFailureCode::SessionWorkspaceMismatch => "session-workspace-mismatch",
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

#[derive(Debug, Error)]
pub enum NodeServerError {
    #[error("node endpoint must be a non-empty Windows named-pipe path")]
    InvalidEndpoint,
    #[error("node access token must contain 1..=4096 bytes")]
    InvalidAccessToken,
    #[error("node HTTP observer must listen on a loopback address: {0}")]
    InvalidApiListen(std::net::SocketAddr),
    #[error("node durable state path must be an absolute file path: {0}")]
    InvalidStatePath(String),
    #[error("LOCALAPPDATA is unavailable; cannot derive the product node state path")]
    LocalAppDataUnavailable,
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
    #[error("active agent registry failed: {0}")]
    Registry(String),
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
mod tests {
    use super::*;

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
            vec![AgentProvider::Claude, AgentProvider::Codex, AgentProvider::Kimi],
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
            vec![AgentProvider::Claude],
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
            vec![AgentProvider::Claude],
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
            vec![AgentProvider::Claude],
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
            vec![AgentProvider::Claude],
        );
        let record_id = SessionRecordId::new("sr-collision").unwrap();
        let original = ManagedSessionRecord {
            record_id: record_id.clone(),
            display_name: "original".to_owned(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: workspace.workspace_id,
            canonical_root: opaque_windows_path(workspace.canonical_root),
            provider_session: None,
            active_session: None,
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
            vec![AgentProvider::Claude],
        );
        let record_id = SessionRecordId::new("sr-runtime-error").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: record_id.clone(),
                display_name: "runtime error".to_owned(),
                provider: AgentProvider::Claude,
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: workspace.workspace_id,
                canonical_root: opaque_windows_path(workspace.canonical_root),
                provider_session: None,
                active_session: None,
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
            vec![AgentProvider::Claude],
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
                provider: AgentProvider::Claude,
                mode: SessionMode::Pty,
                state: ManagedSessionState::Dormant,
                workspace_id: primary.workspace_id,
                canonical_root: opaque_windows_path(primary.canonical_root),
                provider_session: Some(identity.clone()),
                active_session: None,
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
                provider: AgentProvider::Claude,
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: secondary.workspace_id,
                canonical_root: opaque_windows_path(secondary.canonical_root),
                provider_session: None,
                active_session: Some(address.clone()),
                created_at_unix_ms: 2,
                updated_at_unix_ms: 2,
                last_error: None,
            })
            .unwrap();
        shared.bind_managed_session(&address, current_id.clone());
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
        shared.reconcile_managed_record(&current_id, &address, &event, false);

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
            vec![AgentProvider::Claude],
            Some(state_path.clone()),
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
                provider: AgentProvider::Claude,
                mode: SessionMode::Pty,
                state: ManagedSessionState::Dormant,
                workspace_id: workspace.workspace_id.clone(),
                canonical_root: opaque_windows_path(workspace.canonical_root.clone()),
                provider_session: Some(persisted_identity.clone()),
                active_session: None,
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
                provider: AgentProvider::Claude,
                mode: SessionMode::Pty,
                state: ManagedSessionState::IdentityPending,
                workspace_id: workspace.workspace_id.clone(),
                canonical_root: opaque_windows_path(workspace.canonical_root.clone()),
                provider_session: None,
                active_session: Some(address.clone()),
                created_at_unix_ms: 2,
                updated_at_unix_ms: 2,
                last_error: None,
            })
            .unwrap();
        shared.bind_managed_session(&address, pending_id.clone());
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
        shared.reconcile_managed_record(&pending_id, &address, &event, false);

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
            vec![AgentProvider::Claude],
            None,
            loaded.records,
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
            vec![AgentProvider::Claude],
        );
        let record_id = SessionRecordId::new("sr-workspace").unwrap();
        shared
            .insert_record(ManagedSessionRecord {
                record_id: record_id.clone(),
                display_name: "workspace record".to_owned(),
                provider: AgentProvider::Claude,
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
            vec![AgentProvider::Claude],
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
            normalize_windows_verbatim_path(r"\\?\C:\repo\worktree".to_owned()),
            r"C:\repo\worktree",
        );
        assert_eq!(
            normalize_windows_verbatim_path(r"\\?\UNC\server\share\repo".to_owned()),
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
            vec![AgentProvider::Claude],
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
        shared.arm_resume(&original, CommandId(41)).unwrap();

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

    #[test]
    fn workspace_tree_is_relative_and_skips_heavy_directories() {
        let root = temporary_workspace_root("tree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/package")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn fixture() {}\n").unwrap();
        std::fs::write(root.join("README.md"), b"fixture\n").unwrap();

        let (entries, truncated) = collect_workspace_entries(&root);
        assert!(!truncated);
        assert!(entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("src")
                && entry.kind == WorkspaceEntryKind::Directory
        }));
        assert!(entries.iter().any(|entry| {
            entry.relative_path.as_utf8() == Some("src/lib.rs")
                && entry.kind == WorkspaceEntryKind::File
        }));
        assert!(entries.iter().all(|entry| {
            let path = entry.relative_path.as_utf8().unwrap();
            !path.starts_with(".git")
                && !path.starts_with("target")
                && !path.starts_with("node_modules")
        }));

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

        let snapshot = inspect_git_workspace(root.to_str().unwrap()).await;

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
        assert!(!snapshot.truncated, "{:?}", snapshot.diagnostic);

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
            vec![AgentProvider::Claude],
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
            truncated: false,
            diagnostic: None,
        }
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
