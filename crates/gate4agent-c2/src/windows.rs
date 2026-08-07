use crate::protocol::{
    C2ErrorCategory, GapKind, HealthResponse, NodeCursor, NodeFreshness, NodeGap, NodeId,
    NodeTransportState, ObservedNode, ReadyResponse, SanitizedError, SlimNodeInventory,
    StatusResponse, C2_API_VERSION, MAX_C2_ENDPOINT_BYTES, MAX_C2_GAPS_PER_NODE, MAX_C2_NODES,
};
use gate4agent_node_protocol::{ClientRole, FrameError, NodeEventEnvelope, NodeFailureCode, NodeRequest, NodeResponse, NodeSnapshot};
use gate4agent_node_wire::{NamedPipeNodeClient, NodeClientError};
use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};

const HEADER_LIMIT_BYTES: usize = 16 * 1024;
const MAX_HTTP_CONNECTIONS: usize = 16;
const RESPONSE_BODY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct C2NodeConfig {
    pub node_id: NodeId,
    pub endpoint: String,
    token: String,
}

impl C2NodeConfig {
    pub fn new(node_id: NodeId, endpoint: impl Into<String>, token: impl Into<String>) -> Result<Self, C2ConfigError> {
        let endpoint = endpoint.into();
        let token = token.into();
        if !endpoint.starts_with(r"\\.\pipe\") || endpoint.len() <= r"\\.\pipe\".len()
            || endpoint.len() > MAX_C2_ENDPOINT_BYTES
        {
            return Err(C2ConfigError::InvalidEndpoint(node_id));
        }
        validate_token(&token)?;
        Ok(Self { node_id, endpoint, token })
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
            poll_interval: Duration::from_secs(1),
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
        if nodes.iter().any(|node| !endpoints.insert(node.endpoint.to_ascii_lowercase())) { return Err(C2ConfigError::DuplicateEndpoint); }
        Ok(Self { api_listen, api_token, nodes, timings: C2Timings::default() })
    }

    pub fn with_timings(mut self, timings: C2Timings) -> Self {
        self.timings = timings;
        self
    }
}

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
    #[error("node '{0}' has an invalid Windows named-pipe endpoint")]
    InvalidEndpoint(NodeId),
    #[error("C2 API listen address must be loopback: {0}")]
    NonLoopback(SocketAddr),
    #[error("C2 requires 1..=64 configured nodes; received {0}")]
    NodeCount(usize),
    #[error("C2 node IDs must be unique")]
    DuplicateNode,
    #[error("C2 named-pipe endpoints must be unique")]
    DuplicateEndpoint,
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

impl Drop for C2Running {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(task) = self.task.take() { task.abort(); }
    }
}

