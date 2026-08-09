use crate::protocol::{
    C2ErrorCategory, C2NodeEvent, C2NodeFailure, C2NodeResponse, C2RelayFailure, C2RelayFailureCode, GapKind, HealthResponse, NodeCursor, NodeFreshness, NodeGap, NodeId,
    NodeIncarnationId, NodeRequest, ResolvedSpawnReceipt, RoutedNodeEvent, RoutedNodeResponse,
    ManagedWorktreeLeaseState, ManagedWorktreeSpawnRequest, SpawnOverride, SpawnSpec,
    NodeTransportState, ObservedNode, ProviderAdapterContractSupport, ProviderContractSupport,
    ReadyResponse, SanitizedError, SlimNodeInventory, StatusResponse,
    C2_API_VERSION, C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY,
    MAX_C2_GAPS_PER_NODE, MAX_C2_NODES,
};
#[cfg(windows)]
use crate::protocol::MAX_C2_ENDPOINT_BYTES;
use gate4agent_node_protocol::{
    ClientRole, FrameError, NegotiatedNodeCompatibility, NodeEvent, NodeEventEnvelope, NodeFailureCode,
    NodeResponse, NodeSnapshot, ServerFrame,
};
use gate4agent_node_wire::{LocalNodeClient, NodeClientError};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout, Instant};

const MANAGED_RESUME_SETTLE_DEADLINE: Duration = Duration::from_secs(30);
const NODE_REQUEST_IO_HEADROOM: Duration = Duration::from_secs(5);

const HEADER_LIMIT_BYTES: usize = 16 * 1024;
const MAX_HTTP_CONNECTIONS: usize = 16;
const RESPONSE_BODY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

mod control;

#[cfg(windows)]
pub const DEFAULT_C2_CONTROL_ENDPOINT: &str = r"\\.\pipe\gate4agent-c2";
#[cfg(unix)]
pub const DEFAULT_C2_CONTROL_ENDPOINT: &str = "gate4agent-c2.sock";

#[cfg(windows)]
pub fn default_c2_control_endpoint() -> Result<String, C2ConfigError> {
    Ok(DEFAULT_C2_CONTROL_ENDPOINT.to_owned())
}

#[cfg(unix)]
pub fn default_c2_control_endpoint() -> Result<String, C2ConfigError> {
    let root = unix_runtime_root()?;
    let directory = root.join("gate4agent");
    let endpoint = directory.join(DEFAULT_C2_CONTROL_ENDPOINT);
    validate_unix_endpoint(&endpoint).map_err(|_| C2ConfigError::InvalidControlEndpoint)?;
    Ok(endpoint.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn unix_runtime_root() -> Result<PathBuf, C2ConfigError> {
    if let Some(root) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if root.is_absolute() { return Ok(root); }
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty())
        .ok_or_else(|| C2ConfigError::RuntimeEndpoint(
            "neither absolute XDG_RUNTIME_DIR nor HOME is available".to_owned(),
        ))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(C2ConfigError::RuntimeEndpoint("HOME is not absolute".to_owned()));
    }
    Ok(home.join(".gate4agent").join("run"))
}

#[derive(Clone)]
pub struct C2NodeConfig {
    pub node_id: NodeId,
    pub endpoint: String,
    route: C2NodeRoute,
    token: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum C2NodeRoute {
    Local,
    SshForwardedLoopback(SocketAddr),
}

impl C2NodeConfig {
    pub fn new(node_id: NodeId, endpoint: impl Into<String>, token: impl Into<String>) -> Result<Self, C2ConfigError> {
        let endpoint = endpoint.into();
        let token = token.into();
        let (endpoint, route) = parse_node_endpoint(&endpoint)
            .ok_or_else(|| C2ConfigError::InvalidEndpoint(node_id.clone()))?;
        validate_token(&token)?;
        Ok(Self { node_id, endpoint, route, token })
    }

    fn transport_label(&self) -> &'static str {
        match self.route {
            C2NodeRoute::Local => local_transport_label(),
            C2NodeRoute::SshForwardedLoopback(_) => "ssh-forwarded-loopback",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct C2Timings {
    pub poll_interval: Duration,
    pub fresh_for: Duration,
    pub attempt_deadline: Duration,
    pub transient_backoffs: [Duration; 5],
    pub parked_backoff: Duration,
    pub http_io_deadline: Duration,
}

impl Default for C2Timings {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(250),
            fresh_for: Duration::from_secs(10),
            attempt_deadline: Duration::from_secs(5),
            transient_backoffs: [
                Duration::from_millis(500), Duration::from_secs(1), Duration::from_secs(2),
                Duration::from_secs(4), Duration::from_secs(8),
            ],
            parked_backoff: Duration::from_secs(30),
            http_io_deadline: Duration::from_secs(3),
        }
    }
}

#[derive(Clone)]
pub struct C2Config {
    pub api_listen: SocketAddr,
    pub control_endpoint: String,
    api_token: String,
    pub nodes: Vec<C2NodeConfig>,
    pub timings: C2Timings,
}

impl C2Config {
    pub fn new(api_listen: SocketAddr, api_token: impl Into<String>, nodes: Vec<C2NodeConfig>) -> Result<Self, C2ConfigError> {
        let api_token = api_token.into();
        if !api_listen.ip().is_loopback() { return Err(C2ConfigError::NonLoopback(api_listen)); }
        validate_token(&api_token)?;
        if nodes.is_empty() || nodes.len() > MAX_C2_NODES { return Err(C2ConfigError::NodeCount(nodes.len())); }
        let mut ids = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        if nodes.iter().any(|node| !ids.insert(node.node_id.clone())) { return Err(C2ConfigError::DuplicateNode); }
        if nodes.iter().any(|node| !endpoints.insert(endpoint_key(&node.endpoint))) { return Err(C2ConfigError::DuplicateEndpoint); }
        let control_endpoint = default_c2_control_endpoint()?;
        if nodes.iter().any(|node| endpoints_equal(&node.endpoint, &control_endpoint)) {
            return Err(C2ConfigError::ControlEndpointConflict);
        }
        Ok(Self { api_listen, control_endpoint, api_token, nodes, timings: C2Timings::default() })
    }

    pub fn with_timings(mut self, timings: C2Timings) -> Self {
        self.timings = timings;
        self
    }

    pub fn with_control_endpoint(mut self, endpoint: impl Into<String>) -> Result<Self, C2ConfigError> {
        let endpoint = endpoint.into();
        validate_control_endpoint(&endpoint)?;
        if self.nodes.iter().any(|node| endpoints_equal(&node.endpoint, &endpoint)) {
            return Err(C2ConfigError::ControlEndpointConflict);
        }
        self.control_endpoint = endpoint;
        Ok(self)
    }
}

fn validate_control_endpoint(endpoint: &str) -> Result<(), C2ConfigError> {
    if !valid_local_endpoint(endpoint) {
        return Err(C2ConfigError::InvalidControlEndpoint);
    }
    Ok(())
}

fn parse_node_endpoint(endpoint: &str) -> Option<(String, C2NodeRoute)> {
    if let Some(authority) = endpoint.strip_prefix("tcp://") {
        let address = authority.parse::<SocketAddr>().ok()?;
        let is_exact_loopback = match address.ip() {
            std::net::IpAddr::V4(ip) => ip == std::net::Ipv4Addr::LOCALHOST,
            std::net::IpAddr::V6(ip) => ip == std::net::Ipv6Addr::LOCALHOST,
        };
        if !is_exact_loopback || address.port() == 0 {
            return None;
        }
        return Some((format!("tcp://{address}"), C2NodeRoute::SshForwardedLoopback(address)));
    }
    if endpoint.contains("://") || !valid_local_endpoint(endpoint) {
        return None;
    }
    Some((endpoint.to_owned(), C2NodeRoute::Local))
}

#[cfg(windows)]
fn valid_local_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with(r"\\.\pipe\") && endpoint.len() > r"\\.\pipe\".len()
        && endpoint.len() <= MAX_C2_ENDPOINT_BYTES
}

#[cfg(unix)]
fn valid_local_endpoint(endpoint: &str) -> bool {
    validate_unix_endpoint(Path::new(endpoint)).is_ok()
}

#[cfg(unix)]
fn validate_unix_endpoint(endpoint: &Path) -> Result<(), ()> {
    const MAX_UNIX_ENDPOINT_BYTES: usize = 103;
    use std::os::unix::ffi::OsStrExt;

    if !endpoint.is_absolute() || endpoint.file_name().is_none()
        || endpoint.as_os_str().as_bytes().len() > MAX_UNIX_ENDPOINT_BYTES
    {
        return Err(());
    }
    Ok(())
}

#[cfg(windows)]
fn endpoint_key(endpoint: &str) -> String {
    if endpoint.starts_with("tcp://") { endpoint.to_owned() } else { endpoint.to_ascii_lowercase() }
}

#[cfg(unix)]
fn endpoint_key(endpoint: &str) -> String { endpoint.to_owned() }

fn endpoints_equal(left: &str, right: &str) -> bool { endpoint_key(left) == endpoint_key(right) }

fn validate_token(token: &str) -> Result<(), C2ConfigError> {
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(C2ConfigError::InvalidToken);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum C2ConfigError {
    #[error("C2 tokens must contain 1..=4096 visible ASCII bytes without whitespace")]
    InvalidToken,
    #[cfg_attr(windows, error("node '{0}' requires a bounded Windows named pipe or exact loopback TCP endpoint"))]
    #[cfg_attr(unix, error("node '{0}' requires a bounded local socket or exact loopback TCP endpoint"))]
    InvalidEndpoint(NodeId),
    #[error("C2 API listen address must be loopback: {0}")]
    NonLoopback(SocketAddr),
    #[error("C2 requires 1..=64 configured nodes; received {0}")]
    NodeCount(usize),
    #[error("C2 node IDs must be unique")]
    DuplicateNode,
    #[error("C2 node endpoints must be unique")]
    DuplicateEndpoint,
    #[cfg_attr(windows, error("C2 control endpoint must be a bounded local Windows named pipe"))]
    #[cfg_attr(unix, error("C2 control endpoint must be a bounded local endpoint"))]
    InvalidControlEndpoint,
    #[error("C2 control endpoint must not equal a configured node endpoint")]
    ControlEndpointConflict,
    #[cfg(unix)]
    #[error("C2 default runtime endpoint is unavailable: {0}")]
    RuntimeEndpoint(String),
}

type RelayResult = Result<RoutedNodeResponse, C2RelayFailure>;

enum RelayCommand {
    Request {
        operator_connection_id: u64,
        expected_incarnation_id: NodeIncarnationId,
        request: NodeRequest,
        reply: oneshot::Sender<RelayResult>,
    },
}

#[derive(Clone)]
struct RelayEndpoint {
    commands: mpsc::Sender<RelayCommand>,
    releases: mpsc::Sender<oneshot::Sender<()>>,
    force_disconnect: watch::Sender<u64>,
}

#[derive(Clone)]
struct OperatorHub {
    sink: Arc<Mutex<Option<OperatorEventSink>>>,
}

#[derive(Clone)]
struct OperatorEventSink {
    connection_id: u64,
    outbound: mpsc::Sender<control::QueuedFrame>,
    budget: Arc<AtomicUsize>,
    disconnect: watch::Sender<bool>,
}

impl OperatorHub {
    fn new() -> Self { Self { sink: Arc::new(Mutex::new(None)) } }

    fn attach(&self, sink: OperatorEventSink) {
        *self.sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    fn detach(&self, connection_id: u64) {
        let mut sink = self.sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if sink.as_ref().is_some_and(|current| current.connection_id == connection_id) { *sink = None; }
    }

    fn is_active(&self, connection_id: u64) -> bool {
        self.sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref().is_some_and(|current| current.connection_id == connection_id)
    }

    fn has_active_operator(&self) -> bool {
        self.sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_some()
    }

    fn publish(&self, event: RoutedNodeEvent) {
        let sink = self.sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sink) = sink.as_ref() {
            if control::queue_operator_event(&sink.outbound, &sink.budget, event).is_err() {
                let _ = sink.disconnect.send(true);
            }
        }
    }
}

fn relay_failure(
    code: C2RelayFailureCode,
    message: &'static str,
    current_incarnation_id: Option<NodeIncarnationId>,
) -> C2RelayFailure {
    C2RelayFailure { code, message: message.to_owned(), current_incarnation_id }
}

#[derive(Debug, Error)]
pub enum C2Error {
    #[error("C2 API failed: {0}")]
    Api(#[from] io::Error),
    #[error("C2 task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub struct C2ShutdownHandle {
    shutdown: watch::Sender<bool>,
}

impl C2ShutdownHandle {
    pub fn shutdown(&self) { let _ = self.shutdown.send(true); }
}

pub struct C2Running {
    api_addr: SocketAddr,
    shutdown: C2ShutdownHandle,
    task: Option<JoinHandle<Result<(), C2Error>>>,
}

impl C2Running {
    pub async fn start(config: C2Config) -> Result<Self, C2Error> {
        prepare_default_control_parent(&config.control_endpoint)?;
        let listener = TcpListener::bind(config.api_listen).await?;
        let api_addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let shutdown = C2ShutdownHandle { shutdown: shutdown_tx };
        let task = tokio::spawn(run_bound(config, listener, shutdown_rx));
        Ok(Self { api_addr, shutdown, task: Some(task) })
    }

    pub fn api_addr(&self) -> SocketAddr { self.api_addr }
    pub fn shutdown_handle(&self) -> C2ShutdownHandle { self.shutdown.clone() }
    pub async fn wait(mut self) -> Result<(), C2Error> {
        self.task.take().expect("C2 task is present").await?
    }
}

#[cfg(windows)]
fn prepare_default_control_parent(_endpoint: &str) -> io::Result<()> { Ok(()) }

#[cfg(unix)]
fn prepare_default_control_parent(endpoint: &str) -> io::Result<()> {
    let default = default_c2_control_endpoint()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    if endpoint != default { return Ok(()); }
    let parent = Path::new(endpoint).parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "default C2 endpoint has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
}

impl Drop for C2Running {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(task) = self.task.take() { task.abort(); }
    }
}

async fn run_bound(config: C2Config, listener: TcpListener, mut shutdown: watch::Receiver<bool>) -> Result<(), C2Error> {
    let now = unix_ms();
    let nodes = config.nodes.iter().map(|node| (node.node_id.clone(), ObservedNode {
        endpoint: node.endpoint.clone(), transport_label: node.transport_label().to_owned(),
        transport: NodeTransportState::Offline, freshness: NodeFreshness::Unavailable,
        cursor: None, inventory: None, last_attempt_unix_ms: None, last_success_unix_ms: None,
        consecutive_failures: 0, last_error: None, gaps: Vec::new(), gaps_truncated: 0,
    })).collect();
    let initial = Arc::new(StatusResponse { api_version: C2_API_VERSION, ready: false, observed_at_unix_ms: now, nodes });
    let (status_tx, status_rx) = watch::channel(initial);
    let (ingress_tx, ingress_rx) = mpsc::channel(config.nodes.len().saturating_mul(2).max(2));
    let mut tasks = JoinSet::new();
    let hub = OperatorHub::new();
    let mut relay_senders = BTreeMap::new();
    let mut relay_receivers = Vec::new();
    for node in config.nodes.clone() {
        let (commands_tx, commands_rx) = mpsc::channel(8);
        let (releases_tx, releases_rx) = mpsc::channel(1);
        let (force_tx, force_rx) = watch::channel(0_u64);
        relay_senders.insert(node.node_id.clone(), RelayEndpoint {
            commands: commands_tx,
            releases: releases_tx,
            force_disconnect: force_tx,
        });
        relay_receivers.push((node, commands_rx, releases_rx, force_rx));
    }
    let relay_senders = Arc::new(relay_senders);
    tasks.spawn(inventory_owner(config.nodes.len(), config.timings.fresh_for, ingress_rx, status_tx, shutdown.clone()));
    for (node, commands, releases, force_disconnect) in relay_receivers {
        tasks.spawn(node_relay_worker(node, config.timings, commands, releases, force_disconnect, ingress_tx.clone(), status_rx.clone(), hub.clone(), shutdown.clone()));
    }
    drop(ingress_tx);
    tasks.spawn(http_server(listener, config.api_token.clone(), config.timings.http_io_deadline, status_rx.clone(), shutdown.clone()));
    tasks.spawn(control::run(
        config.control_endpoint,
        config.api_token,
        relay_senders,
        status_rx,
        hub,
        shutdown.clone(),
    ));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            result = tasks.join_next() => {
                match result {
                    Some(Ok(Ok(()))) if *shutdown.borrow() => break,
                    Some(Ok(Ok(()))) => return Err(C2Error::Api(io::Error::new(io::ErrorKind::Other, "C2 task exited unexpectedly"))),
                    Some(Ok(Err(error))) => return Err(C2Error::Api(error)),
                    Some(Err(error)) => return Err(C2Error::Task(error)),
                    None => break,
                }
            }
        }
    }
    tasks.shutdown().await;
    Ok(())
}

