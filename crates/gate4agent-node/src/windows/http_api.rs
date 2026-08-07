use super::NodeShared;
use crate::protocol::{MAX_NODE_FRAME_BYTES, NODE_PROTOCOL_VERSION};
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

const SERVICE_NAME: &str = "gate4agent-node";
const HEADER_LIMIT_BYTES: usize = 16 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HTTP_CONNECTIONS: usize = 16;
const RESPONSE_BODY_LIMIT_BYTES: usize = MAX_NODE_FRAME_BYTES;
const PUBLIC_PERSISTENCE_ERROR: &str = "durable-state-unavailable";

pub(super) async fn run(
    listen: Option<SocketAddr>,
    shared: Arc<NodeShared>,
) -> io::Result<()> {
    let Some(listen) = listen else {
        wait_for_shutdown(&shared).await;
        return Ok(());
    };
    let listener = TcpListener::bind(listen).await?;
    serve_listener(listener, shared).await
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

async fn serve_listener(listener: TcpListener, shared: Arc<NodeShared>) -> io::Result<()> {
    let permits = Arc::new(Semaphore::new(MAX_HTTP_CONNECTIONS));
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
                let (stream, _) = accepted?;
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let connection_shared = Arc::clone(&shared);
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = serve_connection(stream, connection_shared).await;
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

async fn serve_connection(mut stream: TcpStream, shared: Arc<NodeShared>) -> io::Result<()> {
    let request = match timeout(HEADER_READ_TIMEOUT, read_request(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(ReadRequestError::TooLarge)) => {
            return write_response(&mut stream, Response::plain(413, "Payload Too Large")).await;
        }
        Ok(Err(ReadRequestError::Closed | ReadRequestError::Invalid)) | Err(_) => return Ok(()),
        Ok(Err(ReadRequestError::Io(error))) => return Err(error),
    };
    let response = route(request, &shared);
    write_response(&mut stream, response).await
}

fn route(request: Request, shared: &NodeShared) -> Response {
    if request.method != "GET" {
        return Response::plain(405, "Method Not Allowed").with_header("Allow", "GET");
    }
    let path = request.path.split_once('?').map_or(request.path.as_str(), |(path, _)| path);
    match path {
        "/health" => Response::json(200, health_body(shared)),
        "/ready" => Response::json(200, ready_body(shared)),
        "/status" => {
            if !authorized(request.authorization.as_deref(), &shared.access_token) {
                return Response::plain(401, "Unauthorized")
                    .with_header("WWW-Authenticate", "Bearer");
            }
            Response::json(200, status_body(shared))
        }
        _ => Response::plain(404, "Not Found"),
    }
}

fn health_body(shared: &NodeShared) -> Value {
    json!({
        "ok": true,
        "service": SERVICE_NAME,
        "node_id": shared.node_id,
        "incarnation_id": shared.incarnation_id,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": NODE_PROTOCOL_VERSION,
        "started_at_unix_ms": shared.started_at_unix_ms,
    })
}

fn ready_body(shared: &NodeShared) -> Value {
    let snapshot = shared.snapshot();
    let workspace_count = snapshot.workspaces.len();
    let session_count = snapshot
        .workspaces
        .iter()
        .map(|workspace| workspace.sessions.len())
        .sum::<usize>();
    let shutting_down = shared.shutdown.load(Ordering::Acquire);
    let persistence_error = shared.persistence_error();
    json!({
        "ready": !shutting_down && persistence_error.is_none(),
        "node_id": shared.node_id,
        "counts": {
            "providers": snapshot.enabled_providers.len(),
            "workspaces": workspace_count,
            "sessions": session_count,
            "session_records": snapshot.session_records.len(),
        },
        "capabilities": {
            "named_pipe_control": true,
            "http_observer": true,
            "http_mutations": false,
        },
        "shutdown": shutting_down,
        "persistence_error": persistence_error.map(|_| PUBLIC_PERSISTENCE_ERROR),
    })
}

fn status_body(shared: &NodeShared) -> Value {
    json!({
        "snapshot": shared.snapshot(),
        "incarnation_id": shared.incarnation_id,
        "event_sequence": shared.current_sequence(),
        "controller_active": shared.controller_state().is_some(),
        "shutdown": shared.shutdown.load(Ordering::Acquire),
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "persistence_error": shared.persistence_error(),
    })
}

fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    let Some((scheme, candidate)) = header.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && constant_time_eq(candidate.as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
}

enum ReadRequestError {
    Closed,
    Invalid,
    TooLarge,
    Io(io::Error),
}

async fn read_request(stream: &mut TcpStream) -> Result<Request, ReadRequestError> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).await.map_err(ReadRequestError::Io)?;
        if count == 0 {
            return Err(ReadRequestError::Closed);
        }
        if bytes.len().saturating_add(count) > HEADER_LIMIT_BYTES {
            return Err(ReadRequestError::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ReadRequestError::Invalid)?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines
        .next()
        .ok_or(ReadRequestError::Invalid)?
        .split_whitespace();
    let method = request_line.next().ok_or(ReadRequestError::Invalid)?;
    let path = request_line.next().ok_or(ReadRequestError::Invalid)?;
    let version = request_line.next().ok_or(ReadRequestError::Invalid)?;
    if request_line.next().is_some()
        || !version.starts_with("HTTP/1.")
        || !path.starts_with('/')
    {
        return Err(ReadRequestError::Invalid);
    }
    let mut authorization = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ReadRequestError::Invalid);
        };
        if name.eq_ignore_ascii_case("authorization") {
            if authorization.is_some() {
                return Err(ReadRequestError::Invalid);
            }
            authorization = Some(value.trim().to_owned());
        }
    }
    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        authorization,
    })
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(&'static str, &'static str)>,
}

