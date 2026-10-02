//! Node-envelope HTTP + WebSocket browser bridge.
//!
//! Separate from framed node-wire C2 and from the ops `--api-listen` HTTP
//! observer (`http_api`). Loopback-only bind; optional `GATE4AGENT_BRIDGE_TOKEN`
//! (never logged). Product bytes are HTTP+WS over TCP. UDP is not an
//! application protocol here — it remains the mesh/WG-class underlay each
//! peer will own a slice of later (see design plan connectivity doctrine).
//!
//! Observe projection (tip 3): slim per-session inventory over WS / HTTP
//! snapshot — address, status, screen kind, opaque browser_profile_id when
//! bound. No terminal_frame contents, history messages, cookies, or tokens.
//!
//! Drive (tip 4): optional `POST /bridge/drive` maps a small NodeRequest
//! subset (`prompt` / `paste`) through the node envelope
//! (`dispatch_input_bounded` + runtime policy). Bridge token + loopback are
//! the auth barrier (not C2 controller lease). No raw PTY bytes / TerminalBytes
//! / cookies / OAuth as product API.

use super::NodeShared;
use crate::protocol::{
    BUILD_STAMP, MAX_NODE_TEXT_BYTES, NodeFailure, NodeFailureCode, SessionAddress, SessionKey,
    WorkspaceId,
};
use crate::provider_runtime::ProviderRuntimeRequirement;
use gate4agent_types::{
    AgentInstanceId, InputAction, PreparedInputKind, PromptFraming, PromptPayload, PtyScreenState,
    SessionGeneration, SessionSnapshot, SessionStatus, TerminalSize,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

const SERVICE_NAME: &str = "gate4agent-node-bridge";
const HEADER_LIMIT_BYTES: usize = 16 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BRIDGE_CONNECTIONS: usize = 8;
/// JSON envelope overhead budget on top of `MAX_NODE_TEXT_BYTES` for drive POST.
const BRIDGE_DRIVE_BODY_OVERHEAD: usize = 1_024;
const MAX_BRIDGE_DRIVE_BODY_BYTES: usize = MAX_NODE_TEXT_BYTES + BRIDGE_DRIVE_BODY_OVERHEAD;
/// RFC6455 GUID for `Sec-WebSocket-Accept`.
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Optional node-local bridge shared secret (distinct from NODE_TOKEN).
/// Never log the value. Empty/`None` = loopback bind is the only gate.
#[derive(Clone, Default)]
pub(super) struct BridgeAuth {
    token: Option<String>,
}

impl BridgeAuth {
    pub(super) fn from_optional(token: Option<String>) -> Result<Self, BridgeAuthError> {
        match token {
            None => Ok(Self { token: None }),
            Some(value) if value.is_empty() || value.len() > 4_096 => Err(BridgeAuthError::Invalid),
            Some(value) => Ok(Self { token: Some(value) }),
        }
    }

    fn authorized(&self, provided: Option<&str>) -> bool {
        match self.token.as_deref() {
            None => true,
            Some(expected) => match provided {
                Some(got) => tokens_match(got, expected),
                None => false,
            },
        }
    }
}

#[derive(Debug)]
pub(super) enum BridgeAuthError {
    Invalid,
}

pub(super) async fn run(
    listen: Option<SocketAddr>,
    auth: BridgeAuth,
    shared: Arc<NodeShared>,
) -> io::Result<()> {
    let Some(listen) = listen else {
        wait_for_shutdown(&shared).await;
        return Ok(());
    };
    let listener = TcpListener::bind(listen).await?;
    serve_listener(listener, auth, shared).await
}

async fn wait_for_shutdown(shared: &NodeShared) {
    loop {
        let notified = shared.shutdown_notify.notified();
        tokio::pin!(notified);
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn serve_listener(
    listener: TcpListener,
    auth: BridgeAuth,
    shared: Arc<NodeShared>,
) -> io::Result<()> {
    let permits = Arc::new(Semaphore::new(MAX_BRIDGE_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        let shutdown = shared.shutdown_notify.notified();
        tokio::pin!(shutdown);
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                if shared.shutdown.load(Ordering::Acquire) {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                // Defense in depth: refuse non-loopback peers even if bind was loopback.
                if !peer.ip().is_loopback() {
                    drop(stream);
                    continue;
                }
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let connection_shared = Arc::clone(&shared);
                let connection_auth = auth.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = serve_connection(stream, connection_auth, connection_shared).await;
                });
            }
        }
        while let Some(result) = connections.try_join_next() {
            result.map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        }
    }
    connections.shutdown().await;
    Ok(())
}

async fn serve_connection(
    mut stream: TcpStream,
    auth: BridgeAuth,
    shared: Arc<NodeShared>,
) -> io::Result<()> {
    let request = match timeout(HEADER_READ_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(ReadRequestError::TooLarge)) => {
            return write_http(&mut stream, HttpResponse::plain(413, "Payload Too Large")).await;
        }
        Ok(Err(ReadRequestError::Closed | ReadRequestError::Invalid)) | Err(_) => return Ok(()),
        Ok(Err(ReadRequestError::Io(error))) => return Err(error),
    };

    let path_only = request
        .target
        .split_once('?')
        .map_or(request.target.as_str(), |(path, _)| path);

    if path_only == "/bridge/ws" {
        return handle_websocket_upgrade(stream, request, &auth, &shared).await;
    }

    if path_only == "/bridge/drive" {
        return handle_drive(&mut stream, request, &auth, &shared).await;
    }

    if request.method != "GET" {
        return write_http(
            &mut stream,
            HttpResponse::plain(405, "Method Not Allowed").with_header("Allow", "GET"),
        )
        .await;
    }

    let provided = extract_bridge_token(&request);
    if !auth.authorized(provided.as_deref()) {
        return write_http(
            &mut stream,
            HttpResponse::plain(401, "Unauthorized").with_header("WWW-Authenticate", "Bearer"),
        )
        .await;
    }

    let response = match path_only {
        "/bridge/health" | "/health" => HttpResponse::json(200, health_body(&shared)),
        "/bridge/snapshot" => HttpResponse::json(200, observe_snapshot_body(&shared)),
        _ => HttpResponse::plain(404, "Not Found"),
    };
    write_http(&mut stream, response).await
}

