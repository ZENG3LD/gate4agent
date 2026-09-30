use super::{clear_snapshot_context_packs, project_snapshot_history_for_wire, NodeShared};
use crate::protocol::{BUILD_STAMP, MAX_NODE_FRAME_BYTES};
use gate4agent_runtime_native::tick_profile::Distribution;
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
        "/metrics" => Response::json(200, metrics_body(shared)),
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
        "build_stamp": BUILD_STAMP,
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

/// One windowed distribution as `{p50, p95, max, count}` -- `unit_suffix`
/// picks between microsecond-labelled fields (the phase timings),
/// byte-labelled fields (`terminal_frame_bytes`), and bare fields (the
/// iteration-rate series, whose samples are a per-second count, not a
/// duration).
fn distribution_body(distribution: Distribution, unit_suffix: &str) -> Value {
    json!({
        format!("p50{unit_suffix}"): distribution.p50,
        format!("p95{unit_suffix}"): distribution.p95,
        format!("max{unit_suffix}"): distribution.max,
        "count": distribution.count,
    })
}

/// Read-only tick-cadence diagnostics, unauthenticated under the same rule
/// as `/health`: it reports timings and counts, never session content
/// (prompts, output, environment, credentials) -- see this module's own
/// `authorized` gate for the boundary that keeps `/status` (which DOES
/// carry session content) behind a bearer token while this stays open.
///
/// Answers "where does an idle drive-loop iteration's own CPU go," split
/// into the loop's own three phases plus whatever remains before the
/// trailing sleep, `NativeRuntime::tick`'s own six internal phases, the
/// loop's actual (not merely configured) iteration rate, the two session
/// counts that can legitimately disagree -- see `native_pty_sessions`'s own
/// field comment for why -- and what each live session actually costs per
/// tick: real screen-capture and foreground-probe timings, and published
/// frame size, all in `shell_efficiency` right next to `sessions` so
/// dividing one by `sessions.native_pty` is the obvious next step.
fn metrics_body(shared: &NodeShared) -> Value {
    let drive_loop = shared
        .drive_loop_profile
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot();
    let runtime_tick = *shared
        .runtime_tick_profile
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Snapshotting sorts every ring behind these series, which is exactly
    // why it happens on the request and not in the drive loop -- see
    // `NodeShared::shell_efficiency_profile`. Absent until the runtime
    // starts driving; the default reports empty series (`count: 0`) rather
    // than zeroes that would read as measurements.
    let shell_efficiency = shared
        .shell_efficiency_profile
        .get()
        .map(|profile| {
            profile
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot()
        })
        .unwrap_or_default();
    let native_pty_sessions = shared.native_session_gauge.load(Ordering::Relaxed);
    // The control plane's own session bookkeeping: every session the
    // kernel's backend snapshot still carries, live or already exited but
    // not yet reaped. A count here with no matching `native_pty_sessions`
    // is exactly the gap worth reading `step_control_plane_us` against --
    // `step_control_plane` rebuilds this same snapshot, unconditionally,
    // every tick.
    let control_plane_sessions = shared.handle.snapshot().sessions.len();
    json!({
        "service": SERVICE_NAME,
        "iterations_per_sec": distribution_body(drive_loop.iterations_per_sec, ""),
        "drive_loop_phases_us": {
            "runtime_tick": distribution_body(drive_loop.runtime_tick_us, "_us"),
            "event_drain": distribution_body(drive_loop.event_drain_us, "_us"),
            "publish_terminal_frames":
                distribution_body(drive_loop.publish_terminal_frames_us, "_us"),
            "remainder": distribution_body(drive_loop.remainder_us, "_us"),
        },
        "runtime_tick_phases_us": {
            "drain_observations":
                distribution_body(runtime_tick.drain_observations_us, "_us"),
            "drain_ingress": distribution_body(runtime_tick.drain_ingress_us, "_us"),
            "step_control_plane":
                distribution_body(runtime_tick.step_control_plane_us, "_us"),
            "dispatch_effects": distribution_body(runtime_tick.dispatch_effects_us, "_us"),
            "publish_step": distribution_body(runtime_tick.publish_step_us, "_us"),
            "provider_supervisors":
                distribution_body(runtime_tick.provider_supervisors_us, "_us"),
        },
        "control_commands": {
            "rejected_total": shared.rejected_commands_total.load(Ordering::Relaxed),
        },
        "sessions": {
            "native_pty": native_pty_sessions,
            "control_plane": control_plane_sessions,
        },
        // What a live session actually costs, folded in from every
        // per-instance worker loop's `ShellEfficiencyFacts` -- see
        // `gate4agent_runtime_native::shell_efficiency`'s own doc comment.
        // Every `_total` field below is a LIFETIME total, same rule as
        // `connections`: read twice and divide by the interval for a rate,
        // or divide by `sessions.native_pty` above for a per-session cost.
        "shell_efficiency": {
            // Real `terminal_state()` captures -- the sequence gate already
            // filtered out the cheap no-op case, so this is exactly the
            // "changed-screen capture" cost, and `skips_total` against
            // `captures_total` is the skip/capture ratio that decides
            // whether that gate's remaining cost is worth attacking.
            "terminal_state_us": distribution_body(shell_efficiency.terminal_state_us, "_us"),
            "terminal_state_captures_total": shell_efficiency.terminal_state_captures_total,
            "terminal_state_skips_total": shell_efficiency.terminal_state_skips_total,
            // Published `TerminalFrame` wire size -- `formatted` plus every
            // `scrollback_formatted` row.
            "terminal_frame_bytes":
                distribution_body(shell_efficiency.terminal_frame_bytes, "_bytes"),
            "terminal_frames_published_total": shell_efficiency.terminal_frames_published_total,
            "terminal_frame_bytes_total": shell_efficiency.terminal_frame_bytes_total,
            // `reclassify_foreground`'s OS process-tree probes, broken into
            // the three places one probe's wall-clock time actually goes --
            // see `gate4agent::pty::ForegroundProbeTiming` for what each
            // means. `queued` and `lock_wait` are both waiting, not work;
            // `walk` alone is CPU this probe spent. On a node whose sessions
            // are all `PtyScreenState::Ready`, `probes_total` must stop
            // rising; that is the disarm claim on that method's own doc
            // comment.
            "foreground_probe_phases_us": {
                "queued": distribution_body(shell_efficiency.foreground_probe_queued_us, "_us"),
                "lock_wait":
                    distribution_body(shell_efficiency.foreground_probe_lock_wait_us, "_us"),
                "walk": distribution_body(shell_efficiency.foreground_probe_walk_us, "_us"),
            },
            "foreground_probes_total": shell_efficiency.foreground_probes_total,
        },
        // Lifetime totals across every connection served. Read them twice
        // and divide by the interval for a rate. `iterations` far
        // outrunning `events_sent` means the serve loop is spinning rather
        // than sleeping -- which is where an idle node's CPU goes, since
        // it burns it only while a c2 is attached.
        "connections": {
            "loop_iterations": shared.connection_loop_iterations.load(Ordering::Relaxed),
            "events_sent": shared.connection_events_sent.load(Ordering::Relaxed),
        },
        // Lifetime totals for `drive_runtime_until_shutdown`'s own
        // iterations. Read twice and divide by the interval, same rule as
        // `connections` above. `idle_total / total` is how large a
        // fraction of the drive loop's ~10ms cadence does nothing
        // observable -- see the exact "produced nothing" definition at the
        // site in `drive_runtime_until_shutdown` that increments these.
        "drive_loop_iterations": {
            "total": shared.drive_loop_iterations_total.load(Ordering::Relaxed),
            "idle_total": shared.drive_loop_iterations_idle.load(Ordering::Relaxed),
        },
    })
}

