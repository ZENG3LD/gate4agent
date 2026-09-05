//! Reconnect-with-backoff supervisor for a `gate4agent-c2` control
//! connection.
//!
//! `connect_local` (`runtime.rs`) connects exactly once: if the physical
//! connection dies, every caller holding its `C2ControlHandle`/
//! `C2EventReceiver` is permanently stuck with `C2ControlError::Closed`
//! forever after, with no signal telling them to reconnect. That is fine
//! for a short-lived caller (a CLI command, an E2E test) but wrong for a
//! long-lived daemon that must keep serving requests across a relay
//! restart.
//!
//! This module adds a second, additive entry point,
//! [`connect_local_reconnecting`], that performs the identical first
//! connection `connect_local` does (so a dead endpoint at boot still fails
//! fast, unchanged), then hands the connection to a background supervisor
//! task that keeps it alive: on loss it republishes a `Reconnecting` link
//! state, fails in-flight and new requests fast (`C2ControlError::Closed`,
//! immediately, not after a timeout), and reconnects with an escalating
//! backoff (parking a hard, unfixable-by-retrying failure such as a bad
//! token at a long fixed interval instead of hammering the relay).
//!
//! The returned [`C2ReconnectingHandle`]/[`C2ReconnectingEventReceiver`]/
//! topology `watch::Receiver` are bound to the supervisor, not to any one
//! physical connection: a reconnect is invisible to a caller already
//! holding them -- they never close, and events/topology keep flowing from
//! whichever physical connection is live underneath.

use crate::runtime::{
    connect_local, C2ControlError, C2ControlHandle, C2EventReceiver, C2PendingRequest,
    EVENT_CAPACITY,
};
use gate4agent_c2_protocol::{C2Topology, NodeRequest, NodeRoute, RoutedNodeEvent, RoutedNodeResponse};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Backoff durations for a transient reconnect failure, indexed by
/// `failures - 1` (`failures` starts at 1 on the first failed attempt).
/// Same VALUES as `gate4agent-c2/src/runtime.rs`'s
/// `C2Timings::default().transient_backoffs` -- `gate4agent-c2-client`
/// cannot depend on that type (the dependency direction is the other way),
/// so this module defines its own constant with the same numbers by
/// deliberate choice, not the same type.
const RECONNECT_TRANSIENT_BACKOFFS: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// Backoff for a failure retrying with the same token/endpoint cannot fix
/// (a bad credential, a protocol mismatch), and for any transient failure
/// past `RECONNECT_TRANSIENT_BACKOFFS`'s last step.
const RECONNECT_PARKED_BACKOFF: Duration = Duration::from_secs(30);

/// Whether a reconnect attempt failed in a way that retrying with the same
/// token/endpoint cannot fix.
fn is_hard_reconnect_failure(error: &C2ControlError) -> bool {
    matches!(
        error,
        C2ControlError::InvalidEndpoint
            | C2ControlError::InvalidToken
            | C2ControlError::Authentication(_)
            | C2ControlError::Protocol(_)
    )
}

/// Pure backoff computation, isolated from `tokio::time::sleep` so it is
/// independently testable without a runtime.
fn reconnect_backoff(failures: usize, hard: bool) -> Duration {
    if !hard {
        if let Some(delay) = failures
            .checked_sub(1)
            .and_then(|index| RECONNECT_TRANSIENT_BACKOFFS.get(index))
        {
            return *delay;
        }
    }
    RECONNECT_PARKED_BACKOFF
}

/// Whether the harness's own dedicated C2 control connection is live right
/// now or being re-established after a loss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum C2LinkState {
    Connected,
    Reconnecting,
}

/// The C2 control token, retained for the reconnect supervisor's entire
/// lifetime. Never `Clone`, never formatted with `{}` -- mirrors
/// `HarnessOperatorCredential` (`gate4agent-harness-api/src/lib.rs`) and
/// `C2Client`'s own redacted `Debug` (`gate4agent-c2-client/src/lib.rs`).
struct ReconnectToken(String);

impl fmt::Debug for ReconnectToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReconnectToken([REDACTED])")
    }
}

/// Signals the reconnect supervisor task to stop pumping and reconnecting.
/// Dropped once the last clone of the owning [`C2ReconnectingHandle`] is
/// dropped.
struct SupervisorShutdownGuard(watch::Sender<bool>);