async fn handle_websocket_upgrade(
    mut stream: TcpStream,
    request: Request,
    auth: &BridgeAuth,
    shared: &NodeShared,
) -> io::Result<()> {
    if request.method != "GET" {
        return write_http(
            &mut stream,
            HttpResponse::plain(405, "Method Not Allowed").with_header("Allow", "GET"),
        )
        .await;
    }

    let provided = extract_bridge_token(&request);
    if !auth.authorized(provided.as_deref()) {
        return write_http(
            &mut stream,
            HttpResponse::plain(401, "Unauthorized").with_header("WWW-Authenticate", "Bearer"),
        )
        .await;
    }

    if !is_websocket_upgrade(&request) {
        return write_http(&mut stream, HttpResponse::plain(400, "Bad Request")).await;
    }
    let Some(key) = request.sec_websocket_key.as_deref() else {
        return write_http(&mut stream, HttpResponse::plain(400, "Bad Request")).await;
    };
    if request.sec_websocket_version.as_deref() != Some("13") {
        return write_http(
            &mut stream,
            HttpResponse::plain(426, "Upgrade Required")
                .with_header("Sec-WebSocket-Version", "13"),
        )
        .await;
    }

    let accept = sec_websocket_accept(key);
    let upgrade = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Accept: {accept}\r\n\
\r\n"
    );
    timeout(WRITE_TIMEOUT, stream.write_all(upgrade.as_bytes()))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "bridge ws upgrade write timed out"))??;

    // Observe frame: typed hello + slim per-session projections (tip 3).
    // Drive is HTTP POST /bridge/drive (tip 4). Streaming push remains later.
    let payload = serde_json::to_vec(&observe_frame(shared)).map_err(|error| {
        io::Error::new(io::ErrorKind::InvalidData, error)
    })?;
    let frame = encode_server_text_frame(&payload);
    timeout(WRITE_TIMEOUT, stream.write_all(&frame))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "bridge ws frame write timed out"))??;

    // One observe text frame then close. Streaming push is a later tip.
    let close = [0x88u8, 0x00]; // FIN + opcode close, empty payload
    let _ = timeout(WRITE_TIMEOUT, stream.write_all(&close)).await;
    Ok(())
}

fn is_websocket_upgrade(request: &Request) -> bool {
    let upgrade = request
        .upgrade
        .as_deref()
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let connection = request
        .connection
        .as_deref()
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    upgrade && connection
}

fn health_body(shared: &NodeShared) -> Value {
    json!({
        "ok": true,
        "service": SERVICE_NAME,
        "door": "node-envelope-bridge",
        "node_id": shared.node_id,
        "incarnation_id": shared.incarnation_id,
        "version": env!("CARGO_PKG_VERSION"),
        "build_stamp": BUILD_STAMP,
        "started_at_unix_ms": shared.started_at_unix_ms,
        "protocols": {
            "application": ["http", "websocket"],
            "transport_tcp": true,
            // UDP is mesh/WG-class underlay — not a second app protocol here.
            "transport_udp_underlay": "mesh-or-permit-set-later",
        },
    })
}

fn observe_snapshot_body(shared: &NodeShared) -> Value {
    observe_frame(shared)
}