async fn run_bound(config: C2Config, listener: TcpListener, mut shutdown: watch::Receiver<bool>) -> Result<(), C2Error> {
    let now = unix_ms();
    let nodes = config.nodes.iter().map(|node| (node.node_id.clone(), ObservedNode {
        endpoint: node.endpoint.clone(), transport_label: "windows-named-pipe".to_owned(),
        transport: NodeTransportState::Offline, freshness: NodeFreshness::Unavailable,
        cursor: None, inventory: None, last_attempt_unix_ms: None, last_success_unix_ms: None,
        consecutive_failures: 0, last_error: None, gaps: Vec::new(), gaps_truncated: 0,
    })).collect();
    let initial = Arc::new(StatusResponse { api_version: C2_API_VERSION, ready: false, observed_at_unix_ms: now, nodes });
    let (status_tx, status_rx) = watch::channel(initial);
    let (ingress_tx, ingress_rx) = mpsc::channel(config.nodes.len().saturating_mul(2).max(2));
    let mut tasks = JoinSet::new();
    tasks.spawn(inventory_owner(config.nodes.len(), config.timings.fresh_for, ingress_rx, status_tx, shutdown.clone()));
    for node in config.nodes.clone() {
        tasks.spawn(node_poller(node, config.timings, ingress_tx.clone(), status_rx.clone(), shutdown.clone()));
    }
    drop(ingress_tx);
    tasks.spawn(http_server(listener, config.api_token, config.timings.http_io_deadline, status_rx, shutdown.clone()));
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

enum AttemptResult {
    Success { cursor: NodeCursor, snapshot: NodeSnapshot, gaps: Vec<GapKind> },
    Failure { error: SanitizedError, hard: bool },
}

struct Attempt { node_id: NodeId, at_unix_ms: u64, result: AttemptResult }

async fn node_poller(
    node: C2NodeConfig, timings: C2Timings, ingress: mpsc::Sender<Attempt>,
    status: watch::Receiver<Arc<StatusResponse>>, mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut failures = 0_usize;
    loop {
        if *shutdown.borrow() { return Ok(()); }
        let previous = status.borrow().nodes.get(&node.node_id).and_then(|item| item.cursor);
        let result = match timeout(timings.attempt_deadline, observe_once(&node, previous)).await {
            Ok(Ok(success)) => { failures = 0; AttemptResult::Success { cursor: success.0, snapshot: success.1, gaps: success.2 } }
            Ok(Err(error)) => {
                failures = failures.saturating_add(1);
                let (error, hard) = sanitize_node_error(&error);
                AttemptResult::Failure { error, hard }
            }
            Err(_) => {
                failures = failures.saturating_add(1);
                AttemptResult::Failure { error: SanitizedError { category: C2ErrorCategory::Timeout, message: "node observation deadline exceeded".to_owned() }, hard: false }
            }
        };
        let success = matches!(result, AttemptResult::Success { .. });
        let hard = matches!(result, AttemptResult::Failure { hard: true, .. });
        if ingress.send(Attempt { node_id: node.node_id.clone(), at_unix_ms: unix_ms(), result }).await.is_err() { return Ok(()); }
        let delay = if success { timings.poll_interval } else if hard || failures >= timings.transient_backoffs.len() {
            timings.parked_backoff
        } else {
            timings.transient_backoffs[failures - 1]
        };
        tokio::select! {
            _ = sleep(delay) => {}
            changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { return Ok(()); },
        }
    }
}

async fn observe_once(node: &C2NodeConfig, previous: Option<NodeCursor>) -> Result<(NodeCursor, NodeSnapshot, Vec<GapKind>), NodeClientError> {
    let mut client = connect_observer(node).await?;
    let hello = client.hello().clone();
    let hello_cursor = NodeCursor { incarnation_id: hello.incarnation_id, sequence: hello.event_sequence };
    let Some(previous) = previous else { return Ok((hello_cursor, hello.snapshot, Vec::new())); };
    if hello_cursor.incarnation_id != previous.incarnation_id {
        return Ok((hello_cursor, hello.snapshot, vec![GapKind::IncarnationChanged]));
    }
    if hello_cursor.sequence < previous.sequence {
        return Ok((hello_cursor, hello.snapshot, vec![GapKind::CursorRegression]));
    }
    if hello_cursor.sequence == previous.sequence {
        return Ok((hello_cursor, hello.snapshot, Vec::new()));
    }
    let NodeResponse::Resync { event_sequence, snapshot, events } = client.request(NodeRequest::Resync { after_sequence: previous.sequence }).await? else {
        return Err(NodeClientError::Protocol("resync returned a different response".to_owned()));
    };
    let cursor = NodeCursor { incarnation_id: hello.incarnation_id, sequence: event_sequence };
    let gaps = validate_resync(previous.sequence, hello_cursor.sequence, event_sequence, &events);
    Ok((cursor, snapshot, gaps))
}

fn validate_resync(previous: u64, hello: u64, current: u64, events: &[NodeEventEnvelope]) -> Vec<GapKind> {
    let mut gaps = validate_events(previous, current, events);
    if current < hello && !gaps.contains(&GapKind::CursorRegression) {
        gaps.push(GapKind::CursorRegression);
    }
    gaps
}

// Transport boundary: the milestone only enables Windows named pipes. A future WSS
// implementation belongs behind this constructor, without changing inventory ownership.
async fn connect_observer(node: &C2NodeConfig) -> Result<NamedPipeNodeClient, NodeClientError> {
    NamedPipeNodeClient::connect(&node.endpoint, &node.node_id, ClientRole::Observer, &node.token).await
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
                    AttemptResult::Success { cursor, snapshot, gaps } => {
                        let previous = node.cursor;
                        node.transport = NodeTransportState::Online;
                        node.freshness = NodeFreshness::Fresh;
                        node.cursor = Some(cursor);
                        node.inventory = Some(SlimNodeInventory::from_snapshot(&snapshot));
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
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
        let snapshot = NodeSnapshot { node_id: node_id.clone(), enabled_providers: Vec::new(), workspaces: Vec::new() };
        let cursor = NodeCursor { incarnation_id: gate4agent_node_protocol::NodeIncarnationId::from_bytes([1; 16]), sequence: 0 };
        ingress_tx.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result: AttemptResult::Success { cursor, snapshot: snapshot.clone(), gaps: Vec::new() } }).await.unwrap();
        status_rx.changed().await.unwrap();
        assert_eq!(status_rx.borrow().nodes[&node_id].freshness, NodeFreshness::Fresh);

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
        ingress_tx.send(Attempt { node_id: node_id.clone(), at_unix_ms: unix_ms(), result: AttemptResult::Success { cursor, snapshot, gaps: Vec::new() } }).await.unwrap();
        status_rx.changed().await.unwrap();
        assert_eq!(status_rx.borrow().nodes[&node_id].transport, NodeTransportState::Online);
        assert_eq!(status_rx.borrow().nodes[&node_id].freshness, NodeFreshness::Fresh);
        shutdown_tx.send(true).unwrap();
        owner.await.unwrap().unwrap();
    }
}
