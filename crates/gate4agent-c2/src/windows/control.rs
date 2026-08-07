use super::*;
use crate::protocol::{
    c2_auth_transcript, C2AuthDirection, C2ClientAuthentication, C2ClientFrame,
    C2ReplyEnvelope, C2ServerChallenge, C2Topology,
    C2ServerFrame, C2Hello, C2_CONTROL_PROTOCOL_VERSION, MAX_C2_AUTH_FRAME_BYTES,
    MAX_C2_CLIENT_FRAME_BYTES, MAX_C2_HELLO_FRAME_BYTES, MAX_C2_SERVER_FRAME_BYTES,
};
use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, FrameError,
};
use gate4agent_node_wire::{local_hmac_sha256, proofs_match, random_nonce};
use std::sync::atomic::AtomicUsize;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::OwnedSemaphorePermit;

const MAX_PREAUTH_CONNECTIONS: usize = 4;
const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const FRAME_BODY_DEADLINE: Duration = Duration::from_secs(5);
const MAX_OUTBOUND_FRAMES: usize = 128;
const MAX_OUTBOUND_BYTES: usize = 16 * 1024 * 1024;
const MAX_INBOUND_FRAMES: usize = 4;
const REPLY_QUEUE_DEADLINE: Duration = Duration::from_secs(3);

pub(super) async fn run(
    endpoint: String,
    token: String,
    relays: Arc<BTreeMap<NodeId, RelayEndpoint>>,
    status: watch::Receiver<Arc<StatusResponse>>,
    hub: OperatorHub,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let preauth = Arc::new(Semaphore::new(MAX_PREAUTH_CONNECTIONS));
    let authenticated = Arc::new(Semaphore::new(1));
    let next_connection_id = Arc::new(AtomicU64::new(1));
    let mut connections = JoinSet::new();
    let mut first = true;
    loop {
        let permit = tokio::select! {
            permit = Arc::clone(&preauth).acquire_owned() => permit.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "C2 preauth slots closed"))?,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
                continue;
            }
        };
        let server = create_pipe(&endpoint, first)?;
        tokio::select! {
            connected = server.connect() => connected?,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
                continue;
            }
        }
        first = false;
        let token = token.clone();
        let authenticated = Arc::clone(&authenticated);
        let connection_ids = Arc::clone(&next_connection_id);
        let relays = Arc::clone(&relays);
        let status = status.clone();
        let hub = hub.clone();
        let connection_shutdown = shutdown.clone();
        connections.spawn(async move {
            let _ = serve_connection(
                server, permit, authenticated, connection_ids, &token,
                relays, status, hub, connection_shutdown,
            ).await;
        });
        while connections.try_join_next().is_some() {}
    }
    connections.shutdown().await;
    Ok(())
}

fn create_pipe(endpoint: &str, first: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    options.create(endpoint)
}