/// Typed observe payload with slim per-session projections.
/// Privacy: no terminal_frame contents/formatted, no history messages, no
/// cookies/OAuth, no bridge/node tokens. Screen is kind (+ short classifiers)
/// only — OperatorGate options (on-screen choice labels) are stripped.
fn observe_frame(shared: &NodeShared) -> Value {
    let control = shared.handle.snapshot();
    let control_plane_sessions = control.sessions.len();
    let native_pty_sessions = shared.native_session_gauge.load(Ordering::Relaxed);
    let session_projections = slim_session_projections(shared, &control.sessions);
    json!({
        "type": "bridge.hello",
        "observe": {
            "type": "observe.snapshot",
            "sessions": {
                "control_plane": control_plane_sessions,
                "native_pty": native_pty_sessions,
            },
            "session_projections": session_projections,
            "note": "slim session projection; drive via POST /bridge/drive (prompt|paste); streaming push/mesh later",
        },
        "node_id": shared.node_id,
        "incarnation_id": shared.incarnation_id,
        "door": "node-envelope-bridge",
    })
}

const SLIM_FAILED_MESSAGE_MAX: usize = 160;
const SLIM_PROCESS_NAME_MAX: usize = 64;
const SLIM_FAILING_REASON_MAX: usize = 96;

fn slim_session_projections(
    shared: &NodeShared,
    sessions: &[SessionSnapshot],
) -> Vec<Value> {
    let bindings = shared
        .session_bindings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        let Some(binding) = bindings.get(&session.instance_id) else {
            continue;
        };
        if binding.generation != session.generation {
            continue;
        }
        let browser_profile_id = binding
            .environment_profile
            .as_ref()
            .and_then(|receipt| receipt.browser_profile_id.as_ref())
            .map(|id| id.as_str().to_owned());
        let network_allowlist = binding
            .environment_profile
            .as_ref()
            .and_then(|receipt| receipt.network_allowlist.as_ref())
            .map(|id| id.as_str().to_owned());
        out.push(json!({
            "workspace_id": binding.workspace_id.as_str(),
            "instance_id": session.instance_id.0,
            "generation": session.generation.0,
            "agent_id": session.agent_id.as_str(),
            "transport": session.transport,
            "status": slim_session_status(&session.status),
            "screen": slim_screen_state(&session.screen_state),
            "pending_input": session.pending_input.map(slim_pending_input),
            "terminal_size": session.terminal_size.map(slim_terminal_size),
            "browser_profile_id": browser_profile_id,
            "network_allowlist": network_allowlist,
            // Explicitly absent: terminal_frame, history, provider internals,
            // cwd/paths, cookies, tokens.
        }));
    }
    out
}

fn slim_session_status(status: &SessionStatus) -> Value {
    match status {
        SessionStatus::Registered => json!({ "kind": "registered" }),
        SessionStatus::Starting => json!({ "kind": "starting" }),
        SessionStatus::Running => json!({ "kind": "running" }),
        SessionStatus::Stopping => json!({ "kind": "stopping" }),
        SessionStatus::Exited { exit_code } => json!({
            "kind": "exited",
            "exit_code": exit_code,
        }),
        SessionStatus::Failed { message } => json!({
            "kind": "failed",
            "message": truncate_chars(message, SLIM_FAILED_MESSAGE_MAX),
        }),
    }
}

fn slim_screen_state(screen: &PtyScreenState) -> Value {
    match screen {
        PtyScreenState::Unknown => json!({ "kind": "unknown" }),
        PtyScreenState::NotAgent { observed_process } => json!({
            "kind": "not-agent",
            "observed_process": truncate_chars(
                &basename_process(observed_process),
                SLIM_PROCESS_NAME_MAX,
            ),
        }),
        PtyScreenState::OperatorGate { gate } => json!({
            "kind": "operator-gate",
            // Kind only — strip options (on-screen labels) and subject detail.
            "gate_kind": gate.kind,
        }),
        PtyScreenState::Failing { reason } => json!({
            "kind": "failing",
            "reason": truncate_chars(reason, SLIM_FAILING_REASON_MAX),
        }),
        PtyScreenState::Ready => json!({ "kind": "ready" }),
    }
}

fn slim_pending_input(kind: PreparedInputKind) -> Value {
    // Serialize the enum via serde so rename_all kebab-case stays honest.
    serde_json::to_value(kind).unwrap_or_else(|_| json!("unknown"))
}

fn slim_terminal_size(size: TerminalSize) -> Value {
    json!({ "rows": size.rows, "columns": size.columns })
}