impl Response {
    fn plain(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: reason.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    fn json(status: u16, body: Value) -> Self {
        let body = serde_json::to_vec(&body).expect("node observer JSON must serialize");
        if body.len() > RESPONSE_BODY_LIMIT_BYTES {
            return Self::plain(503, "Service Unavailable");
        }
        Self {
            status,
            reason: "OK",
            content_type: "application/json",
            body,
            headers: Vec::new(),
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

async fn write_response(stream: &mut TcpStream, response: Response) -> io::Result<()> {
    let mut headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len(),
    );
    for (name, value) in response.headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    timeout(WRITE_TIMEOUT, async {
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&response.body).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "HTTP response write timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NodeId, WorkspaceId};
    use crate::{NodeServer, NodeServerConfig, WorkspaceConfig};
    use std::path::PathBuf;

    fn node_server() -> NodeServer {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = WorkspaceConfig::new(WorkspaceId::new("test").unwrap(), root).unwrap();
        let config = NodeServerConfig::new(
            r"\\.\pipe\gate4agent-http-api-unit",
            "test-token",
            NodeId::new("test-node").unwrap(),
            [workspace],
        )
        .unwrap();
        NodeServer::new(config).unwrap()
    }

    #[test]
    fn node_api_listen_is_opt_in_for_libraries_and_loopback_port_zero_is_valid() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = WorkspaceConfig::new(WorkspaceId::new("test").unwrap(), root).unwrap();
        let config = NodeServerConfig::new(
            r"\\.\pipe\gate4agent-http-api-config",
            "test-token",
            NodeId::new("test-node").unwrap(),
            [workspace],
        )
        .unwrap();
        assert_eq!(config.api_listen, None);
        let config = config.with_api_listen("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_eq!(config.api_listen, Some("127.0.0.1:0".parse().unwrap()));
        assert!(config
            .with_api_listen("0.0.0.0:18310".parse().unwrap())
            .is_err());
    }

    #[test]
    fn oversized_status_json_fails_closed_with_a_small_response() {
        let response = Response::json(200, json!({
            "oversized": "x".repeat(RESPONSE_BODY_LIMIT_BYTES),
        }));
        assert_eq!(response.status, 503);
        assert_eq!(response.body, b"Service Unavailable");
    }

    async fn request(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn health_ready_and_authenticated_status_are_bounded_and_read_only() {
        let server = node_server();
        let incarnation_id = server.shared.incarnation_id.to_string();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_listener(listener, Arc::clone(&shared)));

        let health = request(address, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(health.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(health.contains("\"service\":\"gate4agent-node\""));
        assert!(health.contains("\"protocol_version\":"));
        assert!(health.contains(&format!("\"incarnation_id\":\"{incarnation_id}\"")));
        assert!(!health.contains("test-token"));

        let ready = request(address, "GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(ready.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(ready.contains("\"ready\":true"));
        assert!(ready.contains("\"workspaces\":1"));

        shared.set_persistence_error(Some(
            r"provider secret at C:\private\state-v1.json".to_owned(),
        ));
        let degraded = request(address, "GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(degraded.contains("\"ready\":false"));
        assert!(degraded.contains("\"persistence_error\":\"durable-state-unavailable\""));
        assert!(!degraded.contains("provider secret"));
        assert!(!degraded.contains(r"C:\private"));

        let unauthorized = request(address, "GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(!unauthorized.contains("test-token"));

        let authorized = request(
            address,
            "GET /status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer test-token\r\n\r\n",
        )
        .await;
        assert!(authorized.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(authorized.contains("\"event_sequence\":0"));
        assert!(authorized.contains("\"controller_active\":false"));
        assert!(authorized.contains("\"node_id\":\"test-node\""));
        assert!(authorized.contains(&format!("\"incarnation_id\":\"{incarnation_id}\"")));
        assert!(authorized.contains("\"persistence_error\":\"durable-state-commit-failed\""));
        assert!(!authorized.contains("provider secret"));
        assert!(!authorized.contains(r"C:\private"));
        assert!(!authorized.contains("test-token"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn methods_paths_and_oversized_headers_are_rejected_without_mutation() {
        let server = node_server();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_listener(listener, Arc::clone(&server.shared)));

        let method = request(address, "POST /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(method.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        let missing = request(address, "GET /missing HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(missing.starts_with("HTTP/1.1 404 Not Found\r\n"));
        let oversized = format!(
            "GET /health HTTP/1.1\r\nX-Fill: {}\r\n\r\n",
            "x".repeat(HEADER_LIMIT_BYTES),
        );
        let too_large = request(address, &oversized).await;
        assert!(too_large.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));

        server.shutdown_handle().request_shutdown().await.unwrap();
        timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn listener_bind_failure_is_returned_to_the_node_lifecycle() {
        let server = node_server();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let error = run(Some(address), Arc::clone(&server.shared))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }
}