async fn serve_connection(
    mut pipe: NamedPipeServer,
    preauth_permit: OwnedSemaphorePermit,
    authenticated: Arc<Semaphore>,
    connection_ids: Arc<AtomicU64>,
    token: &str,
    relays: Arc<BTreeMap<NodeId, RelayEndpoint>>,
    mut status: watch::Receiver<Arc<StatusResponse>>,
    hub: OperatorHub,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), FrameError> {
    let hello = timeout(AUTH_DEADLINE, read_client_frame(&mut pipe, MAX_C2_AUTH_FRAME_BYTES))
        .await.map_err(|_| FrameError::PrefixTimedOut)??;
    let C2ClientFrame::Hello(hello) = hello else { return Ok(()); };
    if hello.protocol_version != C2_CONTROL_PROTOCOL_VERSION { return Ok(()); }
    let server_nonce = random_nonce().map_err(authentication_frame_error)?;
    let server_proof = c2_proof(token, C2AuthDirection::Server, &hello.client_nonce, &server_nonce)
        .map_err(authentication_frame_error)?;
    timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ServerFrame::Challenge(C2ServerChallenge {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
        }),
        MAX_C2_AUTH_FRAME_BYTES,
    )).await.map_err(|_| FrameError::BodyTimedOut { length: 0 })??;
    let authentication = timeout(AUTH_DEADLINE, read_client_frame(&mut pipe, MAX_C2_AUTH_FRAME_BYTES))
        .await.map_err(|_| FrameError::PrefixTimedOut)??;
    let C2ClientFrame::Authenticate(C2ClientAuthentication { client_proof }) = authentication else { return Ok(()); };
    let expected = c2_proof(token, C2AuthDirection::Client, &hello.client_nonce, &server_nonce)
        .map_err(authentication_frame_error)?;
    if !proofs_match(&client_proof, &expected) { return Ok(()); }
    drop(preauth_permit);

    let operator_permit = match Arc::clone(&authenticated).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            timeout(AUTH_DEADLINE, write_json_frame_limited(
                &mut pipe,
                &C2ServerFrame::Rejected(relay_failure(
                    C2RelayFailureCode::OperatorAlreadyConnected,
                    "another C2 operator is already connected",
                    None,
                )),
                MAX_C2_SERVER_FRAME_BYTES,
            )).await.map_err(|_| FrameError::BodyTimedOut { length: 0 })??;
            return Ok(());
        }
    };
    let connection_id = connection_ids.fetch_add(1, Ordering::AcqRel);
    let (disconnect_tx, mut disconnect_rx) = watch::channel(false);
    let (outbound_tx, mut outbound_rx) = mpsc::channel(MAX_OUTBOUND_FRAMES);
    let budget = Arc::new(AtomicUsize::new(0));
    hub.attach(OperatorEventSink {
        connection_id,
        outbound: outbound_tx.clone(),
        budget: Arc::clone(&budget),
        disconnect: disconnect_tx.clone(),
    });
    let hello_status = refresh_hello_status(connection_id, &relays, &status).await;
    if !prune_pre_hello_events(
        &hub,
        connection_id,
        &mut outbound_rx,
        &outbound_tx,
        &budget,
        &hello_status,
    ) {
        hub.detach(connection_id);
        return Ok(());
    }
    let mut last_topology = C2Topology::from_status(&hello_status);
    if timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ServerFrame::Hello(C2Hello {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            connection_id,
            status: hello_status,
        }),
        MAX_C2_HELLO_FRAME_BYTES,
    )).await.map_err(|_| FrameError::BodyTimedOut { length: 0 }).and_then(|result| result).is_err() {
        hub.detach(connection_id);
        return Ok(());
    }
    let (reader, writer) = tokio::io::split(pipe);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(MAX_INBOUND_FRAMES);
    let reader_task = tokio::spawn(control_reader(reader, incoming_tx));
    let writer_task = tokio::spawn(control_writer(writer, outbound_rx, Arc::clone(&budget), disconnect_tx.clone()));
    let mut dispatches = JoinSet::new();
    let mut last_request_id = 0_u64;

    loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(Ok(C2ClientFrame::Request(request))) = incoming else { break; };
                if request.request_id.0 == 0 || request.request_id.0 <= last_request_id {
                    let failure = relay_failure(C2RelayFailureCode::RequestIdReused, "C2 request IDs must be nonzero and strictly increasing", None);
                    if queue_reply(&outbound_tx, &budget, C2ReplyEnvelope { request_id: request.request_id, result: Err(failure) }).await.is_err() { break; }
                    continue;
                }
                last_request_id = request.request_id.0;
                match dispatch_start(connection_id, request.request, &relays, &status) {
                    DispatchStart::Immediate(result) => {
                        if queue_reply(&outbound_tx, &budget, C2ReplyEnvelope { request_id: request.request_id, result }).await.is_err() { break; }
                    }
                    DispatchStart::Pending(reply) => {
                        let outbound = outbound_tx.clone();
                        let budget = Arc::clone(&budget);
                        let disconnect = disconnect_tx.clone();
                        dispatches.spawn(async move {
                            let result = reply.await.unwrap_or_else(|_| Err(relay_failure(C2RelayFailureCode::NodeOffline, "node relay disconnected", None)));
                            if queue_reply(&outbound, &budget, C2ReplyEnvelope { request_id: request.request_id, result }).await.is_err() {
                                let _ = disconnect.send(true);
                            }
                        });
                    }
                }
            }
            changed = disconnect_rx.changed() => if changed.is_err() || *disconnect_rx.borrow() { break; },
            changed = status.changed() => {
                if changed.is_err() { break; }
                let next_topology = {
                    let latest = status.borrow_and_update();
                    C2Topology::from_status(latest.as_ref())
                };
                if queue_topology_if_changed(
                    &outbound_tx,
                    &budget,
                    &mut last_topology,
                    next_topology,
                ).is_err() { break; }
            }
            changed = shutdown.changed() => if changed.is_err() || *shutdown.borrow() { break; },
        }
        while dispatches.try_join_next().is_some() {}
    }

    hub.detach(connection_id);
    dispatches.shutdown().await;
    release_all_controllers(&relays).await;
    reader_task.abort();
    writer_task.abort();
    drop(operator_permit);
    Ok(())
}