fn basename_process(raw: &str) -> String {
    let trimmed = raw.trim();
    let name = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .trim();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name.to_owned()
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Tip-4 drive envelope: only prompt/paste through node request types.
/// Never TerminalBytes / raw PTY / cookies / OAuth.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum BridgeDriveRequest {
    Prompt {
        workspace_id: String,
        instance_id: u64,
        generation: u64,
        text: String,
    },
    Paste {
        workspace_id: String,
        instance_id: u64,
        generation: u64,
        text: String,
    },
}

impl BridgeDriveRequest {
    fn kind_label(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Paste { .. } => "paste",
        }
    }

    fn parts(&self) -> (&str, u64, u64, &str) {
        match self {
            Self::Prompt {
                workspace_id,
                instance_id,
                generation,
                text,
            }
            | Self::Paste {
                workspace_id,
                instance_id,
                generation,
                text,
            } => (workspace_id.as_str(), *instance_id, *generation, text.as_str()),
        }
    }
}

async fn handle_drive(
    stream: &mut TcpStream,
    request: Request,
    auth: &BridgeAuth,
    shared: &NodeShared,
) -> io::Result<()> {
    if request.method != "POST" {
        return write_http(
            stream,
            HttpResponse::plain(405, "Method Not Allowed").with_header("Allow", "POST"),
        )
        .await;
    }

    let provided = extract_bridge_token(&request);
    if !auth.authorized(provided.as_deref()) {
        return write_http(
            stream,
            HttpResponse::plain(401, "Unauthorized").with_header("WWW-Authenticate", "Bearer"),
        )
        .await;
    }

    let body = match read_http_body(stream, &request).await {
        Ok(body) => body,
        Err(BodyReadError::TooLarge) => {
            return write_http(stream, HttpResponse::plain(413, "Payload Too Large")).await;
        }
        Err(BodyReadError::Invalid) => {
            return write_http(stream, HttpResponse::plain(400, "Bad Request")).await;
        }
        Err(BodyReadError::Io(error)) => return Err(error),
        Err(BodyReadError::TimedOut) => return Ok(()),
    };

    let drive = match serde_json::from_slice::<BridgeDriveRequest>(&body) {
        Ok(drive) => drive,
        Err(_) => {
            return write_http(
                stream,
                HttpResponse::json(
                    400,
                    json!({
                        "ok": false,
                        "type": "drive.error",
                        "code": "invalid-request",
                        "message": "drive body must be JSON prompt|paste with session address + text",
                    }),
                ),
            )
            .await;
        }
    };

    let response = match execute_bridge_drive(shared, drive).await {
        Ok(kind) => HttpResponse::json(
            200,
            json!({
                "ok": true,
                "type": "drive.accepted",
                "kind": kind,
                "door": "node-envelope-bridge",
            }),
        ),
        Err(error) => HttpResponse::json(
            drive_http_status(error.code),
            json!({
                "ok": false,
                "type": "drive.error",
                "code": error.code,
                "message": error.message,
                "door": "node-envelope-bridge",
            }),
        ),
    };
    write_http(stream, response).await
}

async fn execute_bridge_drive(
    shared: &NodeShared,
    drive: BridgeDriveRequest,
) -> Result<&'static str, NodeFailure> {
    let kind = drive.kind_label();
    let (workspace_id_raw, instance_id, generation, text) = drive.parts();
    if text.is_empty() {
        return Err(super::failure(
            NodeFailureCode::InvalidRequest,
            "drive text must be non-empty",
        ));
    }
    if text.len() > MAX_NODE_TEXT_BYTES {
        return Err(super::failure(
            NodeFailureCode::InvalidRequest,
            "drive text exceeds the node text byte limit",
        ));
    }
    // Mirror NodeRequest::Prompt/Paste validate_node_text without logging the text.
    super::validate_node_text(kind, text)?;

    let workspace_id = WorkspaceId::new(workspace_id_raw).map_err(|_| {
        super::failure(
            NodeFailureCode::InvalidRequest,
            "workspace_id is not a valid node identifier",
        )
    })?;
    let session = SessionAddress {
        workspace_id,
        session: SessionKey {
            instance_id: AgentInstanceId(instance_id),
            generation: SessionGeneration(generation),
        },
    };

    // Bridge door auth is loopback + optional BRIDGE_TOKEN — not C2 controller.
    let agent_id = shared.validate_address(&session)?;
    shared.require_session_runtime_policy(
        &session,
        ProviderRuntimeRequirement::SemanticPrompt,
    )?;

    let action = match kind {
        "prompt" => InputAction::SubmitPrompt(PromptPayload {
            text: text.to_owned(),
            framing: super::prompt_framing(&agent_id),
        }),
        "paste" => InputAction::InsertDraft(PromptPayload {
            text: text.to_owned(),
            framing: PromptFraming::BracketedPaste,
        }),
        _ => {
            return Err(super::failure(
                NodeFailureCode::InvalidRequest,
                "unsupported drive kind",
            ));
        }
    };
    shared.dispatch_input_bounded(&session, action).await?;
    Ok(kind)
}