fn status_body(shared: &NodeShared) -> Value {
    let mut snapshot = shared.snapshot();
    project_snapshot_history_for_wire(&mut snapshot, false);
    clear_snapshot_context_packs(&mut snapshot);
    json!({
        "snapshot": snapshot,
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
    use crate::protocol::{
        AgentId, NodeId, ProviderRuntimeContractId, ProviderRuntimeStatus,
        ProviderRuntimeStatuses, ProviderRuntimeVersion, WorkspaceId,
    };
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

    fn node_server_with_verified_runtime_status() -> NodeServer {
        let mut server = node_server();
        Arc::get_mut(&mut server.shared)
            .expect("the test server has one shared-state owner")
            .provider_runtime_statuses = ProviderRuntimeStatuses::new([
                ProviderRuntimeStatus::verified_semantic(
                    AgentId::new("codex").unwrap(),
                    ProviderRuntimeVersion::new("9.8.7").unwrap(),
                    ProviderRuntimeContractId::new("codex.test.9.8.7").unwrap(),
                ),
            ])
            .unwrap();
        server
    }

    #[test]
    fn authorized_status_exposes_only_normalized_version_and_contract_id() {
        let server = node_server_with_verified_runtime_status();
        let encoded = serde_json::to_string(&status_body(&server.shared)).unwrap();
        assert!(encoded.contains("\"version\":\"9.8.7\""));
        assert!(encoded.contains("\"contract_id\":\"codex.test.9.8.7\""));
        for forbidden in [
            "launcher", "stdout", "stderr", "fallback", "reason", "arguments", "environment",
        ] {
            assert!(!encoded.contains(forbidden), "leaked field {forbidden}");
        }
    }

    #[test]
    fn public_health_does_not_expose_provider_runtime_details() {
        let server = node_server_with_verified_runtime_status();
        for body in [health_body(&server.shared), ready_body(&server.shared)] {
            let encoded = serde_json::to_string(&body).unwrap();
            assert!(!encoded.contains("provider_runtime"));
            assert!(!encoded.contains("9.8.7"));
            assert!(!encoded.contains("codex.test.9.8.7"));
        }
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
        assert!(health.contains(&format!("\"build_stamp\":\"{BUILD_STAMP}\"")));
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
    async fn metrics_is_unauthenticated_and_reports_every_phase_and_session_count() {
        let server = node_server();
        let shared = Arc::clone(&server.shared);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_listener(listener, Arc::clone(&shared)));

        // No Authorization header at all -- same rule as `/health`: timings
        // and counts, never session content, so no credential is required.
        let metrics = request(address, "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(metrics.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(metrics.contains("\"service\":\"gate4agent-node\""));
        assert!(metrics.contains("\"iterations_per_sec\""));
        for phase in [
            "runtime_tick", "event_drain", "publish_terminal_frames", "remainder",
        ] {
            assert!(
                metrics.contains(&format!("\"{phase}\"")),
                "missing drive_loop_phases_us.{phase} in {metrics}",
            );
        }
        for phase in [
            "drain_observations", "drain_ingress", "step_control_plane",
            "dispatch_effects", "publish_step", "provider_supervisors",
        ] {
            assert!(
                metrics.contains(&format!("\"{phase}\"")),
                "missing runtime_tick_phases_us.{phase} in {metrics}",
            );
        }
        assert!(metrics.contains("\"native_pty\":0"));
        assert!(metrics.contains("\"control_plane\":0"));
        for field in [
            "terminal_state_us", "terminal_state_captures_total", "terminal_state_skips_total",
            "terminal_frame_bytes", "terminal_frames_published_total",
            "terminal_frame_bytes_total", "foreground_probes_total",
        ] {
            assert!(
                metrics.contains(&format!("\"{field}\"")),
                "missing shell_efficiency.{field} in {metrics}",
            );
        }
        for phase in ["queued", "lock_wait", "walk"] {
            assert!(
                metrics.contains(&format!("\"{phase}\"")),
                "missing shell_efficiency.foreground_probe_phases_us.{phase} in {metrics}",
            );
        }
        assert!(metrics.contains("\"drive_loop_iterations\""));
        assert!(metrics.contains("\"idle_total\""));
        assert!(!metrics.contains("test-token"));

        let posted = request(address, "POST /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(posted.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));

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