async fn refresh_hello_status(
    connection_id: u64,
    relays: &BTreeMap<NodeId, RelayEndpoint>,
    status: &watch::Receiver<Arc<StatusResponse>>,
) -> StatusResponse {
    let mut refreshed = (**status.borrow()).clone();
    let mut requests = JoinSet::new();
    for (node_id, observed) in &refreshed.nodes {
        let Some(cursor) = observed.cursor else { continue; };
        if observed.transport != NodeTransportState::Online { continue; }
        let relay = relays[node_id].commands.clone();
        requests.spawn(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let command = RelayCommand::Request {
                operator_connection_id: connection_id,
                expected_incarnation_id: cursor.incarnation_id,
                request: NodeRequest::Snapshot,
                reply: reply_tx,
            };
            if !matches!(timeout(Duration::from_secs(1), relay.send(command)).await, Ok(Ok(()))) {
                return None;
            }
            timeout(Duration::from_secs(2), reply_rx).await.ok()?.ok()
        });
    }
    let completed = timeout(Duration::from_secs(3), async {
        let mut responses = Vec::new();
        while let Some(result) = requests.join_next().await {
            if let Ok(Some(response)) = result { responses.push(response); }
        }
        responses
    }).await.unwrap_or_default();
    for response in completed {
        let Ok(response) = response else { continue; };
        let Ok(C2NodeResponse::Snapshot { event_sequence, snapshot, .. }) = response.response else { continue; };
        if let Some(observed) = refreshed.nodes.get_mut(&response.node_id) {
            observed.cursor = Some(NodeCursor { incarnation_id: response.incarnation_id, sequence: event_sequence });
            observed.inventory = Some(SlimNodeInventory::from_c2_snapshot(&snapshot));
        }
    }
    refreshed.observed_at_unix_ms = unix_ms();
    refreshed
}

fn prune_pre_hello_events(
    hub: &OperatorHub,
    connection_id: u64,
    outbound: &mut mpsc::Receiver<QueuedFrame>,
    sender: &mpsc::Sender<QueuedFrame>,
    budget: &AtomicUsize,
    hello_status: &StatusResponse,
) -> bool {
    let sink = hub.sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if sink.as_ref().is_none_or(|current| current.connection_id != connection_id) {
        return false;
    }
    let mut retained = Vec::new();
    while let Ok(queued) = outbound.try_recv() {
        let keep = match &queued.frame {
            C2ServerFrame::Event(event) => hello_status
                .nodes
                .get(&event.node_id)
                .and_then(|node| node.cursor)
                .is_none_or(|baseline| {
                    baseline.incarnation_id != event.cursor.incarnation_id
                        || event.cursor.sequence > baseline.sequence
                }),
            _ => true,
        };
        if keep {
            retained.push(queued);
        } else {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
        }
    }
    for queued in retained {
        if sender.try_send(queued).is_err() {
            return false;
        }
    }
    true
}

async fn read_client_frame(
    pipe: &mut NamedPipeServer,
    limit: usize,
) -> Result<C2ClientFrame, FrameError> {
    read_json_frame_limited_body_timeout(pipe, limit, FRAME_BODY_DEADLINE).await
}

async fn control_reader<R>(mut reader: R, frames: mpsc::Sender<Result<C2ClientFrame, FrameError>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame = read_json_frame_limited_body_timeout(
            &mut reader,
            MAX_C2_CLIENT_FRAME_BYTES,
            FRAME_BODY_DEADLINE,
        ).await;
        let terminal = frame.is_err();
        if frames.send(frame).await.is_err() || terminal { return; }
    }
}

pub(super) struct QueuedFrame { frame: C2ServerFrame, bytes: usize }