#[cfg(windows)]
fn local_transport_label() -> &'static str { "windows-named-pipe" }

#[cfg(unix)]
fn local_transport_label() -> &'static str { "unix-domain-socket" }

#[derive(Clone, Default)]
struct ProviderContractManifest {
    provider_contracts: Vec<ProviderContractSupport>,
    provider_adapter_contracts: Vec<ProviderAdapterContractSupport>,
}

impl ProviderContractManifest {
    fn from_compatibility(compatibility: Option<&NegotiatedNodeCompatibility>) -> Self {
        let Some(compatibility) = compatibility.filter(|compatibility| {
            compatibility.capabilities.iter().any(|capability| {
                capability.as_str() == C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY
            })
        }) else {
            return Self::default();
        };
        Self {
            provider_contracts: compatibility.provider_contracts.clone(),
            provider_adapter_contracts: compatibility.provider_adapter_contracts.clone(),
        }
    }
}

enum AttemptResult {
    Connected {
        cursor: NodeCursor,
        snapshot: NodeSnapshot,
        gaps: Vec<GapKind>,
        provider_contract_manifest: ProviderContractManifest,
    },
    Success { cursor: NodeCursor, snapshot: NodeSnapshot, gaps: Vec<GapKind> },
    Cursor {
        cursor: NodeCursor,
        gaps: Vec<GapKind>,
        managed_worktree_events: Vec<NodeEvent>,
    },
    Failure { error: SanitizedError, hard: bool },
}

struct Attempt { node_id: NodeId, at_unix_ms: u64, result: AttemptResult }

async fn node_relay_worker(
    node: C2NodeConfig,
    timings: C2Timings,
    mut commands: mpsc::Receiver<RelayCommand>,
    mut releases: mpsc::Receiver<oneshot::Sender<()>>,
    mut force_disconnect: watch::Receiver<u64>,
    ingress: mpsc::Sender<Attempt>,
    status: watch::Receiver<Arc<StatusResponse>>,
    hub: OperatorHub,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut failures = 0_usize;
    loop {
        if *shutdown.borrow() { return Ok(()); }
        let previous = status.borrow().nodes.get(&node.node_id).and_then(|item| item.cursor);
        let connected = timeout(timings.attempt_deadline, connect_operator(&node)).await;
        let mut client = match connected {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                failures = failures.saturating_add(1);
                let (error, hard) = sanitize_node_error(&error);
                ingress_attempt(&ingress, &node.node_id, AttemptResult::Failure { error, hard }).await?;
                reject_disconnected_commands(&mut commands, previous);
                acknowledge_disconnected_releases(&mut releases);
                relay_backoff(&mut shutdown, timings, failures, hard).await?;
                continue;
            }
            Err(_) => {
                failures = failures.saturating_add(1);
                ingress_attempt(&ingress, &node.node_id, AttemptResult::Failure {
                    error: SanitizedError { category: C2ErrorCategory::Timeout, message: "node connection deadline exceeded".to_owned() },
                    hard: false,
                }).await?;
                reject_disconnected_commands(&mut commands, previous);
                acknowledge_disconnected_releases(&mut releases);
                relay_backoff(&mut shutdown, timings, failures, false).await?;
                continue;
            }
        };
        failures = 0;
        let hello = client.hello().clone();
        let provider_contract_manifest =
            ProviderContractManifest::from_compatibility(hello.compatibility.as_ref());
        let incarnation_id = hello.incarnation_id;
        let connection_id = hello.connection_id;
        let mut controller_owned = hello.controller.as_ref().is_some_and(|controller| controller.connection_id == connection_id);
        let mut cursor = NodeCursor { incarnation_id, sequence: hello.event_sequence };
        let mut snapshot = hello.snapshot;
        let mut gaps = Vec::new();
        let mut did_resync = false;
        if let Some(previous) = previous {
            if previous.incarnation_id != incarnation_id {
                gaps.push(GapKind::IncarnationChanged);
            } else if cursor.sequence < previous.sequence {
                gaps.push(GapKind::CursorRegression);
            } else if cursor.sequence > previous.sequence {
                match bounded_node_request(&mut client, NodeRequest::Resync { after_sequence: previous.sequence }).await {
                    Ok(NodeResponse::Resync { event_sequence, snapshot: resync_snapshot, events }) => {
                        let resync_gaps = validate_resync(previous.sequence, cursor.sequence, event_sequence, &events);
                        if resync_gaps.is_empty() {
                            publish_recovered_events(&node.node_id, incarnation_id, &events, &hub);
                        } else {
                            hub.publish(RoutedNodeEvent {
                                node_id: node.node_id.clone(),
                                cursor: NodeCursor { incarnation_id, sequence: event_sequence },
                                event: C2NodeEvent::ResyncRequired {
                                    oldest_available_sequence: events.first().map_or(event_sequence, |event| event.sequence),
                                },
                            });
                        }
                        gaps.extend(resync_gaps);
                        cursor.sequence = event_sequence;
                        snapshot = resync_snapshot;
                        did_resync = true;
                    }
                    Ok(_) => gaps.push(GapKind::NonContiguousEvents),
                    Err(error) => {
                        ingress_attempt(&ingress, &node.node_id, relay_failure_attempt(&error)).await?;
                        reject_disconnected_commands(&mut commands, Some(cursor));
                        continue;
                    }
                }
            }
        }
        ingress_attempt(&ingress, &node.node_id, AttemptResult::Connected {
            cursor,
            snapshot,
            gaps,
            provider_contract_manifest,
        }).await?;
        if let Err(error) = drain_pending_events(&mut client, &node.node_id, &mut cursor, &hub, &ingress, did_resync, None).await {
            ingress_attempt(&ingress, &node.node_id, relay_failure_attempt(&error)).await?;
            reject_disconnected_commands(&mut commands, Some(cursor));
            continue;
        }

        let cadence = timings.poll_interval.max(Duration::from_millis(1)).min(Duration::from_millis(250));
        let mut snapshot_tick = tokio::time::interval(cadence);
        snapshot_tick.reset();
        let mut lease_tick = tokio::time::interval(Duration::from_secs(30));
        lease_tick.reset();
        let disconnect_error = loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    tokio::select! {
                        result = handle_relay_command(
                            &mut client, command, &node.node_id, incarnation_id, connection_id,
                            &mut controller_owned, &hub, &mut cursor, &ingress,
                        ) => match result {
                            Ok(()) => {}
                            Err(error) => break error,
                        },
                        changed = force_disconnect.changed() => {
                            let _ = changed;
                            break NodeClientError::Io(io::Error::new(io::ErrorKind::ConnectionAborted, "node relay cleanup forced reconnect"));
                        }
                    }
                }
                release = releases.recv() => {
                    let Some(reply) = release else { return Ok(()); };
                    let result = release_controller(&mut client, &mut controller_owned).await;
                    let _ = reply.send(());
                    if let Err(error) = result { break error; }
                }
                changed = force_disconnect.changed() => {
                    let _ = changed;
                    break NodeClientError::Io(io::Error::new(io::ErrorKind::ConnectionAborted, "node relay cleanup forced reconnect"));
                }
                frame = client.recv() => {
                    match frame {
                        Ok(ServerFrame::Event(envelope)) => {
                            if let Err(error) = handle_live_node_event(
                                &mut client,
                                &node.node_id,
                                envelope,
                                &mut cursor,
                                &hub,
                                &ingress,
                            ).await {
                                break error;
                            }
                        }
                        Ok(ServerFrame::Reply(_)
                            | ServerFrame::Challenge(_)
                            | ServerFrame::Hello(_)) => {
                            break NodeClientError::Protocol(
                                "node sent an unexpected idle frame".to_owned(),
                            );
                        }
                        Err(error) => break error,
                    }
                }
                _ = snapshot_tick.tick() => {
                    match bounded_node_request(&mut client, NodeRequest::Snapshot).await {
                        Ok(NodeResponse::Snapshot { event_sequence, snapshot, .. }) => {
                            if let Err(error) = drain_pending_events(&mut client, &node.node_id, &mut cursor, &hub, &ingress, false, None).await { break error; }
                            if event_sequence >= cursor.sequence {
                                cursor.sequence = event_sequence;
                                ingress_attempt(&ingress, &node.node_id, AttemptResult::Success { cursor, snapshot, gaps: Vec::new() }).await?;
                            }
                        }
                        Ok(_) => break NodeClientError::Protocol("snapshot returned a different response".to_owned()),
                        Err(error) => break error,
                    }
                }
                _ = lease_tick.tick() => {
                    if controller_owned {
                        if hub.has_active_operator() {
                            match acquire_controller(&mut client, connection_id).await {
                                Ok(owned) => controller_owned = owned,
                                Err(error) => break error,
                            }
                        } else if let Err(error) = release_controller(&mut client, &mut controller_owned).await {
                            break error;
                        }
                        if let Err(error) = drain_pending_events(&mut client, &node.node_id, &mut cursor, &hub, &ingress, false, None).await { break error; }
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        let _ = release_controller(&mut client, &mut controller_owned).await;
                        return Ok(());
                    }
                }
            }
        };
        let (error, hard) = sanitize_node_error(&disconnect_error);
        ingress_attempt(&ingress, &node.node_id, AttemptResult::Failure { error, hard }).await?;
        reject_disconnected_commands(&mut commands, Some(cursor));
        failures = failures.saturating_add(1);
        relay_backoff(&mut shutdown, timings, failures, hard).await?;
    }
}