fn drive_http_status(code: NodeFailureCode) -> u16 {
    match code {
        NodeFailureCode::UnknownSession | NodeFailureCode::UnknownWorkspace => 404,
        NodeFailureCode::StaleGeneration
        | NodeFailureCode::SessionWorkspaceMismatch
        | NodeFailureCode::TurnInFlight => 409,
        NodeFailureCode::UnsupportedCapability => 422,
        NodeFailureCode::BackendBusy => 503,
        NodeFailureCode::BackendOperationFailed => 502,
        NodeFailureCode::Unauthorized | NodeFailureCode::ControllerRequired => 401,
        _ => 400,
    }
}

enum BodyReadError {
    TooLarge,
    Invalid,
    TimedOut,
    Io(io::Error),
}

async fn read_http_body(stream: &mut TcpStream, request: &Request) -> Result<Vec<u8>, BodyReadError> {
    let Some(content_length) = request.content_length else {
        return Err(BodyReadError::Invalid);
    };
    if content_length > MAX_BRIDGE_DRIVE_BODY_BYTES {
        return Err(BodyReadError::TooLarge);
    }
    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&request.body_prefix);
    if body.len() > content_length {
        return Err(BodyReadError::Invalid);
    }
    while body.len() < content_length {
        let mut chunk = vec![0u8; (content_length - body.len()).min(4_096)];
        match timeout(HEADER_READ_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Err(BodyReadError::Invalid),
            Ok(Ok(read)) => body.extend_from_slice(&chunk[..read]),
            Ok(Err(error)) => return Err(BodyReadError::Io(error)),
            Err(_) => return Err(BodyReadError::TimedOut),
        }
        if body.len() > content_length {
            return Err(BodyReadError::Invalid);
        }
    }
    Ok(body)
}