async fn control_writer<W>(
    mut writer: W,
    mut frames: mpsc::Receiver<QueuedFrame>,
    budget: Arc<AtomicUsize>,
    disconnect: watch::Sender<bool>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(queued) = frames.recv().await {
        let result = timeout(FRAME_BODY_DEADLINE, write_json_frame_limited(
            &mut writer,
            &queued.frame,
            MAX_C2_SERVER_FRAME_BYTES,
        )).await;
        budget.fetch_sub(queued.bytes, Ordering::AcqRel);
        if !matches!(result, Ok(Ok(()))) { break; }
    }
    let _ = disconnect.send(true);
}

fn reserve_budget(budget: &AtomicUsize, bytes: usize) -> bool {
    budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(bytes).filter(|next| *next <= MAX_OUTBOUND_BYTES)
    }).is_ok()
}

fn queued(frame: C2ServerFrame, budget: &AtomicUsize) -> Result<QueuedFrame, ()> {
    let bytes = serde_json::to_vec(&frame).map_err(|_| ())?.len();
    if bytes > MAX_C2_SERVER_FRAME_BYTES || !reserve_budget(budget, bytes) { return Err(()); }
    Ok(QueuedFrame { frame, bytes })
}

async fn queue_reply(
    sender: &mpsc::Sender<QueuedFrame>,
    budget: &AtomicUsize,
    reply: C2ReplyEnvelope,
) -> Result<(), ()> {
    let queued = queued(C2ServerFrame::Reply(reply), budget)?;
    let bytes = queued.bytes;
    match timeout(REPLY_QUEUE_DEADLINE, sender.send(queued)).await {
        Ok(Ok(())) => Ok(()),
        _ => { budget.fetch_sub(bytes, Ordering::AcqRel); Err(()) }
    }
}

fn queue_event(
    sender: &mpsc::Sender<QueuedFrame>,
    budget: &AtomicUsize,
    frame: C2ServerFrame,
) -> Result<(), ()> {
    let queued = queued(frame, budget)?;
    let bytes = queued.bytes;
    match sender.try_send(queued) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Closed(_)) => {
            budget.fetch_sub(bytes, Ordering::AcqRel);
            Err(())
        }
    }
}

pub(super) fn queue_operator_event(
    sender: &mpsc::Sender<QueuedFrame>,
    budget: &AtomicUsize,
    event: RoutedNodeEvent,
) -> Result<(), ()> {
    queue_event(sender, budget, C2ServerFrame::Event(event))
}

fn queue_topology_if_changed(
    sender: &mpsc::Sender<QueuedFrame>,
    budget: &AtomicUsize,
    previous: &mut C2Topology,
    next: C2Topology,
) -> Result<(), ()> {
    if *previous == next { return Ok(()); }
    queue_event(sender, budget, C2ServerFrame::Topology(next.clone()))?;
    *previous = next;
    Ok(())
}

enum DispatchStart {
    Immediate(RelayResult),
    Pending(oneshot::Receiver<RelayResult>),
}

