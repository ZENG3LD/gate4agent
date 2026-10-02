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
    EVENT_CAPACITY, HARNESS_MCP_EVENT_CAPACITY,
};
use gate4agent_c2_protocol::{C2Topology, NodeRequest, NodeRoute, RoutedNodeEvent, RoutedNodeResponse};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Hard cap on the number of physical send attempts
/// [`C2ReconnectingHandle::request_until`] makes for one logical call,
/// independent of the caller's own budget -- a link that keeps flapping
/// back just long enough to fail again must not turn a generous budget
/// into an unbounded retry storm.
const REQUEST_UNTIL_MAX_ATTEMPTS: u32 = 3;

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
/// `HarnessOperatorCredential` (`hatchery-harness-api/src/lib.rs`) and
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

    /// [`Self::request`], but for a caller willing to wait out a physical
    /// reconnect: on `Err(C2ControlError::ConnectionLost { .. })` or
    /// `Err(C2ControlError::Closed)` for a request
    /// [`NodeRequest::is_replay_safe`] admits, awaits the link-state watch
    /// ([`Self::link_state_receiver`]) turning [`C2LinkState::Connected`]
    /// again -- bounded by whatever of `budget` is left -- then resends,
    /// up to [`REQUEST_UNTIL_MAX_ATTEMPTS`] physical attempts total for
    /// this one logical call. A request `is_replay_safe` refuses, or any
    /// failure other than those two, returns on the very first attempt
    /// exactly like `request` does -- this method changes nothing about
    /// `request`'s own behaviour or about `control_owner`'s pending-request
    /// failure handling; the retry lives here, one layer up.
    ///
    /// `budget == Duration::ZERO` degenerates to exactly one attempt (the
    /// first elapsed-budget check trips before any wait), so a caller that
    /// wants `request`'s existing all-or-nothing semantics can pass that
    /// instead of calling `request` directly.
    pub async fn request_until(
        &self,
        route: NodeRoute,
        request: NodeRequest,
        budget: Duration,
    ) -> Result<RoutedNodeResponse, C2ControlError> {
        let deadline = Instant::now() + budget;
        let replay_safe = request.is_replay_safe();
        let mut link_state = self.link_state.clone();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let result = self.request(route.clone(), request.clone()).await;
            let retryable = matches!(
                result,
                Err(C2ControlError::ConnectionLost { .. }) | Err(C2ControlError::Closed)
            );
            if !retryable || !replay_safe || attempts >= REQUEST_UNTIL_MAX_ATTEMPTS {
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                return result;
            }
            if tokio::time::timeout(deadline - now, wait_for_link_connected(&mut link_state))
                .await
                .is_err()
            {
                return result;
            }
        }
    }
}

/// Awaits the link-state watch reporting [`C2LinkState::Connected`],
/// returning immediately if it already does. Only ever raced against a
/// caller-supplied timeout ([`C2ReconnectingHandle::request_until`]) -- a
/// dropped supervisor (the sender side gone) also returns immediately,
/// since a frozen `watch::Receiver` cannot change further and the next
/// attempt in the caller's loop will just observe the same failure again.
async fn wait_for_link_connected(link_state: &mut watch::Receiver<C2LinkState>) {
    while *link_state.borrow() != C2LinkState::Connected {
        if link_state.changed().await.is_err() {
            return;
        }
    }
}

/// The event stream for a reconnecting connection. Backed by two bridge
/// channels created once, before the first physical connection, that only
/// close when the supervisor itself terminates -- a reconnect is invisible
/// to it. The two channels mirror the split `C2EventReceiver` already
/// carries per physical connection (regular events vs. harness_mcp
/// read-proxy events), so a caller that wants harness_mcp events served
/// ahead of a regular-event backlog can drain them independently instead
/// of going through the merged `recv()`.
pub struct C2ReconnectingEventReceiver {
    inner: mpsc::Receiver<RoutedNodeEvent>,
    harness_mcp: mpsc::Receiver<RoutedNodeEvent>,
}