impl Drop for SupervisorShutdownGuard {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Swappable, never-closing counterpart to `C2ControlHandle`. Every
/// accessor clones or reads out of a `watch` cell a background supervisor
/// mutates on reconnect; no caller ever holds a handle bound to one
/// specific physical connection, so a relay restart never permanently
/// breaks it.
#[derive(Clone)]
pub struct C2ReconnectingHandle {
    live: watch::Receiver<Option<C2ControlHandle>>,
    topology: watch::Receiver<Arc<C2Topology>>,
    link_state: watch::Receiver<C2LinkState>,
    _supervisor: Arc<SupervisorShutdownGuard>,
}

impl C2ReconnectingHandle {
    /// Whether the underlying physical connection is live right now.
    pub fn link_state(&self) -> C2LinkState {
        *self.link_state.borrow()
    }

    /// A fresh receiver over the same link-state cell, for a caller that
    /// wants to await transitions rather than poll.
    pub fn link_state_receiver(&self) -> watch::Receiver<C2LinkState> {
        self.link_state.clone()
    }

    pub fn current_topology(&self) -> Arc<C2Topology> {
        Arc::clone(&self.topology.borrow())
    }

    pub fn subscribe_topology(&self) -> watch::Receiver<Arc<C2Topology>> {
        self.topology.clone()
    }

    /// Validates and synchronously enqueues one typed request against
    /// whichever physical connection is live right now. Returns
    /// `Err(C2ControlError::Closed)` immediately -- never blocks or
    /// waits for a reconnect -- while the link is down.
    pub fn start_request(
        &self,
        route: NodeRoute,
        request: NodeRequest,
    ) -> Result<C2PendingRequest, C2ControlError> {
        match &*self.live.borrow() {
            Some(handle) => handle.start_request(route, request),
            None => Err(C2ControlError::Closed),
        }
    }

    pub async fn request(
        &self,
        route: NodeRoute,
        request: NodeRequest,
    ) -> Result<RoutedNodeResponse, C2ControlError> {
        let handle = self.live.borrow().clone();
        match handle {
            Some(handle) => handle.request(route, request).await,
            None => Err(C2ControlError::Closed),
        }
    }
}

/// The sole event stream for a reconnecting connection. Backed by a bridge
/// channel created once, before the first physical connection, that only
/// closes when the supervisor itself terminates -- a reconnect is
/// invisible to it.
pub struct C2ReconnectingEventReceiver {
    inner: mpsc::Receiver<RoutedNodeEvent>,
}

impl C2ReconnectingEventReceiver {
    pub async fn recv(&mut self) -> Option<RoutedNodeEvent> {
        self.inner.recv().await
    }
}

/// The long-lived channels the supervisor publishes into. Held as one
/// struct (rather than five function parameters) purely for readability.
struct SupervisorChannels {
    live: watch::Sender<Option<C2ControlHandle>>,
    topology: watch::Sender<Arc<C2Topology>>,
    link_state: watch::Sender<C2LinkState>,
    events: mpsc::Sender<RoutedNodeEvent>,
    shutdown: watch::Receiver<bool>,
}

/// Outcome of pumping one physical connection to completion.
enum ConnectionEnded {
    /// The connection's event stream or topology watch closed -- the link
    /// died and must be re-established.
    Lost,
    /// The supervisor was told to stop.
    ShuttingDown,
    /// The caller-held event receiver was dropped; nothing is left to
    /// serve, so the whole supervisor should stop.
    ConsumerGone,
}

/// Forwards events and topology changes from one physical connection into
/// the long-lived bridge channels until that connection dies, the
/// supervisor is told to shut down, or the caller's event receiver is
/// gone. Every `.await` point races the shutdown signal.
async fn pump_one_connection(
    events: &mut C2EventReceiver,
    topology: &mut watch::Receiver<Arc<C2Topology>>,
    events_tx: &mpsc::Sender<RoutedNodeEvent>,
    topology_tx: &watch::Sender<Arc<C2Topology>>,
    shutdown: &mut watch::Receiver<bool>,
) -> ConnectionEnded {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return ConnectionEnded::ShuttingDown,
            event = events.recv() => {
                let Some(event) = event else { return ConnectionEnded::Lost; };
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => return ConnectionEnded::ShuttingDown,
                    sent = events_tx.send(event) => {
                        if sent.is_err() { return ConnectionEnded::ConsumerGone; }
                    }
                }
            }
            changed = topology.changed() => {
                if changed.is_err() { return ConnectionEnded::Lost; }
                let next = topology.borrow().clone();
                topology_tx.send_replace(next);
            }
        }
    }
}