fn dispatch_start(
    operator_connection_id: u64,
    request: crate::protocol::RoutedNodeRequest,
    relays: &BTreeMap<NodeId, RelayEndpoint>,
    status: &watch::Receiver<Arc<StatusResponse>>,
) -> DispatchStart {
    if matches!(request.request, NodeRequest::AcquireController { .. } | NodeRequest::ReleaseController | NodeRequest::Shutdown) {
        return DispatchStart::Immediate(Err(relay_failure(C2RelayFailureCode::RequestForbidden, "C2 owns node controller leases and node lifecycle", None)));
    }
    let Some(relay) = relays.get(&request.route.node_id).cloned() else {
        return DispatchStart::Immediate(Err(relay_failure(C2RelayFailureCode::UnknownNode, "node is not configured in C2", None)));
    };
    let observed = status.borrow().nodes.get(&request.route.node_id).cloned();
    if observed.as_ref().map_or(true, |node| node.transport != NodeTransportState::Online) {
        let incarnation = observed.and_then(|node| node.cursor.map(|cursor| cursor.incarnation_id));
        return DispatchStart::Immediate(Err(relay_failure(C2RelayFailureCode::NodeOffline, "node relay is offline", incarnation)));
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let command = RelayCommand::Request {
        operator_connection_id,
        expected_incarnation_id: request.route.expected_incarnation_id,
        request: request.request,
        reply: reply_tx,
    };
    match relay.commands.try_send(command) {
        Ok(()) => DispatchStart::Pending(reply_rx),
        Err(TrySendError::Full(_)) => DispatchStart::Immediate(Err(relay_failure(C2RelayFailureCode::RelayBusy, "node relay command queue is full", None))),
        Err(TrySendError::Closed(_)) => DispatchStart::Immediate(Err(relay_failure(C2RelayFailureCode::NodeOffline, "node relay is unavailable", None))),
    }
}

async fn release_all_controllers(relays: &BTreeMap<NodeId, RelayEndpoint>) {
    let all_relays = relays.values().cloned().collect::<Vec<_>>();
    let mut sends = JoinSet::new();
    for relay in relays.values() {
        let relay = relay.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        sends.spawn(async move {
            let sent = timeout(Duration::from_secs(1), relay.releases.send(reply_tx)).await;
            (relay, sent, reply_rx)
        });
    }
    let sends = timeout(Duration::from_secs(2), async {
        let mut completed = Vec::new();
        while let Some(result) = sends.join_next().await {
            if let Ok(result) = result { completed.push(result); }
        }
        completed
    }).await;
    let Ok(sends) = sends else {
        for relay in &all_relays { force_relay_disconnect(relay); }
        return;
    };
    let mut acknowledgements = JoinSet::new();
    for (relay, sent, reply) in sends {
        if matches!(sent, Ok(Ok(()))) {
            acknowledgements.spawn(async move { (relay, reply.await) });
        } else {
            force_relay_disconnect(&relay);
        }
    }
    if timeout(Duration::from_secs(3), async {
        while let Some(result) = acknowledgements.join_next().await {
            if let Ok((relay, Err(_))) = result { force_relay_disconnect(&relay); }
        }
    }).await.is_err() {
        for relay in &all_relays { force_relay_disconnect(relay); }
    }
}

fn force_relay_disconnect(relay: &RelayEndpoint) {
    relay.force_disconnect.send_modify(|generation| *generation = generation.wrapping_add(1));
}

fn c2_proof(
    token: &str,
    direction: C2AuthDirection,
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
) -> Result<[u8; 32], String> {
    local_hmac_sha256(token.as_bytes(), &c2_auth_transcript(direction, client_nonce, server_nonce))
}

fn authentication_frame_error(message: String) -> FrameError {
    FrameError::Io(io::Error::new(io::ErrorKind::Other, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{C2_API_VERSION, NodeFreshness, ObservedNode};

    fn status(transport: NodeTransportState, incarnation_id: Option<NodeIncarnationId>) -> StatusResponse {
        let node_id = NodeId::new("node-a").unwrap();
        let cursor = incarnation_id.map(|incarnation_id| NodeCursor { incarnation_id, sequence: 7 });
        StatusResponse {
            api_version: C2_API_VERSION,
            ready: true,
            observed_at_unix_ms: 1,
            nodes: BTreeMap::from([(node_id, ObservedNode {
                endpoint: r"\\.\pipe\node-a".to_owned(),
                transport_label: "windows-named-pipe".to_owned(),
                transport,
                freshness: NodeFreshness::Unavailable,
                cursor,
                inventory: None,
                last_attempt_unix_ms: None,
                last_success_unix_ms: None,
                consecutive_failures: 0,
                last_error: None,
                gaps: Vec::new(),
                gaps_truncated: 0,
            })]),
        }
    }

    #[test]
    fn offline_node_recovery_enqueues_one_topology_update() {
        let incarnation_id = NodeIncarnationId::from_bytes([4; 16]);
        let mut previous = C2Topology::from_status(&status(NodeTransportState::Offline, None));
        let mut recovered_status = status(
            NodeTransportState::Online,
            Some(incarnation_id),
        );
        let recovered = C2Topology::from_status(&recovered_status);
        let (sender, mut receiver) = mpsc::channel(2);
        let budget = AtomicUsize::new(0);

        queue_topology_if_changed(&sender, &budget, &mut previous, recovered.clone()).unwrap();
        let queued = receiver.try_recv().unwrap();
        assert_eq!(queued.frame, C2ServerFrame::Topology(recovered.clone()));
        budget.fetch_sub(queued.bytes, Ordering::AcqRel);
        assert_eq!(previous, recovered);

        recovered_status.observed_at_unix_ms += 1;
        queue_topology_if_changed(
            &sender,
            &budget,
            &mut previous,
            C2Topology::from_status(&recovered_status),
        ).unwrap();
        assert!(receiver.try_recv().is_err());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }
}