async fn connect_operator(node: &C2NodeConfig) -> Result<LocalNodeClient, NodeClientError> {
    match node.route {
        C2NodeRoute::Local => {
            LocalNodeClient::connect(&node.endpoint, &node.node_id, ClientRole::Operator, &node.token).await
        }
        C2NodeRoute::SshForwardedLoopback(endpoint) => {
            LocalNodeClient::connect_loopback(endpoint, &node.node_id, ClientRole::Operator, &node.token).await
        }
    }
}

async fn bounded_node_request(
    client: &mut LocalNodeClient,
    request: NodeRequest,
) -> Result<NodeResponse, NodeClientError> {
    bounded_node_request_with_deadline(client, request, None).await
}

async fn bounded_node_request_with_deadline(
    client: &mut LocalNodeClient,
    request: NodeRequest,
    relay_deadline: Option<Instant>,
) -> Result<NodeResponse, NodeClientError> {
    let deadline = request_budget(&request, relay_deadline, Instant::now());
    if deadline.is_zero() {
        return Err(NodeClientError::Frame(FrameError::PrefixTimedOut));
    }
    match timeout(deadline, client.request(request)).await {
        Ok(result) => result,
        Err(_) => Err(NodeClientError::Frame(FrameError::PrefixTimedOut)),
    }
}

fn relay_request_deadline(request: &NodeRequest, now: Instant) -> Option<Instant> {
    matches!(request, NodeRequest::ResumeSessionRecord { .. })
        .then(|| now + node_request_deadline(request))
}

fn request_budget(request: &NodeRequest, relay_deadline: Option<Instant>, now: Instant) -> Duration {
    relay_deadline
        .map(|deadline| deadline.saturating_duration_since(now))
        .unwrap_or_else(|| node_request_deadline(request))
}

fn node_request_deadline(request: &NodeRequest) -> Duration {
    match request {
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::RenameSessionRecord { .. }
        | NodeRequest::ForgetSessionRecord { .. } => Duration::from_secs(5),
        NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::CleanupManagedWorktree { .. } => Duration::from_secs(240),
        NodeRequest::Spawn { .. }
        | NodeRequest::Resume { .. }
        | NodeRequest::Stop { .. } => Duration::from_secs(15),
        NodeRequest::SpawnSpec { spec } =>
            Duration::from_millis(spec.deadline_ms.get()) + NODE_REQUEST_IO_HEADROOM,
        NodeRequest::SpawnManagedWorktree { request } =>
            Duration::from_millis(request.spawn_spec.deadline_ms.get())
                + NODE_REQUEST_IO_HEADROOM,
        NodeRequest::ResumeSessionRecord { .. } =>
            MANAGED_RESUME_SETTLE_DEADLINE + NODE_REQUEST_IO_HEADROOM,
        _ => Duration::from_secs(10),
    }
}

fn validate_resync(previous: u64, hello: u64, current: u64, events: &[NodeEventEnvelope]) -> Vec<GapKind> {
    let mut gaps = validate_events(previous, current, events);
    if current < hello && !gaps.contains(&GapKind::CursorRegression) {
        gaps.push(GapKind::CursorRegression);
    }
    gaps
}

async fn ingress_attempt(
    ingress: &mpsc::Sender<Attempt>,
    node_id: &NodeId,
    result: AttemptResult,
) -> io::Result<()> {
    ingress.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result })
        .await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "inventory owner closed"))
}

async fn relay_backoff(
    shutdown: &mut watch::Receiver<bool>,
    timings: C2Timings,
    failures: usize,
    hard: bool,
) -> io::Result<()> {
    let delay = if hard || failures >= timings.transient_backoffs.len() {
        timings.parked_backoff
    } else {
        timings.transient_backoffs[failures.saturating_sub(1)]
    };
    tokio::select! {
        _ = sleep(delay) => Ok(()),
        _ = shutdown.changed() => {
            Ok(())
        }
    }
}

fn relay_failure_attempt(error: &NodeClientError) -> AttemptResult {
    let (error, hard) = sanitize_node_error(error);
    AttemptResult::Failure { error, hard }
}

async fn handle_relay_command(
    client: &mut LocalNodeClient,
    command: RelayCommand,
    node_id: &NodeId,
    incarnation_id: NodeIncarnationId,
    connection_id: u64,
    controller_owned: &mut bool,
    hub: &OperatorHub,
    cursor: &mut NodeCursor,
    ingress: &mpsc::Sender<Attempt>,
) -> Result<(), NodeClientError> {
    match command {
        RelayCommand::Request { operator_connection_id, expected_incarnation_id, request, reply } => {
            let relay_deadline = relay_request_deadline(&request, Instant::now());
            let expected_spawn = match &request {
                NodeRequest::SpawnSpec { spec } => Some(ExpectedSpawnRequest::Spec(spec.clone())),
                NodeRequest::SpawnManagedWorktree { request } => {
                    Some(ExpectedSpawnRequest::Managed(request.clone()))
                }
                _ => None,
            };
            if !hub.is_active(operator_connection_id) {
                let _ = reply.send(Err(relay_failure(C2RelayFailureCode::ClientLagged, "C2 operator connection is no longer active", Some(incarnation_id))));
                return Ok(());
            }
            if expected_incarnation_id != incarnation_id {
                let _ = reply.send(Err(relay_failure(C2RelayFailureCode::StaleNodeIncarnation, "node incarnation changed", Some(incarnation_id))));
                return Ok(());
            }
            if !is_read_only_request(&request) && !*controller_owned {
                match acquire_controller_with_deadline(client, connection_id, relay_deadline).await {
                    Ok(owned) if owned => *controller_owned = true,
                    Ok(_) => {
                        let _ = reply.send(Err(relay_failure(C2RelayFailureCode::RelayBusy, "node controller lease is unavailable", Some(incarnation_id))));
                        return Ok(());
                    }
                    Err(error) if relay_node_failure(&error).is_some() => {
                        let failure = relay_node_failure(&error)
                            .expect("guarded node request failure");
                        let _ = reply.send(Ok(RoutedNodeResponse {
                            node_id: node_id.clone(),
                            incarnation_id,
                            response: Err(failure),
                        }));
                        return Ok(());
                    }
                    Err(error) => {
                        let _ = reply.send(Err(relay_failure(C2RelayFailureCode::NodeOffline, "node relay disconnected", Some(incarnation_id))));
                        return Err(error);
                    }
                }
            }
            let response = match bounded_node_request_with_deadline(client, request, relay_deadline).await {
                Ok(response) => Ok(response),
                Err(NodeClientError::Node(failure)) => Err(failure),
                Err(error @ NodeClientError::UnsupportedCapability(_)) => {
                    let failure = relay_node_failure(&error)
                        .expect("unsupported capability is a routed node failure");
                    let _ = reply.send(Ok(RoutedNodeResponse {
                        node_id: node_id.clone(),
                        incarnation_id,
                        response: Err(failure),
                    }));
                    return Ok(());
                }
                Err(error) => {
                    let _ = reply.send(Err(relay_failure(C2RelayFailureCode::NodeOffline, "node relay disconnected", Some(incarnation_id))));
                    return Err(error);
                }
            };
            if let Err(message) = validate_spawn_spec_response(
                expected_spawn.as_ref(),
                &response,
                incarnation_id,
            ) {
                let _ = reply.send(Err(relay_failure(
                    C2RelayFailureCode::NodeOffline,
                    "node relay returned an invalid spawn receipt",
                    Some(incarnation_id),
                )));
                return Err(NodeClientError::Protocol(message.to_owned()));
            }
            drain_pending_events(client, node_id, cursor, hub, ingress, false, relay_deadline).await?;
            update_inventory_from_response(node_id, cursor, &response, ingress).await
                .map_err(NodeClientError::Io)?;
            let response = response
                .map(|response| C2NodeResponse::from(&response))
                .map_err(|failure| C2NodeFailure::from(&failure));
            let _ = reply.send(Ok(RoutedNodeResponse { node_id: node_id.clone(), incarnation_id, response }));
        }
    }
    Ok(())
}

enum ExpectedSpawnRequest {
    Spec(SpawnSpec),
    Managed(ManagedWorktreeSpawnRequest),
}

fn validate_spawn_spec_response(
    expected: Option<&ExpectedSpawnRequest>,
    response: &Result<NodeResponse, gate4agent_node_protocol::NodeFailure>,
    relay_incarnation_id: NodeIncarnationId,
) -> Result<(), &'static str> {
    match (expected, response) {
        (Some(ExpectedSpawnRequest::Spec(spec)), Ok(NodeResponse::SpawnSpecAccepted { receipt })) => {
            validate_spawn_receipt(spec, receipt, relay_incarnation_id)
        }
        (
            Some(ExpectedSpawnRequest::Managed(request)),
            Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt }),
        ) => validate_managed_spawn_receipt(request, receipt, relay_incarnation_id),
        (Some(_), Ok(_)) => return Err("spawn spec request returned a different response"),
        (Some(_), Err(_)) => return Ok(()),
        (None, Ok(NodeResponse::SpawnSpecAccepted { .. }
            | NodeResponse::ManagedWorktreeSpawnAccepted { .. })) => {
            return Err("unexpected spawn receipt for a different node request");
        }
        (None, _) => return Ok(()),
    }
}

fn validate_managed_spawn_receipt(
    request: &ManagedWorktreeSpawnRequest,
    receipt: &gate4agent_node_protocol::ManagedWorktreeSpawnReceipt,
    relay_incarnation_id: NodeIncarnationId,
) -> Result<(), &'static str> {
    if receipt.lease.source_workspace_id != request.spawn_spec.target.workspace_id
        || receipt.lease.profile_id != request.worktree_profile_id
        || receipt.lease.state != ManagedWorktreeLeaseState::InUse
        || receipt.lease.cleanup_failure.is_some()
        || receipt.lease.active_session_count != 1
        || receipt.spawn.target.node_id != request.spawn_spec.target.node_id
        || receipt.spawn.target.workspace_id != request.spawn_spec.target.workspace_id
        || receipt.spawn.target.worktree_id.as_ref() != Some(&receipt.lease.workspace_id)
        || receipt.spawn.session.workspace_id != receipt.lease.workspace_id
    {
        return Err("managed spawn receipt does not match routed request");
    }
    let mut resolved_spec = request.spawn_spec.clone();
    resolved_spec.target.worktree_id = Some(receipt.lease.workspace_id.clone());
    validate_spawn_receipt(&resolved_spec, &receipt.spawn, relay_incarnation_id)
}