/// Reconnects with an escalating backoff until a connection succeeds or
/// shutdown is signaled. Returns `None` only on shutdown.
async fn reconnect_with_backoff(
    endpoint: &str,
    token: &ReconnectToken,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<(C2ControlHandle, C2EventReceiver)> {
    let mut failures: usize = 0;
    loop {
        match connect_local(endpoint, &token.0).await {
            Ok(connection) => return Some(connection),
            Err(error) => {
                failures = failures.saturating_add(1);
                let hard = is_hard_reconnect_failure(&error);
                tracing::debug!(
                    endpoint,
                    failures,
                    hard,
                    error = %error,
                    "C2 reconnect attempt failed",
                );
                let delay = reconnect_backoff(failures, hard);
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => return None,
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// One iteration = one physical connection's lifetime: publish it as live,
/// pump it until it dies (or shutdown), publish it as gone, then reconnect
/// with backoff and repeat.
async fn reconnect_supervisor(
    endpoint: String,
    token: ReconnectToken,
    mut channels: SupervisorChannels,
    first_control: C2ControlHandle,
    first_events: C2EventReceiver,
) {
    let mut pending = Some((first_control, first_events));
    let mut was_reconnecting = false;
    loop {
        let (control, mut events) = match pending.take() {
            Some(connection) => connection,
            None => match reconnect_with_backoff(&endpoint, &token, &mut channels.shutdown).await {
                Some(connection) => connection,
                None => return,
            },
        };
        if was_reconnecting {
            tracing::info!(endpoint = %endpoint, "C2 relay reconnected");
        }
        was_reconnecting = false;

        let mut topology = control.subscribe_topology();
        let current_topology = control.current_topology();
        channels.live.send_replace(Some(control));
        channels.topology.send_replace(current_topology);
        channels.link_state.send_replace(C2LinkState::Connected);

        let outcome = pump_one_connection(
            &mut events,
            &mut topology,
            &channels.events,
            &channels.topology,
            &mut channels.shutdown,
        )
        .await;

        match outcome {
            ConnectionEnded::ShuttingDown | ConnectionEnded::ConsumerGone => return,
            ConnectionEnded::Lost => {
                channels.live.send_replace(None);
                channels.link_state.send_replace(C2LinkState::Reconnecting);
                if !was_reconnecting {
                    tracing::warn!(endpoint = %endpoint, "C2 relay connection lost, reconnecting");
                }
                was_reconnecting = true;
            }
        }
    }
}

/// Connects to a local `gate4agent-c2` control endpoint the same way
/// `connect_local` does -- a dead endpoint at boot still fails fast with
/// the identical error, unchanged -- then hands the connection to a
/// background supervisor that keeps it alive for as long as the returned
/// handle (or its event receiver) is held.
pub async fn connect_local_reconnecting(
    endpoint: impl Into<String>,
    token: impl Into<String>,
) -> Result<(C2ReconnectingHandle, C2ReconnectingEventReceiver), C2ControlError> {
    let endpoint = endpoint.into();
    let token = ReconnectToken(token.into());
    let (control, events) = connect_local(&endpoint, &token.0).await?;

    let initial_topology = control.current_topology();
    let seed_control = control.clone();
    let (live_tx, live_rx) = watch::channel(Some(seed_control));
    let (topology_tx, topology_rx) = watch::channel(initial_topology);
    let (link_state_tx, link_state_rx) = watch::channel(C2LinkState::Connected);
    let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(reconnect_supervisor(
        endpoint,
        token,
        SupervisorChannels {
            live: live_tx,
            topology: topology_tx,
            link_state: link_state_tx,
            events: events_tx,
            shutdown: shutdown_rx,
        },
        control,
        events,
    ));

    Ok((
        C2ReconnectingHandle {
            live: live_rx,
            topology: topology_rx,
            link_state: link_state_rx,
            _supervisor: Arc::new(SupervisorShutdownGuard(shutdown_tx)),
        },
        C2ReconnectingEventReceiver { inner: events_rx },
    ))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::runtime::{c2_proof, client_compatibility_offer};
    use gate4agent_c2_protocol::{
        AgentId, ArchitectureId, C2AuthDirection, C2ClientFrame, C2Hello, C2NodeEvent,
        C2NodeResponse, C2NodeSnapshot, C2ReplyEnvelope, C2RelayRoute, C2RequestEnvelope,
        C2ServerChallenge, C2ServerFrame, C2TopologyNode, HostDescriptor,
        NegotiatedC2ControlCompatibility, NodeCursor, NodeId, NodeTransportState,
        OperatingSystemId, PathEncoding, PathSemantics, PathStyle, StatusResponse,
        BUILD_STAMP, C2_AUTH_NONCE_BYTES, MAX_C2_AUTH_FRAME_BYTES,
        MAX_C2_CLIENT_FRAME_BYTES, MAX_C2_HELLO_FRAME_BYTES, MAX_C2_SERVER_FRAME_BYTES,
    };
    use gate4agent_node_protocol::{
        read_json_frame_limited_body_timeout, write_json_frame_limited, NodeIncarnationId,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::task::JoinHandle;

    fn unique_control_endpoint() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!(
            r"\\.\pipe\gate4agent-c2-client-reconnect-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )
    }

    fn empty_status() -> StatusResponse {
        StatusResponse {
            api_version: 1,
            ready: true,
            observed_at_unix_ms: 0,
            nodes: BTreeMap::new(),
        }
    }

    fn test_route() -> NodeRoute {
        NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        }
    }

    fn routed_event(node_id: &NodeId, sequence: u64) -> RoutedNodeEvent {
        RoutedNodeEvent {
            node_id: node_id.clone(),
            cursor: NodeCursor { incarnation_id: NodeIncarnationId::from_bytes([7; 16]), sequence },
            event: C2NodeEvent::ResyncRequired { oldest_available_sequence: sequence },
        }
    }

    fn topology_with_one_node(node_id: &NodeId) -> C2Topology {
        C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: "test-endpoint".to_owned(),
                relay_route: C2RelayRoute::default(),
                transport: NodeTransportState::Online,
                current_incarnation_id: None,
                provider_contracts: Vec::new(),
                provider_adapter_contracts: Vec::new(),
                provider_runtime_statuses: Default::default(),
                observation_support: None,
            }],
        }
    }

    fn canned_node_snapshot(node_id: &NodeId, tag: &str) -> C2NodeSnapshot {
        C2NodeSnapshot {
            node_id: node_id.clone(),
            enabled_providers: vec![AgentId::new(tag).unwrap()],
            provider_runtime_statuses: Default::default(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            observation_support: None,
        }
    }

    fn canned_reply(envelope: &C2RequestEnvelope, tag: &str) -> C2ReplyEnvelope {
        assert!(matches!(envelope.request.request, NodeRequest::Snapshot));
        C2ReplyEnvelope {
            request_id: envelope.request_id,
            result: Ok(RoutedNodeResponse {
                node_id: envelope.request.route.node_id.clone(),
                incarnation_id: envelope.request.route.expected_incarnation_id,
                response: Ok(C2NodeResponse::Snapshot {
                    event_sequence: 0,
                    controller: None,
                    snapshot: canned_node_snapshot(&envelope.request.route.node_id, tag),
                }),
            }),
        }
    }

    fn assert_snapshot_tag(response: &RoutedNodeResponse, tag: &str) {
        let Ok(C2NodeResponse::Snapshot { snapshot, .. }) = &response.response else {
            panic!("expected a Snapshot response, got {:?}", response.response);
        };
        assert_eq!(snapshot.enabled_providers[0].as_str(), tag);
    }

    /// Reads the client's Hello, sends a Challenge (with a deliberately
    /// wrong proof when `corrupt_proof` is set), and -- unless the proof
    /// was corrupted, in which case the client aborts right here -- reads
    /// the client's Authenticate frame. Returns the negotiated
    /// compatibility the caller must echo back in the final Hello.
    async fn accept_handshake(
        pipe: &mut NamedPipeServer,
        token: &str,
        corrupt_proof: bool,
    ) -> Option<NegotiatedC2ControlCompatibility> {
        let frame = read_json_frame_limited_body_timeout::<_, C2ClientFrame>(
            pipe,
            MAX_C2_AUTH_FRAME_BYTES,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let C2ClientFrame::Hello(hello) = frame else { panic!("client did not send hello") };
        let offer = client_compatibility_offer().unwrap();
        let selected = NegotiatedC2ControlCompatibility {
            build_stamp: offer.build_stamp.clone(),
            capabilities: offer.capabilities.clone(),
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics { style: PathStyle::Windows, encoding: PathEncoding::Utf8 },
        };
        let server_nonce = [7_u8; C2_AUTH_NONCE_BYTES];
        let mut server_proof = c2_proof(
            token,
            C2AuthDirection::Server,
            &hello.client_nonce,
            &server_nonce,
            Some((&offer, &selected)),
        )
        .unwrap();
        if corrupt_proof {
            server_proof[0] ^= 0xFF;
        }
        write_json_frame_limited(
            pipe,
            &C2ServerFrame::Challenge(C2ServerChallenge {
                build_stamp: BUILD_STAMP.to_owned(),
                server_nonce,
                server_proof,
                compatibility: Some(selected.clone()),
            }),
            MAX_C2_AUTH_FRAME_BYTES,
        )
        .await
        .unwrap();
        if corrupt_proof {
            return None;
        }
        let _authenticate = read_json_frame_limited_body_timeout::<_, C2ClientFrame>(
            pipe,
            MAX_C2_AUTH_FRAME_BYTES,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        Some(selected)
    }

    /// A fully-authenticated fake C2 server: answers every
    /// `NodeRequest::Snapshot` with a canned reply tagged `tag` (so a test
    /// can prove which server instance actually answered a request), and
    /// forwards any frame pushed on `to_client` to the wire in order.
    ///
    /// Runs entirely inside one spawned task, including the initial
    /// `.connect()`/handshake -- `spawn_fake_server` itself returns
    /// immediately (it is not `async` and is never awaited by the caller
    /// before that), so a caller can spawn this and then start the CLIENT
    /// side (`connect_local_reconnecting`) concurrently. Awaiting the
    /// handshake to complete here first would deadlock: nothing would ever
    /// drive the client side that this server is waiting to accept.
    struct FakeServer {
        to_client: mpsc::Sender<C2ServerFrame>,
        task: JoinHandle<()>,
    }

    impl FakeServer {
        /// Drops the pipe, simulating the relay dying mid-session, and
        /// waits for the OS handle to be fully released so a new
        /// `first_pipe_instance` server can bind the same endpoint next.
        async fn kill(self) {
            self.task.abort();
            let _ = self.task.await;
        }
    }

    fn spawn_fake_server(
        endpoint: &str,
        token: &str,
        status: StatusResponse,
        tag: &'static str,
    ) -> FakeServer {
        let pipe = ServerOptions::new().first_pipe_instance(true).create(endpoint).unwrap();
        let token = token.to_owned();
        let (to_client, mut to_client_rx) = mpsc::channel::<C2ServerFrame>(16);
        let task = tokio::spawn(async move {
            let mut pipe = pipe;
            pipe.connect().await.unwrap();
            let selected = accept_handshake(&mut pipe, &token, false).await.unwrap();
            write_json_frame_limited(
                &mut pipe,
                &C2ServerFrame::Hello(C2Hello {
                    build_stamp: BUILD_STAMP.to_owned(),
                    connection_id: 1,
                    status,
                    compatibility: Some(selected),
                }),
                MAX_C2_HELLO_FRAME_BYTES,
            )
            .await
            .unwrap();

            let (mut reader, mut writer) = tokio::io::split(pipe);
            loop {
                tokio::select! {
                    outgoing = to_client_rx.recv() => {
                        let Some(frame) = outgoing else { return; };
                        if write_json_frame_limited(&mut writer, &frame, MAX_C2_SERVER_FRAME_BYTES)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    incoming = read_json_frame_limited_body_timeout::<_, C2ClientFrame>(
                        &mut reader,
                        MAX_C2_CLIENT_FRAME_BYTES,
                        Duration::from_secs(60),
                    ) => {
                        match incoming {
                            Ok(C2ClientFrame::Request(envelope)) => {
                                let reply = C2ServerFrame::Reply(canned_reply(&envelope, tag));
                                if write_json_frame_limited(&mut writer, &reply, MAX_C2_SERVER_FRAME_BYTES)
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            _ => return,
                        }
                    }
                }
            }
        });
        FakeServer { to_client, task }
    }

    /// One long-lived server, listening for the entire test: every
    /// connection attempt that lands here (however many transient ones
    /// happened before this existed, and every one after) authenticates
    /// far enough to receive a corrupted Challenge -- which the client
    /// rejects as `Authentication`, a hard reconnect failure. Every
    /// connection AFTER that first one is only observed, not handshaken:
    /// this fixture exists to measure WHEN a reconnect attempt lands
    /// (cadence), not to re-prove the classification a second time, and a
    /// bare accept avoids racing the client's own auth deadline under
    /// paused time. A single persistent pipe instance (never re-created)
    /// also avoids racing Windows's own pipe-name-reuse cleanup, unlike
    /// creating a fresh `first_pipe_instance` server after each attempt
    /// would.
    fn spawn_looping_hard_failure_server(
        endpoint: &str,
        token: &str,
    ) -> (JoinHandle<()>, watch::Receiver<usize>) {
        let endpoint = endpoint.to_owned();
        let token = token.to_owned();
        let (count_tx, count_rx) = watch::channel(0_usize);
        let task = tokio::spawn(async move {
            let mut pipe = ServerOptions::new().first_pipe_instance(true).create(&endpoint).unwrap();
            if pipe.connect().await.is_err() {
                return;
            }
            accept_handshake(&mut pipe, &token, true).await;
            let mut count = 1_usize;
            count_tx.send_replace(count);
            if pipe.disconnect().is_err() {
                return;
            }
            loop {
                if pipe.connect().await.is_err() {
                    return;
                }
                count += 1;
                count_tx.send_replace(count);
                if pipe.disconnect().is_err() {
                    return;
                }
            }
        });
        (task, count_rx)
    }

    async fn wait_for_link_state(receiver: &mut watch::Receiver<C2LinkState>, expected: C2LinkState) {
        while *receiver.borrow() != expected {
            receiver.changed().await.unwrap();
        }
    }

    async fn wait_for_attempt_count(receiver: &mut watch::Receiver<usize>, expected: usize) {
        while *receiver.borrow() < expected {
            receiver.changed().await.unwrap();
        }
    }

    /// `send_replace` always notifies, even when the republished value is
    /// unchanged (e.g. the reconnect-time republish of an already-empty
    /// topology) -- so a single `.changed()` call is not guaranteed to
    /// land on the specific content a test is waiting for. Loops until the
    /// held value actually matches, on the SAME pre-acquired receiver.
    async fn wait_for_topology_node_count(
        receiver: &mut watch::Receiver<Arc<C2Topology>>,
        expected: usize,
    ) {
        while receiver.borrow().nodes.len() != expected {
            receiver.changed().await.unwrap();
        }
    }

    #[test]
    fn reconnect_backoff_uses_the_transient_array_then_parks() {
        assert_eq!(reconnect_backoff(1, false), RECONNECT_TRANSIENT_BACKOFFS[0]);
        assert_eq!(reconnect_backoff(5, false), RECONNECT_TRANSIENT_BACKOFFS[4]);
        assert_eq!(reconnect_backoff(6, false), RECONNECT_PARKED_BACKOFF);
        assert_eq!(reconnect_backoff(1, true), RECONNECT_PARKED_BACKOFF);
        assert_eq!(reconnect_backoff(0, false), RECONNECT_PARKED_BACKOFF);
    }

    #[tokio::test]
    async fn reconnect_recovers_after_server_drops_the_pipe() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-1";
        let server1 = spawn_fake_server(&endpoint, token, empty_status(), "server-1");
        let (handle, _events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();
        let mut link_state = handle.link_state_receiver();

        let first = handle.request(test_route(), NodeRequest::Snapshot).await.unwrap();
        assert_snapshot_tag(&first, "server-1");

        server1.kill().await;
        wait_for_link_state(&mut link_state, C2LinkState::Reconnecting).await;

        let server2 = spawn_fake_server(&endpoint, token, empty_status(), "server-2");
        wait_for_link_state(&mut link_state, C2LinkState::Connected).await;

        let second = handle.request(test_route(), NodeRequest::Snapshot).await.unwrap();
        assert_snapshot_tag(&second, "server-2");

        server2.kill().await;
    }

    #[tokio::test]
    async fn reconnect_keeps_the_original_event_and_topology_receivers_live() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-2";
        let server1 = spawn_fake_server(&endpoint, token, empty_status(), "server-1");
        let (handle, mut events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();
        let mut topology = handle.subscribe_topology();
        let mut link_state = handle.link_state_receiver();

        server1.kill().await;
        wait_for_link_state(&mut link_state, C2LinkState::Reconnecting).await;

        let server2 = spawn_fake_server(&endpoint, token, empty_status(), "server-2");
        wait_for_link_state(&mut link_state, C2LinkState::Connected).await;

        let node_id = NodeId::new("node-a").unwrap();
        server2.to_client.send(C2ServerFrame::Event(routed_event(&node_id, 1))).await.unwrap();
        server2
            .to_client
            .send(C2ServerFrame::Topology(topology_with_one_node(&node_id)))
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event must arrive on the receiver acquired before the outage")
            .expect("event stream must not have closed");
        assert_eq!(event.node_id, node_id);

        tokio::time::timeout(Duration::from_secs(5), wait_for_topology_node_count(&mut topology, 1))
            .await
            .expect("topology change must arrive on the receiver acquired before the outage");

        server2.kill().await;
    }

    #[tokio::test]
    async fn reconnect_requests_fail_fast_while_reconnecting() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-3";
        let server1 = spawn_fake_server(&endpoint, token, empty_status(), "server-1");
        let (handle, _events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();
        let mut link_state = handle.link_state_receiver();

        server1.kill().await;
        wait_for_link_state(&mut link_state, C2LinkState::Reconnecting).await;

        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            handle.request(test_route(), NodeRequest::Snapshot),
        )
        .await
        .expect("request must fail fast, not hang while reconnecting");
        assert!(matches!(outcome, Err(C2ControlError::Closed)));

        assert!(matches!(
            handle.start_request(test_route(), NodeRequest::Snapshot),
            Err(C2ControlError::Closed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_hard_failure_parks_without_hammering() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-4";
        let server1 = spawn_fake_server(&endpoint, token, empty_status(), "server-1");
        let (handle, _events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();
        let mut link_state = handle.link_state_receiver();

        server1.kill().await;
        wait_for_link_state(&mut link_state, C2LinkState::Reconnecting).await;

        // The reconnect loop's first attempt fires immediately (before any
        // backoff); this server is already listening by the time it can
        // possibly land, so it is the one that answers -- whether that is
        // attempt 1 or a later one is irrelevant here; paused time
        // auto-advances past any preceding transient backoff while this
        // bare `.await` is the only thing the runtime has left to do.
        let (_hard_server, mut attempts) = spawn_looping_hard_failure_server(&endpoint, token);
        wait_for_attempt_count(&mut attempts, 1).await;

        // That attempt hit the hard-failure server and got
        // `Authentication`, so the supervisor is now parked for
        // `RECONNECT_PARKED_BACKOFF`, not the short transient array. Measure
        // the actual virtual-time gap to the next attempt -- rather than a
        // fixed `time::advance` window followed by a synchronous check --
        // because paused time's own auto-advance only guarantees the clock
        // reaches the next timer eventually, not that the resulting real
        // I/O (a fresh handshake) has already completed by some arbitrary
        // point partway through a manually bounded jump.
        let before = tokio::time::Instant::now();
        wait_for_attempt_count(&mut attempts, 2).await;
        let gap = tokio::time::Instant::now() - before;
        assert!(gap >= RECONNECT_PARKED_BACKOFF, "parked backoff must not fire early: gap was {gap:?}");
        // A generous sanity ceiling, not a tight bound: paused-time
        // auto-advance settling real I/O for the retry adds some slop on
        // top of the exact 30s deadline. What this actually rules out is
        // the supervisor stacking additional backoff on top of parking
        // (e.g. parking twice, or parking then also running the transient
        // array) -- either of which would clear this by a wide margin.
        assert!(
            gap < RECONNECT_PARKED_BACKOFF * 2,
            "parked backoff must fire close to {RECONNECT_PARKED_BACKOFF:?}, not stall further: gap was {gap:?}",
        );
    }
}
