//! Loopback-only native authority for raw provider Hook ingress.
//!
//! The listener owns socket authentication, bounded HTTP decoding, route
//! tokens, receipt ordering, and the stateful pure Hook reducer. It never
//! mutates canonical session state directly: accepted events enter through
//! the existing bounded handle as `IngestProvider` commands.

use gate4agent_adapters::{HookEventEnvelope, HookSessionReducer, HookSubagentSeed};
use gate4agent_handle::{Gate4AgentHandle, PortDispatchError};
use gate4agent_types::{
    AdapterBinding, AdapterFamily, AdapterId, AgentInstanceId, CommandEnvelope, CommandId,
    ControlCommand, ProviderSource, SessionGeneration, CONTROL_PROTOCOL_VERSION,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

pub const HOOK_INGRESS_PROTOCOL_VERSION: &str = "gate4agent-hook-ingress/v1";
pub const HOOK_REQUEST_MAX_BYTES: usize = 1_000_000;
pub const HOOK_REQUEST_HEADER_MAX_BYTES: usize = 16 * 1024;
pub const HOOK_REQUEST_SLOWLORIS_MS: u64 = 5_000;
pub const HOOK_INGRESS_MAX_CONNECTIONS: usize = 64;
pub const HOOK_INGRESS_MAX_ROUTES: usize = 1_024;

pub const HOOK_PORT_ENV: &str = "GATE4AGENT_HOOK_PORT";
pub const HOOK_TOKEN_ENV: &str = "GATE4AGENT_HOOK_TOKEN";
pub const HOOK_ROUTE_ENV: &str = "GATE4AGENT_HOOK_ROUTE";
pub const HOOK_URL_ENV: &str = "GATE4AGENT_HOOK_URL";
pub const HOOK_VERSION_ENV: &str = "GATE4AGENT_HOOK_VERSION";
pub const HOOK_TOKEN_HEADER: &str = "x-gate4agent-hook-token";
pub const HOOK_ROUTE_HEADER: &str = "x-gate4agent-hook-route";

const HOOK_COMMAND_ID_PREFIX: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookIngressConfig {
    pub port: u16,
    pub request_timeout_ms: u64,
    pub header_max_bytes: usize,
    pub body_max_bytes: usize,
    pub max_connections: usize,
    pub max_routes: usize,
}

impl Default for HookIngressConfig {
    fn default() -> Self {
        Self {
            port: 0,
            request_timeout_ms: HOOK_REQUEST_SLOWLORIS_MS,
            header_max_bytes: HOOK_REQUEST_HEADER_MAX_BYTES,
            body_max_bytes: HOOK_REQUEST_MAX_BYTES,
            max_connections: HOOK_INGRESS_MAX_CONNECTIONS,
            max_routes: HOOK_INGRESS_MAX_ROUTES,
        }
    }
}

impl HookIngressConfig {
    fn validate(self) -> Result<Self, HookIngressStartError> {
        if self.request_timeout_ms == 0
            || self.request_timeout_ms > HOOK_REQUEST_SLOWLORIS_MS
            || self.header_max_bytes == 0
            || self.header_max_bytes > HOOK_REQUEST_HEADER_MAX_BYTES
            || self.body_max_bytes == 0
            || self.body_max_bytes > HOOK_REQUEST_MAX_BYTES
            || self.body_max_bytes > gate4agent_adapters::HOOK_PAYLOAD_MAX_BYTES
            || self.max_connections == 0
            || self.max_connections > HOOK_INGRESS_MAX_CONNECTIONS
            || self.max_routes == 0
            || self.max_routes > HOOK_INGRESS_MAX_ROUTES
        {
            return Err(HookIngressStartError::InvalidConfig);
        }
        Ok(self)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HookIngressEndpoint {
    address: SocketAddr,
    token: Arc<str>,
}

impl HookIngressEndpoint {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    pub fn authorization_token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for HookIngressEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookIngressEndpoint")
            .field("address", &self.address)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HookIngressRoute {
    endpoint: HookIngressEndpoint,
    route_token: Arc<str>,
    hook_path: &'static str,
}

impl HookIngressRoute {
    pub fn environment(&self) -> Vec<(String, String)> {
        vec![
            (HOOK_PORT_ENV.to_owned(), self.endpoint.port().to_string()),
            (
                HOOK_TOKEN_ENV.to_owned(),
                self.endpoint.authorization_token().to_owned(),
            ),
            (HOOK_ROUTE_ENV.to_owned(), self.route_token.to_string()),
            (
                HOOK_URL_ENV.to_owned(),
                format!("http://{}{}", self.endpoint.address(), self.hook_path),
            ),
            (
                HOOK_VERSION_ENV.to_owned(),
                HOOK_INGRESS_PROTOCOL_VERSION.to_owned(),
            ),
        ]
    }
}

impl fmt::Debug for HookIngressRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookIngressRoute")
            .field("endpoint", &self.endpoint)
            .field("route_token", &"[REDACTED]")
            .field("hook_path", &self.hook_path)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
}

struct HookRouteState {
    key: SessionKey,
    binding: AdapterBinding,
    reducer: HookSessionReducer,
    next_receipt_sequence: u64,
    next_dispatch_sequence: u64,
}

#[derive(Default)]
struct HookRouteTable {
    by_token: HashMap<String, HookRouteState>,
    by_session: HashMap<SessionKey, String>,
}

struct HookIngressShared {
    endpoint: HookIngressEndpoint,
    handle: Gate4AgentHandle,
    routes: Mutex<HookRouteTable>,
    next_command_id: AtomicU64,
    active: AtomicBool,
    max_routes: usize,
}

#[derive(Clone)]
pub struct HookIngressControl {
    shared: Arc<HookIngressShared>,
}

impl HookIngressControl {
    pub fn endpoint(&self) -> HookIngressEndpoint {
        self.shared.endpoint.clone()
    }

    pub fn is_running(&self) -> bool {
        self.shared.active.load(Ordering::Acquire)
    }

    pub fn active_route_count(&self) -> usize {
        self.shared
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_token
            .len()
    }

    pub fn register_route(
        &self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        binding: AdapterBinding,
    ) -> Result<HookIngressRoute, HookIngressRouteError> {
        if !self.is_running() {
            return Err(HookIngressRouteError::NotRunning);
        }
        let hook_path = hook_path_for_adapter(&binding.id).ok_or_else(|| {
            HookIngressRouteError::UnsupportedAdapter(binding.id.as_str().to_owned())
        })?;
        let key = SessionKey {
            instance_id,
            generation,
        };
        let mut routes = self
            .shared
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(token) = routes.by_session.get(&key).cloned() {
            if routes
                .by_token
                .get(&token)
                .is_some_and(|route| route.binding == binding)
            {
                return Ok(HookIngressRoute {
                    endpoint: self.shared.endpoint.clone(),
                    route_token: Arc::from(token),
                    hook_path,
                });
            }
            routes.by_session.remove(&key);
            routes.by_token.remove(&token);
        }
        let stale_keys = routes
            .by_session
            .keys()
            .filter(|candidate| candidate.instance_id == instance_id)
            .copied()
            .collect::<Vec<_>>();
        for stale_key in stale_keys {
            if let Some(token) = routes.by_session.remove(&stale_key) {
                routes.by_token.remove(&token);
            }
        }
        if routes.by_token.len() >= self.shared.max_routes {
            return Err(HookIngressRouteError::Capacity);
        }

        let (next_dispatch_sequence, seeds) =
            route_seed(&self.shared.handle, instance_id, generation, &binding);
        let mut reducer = HookSessionReducer::new(binding.id.clone());
        reducer.seed_live_subagents(&seeds);
        let route_token = loop {
            let candidate = Uuid::new_v4().simple().to_string();
            if !routes.by_token.contains_key(&candidate) {
                break candidate;
            }
        };
        routes.by_session.insert(key, route_token.clone());
        routes.by_token.insert(
            route_token.clone(),
            HookRouteState {
                key,
                binding,
                reducer,
                next_receipt_sequence: 1,
                next_dispatch_sequence,
            },
        );
        Ok(HookIngressRoute {
            endpoint: self.shared.endpoint.clone(),
            route_token: Arc::from(route_token),
            hook_path,
        })
    }

    pub fn remove_route(
        &self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    ) -> bool {
        let key = SessionKey {
            instance_id,
            generation,
        };
        let mut routes = self
            .shared
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(token) = routes.by_session.remove(&key) else {
            return false;
        };
        routes.by_token.remove(&token);
        true
    }

    fn clear_routes(&self) {
        let mut routes = self
            .shared
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        routes.by_token.clear();
        routes.by_session.clear();
    }
}

pub struct HookIngressServer {
    endpoint: HookIngressEndpoint,
    control: HookIngressControl,
    shutdown_tx: Option<watch::Sender<bool>>,
    task: Option<JoinHandle<()>>,
}

impl HookIngressServer {
    pub async fn start(
        handle: Gate4AgentHandle,
        config: HookIngressConfig,
    ) -> Result<Self, HookIngressStartError> {
        let config = config.validate()?;
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.port))
            .await
            .map_err(HookIngressStartError::Bind)?;
        let address = listener
            .local_addr()
            .map_err(HookIngressStartError::LocalAddress)?;
        if !address.ip().is_loopback() {
            return Err(HookIngressStartError::NonLoopbackBind(address));
        }
        let endpoint = HookIngressEndpoint {
            address,
            token: Arc::from(Uuid::new_v4().simple().to_string()),
        };
        let shared = Arc::new(HookIngressShared {
            endpoint: endpoint.clone(),
            handle,
            routes: Mutex::new(HookRouteTable::default()),
            next_command_id: AtomicU64::new(1),
            active: AtomicBool::new(true),
            max_routes: config.max_routes,
        });
        let control = HookIngressControl {
            shared: Arc::clone(&shared),
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_listener(listener, shared, config, shutdown_rx));
        Ok(Self {
            endpoint,
            control,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }

    pub fn endpoint(&self) -> &HookIngressEndpoint {
        &self.endpoint
    }

    pub fn control(&self) -> HookIngressControl {
        self.control.clone()
    }

    pub fn is_running(&self) -> bool {
        self.control.is_running() && self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    pub async fn stop(mut self) {
        self.stop_inner().await;
    }

    async fn stop_inner(&mut self) {
        self.control.shared.active.store(false, Ordering::Release);
        self.control.clear_routes();
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(true);
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HookIngressServer {
    fn drop(&mut self) {
        self.control.shared.active.store(false, Ordering::Release);
        self.control.clear_routes();
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(true);
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn route_seed(
    handle: &Gate4AgentHandle,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    binding: &AdapterBinding,
) -> (u64, Vec<HookSubagentSeed>) {
    let snapshot = handle.snapshot();
    let Some(session) = snapshot
        .sessions
        .iter()
        .find(|session| session.instance_id == instance_id && session.generation == generation)
    else {
        return (1, Vec::new());
    };
    let source = ProviderSource {
        family: AdapterFamily::Hook,
        binding: binding.clone(),
    };
    let next_sequence = session
        .provider
        .sources
        .iter()
        .find(|cursor| cursor.source == source)
        .map_or(1, |cursor| cursor.sequence.saturating_add(1).max(1));
    let seeds = session
        .provider
        .subagents
        .iter()
        .filter(|subagent| subagent.source == source)
        .map(|subagent| HookSubagentSeed {
            provider_agent_id: subagent.provider_agent_id.clone(),
            agent_type: subagent.agent_type.clone(),
            description: subagent.description.clone(),
        })
        .collect();
    (next_sequence, seeds)
}

async fn run_listener(
    listener: TcpListener,
    shared: Arc<HookIngressShared>,
    config: HookIngressConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let semaphore = Arc::new(Semaphore::new(config.max_connections));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { break };
                if !peer.ip().is_loopback() {
                    continue;
                }
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    continue;
                };
                let connection_shared = Arc::clone(&shared);
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(
                        Duration::from_millis(config.request_timeout_ms),
                        serve_connection(stream, connection_shared, config),
                    )
                    .await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    shared.active.store(false, Ordering::Release);
}

async fn serve_connection(
    mut stream: TcpStream,
    shared: Arc<HookIngressShared>,
    config: HookIngressConfig,
) -> io::Result<()> {
    let status = match read_http_request(&mut stream, &shared, config).await {
        Ok(request) => handle_request(&shared, request),
        Err(HttpReadError::HeaderTooLarge | HttpReadError::BodyTooLarge) => {
            HttpStatus::PayloadTooLarge
        }
        Err(HttpReadError::LengthRequired) => HttpStatus::LengthRequired,
        Err(HttpReadError::Forbidden) => HttpStatus::Forbidden,
        Err(HttpReadError::NotFound) => HttpStatus::NotFound,
        Err(HttpReadError::UnsupportedTransferEncoding | HttpReadError::Malformed) => {
            HttpStatus::BadRequest
        }
        Err(HttpReadError::Io(error)) => return Err(error),
    };
    write_response(&mut stream, status).await
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, Error)]
enum HttpReadError {
    #[error("request headers exceed the configured bound")]
    HeaderTooLarge,
    #[error("request body exceeds the configured bound")]
    BodyTooLarge,
    #[error("content-length is required")]
    LengthRequired,
    #[error("hook authorization failed")]
    Forbidden,
    #[error("hook route was not found")]
    NotFound,
    #[error("transfer-encoding is unsupported")]
    UnsupportedTransferEncoding,
    #[error("malformed HTTP request")]
    Malformed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

async fn read_http_request(
    stream: &mut TcpStream,
    shared: &HookIngressShared,
    config: HookIngressConfig,
) -> Result<HttpRequest, HttpReadError> {
    let mut buffered = Vec::with_capacity(config.header_max_bytes.min(4_096));
    let mut chunk = [0_u8; 8 * 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(HttpReadError::Malformed);
        }
        buffered.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_bytes(&buffered, b"\r\n\r\n") {
            if index > config.header_max_bytes {
                return Err(HttpReadError::HeaderTooLarge);
            }
            break index + 4;
        }
        if buffered.len() > config.header_max_bytes {
            return Err(HttpReadError::HeaderTooLarge);
        }
    };

    let header_text =
        std::str::from_utf8(&buffered[..header_end]).map_err(|_| HttpReadError::Malformed)?;
    let mut lines = header_text[..header_text.len() - 4].split("\r\n");
    let request_line = lines.next().ok_or(HttpReadError::Malformed)?;
    let mut request_parts = request_line.split_ascii_whitespace();
    let method = request_parts.next().ok_or(HttpReadError::Malformed)?;
    let path = request_parts.next().ok_or(HttpReadError::Malformed)?;
    let version = request_parts.next().ok_or(HttpReadError::Malformed)?;
    if request_parts.next().is_some()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || !path.starts_with('/')
    {
        return Err(HttpReadError::Malformed);
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            return Err(HttpReadError::Malformed);
        }
        let (name, value) = line.split_once(':').ok_or(HttpReadError::Malformed)?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if name.is_empty() || headers.insert(name, value).is_some() {
            return Err(HttpReadError::Malformed);
        }
    }
    let body_length = if method == "POST" {
        let token = headers
            .get(HOOK_TOKEN_HEADER)
            .ok_or(HttpReadError::Forbidden)?;
        if !constant_time_equal(
            token.as_bytes(),
            shared.endpoint.authorization_token().as_bytes(),
        ) {
            return Err(HttpReadError::Forbidden);
        }
        let request_path = path.split('?').next().unwrap_or(path);
        let expected_adapter =
            hook_adapter_for_path(request_path).ok_or(HttpReadError::NotFound)?;
        let route_token = headers
            .get(HOOK_ROUTE_HEADER)
            .ok_or(HttpReadError::NotFound)?;
        let routes = shared
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if routes
            .by_token
            .get(route_token)
            .is_none_or(|route| route.binding.id.as_str() != expected_adapter)
        {
            return Err(HttpReadError::NotFound);
        }
        drop(routes);
        if headers.contains_key("transfer-encoding") {
            return Err(HttpReadError::UnsupportedTransferEncoding);
        }
        headers
            .get("content-length")
            .ok_or(HttpReadError::LengthRequired)?
            .parse::<usize>()
            .map_err(|_| HttpReadError::Malformed)?
    } else {
        0
    };
    if body_length > config.body_max_bytes {
        return Err(HttpReadError::BodyTooLarge);
    }

    let already_read = buffered.len().saturating_sub(header_end);
    let mut body = Vec::with_capacity(body_length);
    body.extend_from_slice(&buffered[header_end..header_end + already_read.min(body_length)]);
    while body.len() < body_length {
        let remaining = body_length - body.len();
        let read_limit = remaining.min(chunk.len());
        let read = stream.read(&mut chunk[..read_limit]).await?;
        if read == 0 {
            return Err(HttpReadError::Malformed);
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(HttpRequest {
        method: method.to_owned(),
        path: path.split('?').next().unwrap_or(path).to_owned(),
        headers,
        body,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

struct DecodedHookEvent {
    event_name: String,
    event_id: Option<String>,
    payload: Value,
}

fn decode_hook_event(request: &HttpRequest) -> Option<DecodedHookEvent> {
    let content_type = request
        .headers
        .get("content-type")
        .map_or("", String::as_str)
        .to_ascii_lowercase();
    let body = if content_type.contains("application/x-www-form-urlencoded") {
        decode_form_body(&request.body)?
    } else {
        serde_json::from_slice::<Value>(&request.body).ok()?
    };
    let record = body.as_object()?;
    let payload = match record.get("payload") {
        Some(Value::String(payload)) => serde_json::from_str::<Value>(payload).ok()?,
        Some(payload) => payload.clone(),
        None => body.clone(),
    };
    let payload_record = payload.as_object()?;
    let event_name = first_nonempty_string(
        record,
        &[
            "event_name",
            "hook_event_name",
            "hookEventName",
            "hook_type",
            "hookType",
        ],
    )
    .or_else(|| {
        first_nonempty_string(
            payload_record,
            &[
                "event_name",
                "hook_event_name",
                "hookEventName",
                "hook_type",
                "hookType",
            ],
        )
    })?;
    let event_id = first_nonempty_string(record, &["event_id", "eventId"])
        .or_else(|| first_nonempty_string(payload_record, &["event_id", "eventId"]));
    Some(DecodedHookEvent {
        event_name,
        event_id,
        payload,
    })
}

fn first_nonempty_string(record: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = record.get(*key)?.as_str()?.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn decode_form_body(body: &[u8]) -> Option<Value> {
    let mut record = serde_json::Map::new();
    if body.is_empty() {
        return Some(Value::Object(record));
    }
    for pair in body.split(|byte| *byte == b'&') {
        let (key, value) = pair
            .iter()
            .position(|byte| *byte == b'=')
            .map_or((pair, &[][..]), |separator| {
                (&pair[..separator], &pair[separator + 1..])
            });
        let key = decode_form_component(key)?;
        let value = decode_form_component(value)?;
        if key.is_empty() || record.insert(key, Value::String(value)).is_some() {
            return None;
        }
    }
    Some(Value::Object(record))
}

fn decode_form_component(component: &[u8]) -> Option<String> {
    let mut decoded = Vec::with_capacity(component.len());
    let mut index = 0;
    while index < component.len() {
        match component[index] {
            b'+' => decoded.push(b' '),
            b'%' => {
                let high = *component.get(index + 1)?;
                let low = *component.get(index + 2)?;
                decoded.push((hex_value(high)? << 4) | hex_value(low)?);
                index += 2;
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn handle_request(shared: &HookIngressShared, request: HttpRequest) -> HttpStatus {
    if request.method != "POST" {
        return HttpStatus::NotFound;
    }
    let Some(token) = request.headers.get(HOOK_TOKEN_HEADER) else {
        return HttpStatus::Forbidden;
    };
    if !constant_time_equal(
        token.as_bytes(),
        shared.endpoint.authorization_token().as_bytes(),
    ) {
        return HttpStatus::Forbidden;
    }
    let Some(expected_adapter) = hook_adapter_for_path(&request.path) else {
        return HttpStatus::NotFound;
    };
    let Some(route_token) = request.headers.get(HOOK_ROUTE_HEADER) else {
        return HttpStatus::NotFound;
    };
    let Some(body) = decode_hook_event(&request) else {
        return HttpStatus::NoContent;
    };

    let mut routes = shared
        .routes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(route) = routes.by_token.get_mut(route_token) else {
        return HttpStatus::NotFound;
    };
    if route.binding.id.as_str() != expected_adapter {
        return HttpStatus::NotFound;
    }
    let mut reducer = route.reducer.clone();
    let reduction = match reducer.reduce(HookEventEnvelope {
        source_sequence: route.next_receipt_sequence,
        event_id: body.event_id,
        event_name: body.event_name,
        payload: body.payload,
    }) {
        Ok(reduction) => reduction,
        Err(_) => return HttpStatus::NoContent,
    };
    if reduction.events.is_empty() {
        route.reducer = reducer;
        route.next_receipt_sequence = route.next_receipt_sequence.saturating_add(1).max(1);
        return HttpStatus::NoContent;
    }

    let command_counter = shared.next_command_id.fetch_add(1, Ordering::AcqRel);
    let command = CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(HOOK_COMMAND_ID_PREFIX | command_counter),
        command: ControlCommand::IngestProvider {
            instance_id: route.key.instance_id,
            generation: route.key.generation,
            source: ProviderSource {
                family: AdapterFamily::Hook,
                binding: route.binding.clone(),
            },
            source_sequence: route.next_dispatch_sequence,
            events: reduction.events,
        },
    };
    match shared.handle.dispatch(command) {
        Ok(()) => {
            route.reducer = reducer;
            route.next_receipt_sequence = route.next_receipt_sequence.saturating_add(1).max(1);
            route.next_dispatch_sequence = route.next_dispatch_sequence.saturating_add(1).max(1);
            HttpStatus::NoContent
        }
        Err(PortDispatchError::Full | PortDispatchError::Disconnected) => {
            HttpStatus::ServiceUnavailable
        }
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HttpStatus {
    NoContent,
    BadRequest,
    Forbidden,
    NotFound,
    LengthRequired,
    PayloadTooLarge,
    ServiceUnavailable,
}

impl HttpStatus {
    fn line(self) -> &'static str {
        match self {
            Self::NoContent => "204 No Content",
            Self::BadRequest => "400 Bad Request",
            Self::Forbidden => "403 Forbidden",
            Self::NotFound => "404 Not Found",
            Self::LengthRequired => "411 Length Required",
            Self::PayloadTooLarge => "413 Payload Too Large",
            Self::ServiceUnavailable => "503 Service Unavailable",
        }
    }
}

async fn write_response(stream: &mut TcpStream, status: HttpStatus) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status.line()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

pub fn hook_path_for_adapter(adapter_id: &AdapterId) -> Option<&'static str> {
    match adapter_id.as_str() {
        "claude-code" => Some("/hook/claude"),
        "codex" => Some("/hook/codex"),
        "gemini" => Some("/hook/gemini"),
        "antigravity" => Some("/hook/antigravity"),
        "amp" => Some("/hook/amp"),
        "opencode" => Some("/hook/opencode"),
        "mimo-code" => Some("/hook/mimo-code"),
        "cursor" => Some("/hook/cursor"),
        "pi" => Some("/hook/pi"),
        "omp" => Some("/hook/omp"),
        "droid" => Some("/hook/droid"),
        "command-code" => Some("/hook/command-code"),
        "grok" => Some("/hook/grok"),
        "copilot" => Some("/hook/copilot"),
        "hermes" => Some("/hook/hermes"),
        "devin" => Some("/hook/devin"),
        "kimi" => Some("/hook/kimi"),
        _ => None,
    }
}

fn hook_adapter_for_path(path: &str) -> Option<&'static str> {
    match path {
        "/hook/claude" => Some("claude-code"),
        "/hook/codex" => Some("codex"),
        "/hook/gemini" => Some("gemini"),
        "/hook/antigravity" => Some("antigravity"),
        "/hook/amp" => Some("amp"),
        "/hook/opencode" => Some("opencode"),
        "/hook/mimo-code" => Some("mimo-code"),
        "/hook/cursor" => Some("cursor"),
        "/hook/pi" => Some("pi"),
        "/hook/omp" => Some("omp"),
        "/hook/droid" => Some("droid"),
        "/hook/command-code" => Some("command-code"),
        "/hook/grok" => Some("grok"),
        "/hook/copilot" => Some("copilot"),
        "/hook/hermes" => Some("hermes"),
        "/hook/devin" => Some("devin"),
        "/hook/kimi" => Some("kimi"),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum HookIngressStartError {
    #[error("hook-ingress configuration is invalid")]
    InvalidConfig,
    #[error("failed to bind the loopback hook listener: {0}")]
    Bind(#[source] io::Error),
    #[error("failed to read the hook listener address: {0}")]
    LocalAddress(#[source] io::Error),
    #[error("hook listener unexpectedly bound a non-loopback address: {0}")]
    NonLoopbackBind(SocketAddr),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HookIngressRouteError {
    #[error("hook ingress is not running")]
    NotRunning,
    #[error("hook adapter {0} has no loopback route")]
    UnsupportedAdapter(String),
    #[error("hook route capacity is exhausted")]
    Capacity,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_adapters::builtin_adapter_registry;
    use gate4agent_handle::bounded_port;
    use gate4agent_types::{AgentId, ProviderEvent, TransportKind};
    use serde_json::json;

    fn binding(id: &str) -> AdapterBinding {
        builtin_adapter_registry()
            .binding(AdapterFamily::Hook, id)
            .unwrap()
            .clone()
    }

    async fn request(address: SocketAddr, request: String) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    async fn post(
        endpoint: &HookIngressEndpoint,
        route: &HookIngressRoute,
        path: &str,
        body: &Value,
    ) -> String {
        let body = serde_json::to_string(body).unwrap();
        request(
            endpoint.address(),
            format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{HOOK_TOKEN_HEADER}: {}\r\n{HOOK_ROUTE_HEADER}: {}\r\n\r\n{body}",
                body.len(),
                endpoint.authorization_token(),
                route.route_token,
            ),
        )
        .await
    }

    #[test]
    fn decodes_orca_json_and_form_envelopes() {
        let json_request = HttpRequest {
            method: "POST".to_owned(),
            path: "/hook/claude".to_owned(),
            headers: HashMap::from([(
                "content-type".to_owned(),
                "application/json; charset=utf-8".to_owned(),
            )]),
            body: serde_json::to_vec(&json!({
                "hook_event_name": "UserPromptSubmit",
                "event_id": "json-1",
                "payload": {
                    "hook_event_name": "UserPromptSubmit",
                    "prompt": "hello json"
                }
            }))
            .unwrap(),
        };
        let decoded = decode_hook_event(&json_request).unwrap();
        assert_eq!(decoded.event_name, "UserPromptSubmit");
        assert_eq!(decoded.event_id.as_deref(), Some("json-1"));
        assert_eq!(decoded.payload["prompt"], "hello json");

        let form_request = HttpRequest {
            method: "POST".to_owned(),
            path: "/hook/claude".to_owned(),
            headers: HashMap::from([(
                "content-type".to_owned(),
                "application/x-www-form-urlencoded".to_owned(),
            )]),
            body: b"hookEventName=UserPromptSubmit&event_id=form-1&payload=%7B%22prompt%22%3A%22hello+form%22%7D".to_vec(),
        };
        let decoded = decode_hook_event(&form_request).unwrap();
        assert_eq!(decoded.event_name, "UserPromptSubmit");
        assert_eq!(decoded.event_id.as_deref(), Some("form-1"));
        assert_eq!(decoded.payload["prompt"], "hello form");
    }

    #[test]
    fn configured_limits_can_only_tighten_the_hard_ingress_bounds() {
        assert!(matches!(
            HookIngressConfig {
                max_connections: HOOK_INGRESS_MAX_CONNECTIONS + 1,
                ..HookIngressConfig::default()
            }
            .validate(),
            Err(HookIngressStartError::InvalidConfig)
        ));
        assert!(matches!(
            HookIngressConfig {
                header_max_bytes: HOOK_REQUEST_HEADER_MAX_BYTES + 1,
                ..HookIngressConfig::default()
            }
            .validate(),
            Err(HookIngressStartError::InvalidConfig)
        ));
    }

    #[tokio::test]
    async fn loopback_route_reduces_and_dispatches_without_a_second_state_writer() {
        let (handle, port) = bounded_port(8);
        let server = HookIngressServer::start(handle, HookIngressConfig::default())
            .await
            .unwrap();
        assert!(server.endpoint().address().ip().is_loopback());
        assert!(
            !format!("{:?}", server.endpoint()).contains(server.endpoint().authorization_token())
        );

        let route = server
            .control()
            .register_route(AgentInstanceId(7), SessionGeneration(2), binding("grok"))
            .unwrap();
        let environment = route.environment();
        assert!(environment
            .iter()
            .any(|(key, value)| key == HOOK_URL_ENV && value.ends_with("/hook/grok")));

        let first = post(
            server.endpoint(),
            &route,
            "/hook/grok",
            &json!({
                "event_name": "UserPromptSubmit",
                "event_id": "evt-1",
                "payload": {"prompt": "inspect state"}
            }),
        )
        .await;
        assert!(first.starts_with("HTTP/1.1 204"));
        let commands = port.drain_commands(8);
        assert!(matches!(
            commands.as_slice(),
            [CommandEnvelope {
                command: ControlCommand::IngestProvider {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                    source_sequence: 1,
                    events,
                    ..
                },
                ..
            }] if matches!(
                events.as_slice(),
                [ProviderEvent::TurnStarted { prompt: Some(prompt) }] if prompt == "inspect state"
            )
        ));

        let duplicate = post(
            server.endpoint(),
            &route,
            "/hook/grok",
            &json!({
                "event_name": "UserPromptSubmit",
                "event_id": "evt-1",
                "payload": {"prompt": "inspect state"}
            }),
        )
        .await;
        assert!(duplicate.starts_with("HTTP/1.1 204"));
        assert!(port.drain_commands(8).is_empty());

        let second = post(
            server.endpoint(),
            &route,
            "/hook/grok",
            &json!({
                "event_name": "Stop",
                "event_id": "evt-2",
                "payload": {}
            }),
        )
        .await;
        assert!(second.starts_with("HTTP/1.1 204"));
        assert!(matches!(
            port.drain_commands(8).as_slice(),
            [CommandEnvelope {
                command: ControlCommand::IngestProvider {
                    source_sequence: 2,
                    ..
                },
                ..
            }]
        ));

        let control = server.control();
        assert_eq!(control.active_route_count(), 1);
        server.stop().await;
        assert!(!control.is_running());
        assert_eq!(control.active_route_count(), 0);
    }

    #[tokio::test]
    async fn auth_source_bounds_and_slow_client_fail_closed_at_the_socket() {
        let (handle, port) = bounded_port(8);
        let server = HookIngressServer::start(
            handle,
            HookIngressConfig {
                request_timeout_ms: 50,
                body_max_bytes: 256,
                ..HookIngressConfig::default()
            },
        )
        .await
        .unwrap();
        let route = server
            .control()
            .register_route(AgentInstanceId(8), SessionGeneration(1), binding("grok"))
            .unwrap();

        let unauthorized = request(
            server.endpoint().address(),
            format!(
                "POST /hook/grok HTTP/1.1\r\nContent-Length: 1000000\r\n{HOOK_TOKEN_HEADER}: wrong\r\n{HOOK_ROUTE_HEADER}: {}\r\n\r\n",
                route.route_token
            ),
        )
        .await;
        assert!(unauthorized.starts_with("HTTP/1.1 403"));

        let wrong_source = post(
            server.endpoint(),
            &route,
            "/hook/codex",
            &json!({"event_name": "Stop", "payload": {}}),
        )
        .await;
        assert!(wrong_source.starts_with("HTTP/1.1 404"));

        let wrong_method = request(
            server.endpoint().address(),
            "GET /hook/grok HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_owned(),
        )
        .await;
        assert!(wrong_method.starts_with("HTTP/1.1 404"));

        let oversized = request(
            server.endpoint().address(),
            format!(
                "POST /hook/grok HTTP/1.1\r\nContent-Length: 257\r\n{HOOK_TOKEN_HEADER}: {}\r\n{HOOK_ROUTE_HEADER}: {}\r\n\r\n",
                server.endpoint().authorization_token(),
                route.route_token
            ),
        )
        .await;
        assert!(oversized.starts_with("HTTP/1.1 413"));
        assert!(port.drain_commands(8).is_empty());

        let mut slow = TcpStream::connect(server.endpoint().address())
            .await
            .unwrap();
        slow.write_all(b"POST /hook/grok HTTP/1.1\r\nContent-Length: 10\r\n")
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(1), slow.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closed, 0);
        server.stop().await;
    }

    #[tokio::test]
    async fn replacing_a_session_route_at_capacity_revokes_the_old_capability() {
        let (handle, _port) = bounded_port(8);
        let server = HookIngressServer::start(
            handle,
            HookIngressConfig {
                max_routes: 1,
                ..HookIngressConfig::default()
            },
        )
        .await
        .unwrap();
        let control = server.control();
        let old_route = control
            .register_route(AgentInstanceId(10), SessionGeneration(1), binding("grok"))
            .unwrap();
        let new_route = control
            .register_route(AgentInstanceId(10), SessionGeneration(2), binding("grok"))
            .unwrap();
        assert_eq!(control.active_route_count(), 1);

        let body = json!({"event_name": "Stop", "payload": {}});
        let revoked = post(server.endpoint(), &old_route, "/hook/grok", &body).await;
        assert!(revoked.starts_with("HTTP/1.1 404"));
        let accepted = post(server.endpoint(), &new_route, "/hook/grok", &body).await;
        assert!(accepted.starts_with("HTTP/1.1 204"));

        drop(server);
        assert!(!control.is_running());
        assert_eq!(control.active_route_count(), 0);
    }

    #[tokio::test]
    async fn full_command_ingress_returns_retryable_without_consuming_reducer_state() {
        let (handle, port) = bounded_port(1);
        handle
            .dispatch(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(1),
                command: ControlCommand::Register {
                    instance_id: AgentInstanceId(99),
                    agent_id: AgentId::new("grok").unwrap(),
                    transport: TransportKind::Pty,
                },
            })
            .unwrap();
        let server = HookIngressServer::start(handle, HookIngressConfig::default())
            .await
            .unwrap();
        let route = server
            .control()
            .register_route(AgentInstanceId(9), SessionGeneration(1), binding("grok"))
            .unwrap();
        let body = json!({
            "event_name": "UserPromptSubmit",
            "event_id": "retry-1",
            "payload": {"prompt": "retry me"}
        });
        let full = post(server.endpoint(), &route, "/hook/grok", &body).await;
        assert!(full.starts_with("HTTP/1.1 503"));

        assert_eq!(port.drain_commands(1).len(), 1);
        let retried = post(server.endpoint(), &route, "/hook/grok", &body).await;
        assert!(retried.starts_with("HTTP/1.1 204"));
        assert!(matches!(
            port.drain_commands(1).as_slice(),
            [CommandEnvelope {
                command: ControlCommand::IngestProvider {
                    source_sequence: 1,
                    ..
                },
                ..
            }]
        ));
        server.stop().await;
    }

    #[test]
    fn pinned_orca_hook_paths_are_exact_and_complete() {
        let expected = [
            ("claude-code", "/hook/claude"),
            ("codex", "/hook/codex"),
            ("gemini", "/hook/gemini"),
            ("antigravity", "/hook/antigravity"),
            ("amp", "/hook/amp"),
            ("opencode", "/hook/opencode"),
            ("mimo-code", "/hook/mimo-code"),
            ("cursor", "/hook/cursor"),
            ("pi", "/hook/pi"),
            ("omp", "/hook/omp"),
            ("droid", "/hook/droid"),
            ("command-code", "/hook/command-code"),
            ("grok", "/hook/grok"),
            ("copilot", "/hook/copilot"),
            ("hermes", "/hook/hermes"),
            ("devin", "/hook/devin"),
            ("kimi", "/hook/kimi"),
        ];
        for (adapter, path) in expected {
            let adapter = AdapterId::new(adapter).unwrap();
            assert_eq!(hook_path_for_adapter(&adapter), Some(path));
            assert_eq!(hook_adapter_for_path(path), Some(adapter.as_str()));
        }
    }
}