fn validate_spawn_receipt(
    spec: &SpawnSpec,
    receipt: &ResolvedSpawnReceipt,
    relay_incarnation_id: NodeIncarnationId,
) -> Result<(), &'static str> {
    if receipt.incarnation_id != relay_incarnation_id
        || receipt.target != spec.target
        || receipt.profile_id != spec.profile_id
        || receipt.idempotency_key != spec.idempotency_key
        || receipt.deadline_ms != spec.deadline_ms
        || receipt.required_capabilities != spec.required_capabilities
        || receipt.bundle.as_ref().is_some_and(|bundle| {
            receipt.bundle_id.as_ref() != Some(&bundle.id)
        })
        || &receipt.session.workspace_id
            != spec
                .target
                .worktree_id
                .as_ref()
                .unwrap_or(&spec.target.workspace_id)
    {
        return Err("spawn receipt does not match routed request");
    }
    if !required_override_matches(&spec.overrides.provider, &receipt.provider)
        || !required_override_matches(&spec.overrides.mode, &receipt.mode)
        || !required_override_matches(&spec.overrides.terminal_size, &receipt.terminal_size)
        || !optional_override_matches(&spec.overrides.bundle_id, &receipt.bundle_id)
        || !optional_override_matches(&spec.overrides.context_id, &receipt.context_id)
        || !environment_profile_override_matches(
            &spec.overrides.environment_profile_id,
            receipt.environment_profile.as_ref(),
        )
    {
        return Err("spawn receipt contradicts explicit overrides");
    }
    match &spec.overrides.prompt {
        SpawnOverride::Inherit => {}
        SpawnOverride::Set { value }
            if receipt.prompt.present
                && receipt.prompt.byte_len == u32::try_from(value.byte_len()).unwrap_or(0) => {}
        SpawnOverride::Clear if !receipt.prompt.present && receipt.prompt.byte_len == 0 => {}
        SpawnOverride::Set { .. } | SpawnOverride::Clear => {
            return Err("spawn receipt contradicts explicit prompt override");
        }
    }
    Ok(())
}

fn environment_profile_override_matches(
    expected: &SpawnOverride<gate4agent_node_protocol::SpawnEnvironmentProfileId>,
    actual: Option<&gate4agent_node_protocol::ResolvedEnvironmentProfileReceipt>,
) -> bool {
    match expected {
        SpawnOverride::Inherit => true,
        SpawnOverride::Set { value } => {
            actual.is_some_and(|receipt| &receipt.profile_id == value)
        }
        SpawnOverride::Clear => actual.is_none(),
    }
}

fn required_override_matches<T: Eq>(override_value: &SpawnOverride<T>, actual: &T) -> bool {
    match override_value {
        SpawnOverride::Inherit => true,
        SpawnOverride::Set { value } => value == actual,
        SpawnOverride::Clear => false,
    }
}

fn optional_override_matches<T: Eq>(
    override_value: &SpawnOverride<T>,
    actual: &Option<T>,
) -> bool {
    match override_value {
        SpawnOverride::Inherit => true,
        SpawnOverride::Set { value } => actual.as_ref() == Some(value),
        SpawnOverride::Clear => actual.is_none(),
    }
}

fn relay_node_failure(error: &NodeClientError) -> Option<C2NodeFailure> {
    match error {
        NodeClientError::Node(failure) => Some(C2NodeFailure::from(failure)),
        NodeClientError::UnsupportedCapability(_) => Some(C2NodeFailure {
            code: NodeFailureCode::UnsupportedCapability,
            message: "required capability unavailable".to_owned(),
        }),
        NodeClientError::Io(_)
        | NodeClientError::Frame(_)
        | NodeClientError::Protocol(_)
        | NodeClientError::AuthenticationTimedOut
        | NodeClientError::Authentication(_)
        | NodeClientError::RequestIdExhausted => None,
    }
}

fn is_read_only_request(request: &NodeRequest) -> bool {
    matches!(request,
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
    )
}

async fn acquire_controller(
    client: &mut LocalNodeClient,
    connection_id: u64,
) -> Result<bool, NodeClientError> {
    acquire_controller_with_deadline(client, connection_id, None).await
}

async fn acquire_controller_with_deadline(
    client: &mut LocalNodeClient,
    connection_id: u64,
    relay_deadline: Option<Instant>,
) -> Result<bool, NodeClientError> {
    match bounded_node_request_with_deadline(
        client,
        NodeRequest::AcquireController { lease_ms: gate4agent_node_protocol::MAX_CONTROLLER_LEASE_MS },
        relay_deadline,
    ).await? {
        NodeResponse::Controller { controller } => Ok(controller.as_ref().is_some_and(|state| state.connection_id == connection_id)),
        _ => Err(NodeClientError::Protocol("controller acquisition returned a different response".to_owned())),
    }
}

async fn release_controller(
    client: &mut LocalNodeClient,
    controller_owned: &mut bool,
) -> Result<(), NodeClientError> {
    if !*controller_owned { return Ok(()); }
    match bounded_node_request(client, NodeRequest::ReleaseController).await? {
        NodeResponse::Controller { .. } => { *controller_owned = false; Ok(()) }
        _ => Err(NodeClientError::Protocol("controller release returned a different response".to_owned())),
    }
}

async fn update_inventory_from_response(
    node_id: &NodeId,
    cursor: &mut NodeCursor,
    response: &Result<NodeResponse, gate4agent_node_protocol::NodeFailure>,
    ingress: &mpsc::Sender<Attempt>,
) -> io::Result<()> {
    match response {
        Ok(NodeResponse::Snapshot { event_sequence, snapshot, .. })
        | Ok(NodeResponse::Resync { event_sequence, snapshot, .. }) => {
            if *event_sequence < cursor.sequence { return Ok(()); }
            cursor.sequence = *event_sequence;
            ingress_attempt(ingress, node_id, AttemptResult::Success { cursor: *cursor, snapshot: snapshot.clone(), gaps: Vec::new() }).await
        }
        _ => Ok(()),
    }
}

fn publish_recovered_events(
    node_id: &NodeId,
    incarnation_id: NodeIncarnationId,
    events: &[NodeEventEnvelope],
    hub: &OperatorHub,
) {
    for envelope in events {
        hub.publish(RoutedNodeEvent {
            node_id: node_id.clone(),
            cursor: NodeCursor { incarnation_id, sequence: envelope.sequence },
            event: C2NodeEvent::from(&envelope.event),
        });
    }
}

async fn drain_pending_events(
    client: &mut LocalNodeClient,
    node_id: &NodeId,
    cursor: &mut NodeCursor,
    hub: &OperatorHub,
    ingress: &mpsc::Sender<Attempt>,
    skip_replayed: bool,
    relay_deadline: Option<Instant>,
) -> Result<(), NodeClientError> {
    let mut skip_replayed = skip_replayed;
    for repair_pass in 0..=1 {
        let mut gaps = Vec::new();
        let mut managed_worktree_events = Vec::new();
        let mut changed = false;
        let mut repair = false;
        while let Some(envelope) = client.take_event() {
            if skip_replayed && envelope.sequence <= cursor.sequence { continue; }
            let resync_required = matches!(&envelope.event, gate4agent_node_protocol::NodeEvent::ResyncRequired { .. });
            if resync_required || envelope.sequence != cursor.sequence.saturating_add(1) {
                gaps.push(if envelope.sequence <= cursor.sequence && !resync_required {
                    GapKind::CursorRegression
                } else if resync_required {
                    GapKind::HistoryEvicted
                } else {
                    GapKind::NonContiguousEvents
                });
                repair = true;
                continue;
            }
            let event_cursor = NodeCursor { incarnation_id: cursor.incarnation_id, sequence: envelope.sequence };
            if matches!(
                &envelope.event,
                NodeEvent::ManagedWorktreeUpserted { .. }
                    | NodeEvent::ManagedWorktreeRemoved { .. }
            ) {
                managed_worktree_events.push(envelope.event.clone());
            }
            hub.publish(RoutedNodeEvent {
                node_id: node_id.clone(),
                cursor: event_cursor,
                event: C2NodeEvent::from(&envelope.event),
            });
            cursor.sequence = envelope.sequence;
            changed = true;
        }
        if !repair {
            if changed || !gaps.is_empty() {
                ingress_attempt(ingress, node_id, AttemptResult::Cursor {
                    cursor: *cursor,
                    gaps,
                    managed_worktree_events,
                }).await
                    .map_err(NodeClientError::Io)?;
            }
            return Ok(());
        }
        if repair_pass == 1 {
            ingress_attempt(ingress, node_id, AttemptResult::Cursor {
                cursor: *cursor,
                gaps,
                managed_worktree_events,
            }).await
                .map_err(NodeClientError::Io)?;
            return Err(NodeClientError::Protocol("node event stream remained noncontiguous after resync".to_owned()));
        }
        let after_sequence = cursor.sequence;
        let response = bounded_node_request_with_deadline(
            client,
            NodeRequest::Resync { after_sequence },
            relay_deadline,
        ).await?;
        let NodeResponse::Resync { event_sequence, snapshot, events } = response else {
            return Err(NodeClientError::Protocol("event repair resync returned a different response".to_owned()));
        };
        let repair_gaps = validate_events(after_sequence, event_sequence, &events);
        let contiguous = repair_gaps.is_empty();
        gaps.extend(repair_gaps);
        if contiguous {
            for envelope in events.iter().filter(|event| event.sequence > after_sequence) {
                hub.publish(RoutedNodeEvent {
                    node_id: node_id.clone(),
                    cursor: NodeCursor { incarnation_id: cursor.incarnation_id, sequence: envelope.sequence },
                    event: C2NodeEvent::from(&envelope.event),
                });
            }
        } else {
            hub.publish(RoutedNodeEvent {
                node_id: node_id.clone(),
                cursor: NodeCursor { incarnation_id: cursor.incarnation_id, sequence: event_sequence },
                event: C2NodeEvent::ResyncRequired {
                    oldest_available_sequence: events.first().map_or(event_sequence, |event| event.sequence),
                },
            });
        }
        cursor.sequence = event_sequence;
        ingress_attempt(ingress, node_id, AttemptResult::Success { cursor: *cursor, snapshot, gaps }).await
            .map_err(NodeClientError::Io)?;
        skip_replayed = true;
    }
    Ok(())
}

async fn handle_live_node_event(
    client: &mut LocalNodeClient,
    node_id: &NodeId,
    envelope: NodeEventEnvelope,
    cursor: &mut NodeCursor,
    hub: &OperatorHub,
    ingress: &mpsc::Sender<Attempt>,
) -> Result<(), NodeClientError> {
    if let Some(gap) = live_event_gap(cursor.sequence, &envelope) {
        let after_sequence = cursor.sequence;
        let response = bounded_node_request(
            client,
            NodeRequest::Resync { after_sequence },
        ).await?;
        let NodeResponse::Resync { event_sequence, snapshot, events } = response else {
            return Err(NodeClientError::Protocol(
                "live event repair resync returned a different response".to_owned(),
            ));
        };
        let mut gaps = vec![gap];
        let repair_gaps = validate_events(after_sequence, event_sequence, &events);
        if repair_gaps.is_empty() {
            publish_recovered_events(node_id, cursor.incarnation_id, &events, hub);
        } else {
            hub.publish(RoutedNodeEvent {
                node_id: node_id.clone(),
                cursor: NodeCursor {
                    incarnation_id: cursor.incarnation_id,
                    sequence: event_sequence,
                },
                event: C2NodeEvent::ResyncRequired {
                    oldest_available_sequence: events
                        .first()
                        .map_or(event_sequence, |event| event.sequence),
                },
            });
        }
        gaps.extend(repair_gaps);
        cursor.sequence = event_sequence;
        ingress_attempt(
            ingress,
            node_id,
            AttemptResult::Success {
                cursor: *cursor,
                snapshot,
                gaps,
            },
        ).await.map_err(NodeClientError::Io)?;
        return drain_pending_events(
            client,
            node_id,
            cursor,
            hub,
            ingress,
            true,
            None,
        ).await;
    }

    cursor.sequence = envelope.sequence;
    let managed_worktree_events = matches!(
        &envelope.event,
        NodeEvent::ManagedWorktreeUpserted { .. }
            | NodeEvent::ManagedWorktreeRemoved { .. }
    )
    .then(|| vec![envelope.event.clone()])
    .unwrap_or_default();
    hub.publish(RoutedNodeEvent {
        node_id: node_id.clone(),
        cursor: *cursor,
        event: C2NodeEvent::from(&envelope.event),
    });
    ingress_attempt(
        ingress,
        node_id,
        AttemptResult::Cursor {
            cursor: *cursor,
            gaps: Vec::new(),
            managed_worktree_events,
        },
    ).await.map_err(NodeClientError::Io)
}

fn live_event_gap(previous: u64, envelope: &NodeEventEnvelope) -> Option<GapKind> {
    if matches!(
        &envelope.event,
        gate4agent_node_protocol::NodeEvent::ResyncRequired { .. }
    ) {
        return Some(GapKind::HistoryEvicted);
    }
    if envelope.sequence <= previous {
        return Some(GapKind::CursorRegression);
    }
    (envelope.sequence != previous.saturating_add(1))
        .then_some(GapKind::NonContiguousEvents)
}

fn reject_disconnected_commands(commands: &mut mpsc::Receiver<RelayCommand>, cursor: Option<NodeCursor>) {
    while let Ok(command) = commands.try_recv() {
        match command {
            RelayCommand::Request { reply, .. } => {
                let _ = reply.send(Err(relay_failure(
                    C2RelayFailureCode::NodeOffline,
                    "node relay disconnected before request dispatch",
                    cursor.map(|value| value.incarnation_id),
                )));
            }
        }
    }
}