fn extract_bridge_token(request: &Request) -> Option<String> {
    if let Some(header) = request.authorization.as_deref() {
        let bearer = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "));
        if let Some(token) = bearer {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    if let Some(header) = request.bridge_token_header.as_deref() {
        let trimmed = header.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    query_param(&request.target, "bridge_token")
}

fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key && !value.is_empty() {
            // Percent-decoding is out of scope for this slice; operators
            // should prefer Authorization / header with unescaped secrets.
            return Some(value.to_owned());
        }
    }
    None
}

fn tokens_match(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    // Length mismatch is an immediate refuse; equal-length path is a
    // byte-wise XOR fold (no early exit) so compare cost does not track
    // the first differing index. Distinct from NODE_TOKEN / C2 auth.
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn sec_websocket_accept(key: &str) -> String {
    let mut material = Vec::with_capacity(key.len() + WS_GUID.len());
    material.extend_from_slice(key.as_bytes());
    material.extend_from_slice(WS_GUID);
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &material);
    base64_encode(digest.as_ref())
}

fn encode_server_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(2 + payload.len() + 8);
    frame.push(0x81); // FIN + text
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    match input.len() - i {
        1 => {
            let n = (input[i] as u32) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
    bridge_token_header: Option<String>,
    upgrade: Option<String>,
    connection: Option<String>,
    sec_websocket_key: Option<String>,
    sec_websocket_version: Option<String>,
    content_length: Option<usize>,
    /// Bytes already read past the header terminator (may be empty).
    body_prefix: Vec<u8>,
}

enum ReadRequestError {
    Closed,
    Invalid,
    TooLarge,
    Io(io::Error),
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {
    let mut buffer = Vec::with_capacity(512);
    loop {
        if buffer.len() > HEADER_LIMIT_BYTES {
            return Err(ReadRequestError::TooLarge);
        }
        let mut chunk = [0u8; 512];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(ReadRequestError::Io)?;
        if read == 0 {
            return Err(ReadRequestError::Closed);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > HEADER_LIMIT_BYTES {
            return Err(ReadRequestError::TooLarge);
        }
        if let Some(header_end) = find_header_end(&buffer) {
            let header = std::str::from_utf8(&buffer[..header_end])
                .map_err(|_| ReadRequestError::Invalid)?;
            let body_prefix = buffer[header_end..].to_vec();
            return parse_request(header, body_prefix);
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4)
}

fn parse_request(header: &str, body_prefix: Vec<u8>) -> Result<Request, ReadRequestError> {
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or(ReadRequestError::Invalid)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(ReadRequestError::Invalid)?.to_owned();
    let target = parts.next().ok_or(ReadRequestError::Invalid)?.to_owned();
    let _version = parts.next().ok_or(ReadRequestError::Invalid)?;
    if parts.next().is_some() {
        return Err(ReadRequestError::Invalid);
    }
    let mut authorization = None;
    let mut bridge_token_header = None;
    let mut upgrade = None;
    let mut connection = None;
    let mut sec_websocket_key = None;
    let mut sec_websocket_version = None;
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadRequestError::Invalid);
        };
        let name = name.trim();
        let value = value.trim().to_owned();
        if name.eq_ignore_ascii_case("Authorization") {
            authorization = Some(value);
        } else if name.eq_ignore_ascii_case("X-Gate4agent-Bridge-Token") {
            bridge_token_header = Some(value);
        } else if name.eq_ignore_ascii_case("Upgrade") {
            upgrade = Some(value);
        } else if name.eq_ignore_ascii_case("Connection") {
            connection = Some(value);
        } else if name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
            sec_websocket_key = Some(value);
        } else if name.eq_ignore_ascii_case("Sec-WebSocket-Version") {
            sec_websocket_version = Some(value);
        } else if name.eq_ignore_ascii_case("Content-Length") {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| ReadRequestError::Invalid)?;
            if parsed > MAX_BRIDGE_DRIVE_BODY_BYTES {
                return Err(ReadRequestError::TooLarge);
            }
            content_length = Some(parsed);
        }
    }
    Ok(Request {
        method,
        target,
        authorization,
        bridge_token_header,
        upgrade,
        connection,
        sec_websocket_key,
        sec_websocket_version,
        content_length,
        body_prefix,
    })
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn plain(status: u16, body: &'static str) -> Self {
        Self {
            status,
            reason: reason_phrase(status),
            headers: vec![("Content-Type", "text/plain; charset=utf-8".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn json(status: u16, body: Value) -> Self {
        match serde_json::to_vec(&body) {
            Ok(bytes) => Self {
                status,
                reason: reason_phrase(status),
                headers: vec![("Content-Type", "application/json".into())],
                body: bytes,
            },
            Err(_) => Self::plain(503, "Service Unavailable"),
        }
    }

    fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        426 => "Upgrade Required",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

async fn write_http(stream: &mut TcpStream, response: HttpResponse) -> io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.reason,
        response.body.len()
    );
    for (name, value) in &response.headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    timeout(WRITE_TIMEOUT, async {
        stream.write_all(out.as_bytes()).await?;
        stream.write_all(&response.body).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "bridge http write timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NodeId, WorkspaceId};
    use crate::{NodeServer, NodeServerConfig, WorkspaceConfig};
    use std::path::PathBuf;

    fn test_local_endpoint(label: &str) -> String {
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\gate4agent-bridge-{label}")
        }
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!(
                "g4a-bridge-{}-{}",
                label,
                std::process::id()
            ));
            let _ = std::fs::create_dir_all(&dir);
            dir.join("n.sock").to_string_lossy().into_owned()
        }
    }

    fn node_server() -> NodeServer {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = WorkspaceConfig::new(WorkspaceId::new("test").unwrap(), root).unwrap();
        let config = NodeServerConfig::new(
            test_local_endpoint("unit"),
            "test-token",
            NodeId::new("test-node").unwrap(),
            [workspace],
        )
        .unwrap();
        NodeServer::new(config).unwrap()
    }

    async fn raw_request(address: SocketAddr, request: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        // For WS we may leave the connection open briefly; read with a short timeout.
        let _ = timeout(Duration::from_millis(500), stream.read_to_end(&mut response)).await;
        response
    }

    async fn http_text(address: SocketAddr, request: &str) -> String {
        String::from_utf8(raw_request(address, request).await).unwrap()
    }

    #[test]
    fn bridge_listen_is_opt_in_and_refuses_non_loopback() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = WorkspaceConfig::new(WorkspaceId::new("test").unwrap(), root).unwrap();
        let config = NodeServerConfig::new(
            test_local_endpoint("config"),
            "test-token",
            NodeId::new("test-node").unwrap(),
            [workspace],
        )
        .unwrap();
        assert_eq!(config.bridge_listen, None);
        let config = config
            .with_bridge_listen("127.0.0.1:0".parse().unwrap())
            .unwrap();
        assert_eq!(config.bridge_listen, Some("127.0.0.1:0".parse().unwrap()));
        assert!(config
            .clone()
            .with_bridge_listen("0.0.0.0:18410".parse().unwrap())
            .is_err());
        assert!(config
            .clone()
            .with_bridge_listen("[::1]:0".parse().unwrap())
            .is_ok());
    }

    #[test]
    fn sec_websocket_accept_matches_rfc6455_example() {
        // RFC6455 §1.3 / §4.2.2 example key.
        assert_eq!(
            sec_websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn bridge_token_compare_is_constant_time_equality() {
        let auth = BridgeAuth::from_optional(Some("bridge-secret".into())).unwrap();
        assert!(auth.authorized(Some("bridge-secret")));
        assert!(!auth.authorized(Some("wrong-secret!!")));
        assert!(!auth.authorized(None));
        let open = BridgeAuth::from_optional(None).unwrap();
        assert!(open.authorized(None));
        assert!(open.authorized(Some("anything")));
    }

    #[tokio::test]
    async fn bridge_health_returns_200_on_loopback_without_token() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(None).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let health = http_text(
            address,
            "GET /bridge/health HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(
            health.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected health response: {health}"
        );
        assert!(health.contains("\"service\":\"gate4agent-node-bridge\""));
        assert!(health.contains("\"door\":\"node-envelope-bridge\""));
        assert!(health.contains("\"application\":[\"http\",\"websocket\"]"));
        assert!(!health.contains("test-token"));
        assert!(!health.contains("GATE4AGENT"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bridge_ws_upgrade_sends_observe_text_frame() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(None).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let request = format!(
            "GET /bridge/ws HTTP/1.1\r\n\
Host: localhost\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Key: {key}\r\n\
Sec-WebSocket-Version: 13\r\n\
\r\n"
        );
        let bytes = raw_request(address, &request).await;
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("HTTP/1.1 101 Switching Protocols"),
            "missing 101: {text}"
        );
        assert!(text.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));

        // Find start of WS frames (after header blank line).
        let header_end = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("ws response must end headers")
            + 4;
        let frame = &bytes[header_end..];
        assert!(
            !frame.is_empty() && frame[0] & 0x0f == 0x01,
            "expected text opcode, got {:02x?}",
            &frame[..frame.len().min(8)]
        );
        let (payload_start, payload_len) = if frame[1] < 126 {
            (2usize, frame[1] as usize)
        } else if frame[1] == 126 {
            let len = u16::from_be_bytes([frame[2], frame[3]]) as usize;
            (4, len)
        } else {
            panic!("unexpected 64-bit length in test frame");
        };
        let payload = &frame[payload_start..payload_start + payload_len];
        let body = std::str::from_utf8(payload).unwrap();
        assert!(body.contains("\"type\":\"bridge.hello\""));
        assert!(body.contains("\"type\":\"observe.snapshot\""));
        assert!(body.contains("\"session_projections\":[]"));
        assert!(body.contains("slim session projection"));
        assert!(!body.contains("test-token"));
        assert!(!body.contains("terminal_frame"));
        assert!(!body.contains("GATE4AGENT"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bridge_token_gate_rejects_missing_secret_when_configured() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(Some("bridge-secret".into())).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let denied = http_text(
            address,
            "GET /bridge/health HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(denied.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(!denied.contains("bridge-secret"));

        let ok = http_text(
            address,
            "GET /bridge/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer bridge-secret\r\n\r\n",
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(!ok.contains("bridge-secret"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn slim_screen_strips_operator_gate_options_and_basenames_process() {
        use gate4agent_types::{
            OperatorGateInput, OperatorGateKind, OperatorGateOption,
            OperatorGateOptionSemantics, OperatorGateState,
        };

        let mut gate = OperatorGateState::new(OperatorGateKind::Authentication);
        // Force an on-screen option label into the gate so we can assert the
        // slim projector strips it (options stay station/operator-local).
        gate.input = OperatorGateInput::NumberedList;
        gate.options = vec![OperatorGateOption {
            text: "sign in with cookie-jar secret".into(),
            semantics: OperatorGateOptionSemantics::Accept,
            selected: false,
        }];
        let projected = slim_screen_state(&PtyScreenState::OperatorGate { gate });
        let text = projected.to_string();
        assert!(text.contains("\"kind\":\"operator-gate\""));
        assert!(text.contains("authentication"));
        assert!(!text.contains("cookie-jar"));
        assert!(!text.contains("sign in"));

        let not_agent = slim_screen_state(&PtyScreenState::NotAgent {
            observed_process: "/home/fixture/bin/apt-get".into(),
        });
        assert_eq!(
            not_agent,
            json!({
                "kind": "not-agent",
                "observed_process": "apt-get",
            })
        );
    }

    #[test]
    fn slim_status_truncates_failed_message() {
        let long = "x".repeat(SLIM_FAILED_MESSAGE_MAX + 40);
        let projected = slim_session_status(&SessionStatus::Failed {
            message: long.clone(),
        });
        let message = projected["message"].as_str().unwrap();
        assert!(message.chars().count() <= SLIM_FAILED_MESSAGE_MAX);
        assert!(message.ends_with('…'));
        assert!(!projected.to_string().contains(&long));
    }

    #[tokio::test]
    async fn bridge_http_snapshot_exposes_slim_projection_shape() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(None).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let body = http_text(
            address,
            "GET /bridge/snapshot HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(
            body.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected snapshot response: {body}"
        );
        assert!(body.contains("\"type\":\"bridge.hello\""));
        assert!(body.contains("\"session_projections\":[]"));
        assert!(body.contains("slim session projection"));
        assert!(body.contains("POST /bridge/drive"));
        assert!(!body.contains("test-token"));
        assert!(!body.contains("terminal_frame"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    fn drive_post(body: &str) -> String {
        format!(
            "POST /bridge/drive HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    #[test]
    fn bridge_drive_request_parses_prompt_and_paste_only() {
        let prompt: BridgeDriveRequest = serde_json::from_str(
            r#"{"kind":"prompt","workspace_id":"ws","instance_id":1,"generation":2,"text":"hi"}"#,
        )
        .unwrap();
        assert_eq!(prompt.kind_label(), "prompt");
        let paste: BridgeDriveRequest = serde_json::from_str(
            r#"{"kind":"paste","workspace_id":"ws","instance_id":1,"generation":2,"text":"hi"}"#,
        )
        .unwrap();
        assert_eq!(paste.kind_label(), "paste");
        // Refuse raw terminal / unknown kinds — not a product drive surface.
        assert!(serde_json::from_str::<BridgeDriveRequest>(
            r#"{"kind":"terminal-bytes","workspace_id":"ws","instance_id":1,"generation":2,"text":"x"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<BridgeDriveRequest>(
            r#"{"kind":"input","workspace_id":"ws","instance_id":1,"generation":2,"text":"x"}"#
        )
        .is_err());
    }

    #[tokio::test]
    async fn bridge_drive_get_is_method_not_allowed() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(None).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let body = http_text(
            address,
            "GET /bridge/drive HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(
            body.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
            "unexpected: {body}"
        );
        assert!(body.contains("Allow: POST"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bridge_drive_rejects_malformed_and_unknown_session() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(None).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let bad = http_text(address, &drive_post(r#"{"not":"a-drive"}"#)).await;
        assert!(
            bad.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "unexpected malformed: {bad}"
        );
        assert!(bad.contains("\"type\":\"drive.error\""));
        assert!(!bad.contains("test-token"));

        let missing = http_text(
            address,
            &drive_post(
                r#"{"kind":"prompt","workspace_id":"test","instance_id":99,"generation":1,"text":"hello"}"#,
            ),
        )
        .await;
        assert!(
            missing.starts_with("HTTP/1.1 404 Not Found\r\n")
                || missing.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "unexpected unknown-session: {missing}"
        );
        assert!(missing.contains("\"type\":\"drive.error\""));
        assert!(
            missing.contains("unknown-session") || missing.contains("unknown-workspace"),
            "expected session/workspace failure code: {missing}"
        );
        assert!(!missing.contains("GATE4AGENT"));

        let empty = http_text(
            address,
            &drive_post(
                r#"{"kind":"paste","workspace_id":"test","instance_id":1,"generation":1,"text":""}"#,
            ),
        )
        .await;
        assert!(
            empty.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "unexpected empty text: {empty}"
        );

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bridge_drive_token_gate_rejects_missing_secret() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = BridgeAuth::from_optional(Some("bridge-secret".into())).unwrap();
        let task = tokio::spawn(serve_listener(listener, auth, Arc::clone(&shared)));

        let denied = http_text(
            address,
            &drive_post(
                r#"{"kind":"prompt","workspace_id":"test","instance_id":1,"generation":1,"text":"x"}"#,
            ),
        )
        .await;
        assert!(
            denied.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "unexpected: {denied}"
        );
        assert!(!denied.contains("bridge-secret"));

        let drive_body =
            r#"{"kind":"prompt","workspace_id":"test","instance_id":1,"generation":1,"text":"x"}"#;
        let authorized = format!(
            "POST /bridge/drive HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer bridge-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            drive_body.len(),
            drive_body
        );
        let okish = http_text(address, &authorized).await;
        // Auth passed; session still missing → drive.error, never leaks token.
        assert!(!okish.starts_with("HTTP/1.1 401 "), "auth should pass: {okish}");
        assert!(okish.contains("\"type\":\"drive.error\""));
        assert!(!okish.contains("bridge-secret"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn drive_http_status_maps_node_failure_codes() {
        assert_eq!(drive_http_status(NodeFailureCode::UnknownSession), 404);
        assert_eq!(drive_http_status(NodeFailureCode::StaleGeneration), 409);
        assert_eq!(drive_http_status(NodeFailureCode::UnsupportedCapability), 422);
        assert_eq!(drive_http_status(NodeFailureCode::BackendBusy), 503);
        assert_eq!(drive_http_status(NodeFailureCode::InvalidRequest), 400);
    }
}