impl C2ReconnectingEventReceiver {
    /// Returns every event from both channels, harness_mcp-first when both
    /// are ready -- the exact merge algorithm `C2EventReceiver::recv`
    /// already implements per physical connection, moved one layer up to
    /// this bridge's own two channels. Existing callers that only ever call
    /// this method see the identical observable behavior as before the
    /// split: same events, same relative order.
    pub async fn recv(&mut self) -> Option<RoutedNodeEvent> {
        loop {
            if self.harness_mcp.is_closed() && self.harness_mcp.is_empty() {
                return self.inner.recv().await;
            }
            if self.inner.is_closed() && self.inner.is_empty() {
                return self.harness_mcp.recv().await;
            }
            tokio::select! {
                biased;
                event = self.harness_mcp.recv() => {
                    if event.is_some() { return event; }
                }
                event = self.inner.recv() => {
                    if event.is_some() { return event; }
                }
            }
        }
    }

    /// Drains only the regular-event channel, leaving harness_mcp events
    /// untouched.
    pub async fn recv_regular(&mut self) -> Option<RoutedNodeEvent> {
        self.inner.recv().await
    }

    /// Drains only the harness_mcp channel, leaving regular events
    /// untouched.
    pub async fn recv_harness_mcp(&mut self) -> Option<RoutedNodeEvent> {
        self.harness_mcp.recv().await
    }

    /// Splits into two independent mutable borrows, one per channel, so a
    /// caller can race both in a single `tokio::select!` (the harness
    /// runtime's own main loop, prioritizing `HarnessMcpReadCall` ahead of
    /// its regular-event backlog). Two separate `recv_regular`/
    /// `recv_harness_mcp` calls cannot be used together in one `select!`:
    /// each is an `&mut self` async method, so the future each returns
    /// borrows the whole receiver for its lifetime, and the borrow checker
    /// rejects two such futures coexisting even though they touch disjoint
    /// fields. Splitting once, up front, hands out two genuinely disjoint
    /// borrows instead.
    pub fn split_mut(
        &mut self,
    ) -> (&mut mpsc::Receiver<RoutedNodeEvent>, &mut mpsc::Receiver<RoutedNodeEvent>) {
        (&mut self.inner, &mut self.harness_mcp)
    }
}