fn acknowledge_disconnected_releases(releases: &mut mpsc::Receiver<oneshot::Sender<()>>) {
    while let Ok(reply) = releases.try_recv() { let _ = reply.send(()); }
}

fn validate_events(previous: u64, current: u64, events: &[NodeEventEnvelope]) -> Vec<GapKind> {
    if current < previous { return vec![GapKind::CursorRegression]; }
    if current == previous { return Vec::new(); }
    let expected_first = previous.saturating_add(1);
    if events.first().map(|event| event.sequence) != Some(expected_first) {
        return vec![GapKind::HistoryEvicted];
    }
    let contiguous = events.windows(2).all(|pair| pair[0].sequence.checked_add(1) == Some(pair[1].sequence));
    if !contiguous || events.last().map(|event| event.sequence) != Some(current) {
        return vec![GapKind::NonContiguousEvents];
    }
    Vec::new()
}

fn sanitize_node_error(error: &NodeClientError) -> (SanitizedError, bool) {
    let (category, message, hard) = match error {
        NodeClientError::Protocol(message) if message.contains("identity mismatch") =>
            (C2ErrorCategory::Identity, "node identity mismatch", true),
        NodeClientError::Protocol(message) if message.contains("access-token proof") || message.contains("access denied") =>
            (C2ErrorCategory::Authentication, "node authentication failed", true),
        NodeClientError::Protocol(_) | NodeClientError::Frame(FrameError::Json(_) | FrameError::InvalidLength { .. }) =>
            (C2ErrorCategory::Protocol, "node protocol failed", true),
        NodeClientError::UnsupportedCapability(_) =>
            (C2ErrorCategory::Protocol, "node capability unavailable", true),
        NodeClientError::Node(failure) if failure.code == NodeFailureCode::Unauthorized =>
            (C2ErrorCategory::Authentication, "node request authentication failed", true),
        NodeClientError::Node(_) =>
            (C2ErrorCategory::Protocol, "node rejected observer request", true),
        NodeClientError::Frame(FrameError::BodyTimedOut { .. } | FrameError::PrefixTimedOut)
            | NodeClientError::AuthenticationTimedOut =>
            (C2ErrorCategory::Timeout, "node observation deadline exceeded", false),
        NodeClientError::Authentication(_) | NodeClientError::RequestIdExhausted =>
            (C2ErrorCategory::Internal, "node client failed internally", true),
        NodeClientError::Io(_) | NodeClientError::Frame(FrameError::Io(_)) =>
            (C2ErrorCategory::Transport, "node transport unavailable", false),
    };
    (SanitizedError { category, message: message.to_owned() }, hard)
}

async fn inventory_owner(
    configured: usize, fresh_for: Duration, mut ingress: mpsc::Receiver<Attempt>,
    status: watch::Sender<Arc<StatusResponse>>, mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut current = (**status.borrow()).clone();
    let mut attempted = BTreeSet::new();
    loop {
        tokio::select! {
            attempt = ingress.recv() => {
                let Some(attempt) = attempt else { return Ok(()); };
                attempted.insert(attempt.node_id.clone());
                let node = current.nodes.get_mut(&attempt.node_id).expect("configured poller node exists");
                node.last_attempt_unix_ms = Some(attempt.at_unix_ms);
                match attempt.result {
                    AttemptResult::Connected {
                        cursor,
                        snapshot,
                        gaps,
                        provider_contract_manifest,
                    } => {
                        let previous = node.cursor;
                        let mut inventory = SlimNodeInventory::from_snapshot(&snapshot);
                        inventory.provider_contracts =
                            provider_contract_manifest.provider_contracts;
                        inventory.provider_adapter_contracts =
                            provider_contract_manifest.provider_adapter_contracts;
                        node.transport = NodeTransportState::Online;
                        node.freshness = NodeFreshness::Fresh;
                        node.cursor = Some(cursor);
                        node.inventory = Some(inventory);
                        node.last_success_unix_ms = Some(attempt.at_unix_ms);
                        node.consecutive_failures = 0;
                        node.last_error = None;
                        for kind in gaps {
                            if node.gaps.len() == MAX_C2_GAPS_PER_NODE { node.gaps.remove(0); node.gaps_truncated += 1; }
                            node.gaps.push(NodeGap { kind, detected_at_unix_ms: attempt.at_unix_ms, previous, observed: cursor });
                        }
                    }
                    AttemptResult::Success { cursor, snapshot, gaps } => {
                        let previous = node.cursor;
                        let provider_contract_manifest = node.inventory.as_ref().map(|inventory| {
                            ProviderContractManifest {
                                provider_contracts: inventory.provider_contracts.clone(),
                                provider_adapter_contracts: inventory.provider_adapter_contracts.clone(),
                            }
                        }).unwrap_or_default();
                        let mut inventory = SlimNodeInventory::from_snapshot(&snapshot);
                        inventory.provider_contracts =
                            provider_contract_manifest.provider_contracts;
                        inventory.provider_adapter_contracts =
                            provider_contract_manifest.provider_adapter_contracts;
                        node.transport = NodeTransportState::Online;
                        node.freshness = NodeFreshness::Fresh;
                        node.cursor = Some(cursor);
                        node.inventory = Some(inventory);
                        node.last_success_unix_ms = Some(attempt.at_unix_ms);
                        node.consecutive_failures = 0;
                        node.last_error = None;
                        for kind in gaps {
                            if node.gaps.len() == MAX_C2_GAPS_PER_NODE { node.gaps.remove(0); node.gaps_truncated += 1; }
                            node.gaps.push(NodeGap { kind, detected_at_unix_ms: attempt.at_unix_ms, previous, observed: cursor });
                        }
                    }
                    AttemptResult::Cursor {
                        cursor,
                        gaps,
                        managed_worktree_events,
                    } => {
                        let previous = node.cursor;
                        let incarnation_changed = previous.is_some_and(|previous| {
                            previous.incarnation_id != cursor.incarnation_id
                        });
                        apply_managed_worktree_cursor(
                            node.inventory.as_mut(),
                            incarnation_changed,
                            &managed_worktree_events,
                        );
                        node.transport = NodeTransportState::Online;
                        node.freshness = NodeFreshness::Fresh;
                        node.cursor = Some(cursor);
                        node.last_success_unix_ms = Some(attempt.at_unix_ms);
                        node.consecutive_failures = 0;
                        node.last_error = None;
                        for kind in gaps {
                            if node.gaps.len() == MAX_C2_GAPS_PER_NODE { node.gaps.remove(0); node.gaps_truncated += 1; }
                            node.gaps.push(NodeGap { kind, detected_at_unix_ms: attempt.at_unix_ms, previous, observed: cursor });
                        }
                    }
                    AttemptResult::Failure { error, hard } => {
                        node.consecutive_failures = node.consecutive_failures.saturating_add(1);
                        node.transport = if hard || node.consecutive_failures >= 5 { NodeTransportState::Parked } else { NodeTransportState::Offline };
                        node.last_error = Some(error);
                    }
                }
                current.ready = attempted.len() == configured;
                refresh_freshness(&mut current, fresh_for);
                current.observed_at_unix_ms = unix_ms();
                status.send_replace(Arc::new(current.clone()));
            }
            _ = sleep(Duration::from_millis(250)) => {
                refresh_freshness(&mut current, fresh_for);
                current.observed_at_unix_ms = unix_ms();
                status.send_replace(Arc::new(current.clone()));
            }
            changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return Ok(()); },
        }
    }
}

fn apply_managed_worktree_cursor(
    inventory: Option<&mut SlimNodeInventory>,
    incarnation_changed: bool,
    events: &[NodeEvent],
) {
    let Some(inventory) = inventory else { return; };
    if incarnation_changed {
        inventory.provider_runtime_statuses.clear();
        inventory.managed_worktrees.clear();
        inventory.managed_worktree_count = 0;
        inventory.managed_worktrees_truncated = false;
        return;
    }
    for event in events {
        match event {
            NodeEvent::ManagedWorktreeUpserted { lease } => {
                inventory.managed_worktrees.retain(|existing| {
                    existing.lease_id != lease.lease_id
                        && existing.workspace_id != lease.workspace_id
                });
                inventory.managed_worktrees.push(lease.clone());
                inventory.managed_worktrees.sort_by(|left, right| {
                    left.lease_id.cmp(&right.lease_id)
                });
                inventory.managed_worktree_count = inventory.managed_worktrees.len();
                inventory.managed_worktrees.truncate(
                    crate::protocol::MAX_C2_MANAGED_WORKTREES_PER_NODE,
                );
                inventory.managed_worktrees_truncated =
                    inventory.managed_worktrees.len() < inventory.managed_worktree_count;
            }
            NodeEvent::ManagedWorktreeRemoved { lease_id } => {
                let before = inventory.managed_worktrees.len();
                inventory
                    .managed_worktrees
                    .retain(|lease| &lease.lease_id != lease_id);
                if inventory.managed_worktrees.len() < before
                    || inventory.managed_worktrees_truncated
                {
                    inventory.managed_worktree_count =
                        inventory.managed_worktree_count.saturating_sub(1);
                }
                inventory.managed_worktrees_truncated =
                    inventory.managed_worktrees.len() < inventory.managed_worktree_count;
            }
            _ => {}
        }
    }
}

fn refresh_freshness(status: &mut StatusResponse, fresh_for: Duration) {
    let now = unix_ms();
    let fresh_ms = fresh_for.as_millis().min(u64::MAX as u128) as u64;
    for node in status.nodes.values_mut() {
        node.freshness = match node.last_success_unix_ms {
            None => NodeFreshness::Unavailable,
            Some(last) if now.saturating_sub(last) <= fresh_ms => NodeFreshness::Fresh,
            Some(_) => NodeFreshness::Stale,
        };
    }
}

async fn http_server(
    listener: TcpListener, token: String, io_deadline: Duration,
    status: watch::Receiver<Arc<StatusResponse>>, mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let permits = Arc::new(Semaphore::new(MAX_HTTP_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else { drop(stream); continue; };
                let token = token.clone();
                let status = status.clone();
                connections.spawn(async move { let _permit = permit; let _ = serve_http(stream, &token, io_deadline, &status).await; });
            }
            changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { break; },
        }
        while let Some(result) = connections.try_join_next() { result.map_err(io::Error::other)?; }
    }
    connections.shutdown().await;
    Ok(())
}

async fn serve_http(mut stream: TcpStream, token: &str, deadline: Duration, status: &watch::Receiver<Arc<StatusResponse>>) -> io::Result<()> {
    let request = match timeout(deadline, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(ReadError::TooLarge)) => return write_response(&mut stream, deadline, Response::plain(413, "Payload Too Large")).await,
        Ok(Err(ReadError::Io(error))) => return Err(error),
        _ => return Ok(()),
    };
    let response = route(request, token, status.borrow().as_ref());
    write_response(&mut stream, deadline, response).await
}

struct Request { method: String, path: String, authorization: Option<String> }
enum ReadError { Closed, Invalid, TooLarge, Io(io::Error) }

async fn read_request(stream: &mut TcpStream) -> Result<Request, ReadError> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).await.map_err(ReadError::Io)?;
        if count == 0 { return Err(ReadError::Closed); }
        if bytes.len().saturating_add(count) > HEADER_LIMIT_BYTES { return Err(ReadError::TooLarge); }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") { break; }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ReadError::Invalid)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().ok_or(ReadError::Invalid)?.split_whitespace();
    let method = first.next().ok_or(ReadError::Invalid)?;
    let path = first.next().ok_or(ReadError::Invalid)?;
    let version = first.next().ok_or(ReadError::Invalid)?;
    if first.next().is_some() || !version.starts_with("HTTP/1.") || !path.starts_with('/') { return Err(ReadError::Invalid); }
    let mut authorization = None;
    for line in lines {
        if line.is_empty() { break; }
        let (name, value) = line.split_once(':').ok_or(ReadError::Invalid)?;
        if name.eq_ignore_ascii_case("authorization") {
            if authorization.is_some() { return Err(ReadError::Invalid); }
            authorization = Some(value.trim().to_owned());
        }
    }
    Ok(Request { method: method.to_owned(), path: path.to_owned(), authorization })
}

fn route(request: Request, token: &str, status: &StatusResponse) -> Response {
    if request.method != "GET" { return Response::plain(405, "Method Not Allowed").allow_get(); }
    let path = request.path.split_once('?').map_or(request.path.as_str(), |pair| pair.0);
    match path {
        "/health" => Response::json(200, &HealthResponse { ok: true, service: "gate4agent-c2".to_owned(), api_version: C2_API_VERSION, pid: std::process::id(), version: env!("CARGO_PKG_VERSION").to_owned() }),
        "/ready" => {
            let online_nodes = status.nodes.values().filter(|node| node.transport == NodeTransportState::Online).count();
            let offline_nodes = status.nodes.values().filter(|node| node.transport == NodeTransportState::Offline).count();
            let parked_nodes = status.nodes.values().filter(|node| node.transport == NodeTransportState::Parked).count();
            let body = ReadyResponse { ready: status.ready, api_version: C2_API_VERSION, configured_nodes: status.nodes.len(), attempted_nodes: status.nodes.values().filter(|node| node.last_attempt_unix_ms.is_some()).count(), online_nodes, offline_nodes, parked_nodes };
            Response::json(if status.ready { 200 } else { 503 }, &body)
        }
        "/status" => {
            if !authorized(request.authorization.as_deref(), token) { Response::plain(401, "Unauthorized").authenticate() }
            else { Response::json(200, status) }
        }
        _ => Response::plain(404, "Not Found"),
    }
}

fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some((scheme, candidate)) = header.and_then(|value| value.split_once(' ')) else { return false; };
    scheme.eq_ignore_ascii_case("bearer") && constant_time_eq(candidate.as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() { return false; }
    left.iter().zip(right).fold(0_u8, |difference, (left, right)| difference | (left ^ right)) == 0
}