/// The long-lived channels the supervisor publishes into. Held as one
/// struct (rather than five function parameters) purely for readability.
struct SupervisorChannels {
    live: watch::Sender<Option<C2ControlHandle>>,
    topology: watch::Sender<Arc<C2Topology>>,
    link_state: watch::Sender<C2LinkState>,
    events: mpsc::Sender<RoutedNodeEvent>,
    harness_mcp_events: mpsc::Sender<RoutedNodeEvent>,
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
    harness_mcp_events_tx: &mpsc::Sender<RoutedNodeEvent>,
    topology_tx: &watch::Sender<Arc<C2Topology>>,
    shutdown: &mut watch::Receiver<bool>,
) -> ConnectionEnded {
    // Split once, up front: `events.recv_harness_mcp()` and
    // `events.recv_regular()` cannot both appear as arms of the same
    // `select!` below -- each is an `&mut self` async method, so the two
    // futures would borrow the whole `C2EventReceiver` for the same
    // lifetime, which the borrow checker rejects even though the methods
    // only ever touch their own disjoint field. `split_mut` hands out the
    // two channels' receivers as genuinely independent `&mut` borrows
    // instead.
    let (regular_events, harness_mcp_events) = events.split_mut();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return ConnectionEnded::ShuttingDown,
            event = harness_mcp_events.recv() => {
                let Some(event) = event else { return ConnectionEnded::Lost; };
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => return ConnectionEnded::ShuttingDown,
                    sent = harness_mcp_events_tx.send(event) => {
                        if sent.is_err() { return ConnectionEnded::ConsumerGone; }
                    }
                }
            }
            event = regular_events.recv() => {
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
            &channels.harness_mcp_events,
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
    let (harness_mcp_events_tx, harness_mcp_events_rx) = mpsc::channel(HARNESS_MCP_EVENT_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(reconnect_supervisor(
        endpoint,
        token,
        SupervisorChannels {
            live: live_tx,
            topology: topology_tx,
            link_state: link_state_tx,
            events: events_tx,
            harness_mcp_events: harness_mcp_events_tx,
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
        C2ReconnectingEventReceiver { inner: events_rx, harness_mcp: harness_mcp_events_rx },
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

    fn now_unix_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().try_into().unwrap()
    }

    /// A `HarnessMcpReadCall` event whose deadline is far enough in the
    /// future (2.5s, under `MAX_HARNESS_MCP_CALL_DEADLINE_MS = 3s`) to
    /// still pass `harness_mcp_contract_is_valid_at` by the time it lands
    /// on the client, so `control_owner` classifies it onto the harness_mcp
    /// channel rather than dropping it as an expired contract.
    fn harness_mcp_routed_event(node_id: &NodeId, call_index: usize) -> RoutedNodeEvent {
        RoutedNodeEvent {
            node_id: node_id.clone(),
            cursor: NodeCursor { incarnation_id: NodeIncarnationId::from_bytes([7; 16]), sequence: 41 },
            event: C2NodeEvent::HarnessMcpReadCall {
                reservation_id: gate4agent_node_protocol::HarnessMcpReservationId::new(
                    format!("hmcpres_{:024x}", 1),
                ).unwrap(),
                activation_digest: gate4agent_node_protocol::HarnessMcpActivationDigest::new(
                    format!("sha256:{}", "b".repeat(64)),
                ).unwrap(),
                record_id: gate4agent_node_protocol::SessionRecordId::new("session-001").unwrap(),
                session: gate4agent_node_protocol::SessionAddress {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new("primary").unwrap(),
                    session: gate4agent_node_protocol::SessionKey {
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(2),
                    },
                },
                call_id: gate4agent_node_protocol::HarnessMcpCallId::new(
                    format!("hmcpcall_{call_index:024x}"),
                ).unwrap(),
                request: gate4agent_node_protocol::HarnessMcpOpaquePayloadV1 {
                    content_type:
                        gate4agent_node_protocol::HarnessMcpContentTypeV1::HarnessReadRequestJsonV1,
                    body: br#"{"kind":"context-get"}"#.to_vec(),
                },
                deadline_unix_ms: now_unix_ms() + 2_500,
            },
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

    /// The bridge's split channels must stay genuinely independent, not
    /// merely `recv()`'s internal biased-merge order: pushes a regular
    /// event FIRST, then a harness_mcp event, both landing on the bridge
    /// before either is drained -- `recv_harness_mcp()` must still return
    /// the harness_mcp event without waiting on the regular one enqueued
    /// ahead of it, and `recv_regular()` must still return the regular
    /// event without ever seeing the harness_mcp one.
    #[tokio::test]
    async fn reconnecting_receiver_split_accessors_drain_only_their_own_channel() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-split-1";
        let server = spawn_fake_server(&endpoint, token, empty_status(), "server-1");
        let (_handle, mut events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();

        let node_id = NodeId::new("node-a").unwrap();
        server.to_client.send(C2ServerFrame::Event(routed_event(&node_id, 1))).await.unwrap();
        server.to_client.send(C2ServerFrame::Event(harness_mcp_routed_event(&node_id, 1))).await.unwrap();

        let mcp_event = tokio::time::timeout(Duration::from_secs(5), events.recv_harness_mcp())
            .await
            .expect("recv_harness_mcp must not block on the regular event queued ahead of it")
            .expect("harness_mcp channel must not have closed");
        assert!(matches!(mcp_event.event, C2NodeEvent::HarnessMcpReadCall { .. }));

        let regular_event = tokio::time::timeout(Duration::from_secs(5), events.recv_regular())
            .await
            .expect("recv_regular must not block on the harness_mcp channel")
            .expect("regular channel must not have closed");
        assert!(matches!(regular_event.event, C2NodeEvent::ResyncRequired { .. }));

        server.kill().await;
    }

    /// Legacy callers that only ever call the merged `recv()` must see the
    /// exact same events, still harness-mcp-first when both are queued --
    /// the split must be invisible to them.
    #[tokio::test]
    async fn reconnecting_receiver_recv_still_merges_both_harness_mcp_first() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-split-2";
        let server = spawn_fake_server(&endpoint, token, empty_status(), "server-1");
        let (_handle, mut events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();

        let node_id = NodeId::new("node-a").unwrap();
        server.to_client.send(C2ServerFrame::Event(routed_event(&node_id, 1))).await.unwrap();
        server.to_client.send(C2ServerFrame::Event(harness_mcp_routed_event(&node_id, 1))).await.unwrap();

        // Both events must be sitting in their respective bridge channels
        // BEFORE `recv()` is called -- otherwise `select!`'s `biased`
        // ordering (which only matters when multiple arms are
        // simultaneously ready) is not actually exercised: the regular
        // event, sent first on the wire, is always classified and
        // forwarded strictly before the harness_mcp one that followed it,
        // so a `recv()` issued too early would just see the regular event
        // as the only one ready and return it -- not proof of anything.
        tokio::time::timeout(Duration::from_secs(5), async {
            while events.inner.len() != 1 || events.harness_mcp.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both events must reach the bridge's own channels");

        let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("recv must not hang")
            .expect("event stream must not have closed");
        assert!(matches!(first.event, C2NodeEvent::HarnessMcpReadCall { .. }));

        let second = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("recv must not hang")
            .expect("event stream must not have closed");
        assert!(matches!(second.event, C2NodeEvent::ResyncRequired { .. }));

        server.kill().await;
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

    /// A fully-authenticated fake C2 server that drops the physical
    /// connection the instant the client's first `Request` frame arrives --
    /// never replies -- so the pending request fails with
    /// `C2ControlError::ConnectionLost`, exactly the failure mode the
    /// relay's own mid-flight disconnect produces
    /// (`control_owner`, `runtime.rs:3014-3024`). Returns an
    /// `AtomicUsize` the caller can read after the fact to prove how many
    /// physical `Request` frames this connection actually received --
    /// `request_until`'s own attempt cap and replay-safety gate are only
    /// provable by counting real sends, not just by timing.
    fn spawn_fake_server_drop_on_request(
        endpoint: &str,
        token: &str,
    ) -> (FakeServer, Arc<AtomicUsize>) {
        let pipe = ServerOptions::new().first_pipe_instance(true).create(endpoint).unwrap();
        let token = token.to_owned();
        let requests_seen = Arc::new(AtomicUsize::new(0));
        let requests_seen_in_task = Arc::clone(&requests_seen);
        let (to_client, _to_client_rx) = mpsc::channel::<C2ServerFrame>(16);
        let task = tokio::spawn(async move {
            let mut pipe = pipe;
            pipe.connect().await.unwrap();
            let Some(selected) = accept_handshake(&mut pipe, &token, false).await else { return; };
            write_json_frame_limited(
                &mut pipe,
                &C2ServerFrame::Hello(C2Hello {
                    build_stamp: BUILD_STAMP.to_owned(),
                    connection_id: 1,
                    status: empty_status(),
                    compatibility: Some(selected),
                }),
                MAX_C2_HELLO_FRAME_BYTES,
            )
            .await
            .unwrap();
            let (mut reader, _writer) = tokio::io::split(pipe);
            if read_json_frame_limited_body_timeout::<_, C2ClientFrame>(
                &mut reader,
                MAX_C2_CLIENT_FRAME_BYTES,
                Duration::from_secs(60),
            )
            .await
            .is_ok()
            {
                requests_seen_in_task.fetch_add(1, Ordering::SeqCst);
            }
            // `reader`/`_writer`/`pipe` drop here, closing the physical
            // connection without ever sending a `Reply` -- the client's
            // pending request fails with `ConnectionLost`, not a clean
            // answer.
        });
        (FakeServer { to_client, task }, requests_seen)
    }

    /// A replay-safe request (`NodeRequest::Snapshot`) whose first attempt
    /// fails with `ConnectionLost` (the fake server drops the connection
    /// the instant the request lands) must come back with the real answer
    /// once the link is reconnected -- `request_until` retries it against
    /// whichever physical connection the supervisor reconnects to, all
    /// within its own budget, without the caller ever seeing the
    /// intermediate failure.
    #[tokio::test]
    async fn request_until_survives_a_reconnect_for_a_replay_safe_request() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-request-until-1";
        let (server1, requests_seen) = spawn_fake_server_drop_on_request(&endpoint, token);
        let (handle, _events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();

        let mut link_state_for_server2 = handle.link_state_receiver();
        let endpoint_for_server2 = endpoint.clone();
        let token_for_server2 = token.to_owned();
        let start_server2 = tokio::spawn(async move {
            wait_for_link_state(&mut link_state_for_server2, C2LinkState::Reconnecting).await;
            spawn_fake_server(&endpoint_for_server2, &token_for_server2, empty_status(), "server-2")
        });

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            handle.request_until(test_route(), NodeRequest::Snapshot, Duration::from_secs(5)),
        )
        .await
        .expect("a replay-safe request must not hang past its own budget");
        let response = result.expect("must succeed once the link comes back within the budget");
        assert_snapshot_tag(&response, "server-2");
        assert_eq!(
            requests_seen.load(Ordering::SeqCst),
            1,
            "the first attempt must have actually gone out on server-1's connection",
        );

        let server2 = start_server2.await.unwrap();
        server1.kill().await;
        server2.kill().await;
    }

    /// The same connection-drops-on-request failure as above, but with a
    /// request `NodeRequest::is_replay_safe` refuses
    /// (`AcquireController`): it must fail immediately with the original
    /// `ConnectionLost`, never wait for a reconnect, and never be resent --
    /// proven by an outer timeout far shorter than the budget it was given,
    /// and by the fake server's own attempt counter staying at exactly one.
    #[tokio::test]
    async fn request_until_never_retries_a_non_replay_safe_request() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-request-until-2";
        let (server1, requests_seen) = spawn_fake_server_drop_on_request(&endpoint, token);
        let (handle, _events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            handle.request_until(
                test_route(),
                NodeRequest::AcquireController { lease_ms: 1_000 },
                Duration::from_secs(30),
            ),
        )
        .await
        .expect(
            "a non-replay-safe request must not wait on a reconnect it will never use, \
             no matter how generous its budget is",
        );
        assert!(matches!(result, Err(C2ControlError::ConnectionLost { .. })));
        assert_eq!(
            requests_seen.load(Ordering::SeqCst),
            1,
            "a non-replay-safe request must never be resent after its first attempt fails",
        );

        server1.kill().await;
    }

    /// A replay-safe request against a link that never comes back must
    /// still end -- bounded by its own budget, not by spinning forever
    /// waiting on a reconnect that never happens, and without exceeding
    /// `REQUEST_UNTIL_MAX_ATTEMPTS` real sends in the process.
    #[tokio::test]
    async fn request_until_ends_by_its_budget_when_the_link_never_returns() {
        let endpoint = unique_control_endpoint();
        let token = "reconnect-token-request-until-3";
        let (server1, requests_seen) = spawn_fake_server_drop_on_request(&endpoint, token);
        let (handle, _events) =
            connect_local_reconnecting(endpoint.clone(), token.to_owned()).await.unwrap();

        let budget = Duration::from_millis(600);
        let before = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            handle.request_until(test_route(), NodeRequest::Snapshot, budget),
        )
        .await
        .expect("request_until must end on its own once its budget is exhausted, not hang");
        let elapsed = tokio::time::Instant::now() - before;

        assert!(matches!(result, Err(C2ControlError::ConnectionLost { .. })));
        assert_eq!(
            requests_seen.load(Ordering::SeqCst),
            1,
            "no server ever reconnected, so only the first attempt could ever be sent",
        );
        assert!(
            elapsed + Duration::from_millis(50) >= budget,
            "must not return well before the budget it was given: elapsed {elapsed:?}",
        );
        assert!(
            elapsed < budget * 3,
            "must not spin well past its own budget waiting on a link that never returns: \
             elapsed {elapsed:?}",
        );

        server1.kill().await;
    }
}