struct Response { status: u16, reason: &'static str, content_type: &'static str, body: Vec<u8>, headers: Vec<(&'static str, &'static str)> }
impl Response {
    fn plain(status: u16, reason: &'static str) -> Self { Self { status, reason, content_type: "text/plain; charset=utf-8", body: reason.as_bytes().to_vec(), headers: Vec::new() } }
    fn json<T: serde::Serialize>(status: u16, value: &T) -> Self {
        let body = serde_json::to_vec(value).expect("C2 DTO must serialize");
        if body.len() > RESPONSE_BODY_LIMIT_BYTES { return Self::plain(503, "Service Unavailable"); }
        let reason = if status == 200 { "OK" } else { "Service Unavailable" };
        Self { status, reason, content_type: "application/json", body, headers: Vec::new() }
    }
    fn allow_get(mut self) -> Self { self.headers.push(("Allow", "GET")); self }
    fn authenticate(mut self) -> Self { self.headers.push(("WWW-Authenticate", "Bearer")); self }
}

async fn write_response(stream: &mut TcpStream, deadline: Duration, response: Response) -> io::Result<()> {
    let mut headers = format!("HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", response.status, response.reason, response.content_type, response.body.len());
    for (name, value) in response.headers { headers.push_str(name); headers.push_str(": "); headers.push_str(value); headers.push_str("\r\n"); }
    headers.push_str("\r\n");
    timeout(deadline, async { stream.write_all(headers.as_bytes()).await?; stream.write_all(&response.body).await?; stream.shutdown().await }).await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "C2 HTTP write timed out"))?
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    fn node(endpoint: &str) -> Result<C2NodeConfig, C2ConfigError> {
        C2NodeConfig::new(NodeId::new("remote-node").unwrap(), endpoint, "safe-token")
    }

    #[test]
    fn ssh_forwarded_loopback_route_is_strict_canonical_and_control_stays_local() {
        let ipv4 = node("tcp://127.0.0.1:48100").unwrap();
        assert_eq!(ipv4.endpoint, "tcp://127.0.0.1:48100");
        assert_eq!(ipv4.route, C2NodeRoute::SshForwardedLoopback("127.0.0.1:48100".parse().unwrap()));
        assert_eq!(ipv4.transport_label(), "ssh-forwarded-loopback");

        let ipv6 = node("tcp://[0:0:0:0:0:0:0:1]:48100").unwrap();
        assert_eq!(ipv6.endpoint, "tcp://[::1]:48100");
        assert_eq!(ipv6.transport_label(), "ssh-forwarded-loopback");

        for invalid in [
            "tcp://localhost:48100",
            "tcp://127.0.0.2:48100",
            "tcp://0.0.0.0:48100",
            "tcp://127.0.0.1:0",
            "tcp://user@127.0.0.1:48100",
            "tcp://127.0.0.1:48100/path",
            "TCP://127.0.0.1:48100",
        ] {
            assert!(matches!(node(invalid), Err(C2ConfigError::InvalidEndpoint(_))), "accepted {invalid}");
        }

        assert!(matches!(
            C2Config::new(
                "127.0.0.1:0".parse().unwrap(),
                "safe-token",
                vec![ipv4.clone(), C2NodeConfig::new(
                    NodeId::new("duplicate-route").unwrap(),
                    "tcp://127.0.0.1:48100",
                    "safe-token",
                ).unwrap()],
            ),
            Err(C2ConfigError::DuplicateEndpoint)
        ));
        assert!(matches!(
            C2Config::new("127.0.0.1:0".parse().unwrap(), "safe-token", vec![ipv4])
                .unwrap()
                .with_control_endpoint("tcp://127.0.0.1:48101"),
            Err(C2ConfigError::InvalidControlEndpoint)
        ));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{
        AgentId, CapabilityId, SessionAddress, SessionKey, SessionMode, SessionRecordId,
        SpawnDeadlineMs, SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverrides,
        SpawnProfileId, SpawnProfileRevision, SpawnPrompt, SpawnPromptMetadata,
        SpawnRequiredCapabilities, SpawnResolutionProvenance, SpawnTarget, WorkspaceId,
        ManagedWorktreeCleanupFailure, ManagedWorktreeLeaseId,
        ManagedWorktreeLeaseSnapshot, ManagedWorktreeRetention, ManagedWorktreeSpawnReceipt,
        WorktreeProfileId, WorktreeProfileRevision,
    };
    use gate4agent_types::{AgentInstanceId, SessionGeneration, TerminalSize};
    use std::collections::BTreeMap;

    fn agent(value: &str) -> gate4agent_node_protocol::AgentId {
        gate4agent_node_protocol::AgentId::new(value).unwrap()
    }

    fn managed_lease(
        lease_id: &str,
        workspace_id: &str,
        state: ManagedWorktreeLeaseState,
    ) -> ManagedWorktreeLeaseSnapshot {
        let in_use = state == ManagedWorktreeLeaseState::InUse;
        ManagedWorktreeLeaseSnapshot {
            lease_id: ManagedWorktreeLeaseId::new(lease_id).unwrap(),
            source_workspace_id: WorkspaceId::new("repo").unwrap(),
            workspace_id: WorkspaceId::new(workspace_id).unwrap(),
            profile_id: WorktreeProfileId::new("review").unwrap(),
            profile_revision: WorktreeProfileRevision::new("review.r1").unwrap(),
            retention: ManagedWorktreeRetention::RemoveWhenReleased,
            state,
            active_session_count: u16::from(in_use),
            managed_record_count: u16::from(in_use),
            cleanup_failure: None::<ManagedWorktreeCleanupFailure>,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
        }
    }

    #[test]
    fn spawn_spec_receipt_correlation_rejects_mismatches_before_forwarding() {
        let incarnation_id = NodeIncarnationId::from_bytes([7; 16]);
        let terminal_size = TerminalSize { rows: 24, columns: 80 };
        let prompt = SpawnPrompt::new("hi").unwrap();
        let required_capabilities = SpawnRequiredCapabilities::new([
            CapabilityId::new("raw-pty-lifecycle").unwrap(),
        ]).unwrap();
        let spec = SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("repo").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("default").unwrap(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Set { value: AgentId::new("codex").unwrap() },
                mode: SpawnOverride::Set { value: SessionMode::Pty },
                terminal_size: SpawnOverride::Set { value: terminal_size },
                prompt: SpawnOverride::Set { value: prompt.clone() },
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Clear,
                environment_profile_id: SpawnOverride::Clear,
            },
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("spawn-1").unwrap(),
            required_capabilities: required_capabilities.clone(),
        };
        let receipt = ResolvedSpawnReceipt {
            incarnation_id,
            session: SessionAddress {
                workspace_id: spec.target.workspace_id.clone(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
            target: spec.target.clone(),
            profile_id: spec.profile_id.clone(),
            profile_revision: SpawnProfileRevision::new("r1").unwrap(),
            provider: AgentId::new("codex").unwrap(),
            mode: SessionMode::Pty,
            terminal_size,
            prompt: SpawnPromptMetadata::from_prompt(Some(&prompt)),
            bundle_id: None,
            bundle: None,
            context_id: None,
            environment_profile: None,
            deadline_ms: spec.deadline_ms,
            idempotency_key: spec.idempotency_key.clone(),
            required_capabilities,
            provenance: SpawnResolutionProvenance {
                provider: SpawnFieldProvenance::Override,
                mode: SpawnFieldProvenance::Override,
                terminal_size: SpawnFieldProvenance::Override,
                prompt: SpawnFieldProvenance::Override,
                bundle_id: SpawnFieldProvenance::Cleared,
                context_id: SpawnFieldProvenance::Cleared,
                environment_profile_id: SpawnFieldProvenance::Cleared,
            },
        };
        let response = |receipt| Ok(NodeResponse::SpawnSpecAccepted { receipt });
        let expected = ExpectedSpawnRequest::Spec(spec.clone());
        assert!(validate_spawn_spec_response(
            Some(&expected),
            &response(receipt.clone()),
            incarnation_id,
        ).is_ok());

        let mut mismatches = Vec::new();
        let mut changed = receipt.clone();
        changed.incarnation_id = NodeIncarnationId::from_bytes([8; 16]);
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.target.node_id = NodeId::new("node-b").unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.profile_id = SpawnProfileId::new("other").unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.idempotency_key = SpawnIdempotencyKey::new("spawn-2").unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.deadline_ms = SpawnDeadlineMs::new(4_999).unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.required_capabilities = SpawnRequiredCapabilities::default();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.session.workspace_id = WorkspaceId::new("other").unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.provider = AgentId::new("claude").unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.prompt = SpawnPromptMetadata { present: false, byte_len: 0 };
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.environment_profile = Some(
            gate4agent_node_protocol::ResolvedEnvironmentProfileReceipt {
                profile_id:
                    gate4agent_node_protocol::SpawnEnvironmentProfileId::new(
                        "local-default",
                    )
                    .unwrap(),
                profile_revision:
                    gate4agent_node_protocol::SpawnEnvironmentProfileRevision::new(
                        "local-default.r1",
                    )
                    .unwrap(),
            },
        );
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.bundle = Some(gate4agent_node_protocol::ResolvedBundleReceipt {
            id: gate4agent_node_protocol::SpawnBundleId::new("unexpected-bundle")
                .unwrap(),
            revision: gate4agent_node_protocol::SpawnBundleRevision::new(
                "unexpected-bundle.r1",
            )
            .unwrap(),
            digest: gate4agent_node_protocol::SpawnBundleDigest::new(format!(
                "sha256:{}",
                "a".repeat(64),
            ))
            .unwrap(),
        });
        mismatches.push(changed);

        for mismatch in mismatches {
            assert!(validate_spawn_spec_response(
                Some(&expected),
                &response(mismatch),
                incarnation_id,
            ).is_err());
        }

        let mut environment_spec = spec;
        environment_spec.overrides.environment_profile_id = SpawnOverride::Set {
            value: gate4agent_node_protocol::SpawnEnvironmentProfileId::new(
                "local-default",
            )
            .unwrap(),
        };
        let environment_expected = ExpectedSpawnRequest::Spec(environment_spec);
        let mut environment_receipt = receipt;
        environment_receipt.environment_profile = Some(
            gate4agent_node_protocol::ResolvedEnvironmentProfileReceipt {
                profile_id:
                    gate4agent_node_protocol::SpawnEnvironmentProfileId::new(
                        "local-default",
                    )
                    .unwrap(),
                profile_revision:
                    gate4agent_node_protocol::SpawnEnvironmentProfileRevision::new(
                        "local-default.r1",
                    )
                    .unwrap(),
            },
        );
        assert!(validate_spawn_spec_response(
            Some(&environment_expected),
            &response(environment_receipt.clone()),
            incarnation_id,
        )
        .is_ok());
        environment_receipt.environment_profile.as_mut().unwrap().profile_id =
            gate4agent_node_protocol::SpawnEnvironmentProfileId::new("other")
                .unwrap();
        assert!(validate_spawn_spec_response(
            Some(&environment_expected),
            &response(environment_receipt),
            incarnation_id,
        )
        .is_err());
    }

    #[test]
    fn managed_spawn_receipt_correlation_rejects_non_in_use_or_mismatched_leases() {
        let incarnation_id = NodeIncarnationId::from_bytes([7; 16]);
        let spec = SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("repo").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("default").unwrap(),
            overrides: SpawnOverrides::default(),
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("managed-1").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
        };
        let managed = ManagedWorktreeSpawnRequest {
            spawn_spec: spec.clone(),
            worktree_profile_id: WorktreeProfileId::new("review").unwrap(),
        };
        let workspace_id = WorkspaceId::new("managed-a").unwrap();
        let spawn = ResolvedSpawnReceipt {
            incarnation_id,
            session: SessionAddress {
                workspace_id: workspace_id.clone(),
                session: SessionKey {
                    instance_id: AgentInstanceId(8),
                    generation: SessionGeneration(1),
                },
            },
            target: SpawnTarget {
                node_id: spec.target.node_id.clone(),
                workspace_id: spec.target.workspace_id.clone(),
                worktree_id: Some(workspace_id.clone()),
            },
            profile_id: spec.profile_id.clone(),
            profile_revision: SpawnProfileRevision::new("default.r1").unwrap(),
            provider: AgentId::new("claude").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            prompt: SpawnPromptMetadata { present: false, byte_len: 0 },
            bundle_id: None,
            bundle: None,
            context_id: None,
            environment_profile: None,
            deadline_ms: spec.deadline_ms,
            idempotency_key: spec.idempotency_key.clone(),
            required_capabilities: SpawnRequiredCapabilities::default(),
            provenance: SpawnResolutionProvenance {
                provider: SpawnFieldProvenance::Profile,
                mode: SpawnFieldProvenance::Profile,
                terminal_size: SpawnFieldProvenance::Profile,
                prompt: SpawnFieldProvenance::Profile,
                bundle_id: SpawnFieldProvenance::Profile,
                context_id: SpawnFieldProvenance::Profile,
                environment_profile_id: SpawnFieldProvenance::Profile,
            },
        };
        let receipt = ManagedWorktreeSpawnReceipt {
            spawn,
            lease: managed_lease(
                "lease-a",
                "managed-a",
                ManagedWorktreeLeaseState::InUse,
            ),
        };
        let expected = ExpectedSpawnRequest::Managed(managed);
        let response = |receipt| Ok(NodeResponse::ManagedWorktreeSpawnAccepted { receipt });
        assert!(validate_spawn_spec_response(
            Some(&expected),
            &response(receipt.clone()),
            incarnation_id,
        )
        .is_ok());

        let mut mismatches = Vec::new();
        let mut changed = receipt.clone();
        changed.lease.state = ManagedWorktreeLeaseState::Ready;
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.lease.cleanup_failure = Some(ManagedWorktreeCleanupFailure::Busy);
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.lease.active_session_count = 0;
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.lease.profile_id = WorktreeProfileId::new("other").unwrap();
        mismatches.push(changed);
        let mut changed = receipt.clone();
        changed.lease.source_workspace_id = WorkspaceId::new("other").unwrap();
        mismatches.push(changed);

        for mismatch in mismatches {
            assert!(validate_spawn_spec_response(
                Some(&expected),
                &response(mismatch),
                incarnation_id,
            )
            .is_err());
        }
    }

    #[test]
    fn windows_runtime_default_control_endpoint_is_exact_valid_and_distinct_from_a_node() {
        assert_eq!(DEFAULT_C2_CONTROL_ENDPOINT, r"\\.\pipe\gate4agent-c2");
        validate_control_endpoint(DEFAULT_C2_CONTROL_ENDPOINT).unwrap();
        let node = C2NodeConfig::new(
            NodeId::new("node-a").unwrap(),
            r"\\.\pipe\gate4agent-node",
            "safe-token",
        )
        .unwrap();
        let config = C2Config::new(
            "127.0.0.1:0".parse().unwrap(),
            "safe-token",
            vec![node],
        )
        .unwrap();
        assert_eq!(config.control_endpoint, DEFAULT_C2_CONTROL_ENDPOINT);
        assert!(!config.nodes[0]
            .endpoint
            .eq_ignore_ascii_case(&config.control_endpoint));

        let conflicting_node = C2NodeConfig::new(
            NodeId::new("node-b").unwrap(),
            DEFAULT_C2_CONTROL_ENDPOINT,
            "safe-token",
        )
        .unwrap();
        assert!(matches!(
            C2Config::new(
                "127.0.0.1:0".parse().unwrap(),
                "safe-token",
                vec![conflicting_node],
            ),
            Err(C2ConfigError::ControlEndpointConflict)
        ));
    }

    #[test]
    fn state_gap_validation_distinguishes_eviction_and_non_contiguous_events() {
        use gate4agent_node_protocol::{NodeEvent, WorkspaceId};
        let event = |sequence| NodeEventEnvelope { sequence, event: NodeEvent::WorkspaceRemoved { workspace_id: WorkspaceId::new("work").unwrap() } };
        assert_eq!(validate_events(4, 6, &[event(5), event(6)]), Vec::<GapKind>::new());
        assert_eq!(validate_events(4, 6, &[event(6)]), vec![GapKind::HistoryEvicted]);
        assert_eq!(validate_events(4, 7, &[event(5), event(7)]), vec![GapKind::NonContiguousEvents]);
        assert_eq!(validate_events(7, 6, &[]), vec![GapKind::CursorRegression]);
        assert_eq!(validate_resync(4, 6, 4, &[]), vec![GapKind::CursorRegression]);
    }

    #[test]
    fn live_event_gap_preserves_cursor_and_resync_rules() {
        use gate4agent_node_protocol::{NodeEvent, WorkspaceId};
        let event = |sequence, event| NodeEventEnvelope { sequence, event };
        let removed = || NodeEvent::WorkspaceRemoved {
            workspace_id: WorkspaceId::new("work").unwrap(),
        };

        assert_eq!(live_event_gap(4, &event(5, removed())), None);
        assert_eq!(
            live_event_gap(4, &event(4, removed())),
            Some(GapKind::CursorRegression),
        );
        assert_eq!(
            live_event_gap(4, &event(7, removed())),
            Some(GapKind::NonContiguousEvents),
        );
        assert_eq!(
            live_event_gap(4, &event(5, NodeEvent::ResyncRequired {
                oldest_available_sequence: 3,
            })),
            Some(GapKind::HistoryEvicted),
        );
    }

    #[test]
    fn config_rejects_header_injection_duplicate_nodes_and_non_loopback() {
        let id = NodeId::new("node-a").unwrap();
        assert!(matches!(C2NodeConfig::new(id.clone(), r"\\.\pipe\a", "bad\r\ntoken"), Err(C2ConfigError::InvalidToken)));
        let node = C2NodeConfig::new(id, r"\\.\pipe\a", "safe-token").unwrap();
        assert!(matches!(C2Config::new("0.0.0.0:0".parse().unwrap(), "safe", vec![node.clone()]), Err(C2ConfigError::NonLoopback(_))));
        assert!(matches!(C2Config::new("127.0.0.1:0".parse().unwrap(), "safe", vec![node.clone(), node]), Err(C2ConfigError::DuplicateNode)));
        let first = C2NodeConfig::new(NodeId::new("node-a").unwrap(), r"\\.\pipe\same", "safe").unwrap();
        let second = C2NodeConfig::new(NodeId::new("node-b").unwrap(), r"\\.\pipe\same", "safe").unwrap();
        assert!(matches!(C2Config::new("127.0.0.1:0".parse().unwrap(), "safe", vec![first, second]), Err(C2ConfigError::DuplicateEndpoint)));
        let oversized = format!(r"\\.\pipe\{}", "x".repeat(MAX_C2_ENDPOINT_BYTES));
        assert!(matches!(C2NodeConfig::new(NodeId::new("node-c").unwrap(), oversized, "safe"), Err(C2ConfigError::InvalidEndpoint(_))));
    }

    #[test]
    fn durable_session_mutations_require_controller_and_use_bounded_deadlines() {
        let record_id = SessionRecordId::new("session-001").unwrap();
        let rename = NodeRequest::RenameSessionRecord {
            record_id: record_id.clone(),
            display_name: "release shepherd".to_owned(),
        };
        let resume = NodeRequest::ResumeSessionRecord {
            record_id: record_id.clone(),
            terminal_size: TerminalSize { rows: 40, columns: 120 },
            initial_prompt: None,
        };
        let forget = NodeRequest::ForgetSessionRecord { record_id };

        assert!(!is_read_only_request(&rename));
        assert!(!is_read_only_request(&resume));
        assert!(!is_read_only_request(&forget));
        assert_eq!(node_request_deadline(&rename), Duration::from_secs(5));
        assert_eq!(node_request_deadline(&resume), Duration::from_secs(35));
        assert!(node_request_deadline(&resume) > MANAGED_RESUME_SETTLE_DEADLINE);
        assert_eq!(
            node_request_deadline(&resume) - MANAGED_RESUME_SETTLE_DEADLINE,
            NODE_REQUEST_IO_HEADROOM,
        );
        let started = Instant::now();
        let relay_deadline = relay_request_deadline(&resume, started).unwrap();
        assert_eq!(
            request_budget(&resume, Some(relay_deadline), started),
            Duration::from_secs(35),
        );
        assert_eq!(
            request_budget(
                &resume,
                Some(relay_deadline),
                started + Duration::from_secs(5),
            ),
            MANAGED_RESUME_SETTLE_DEADLINE,
        );
        assert_eq!(
            request_budget(
                &resume,
                Some(relay_deadline),
                started + Duration::from_secs(35),
            ),
            Duration::ZERO,
        );
        assert_eq!(node_request_deadline(&forget), Duration::from_secs(5));
    }

    #[test]
    fn workspace_file_reads_are_read_only_with_five_second_deadline() {
        let request = NodeRequest::ReadWorkspaceFile {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("primary").unwrap(),
            path: gate4agent_node_protocol::RepositoryPath::utf8(
                "src/lib.rs".to_owned(),
            ).unwrap(),
        };

        assert!(is_read_only_request(&request));
        assert_eq!(node_request_deadline(&request), Duration::from_secs(5));
        assert!(relay_request_deadline(&request, Instant::now()).is_none());
    }

    #[test]
    fn unsupported_node_capability_is_correlated_without_offline_classification() {
        let error = NodeClientError::UnsupportedCapability(
            "workspace-file-read-v1-private-detail".to_owned(),
        );
        let failure = relay_node_failure(&error)
            .expect("unsupported node capability must remain an in-band node failure");

        assert_eq!(failure.code, NodeFailureCode::UnsupportedCapability);
        assert_eq!(failure.message, "required capability unavailable");
        assert!(!failure.message.contains("private-detail"));
        assert!(relay_node_failure(&NodeClientError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "transport closed",
        ))).is_none());
    }

    async fn raw_request(request: Vec<u8>, status: StatusResponse) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (_status_tx, status_rx) = watch::channel(Arc::new(status));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_http(stream, "api-token", Duration::from_secs(1), &status_rx).await.unwrap();
        });
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        response
    }

    fn empty_status(ready: bool) -> StatusResponse {
        StatusResponse { api_version: C2_API_VERSION, ready, observed_at_unix_ms: 0, nodes: BTreeMap::new() }
    }

    #[tokio::test]
    async fn http_api_enforces_initializing_auth_method_path_and_header_bounds() {
        let initializing = raw_request(b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(), empty_status(false)).await;
        assert!(initializing.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(String::from_utf8_lossy(&initializing).contains("\"ready\":false"));

        let unauthorized = raw_request(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(), empty_status(true)).await;
        assert!(unauthorized.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
        let method = raw_request(b"POST /health HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(), empty_status(true)).await;
        assert!(method.starts_with(b"HTTP/1.1 405 Method Not Allowed\r\n"));
        let missing = raw_request(b"GET /missing HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(), empty_status(true)).await;
        assert!(missing.starts_with(b"HTTP/1.1 404 Not Found\r\n"));

        let mut oversized = b"GET /health HTTP/1.1\r\nX-Fill: ".to_vec();
        oversized.extend(std::iter::repeat(b'x').take(HEADER_LIMIT_BYTES));
        oversized.extend_from_slice(b"\r\n\r\n");
        let rejected = raw_request(oversized, empty_status(true)).await;
        assert!(rejected.starts_with(b"HTTP/1.1 413 Payload Too Large\r\n"));
    }

    #[tokio::test]
    async fn inventory_state_transitions_offline_to_stale_parked_and_recovers() {
        let node_id = NodeId::new("node-a").unwrap();
        let mut nodes = BTreeMap::new();
        nodes.insert(node_id.clone(), ObservedNode {
            endpoint: r"\\.\pipe\a".to_owned(), transport_label: "windows-named-pipe".to_owned(),
            transport: NodeTransportState::Offline, freshness: NodeFreshness::Unavailable,
            cursor: None, inventory: None, last_attempt_unix_ms: None, last_success_unix_ms: None,
            consecutive_failures: 0, last_error: None, gaps: Vec::new(), gaps_truncated: 0,
        });
        let initial = Arc::new(StatusResponse { api_version: C2_API_VERSION, ready: false, observed_at_unix_ms: unix_ms(), nodes });
        let (status_tx, mut status_rx) = watch::channel(initial);
        let (ingress_tx, ingress_rx) = mpsc::channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let owner = tokio::spawn(inventory_owner(1, Duration::from_millis(10), ingress_rx, status_tx, shutdown_rx));
        let snapshot = NodeSnapshot {
            node_id: node_id.clone(),
            enabled_providers: Vec::new(),
            provider_runtime_statuses: crate::protocol::ProviderRuntimeStatuses::default(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
        };
        let cursor = NodeCursor { incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([1; 16]), sequence: 0 };
        let old_manifest = ProviderContractManifest {
            provider_contracts: vec![crate::protocol::ProviderContractSupport {
                provider: agent("codex"),
                revision: crate::protocol::ProviderContractRevision::new("old-contract").unwrap(),
            }],
            provider_adapter_contracts: vec![crate::protocol::ProviderAdapterContractSupport {
                provider: agent("codex"),
                family: crate::protocol::AdapterFamily::PtySemantic,
                adapter_id: crate::protocol::AdapterId::new("codex").unwrap(),
                revision: crate::protocol::AdapterContractRevision::new("old-adapter").unwrap(),
            }],
        };
        ingress_tx.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result: AttemptResult::Connected {
            cursor,
            snapshot: snapshot.clone(),
            gaps: Vec::new(),
            provider_contract_manifest: old_manifest,
        } }).await.unwrap();
        status_rx.changed().await.unwrap();
        assert_eq!(status_rx.borrow().nodes[&node_id].freshness, NodeFreshness::Fresh);
        assert_eq!(
            status_rx.borrow().nodes[&node_id].inventory.as_ref().unwrap()
                .provider_contracts[0].revision.as_str(),
            "old-contract",
        );

        let failure = || AttemptResult::Failure { error: SanitizedError { category: C2ErrorCategory::Transport, message: "node transport unavailable".to_owned() }, hard: false };
        ingress_tx.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result: failure() }).await.unwrap();
        status_rx.changed().await.unwrap();
        assert_eq!(status_rx.borrow().nodes[&node_id].transport, NodeTransportState::Offline);
        timeout(Duration::from_secs(1), async {
            loop {
                status_rx.changed().await.unwrap();
                if status_rx.borrow().nodes[&node_id].freshness == NodeFreshness::Stale { break; }
            }
        }).await.unwrap();
        for _ in 0..4 {
            ingress_tx.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result: failure() }).await.unwrap();
            status_rx.changed().await.unwrap();
        }
        assert_eq!(status_rx.borrow().nodes[&node_id].transport, NodeTransportState::Parked);
        let replacement_manifest = ProviderContractManifest {
            provider_contracts: vec![crate::protocol::ProviderContractSupport {
                provider: agent("claude"),
                revision: crate::protocol::ProviderContractRevision::new("new-contract").unwrap(),
            }],
            provider_adapter_contracts: Vec::new(),
        };
        let replacement_cursor = NodeCursor {
            incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([2; 16]),
            sequence: 0,
        };
        ingress_tx.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result: AttemptResult::Connected {
            cursor: replacement_cursor,
            snapshot,
            gaps: Vec::new(),
            provider_contract_manifest: replacement_manifest,
        } }).await.unwrap();
        status_rx.changed().await.unwrap();
        assert_eq!(status_rx.borrow().nodes[&node_id].transport, NodeTransportState::Online);
        assert_eq!(status_rx.borrow().nodes[&node_id].freshness, NodeFreshness::Fresh);
        let recovered_inventory = status_rx.borrow().nodes[&node_id].inventory.as_ref().unwrap().clone();
        assert_eq!(recovered_inventory.provider_contracts.len(), 1);
        assert_eq!(recovered_inventory.provider_contracts[0].provider, agent("claude"));
        assert_eq!(recovered_inventory.provider_contracts[0].revision.as_str(), "new-contract");
        assert!(recovered_inventory.provider_adapter_contracts.is_empty());
        ingress_tx.send(Attempt {
            node_id: node_id.clone(),
            at_unix_ms: unix_ms(),
            result: AttemptResult::Connected {
                cursor: NodeCursor {
                    incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([3; 16]),
                    sequence: 0,
                },
                snapshot: NodeSnapshot {
                    node_id: node_id.clone(),
                    enabled_providers: Vec::new(),
                    provider_runtime_statuses: crate::protocol::ProviderRuntimeStatuses::default(),
                    workspaces: Vec::new(),
                    session_records: Vec::new(),
                    managed_worktrees: Vec::new(),
                },
                gaps: Vec::new(),
                provider_contract_manifest: ProviderContractManifest::default(),
            },
        }).await.unwrap();
        status_rx.changed().await.unwrap();
        let unpublished = status_rx.borrow().nodes[&node_id].inventory.as_ref().unwrap().clone();
        assert!(unpublished.provider_contracts.is_empty());
        assert!(unpublished.provider_adapter_contracts.is_empty());
        shutdown_tx.send(true).unwrap();
        owner.await.unwrap().unwrap();
    }

    fn runtime_statuses(
        provider: gate4agent_node_protocol::AgentId,
        version: &str,
    ) -> crate::protocol::ProviderRuntimeStatuses {
        crate::protocol::ProviderRuntimeStatuses::new([
            crate::protocol::ProviderRuntimeStatus::raw_passthrough(
                provider,
                Some(crate::protocol::ProviderRuntimeVersion::new(version).unwrap()),
            ),
        ])
        .unwrap()
    }

    async fn runtime_inventory_owner() -> (
        NodeId,
        mpsc::Sender<Attempt>,
        watch::Receiver<Arc<StatusResponse>>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let node_id = NodeId::new("runtime-node").unwrap();
        let nodes = BTreeMap::from([(
            node_id.clone(),
            ObservedNode {
                endpoint: r"\\.\pipe\runtime-node".to_owned(),
                transport_label: "windows-named-pipe".to_owned(),
                transport: NodeTransportState::Offline,
                freshness: NodeFreshness::Unavailable,
                cursor: None,
                inventory: None,
                last_attempt_unix_ms: None,
                last_success_unix_ms: None,
                consecutive_failures: 0,
                last_error: None,
                gaps: Vec::new(),
                gaps_truncated: 0,
            },
        )]);
        let initial = Arc::new(StatusResponse {
            api_version: C2_API_VERSION,
            ready: false,
            observed_at_unix_ms: unix_ms(),
            nodes,
        });
        let (status_tx, status_rx) = watch::channel(initial);
        let (ingress_tx, ingress_rx) = mpsc::channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let owner = tokio::spawn(inventory_owner(
            1,
            Duration::from_secs(1),
            ingress_rx,
            status_tx,
            shutdown_rx,
        ));
        (node_id, ingress_tx, status_rx, shutdown_tx, owner)
    }

    #[tokio::test]
    async fn incarnation_change_replaces_runtime_status() {
        let (node_id, ingress, mut status, shutdown, owner) = runtime_inventory_owner().await;
        for (incarnation, provider, version) in [
            (1, agent("claude"), "1.0.0"),
            (2, agent("codex"), "2.0.0"),
        ] {
            ingress
                .send(Attempt {
                    node_id: node_id.clone(),
                    at_unix_ms: unix_ms(),
                    result: AttemptResult::Connected {
                        cursor: NodeCursor {
                            incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([
                                incarnation;
                                16
                            ]),
                            sequence: 0,
                        },
                        snapshot: NodeSnapshot {
                            node_id: node_id.clone(),
                            enabled_providers: vec![provider.clone()],
                            provider_runtime_statuses: runtime_statuses(provider, version),
                            workspaces: Vec::new(),
                            session_records: Vec::new(),
                            managed_worktrees: Vec::new(),
                        },
                        gaps: Vec::new(),
                        provider_contract_manifest: ProviderContractManifest::default(),
                    },
                })
                .await
                .unwrap();
            status.changed().await.unwrap();
        }
        let current_status = status.borrow();
        let statuses = &current_status.nodes[&node_id]
            .inventory
            .as_ref()
            .unwrap()
            .provider_runtime_statuses;
        assert_eq!(statuses.as_slice().len(), 1);
        assert_eq!(
            statuses.as_slice()[0].provider(),
            &agent("codex"),
        );
        assert_eq!(statuses.as_slice()[0].version().unwrap().as_str(), "2.0.0");
        shutdown.send(true).unwrap();
        owner.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn incarnation_change_without_snapshot_clears_dynamic_inventory() {
        let (node_id, ingress, mut status, shutdown, owner) = runtime_inventory_owner().await;
        ingress
            .send(Attempt {
                node_id: node_id.clone(),
                at_unix_ms: unix_ms(),
                result: AttemptResult::Connected {
                    cursor: NodeCursor {
                        incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([
                            3; 16
                        ]),
                        sequence: 0,
                    },
                    snapshot: NodeSnapshot {
                        node_id: node_id.clone(),
                        enabled_providers: vec![agent("claude")],
                        provider_runtime_statuses: runtime_statuses(
                            agent("claude"),
                            "3.0.0",
                        ),
                        workspaces: Vec::new(),
                        session_records: Vec::new(),
                        managed_worktrees: Vec::new(),
                    },
                    gaps: Vec::new(),
                    provider_contract_manifest: ProviderContractManifest::default(),
                },
            })
            .await
            .unwrap();
        status.changed().await.unwrap();
        ingress
            .send(Attempt {
                node_id: node_id.clone(),
                at_unix_ms: unix_ms(),
                result: AttemptResult::Cursor {
                    cursor: NodeCursor {
                        incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([
                            4; 16
                        ]),
                        sequence: 0,
                    },
                    gaps: vec![GapKind::IncarnationChanged],
                    managed_worktree_events: Vec::new(),
                },
            })
            .await
            .unwrap();
        status.changed().await.unwrap();
        assert!(status.borrow().nodes[&node_id]
            .inventory
            .as_ref()
            .unwrap()
            .provider_runtime_statuses
            .is_empty());
        shutdown.send(true).unwrap();
        owner.await.unwrap().unwrap();
    }

    #[test]
    fn managed_worktree_inventory_events_are_exact_bounded_and_incarnation_fenced() {
        let mut inventory = SlimNodeInventory::from_snapshot(&NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            provider_runtime_statuses: crate::protocol::ProviderRuntimeStatuses::default(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            managed_worktrees: vec![managed_lease(
                "lease-a",
                "managed-a",
                ManagedWorktreeLeaseState::Ready,
            )],
        });
        apply_managed_worktree_cursor(
            Some(&mut inventory),
            false,
            &[
                NodeEvent::ManagedWorktreeUpserted {
                    lease: managed_lease(
                        "lease-b",
                        "managed-b",
                        ManagedWorktreeLeaseState::Ready,
                    ),
                },
                NodeEvent::ManagedWorktreeUpserted {
                    lease: managed_lease(
                        "lease-a",
                        "managed-a",
                        ManagedWorktreeLeaseState::InUse,
                    ),
                },
            ],
        );
        assert_eq!(inventory.managed_worktree_count, 2);
        assert_eq!(inventory.managed_worktrees[0].lease_id.as_str(), "lease-a");
        assert_eq!(
            inventory.managed_worktrees[0].state,
            ManagedWorktreeLeaseState::InUse,
        );

        apply_managed_worktree_cursor(
            Some(&mut inventory),
            false,
            &[NodeEvent::ManagedWorktreeRemoved {
                lease_id: ManagedWorktreeLeaseId::new("lease-a").unwrap(),
            }],
        );
        assert_eq!(inventory.managed_worktree_count, 1);
        assert_eq!(inventory.managed_worktrees[0].lease_id.as_str(), "lease-b");

        apply_managed_worktree_cursor(
            Some(&mut inventory),
            true,
            &[NodeEvent::ManagedWorktreeUpserted {
                lease: managed_lease(
                    "lease-c",
                    "managed-c",
                    ManagedWorktreeLeaseState::Ready,
                ),
            }],
        );
        assert!(inventory.managed_worktrees.is_empty());
        assert_eq!(inventory.managed_worktree_count, 0);
        assert!(!inventory.managed_worktrees_truncated);
    }
}
