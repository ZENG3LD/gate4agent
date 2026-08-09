use super::*;
use crate::protocol::{
    c2_auth_transcript, c2_bound_auth_transcript, provider_id_is_legacy, AgentId, ArchitectureId, C2AuthDirection, C2ClientAuthentication,
    C2ClientFrame, C2ControlCompatibilitySupport, C2Hello, C2ReplyEnvelope,
    C2ServerChallenge, C2ServerFrame, C2Topology, CapabilityId, ClientCompatibilityOffer,
    HostDescriptor, NegotiatedC2ControlCompatibility,
    OperatingSystemId, PathEncoding, PathSemantics, PathStyle, ProtocolRange,
    C2_COMPATIBILITY_METADATA_CAPABILITY, C2_CONTROL_PROTOCOL_VERSION,
    C2_OPAQUE_UNIX_PATH_CAPABILITY, C2_REPOSITORY_PATH_CAPABILITY,
    C2_PROVIDER_ID_OPEN_CAPABILITY,
    C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, C2_PROVIDER_RUNTIME_STATUS_CAPABILITY,
    C2_WORKSPACE_FILE_READ_CAPABILITY,
    MAX_C2_AUTH_FRAME_BYTES, MAX_C2_CLIENT_FRAME_BYTES, MAX_C2_HELLO_FRAME_BYTES,
    MAX_C2_SERVER_FRAME_BYTES,
};
use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, FrameError,
};
use gate4agent_node_wire::{
    local_hmac_sha256, proofs_match, random_nonce, LocalServerStream,
    OwnerOnlyLocalListener,
};
use std::sync::atomic::AtomicUsize;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::OwnedSemaphorePermit;

const MAX_PREAUTH_CONNECTIONS: usize = 4;
const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const FRAME_BODY_DEADLINE: Duration = Duration::from_secs(5);
const MAX_OUTBOUND_FRAMES: usize = 128;
const MAX_OUTBOUND_BYTES: usize = 16 * 1024 * 1024;
const MAX_INBOUND_FRAMES: usize = 4;
const REPLY_QUEUE_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
struct NegotiatedPathCapabilities {
    opaque_host_paths: bool,
    repository_paths: bool,
    workspace_file_read: bool,
    provider_runtime_status: bool,
    provider_ids_open: bool,
}

pub(super) async fn run(
    endpoint: String,
    token: String,
    relays: Arc<BTreeMap<NodeId, RelayEndpoint>>,
    status: watch::Receiver<Arc<StatusResponse>>,
    hub: OperatorHub,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut listener = OwnerOnlyLocalListener::bind(&endpoint).await?;
    let preauth = Arc::new(Semaphore::new(MAX_PREAUTH_CONNECTIONS));
    let authenticated = Arc::new(Semaphore::new(1));
    let next_connection_id = Arc::new(AtomicU64::new(1));
    let mut connections = JoinSet::new();
    loop {
        let permit = tokio::select! {
            permit = Arc::clone(&preauth).acquire_owned() => permit.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "C2 preauth slots closed"))?,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
                continue;
            }
        };
        let server = tokio::select! {
            accepted = listener.accept() => accepted?,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
                continue;
            }
        };
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

async fn serve_connection(
    mut pipe: LocalServerStream,
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
    let negotiated = c2_control_compatibility_support()?
        .negotiate(&hello)
        .map_err(|error| authentication_frame_error(error.to_string()))?;
    let compatibility = hello.compatibility.as_ref().map(|_| negotiated);
    let include_provider_contracts =
        provider_contract_manifest_selected(compatibility.as_ref());
    let path_capabilities = negotiated_path_capabilities(compatibility.as_ref());
    let include_provider_runtime_status = path_capabilities.provider_runtime_status;
    let server_nonce = random_nonce().map_err(authentication_frame_error)?;
    let auth_compatibility = hello.compatibility.as_ref().zip(compatibility.as_ref());
    let server_proof = c2_proof(
        token,
        C2AuthDirection::Server,
        &hello.client_nonce,
        &server_nonce,
        auth_compatibility,
    )
        .map_err(authentication_frame_error)?;
    timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ServerFrame::Challenge(C2ServerChallenge {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
            compatibility: compatibility.clone(),
        }),
        MAX_C2_AUTH_FRAME_BYTES,
    )).await.map_err(|_| FrameError::BodyTimedOut { length: 0 })??;
    let authentication = timeout(AUTH_DEADLINE, read_client_frame(&mut pipe, MAX_C2_AUTH_FRAME_BYTES))
        .await.map_err(|_| FrameError::PrefixTimedOut)??;
    let C2ClientFrame::Authenticate(C2ClientAuthentication { client_proof }) = authentication else { return Ok(()); };
    let expected = c2_proof(
        token,
        C2AuthDirection::Client,
        &hello.client_nonce,
        &server_nonce,
        auth_compatibility,
    )
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
    let mut hello_status = refresh_hello_status(connection_id, &relays, &status).await;
    if !include_provider_contracts {
        clear_provider_contract_manifests(&mut hello_status);
    }
    if !include_provider_runtime_status {
        clear_provider_runtime_statuses(&mut hello_status);
    }
    if !path_capabilities.provider_ids_open {
        retain_legacy_provider_status(&mut hello_status);
    }
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
    let mut last_topology = C2Topology::from_status_with_capabilities(
        &hello_status,
        include_provider_contracts,
        include_provider_runtime_status,
    );
    if timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ServerFrame::Hello(C2Hello {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            connection_id,
            status: hello_status,
            compatibility,
        }),
        MAX_C2_HELLO_FRAME_BYTES,
    )).await.map_err(|_| FrameError::BodyTimedOut { length: 0 }).and_then(|result| result).is_err() {
        hub.detach(connection_id);
        return Ok(());
    }
    let (reader, writer) = tokio::io::split(pipe);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(MAX_INBOUND_FRAMES);
    let reader_task = tokio::spawn(control_reader(reader, incoming_tx));
    let writer_task = tokio::spawn(control_writer(
        writer,
        outbound_rx,
        Arc::clone(&budget),
        disconnect_tx.clone(),
        path_capabilities,
        status.clone(),
    ));
    let mut dispatches = JoinSet::new();
    let mut last_request_id = 0_u64;

    loop {
        tokio::select! {
            incoming = incoming_rx.recv() => {
                let Some(Ok(C2ClientFrame::Request(request))) = incoming else { break; };
                if !path_capabilities.opaque_host_paths
                    && node_request_contains_opaque_unix_path(&request.request.request)
                {
                    break;
                }
                if request.request_id.0 == 0 || request.request_id.0 <= last_request_id {
                    let failure = relay_failure(C2RelayFailureCode::RequestIdReused, "C2 request IDs must be nonzero and strictly increasing", None);
                    if queue_reply(&outbound_tx, &budget, C2ReplyEnvelope { request_id: request.request_id, result: Err(failure) }).await.is_err() { break; }
                    continue;
                }
                last_request_id = request.request_id.0;
                if let Some(failure) = unnegotiated_request_failure(
                    &request.request.request,
                    path_capabilities,
                ) {
                    if queue_reply(&outbound_tx, &budget, C2ReplyEnvelope {
                        request_id: request.request_id,
                        result: Err(failure),
                    }).await.is_err() { break; }
                    continue;
                }
                if !path_capabilities.provider_ids_open
                    && request_targets_unavailable_provider(&request.request, status.borrow().as_ref())
                {
                    let failure = relay_failure(
                        C2RelayFailureCode::RequestForbidden,
                        "provider identity capability was not negotiated with C2",
                        None,
                    );
                    if queue_reply(&outbound_tx, &budget, C2ReplyEnvelope {
                        request_id: request.request_id,
                        result: Err(failure),
                    }).await.is_err() { break; }
                    continue;
                }
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
                    let mut projected = latest.as_ref().clone();
                    if !path_capabilities.provider_ids_open {
                        retain_legacy_provider_status(&mut projected);
                    }
                    C2Topology::from_status_with_capabilities(
                        &projected,
                        include_provider_contracts,
                        include_provider_runtime_status,
                    )
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
            let provider_contract_manifest = provider_contract_manifest_for_snapshot(
                observed,
                response.incarnation_id,
            );
            observed.cursor = Some(NodeCursor { incarnation_id: response.incarnation_id, sequence: event_sequence });
            let mut inventory = SlimNodeInventory::from_c2_snapshot(&snapshot);
            inventory.provider_contracts = provider_contract_manifest.provider_contracts;
            inventory.provider_adapter_contracts =
                provider_contract_manifest.provider_adapter_contracts;
            observed.inventory = Some(inventory);
        }
    }
    refreshed.observed_at_unix_ms = unix_ms();
    refreshed
}

fn provider_contract_manifest_for_snapshot(
    observed: &ObservedNode,
    response_incarnation_id: NodeIncarnationId,
) -> ProviderContractManifest {
    if !observed.cursor.is_some_and(|cursor| {
        cursor.incarnation_id == response_incarnation_id
    }) {
        return ProviderContractManifest::default();
    }
    observed.inventory.as_ref().map(|inventory| ProviderContractManifest {
        provider_contracts: inventory.provider_contracts.clone(),
        provider_adapter_contracts: inventory.provider_adapter_contracts.clone(),
    }).unwrap_or_default()
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
    pipe: &mut LocalServerStream,
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
    path_capabilities: NegotiatedPathCapabilities,
    status: watch::Receiver<Arc<StatusResponse>>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(queued) = frames.recv().await {
        let mut frame = queued.frame;
        if !path_capabilities.provider_runtime_status {
            clear_server_frame_provider_runtime_status(&mut frame);
        }
        if !path_capabilities.provider_ids_open {
            match project_server_frame_for_legacy_provider_ids(
                &mut frame,
                status.borrow().as_ref(),
            ) {
                LegacyFrameProjection::Write => {}
                LegacyFrameProjection::Drop => {
                    budget.fetch_sub(queued.bytes, Ordering::AcqRel);
                    continue;
                }
                LegacyFrameProjection::Reject => {
                    budget.fetch_sub(queued.bytes, Ordering::AcqRel);
                    break;
                }
            }
        }
        if (!path_capabilities.opaque_host_paths
            && server_frame_contains_opaque_unix_path(&frame))
            || (!path_capabilities.repository_paths
                && server_frame_contains_unix_repository_path(&frame))
            || (!path_capabilities.workspace_file_read
                && server_frame_contains_workspace_file_read(&frame))
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        let result = timeout(FRAME_BODY_DEADLINE, write_json_frame_limited(
            &mut writer,
            &frame,
            MAX_C2_SERVER_FRAME_BYTES,
        )).await;
        budget.fetch_sub(queued.bytes, Ordering::AcqRel);
        if !matches!(result, Ok(Ok(()))) { break; }
    }
    let _ = disconnect.send(true);
}

fn negotiated_path_capabilities(
    selected: Option<&NegotiatedC2ControlCompatibility>,
) -> NegotiatedPathCapabilities {
    let selected_has = |expected| {
        selected.is_some_and(|selected| {
            selected.capabilities.iter().any(|capability| {
                capability.as_str() == expected
            })
        })
    };
    NegotiatedPathCapabilities {
        opaque_host_paths: selected_has(C2_OPAQUE_UNIX_PATH_CAPABILITY),
        repository_paths: selected_has(C2_REPOSITORY_PATH_CAPABILITY),
        workspace_file_read: selected_has(C2_WORKSPACE_FILE_READ_CAPABILITY),
        provider_runtime_status: selected_has(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY),
        provider_ids_open: selected_has(C2_PROVIDER_ID_OPEN_CAPABILITY),
    }
}

fn unnegotiated_request_failure(
    request: &NodeRequest,
    capabilities: NegotiatedPathCapabilities,
) -> Option<C2RelayFailure> {
    let required_capability_available = match request.required_capability() {
        None => true,
        Some(C2_WORKSPACE_FILE_READ_CAPABILITY) => capabilities.workspace_file_read,
        Some(_) => false,
    };
    if !required_capability_available {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "request capability was not negotiated with C2",
            None,
        ));
    }
    if !capabilities.provider_ids_open
        && matches!(request, NodeRequest::Spawn { provider, .. } if !provider_id_is_legacy(provider))
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "open provider IDs require negotiated C2 capability",
            None,
        ));
    }
    if !capabilities.repository_paths
        && matches!(request, NodeRequest::ReadWorkspaceFile { path, .. }
            if path.as_unix_bytes().is_some())
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "tagged repository paths require negotiated C2 capability",
            None,
        ));
    }
    None
}

fn node_request_contains_opaque_unix_path(request: &NodeRequest) -> bool {
    match request {
        NodeRequest::RegisterWorkspace { root, .. } => root.as_unix_bytes().is_some(),
        NodeRequest::CreateWorktree { target_root, .. }
        | NodeRequest::RemoveWorktree { target_root, .. } => target_root.as_unix_bytes().is_some(),
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::Spawn { .. }
        | NodeRequest::Resume { .. }
        | NodeRequest::RenameSessionRecord { .. }
        | NodeRequest::ResumeSessionRecord { .. }
        | NodeRequest::ForgetSessionRecord { .. }
        | NodeRequest::Prompt { .. }
        | NodeRequest::Paste { .. }
        | NodeRequest::Input { .. }
        | NodeRequest::TerminalBytes { .. }
        | NodeRequest::TerminalControl { .. }
        | NodeRequest::Resize { .. }
        | NodeRequest::Interrupt { .. }
        | NodeRequest::Stop { .. }
        | NodeRequest::Remove { .. }
        | NodeRequest::Shutdown => false,
    }
}

fn server_frame_contains_opaque_unix_path(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(c2_response_contains_opaque_unix_path)
        }),
        C2ServerFrame::Event(event) => c2_event_contains_opaque_unix_path(&event.event),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn server_frame_contains_unix_repository_path(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(|response| {
                match response {
                    C2NodeResponse::WorkspaceInspected { inspection } => {
                        inspection.entries.iter().any(|entry| {
                            entry.relative_path.as_unix_bytes().is_some()
                        }) || inspection.git.status.iter().any(|entry| {
                            entry.path.as_unix_bytes().is_some()
                                || entry.previous_path.as_ref().is_some_and(|path| {
                                    path.as_unix_bytes().is_some()
                                })
                        })
                    }
                    C2NodeResponse::WorkspaceFileRead { file } => {
                        file.path.as_unix_bytes().is_some()
                    }
                    C2NodeResponse::Snapshot { .. }
                    | C2NodeResponse::Resync { .. }
                    | C2NodeResponse::Controller { .. }
                    | C2NodeResponse::SpawnAccepted { .. }
                    | C2NodeResponse::SessionRecordUpdated { .. }
                    | C2NodeResponse::SessionRecordResumed { .. }
                    | C2NodeResponse::SessionRecordForgotten { .. }
                    | C2NodeResponse::WorkspaceRegistered { .. }
                    | C2NodeResponse::WorkspaceUnregistered { .. }
                    | C2NodeResponse::WorktreeCreated { .. }
                    | C2NodeResponse::WorktreeRemoved { .. }
                    | C2NodeResponse::Accepted
                    | C2NodeResponse::ShuttingDown => false,
                }
            })
        }),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Event(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn server_frame_contains_workspace_file_read(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(|response| {
                matches!(response, C2NodeResponse::WorkspaceFileRead { .. })
            })
        }),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Event(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn c2_response_contains_opaque_unix_path(response: &C2NodeResponse) -> bool {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => c2_snapshot_contains_opaque_unix_path(snapshot),
        C2NodeResponse::Resync { snapshot, events, .. } => {
            c2_snapshot_contains_opaque_unix_path(snapshot)
                || events.iter().any(|event| c2_event_contains_opaque_unix_path(&event.event))
        }
        C2NodeResponse::WorkspaceInspected { inspection } => inspection.git.worktrees
            .iter()
            .any(|worktree| worktree.path.as_unix_bytes().is_some()),
        C2NodeResponse::WorkspaceFileRead { .. } => false,
        C2NodeResponse::WorkspaceRegistered { workspace } => {
            workspace.canonical_root.as_unix_bytes().is_some()
        }
        C2NodeResponse::WorktreeCreated { worktree, workspace } => {
            worktree.path.as_unix_bytes().is_some()
                || workspace.canonical_root.as_unix_bytes().is_some()
        }
        C2NodeResponse::WorktreeRemoved { target_root, .. } => target_root.as_unix_bytes().is_some(),
        C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::SessionRecordUpdated { .. }
        | C2NodeResponse::SessionRecordResumed { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::WorkspaceUnregistered { .. }
        | C2NodeResponse::Accepted
        | C2NodeResponse::ShuttingDown => false,
    }
}

fn c2_snapshot_contains_opaque_unix_path(snapshot: &crate::protocol::C2NodeSnapshot) -> bool {
    snapshot.workspaces.iter().any(|workspace| workspace.canonical_root.as_unix_bytes().is_some())
}

fn c2_event_contains_opaque_unix_path(event: &C2NodeEvent) -> bool {
    match event {
        C2NodeEvent::WorkspaceAdded { workspace } => workspace.canonical_root.as_unix_bytes().is_some(),
        C2NodeEvent::Control { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::SessionRecordUpserted { .. }
        | C2NodeEvent::SessionRecordRemoved { .. }
        | C2NodeEvent::ResyncRequired { .. } => false,
    }
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
    compatibility: Option<(&ClientCompatibilityOffer, &NegotiatedC2ControlCompatibility)>,
) -> Result<[u8; 32], String> {
    let transcript = match compatibility {
        Some((offer, selected)) => c2_bound_auth_transcript(
            direction,
            client_nonce,
            server_nonce,
            offer,
            selected,
        ).map_err(|error| error.to_string())?,
        None => c2_auth_transcript(direction, client_nonce, server_nonce),
    };
    local_hmac_sha256(token.as_bytes(), &transcript)
}

fn authentication_frame_error(message: String) -> FrameError {
    FrameError::Io(io::Error::new(io::ErrorKind::Other, message))
}

fn c2_control_compatibility_support() -> Result<C2ControlCompatibilitySupport, FrameError> {
    Ok(C2ControlCompatibilitySupport {
        protocol_versions: ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION)
            .map_err(|error| authentication_frame_error(error.to_string()))?,
        capabilities: vec![
            CapabilityId::new(C2_COMPATIBILITY_METADATA_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_OPAQUE_UNIX_PATH_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_REPOSITORY_PATH_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_WORKSPACE_FILE_READ_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_ID_OPEN_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
        ],
        host: HostDescriptor {
            operating_system: OperatingSystemId::new(std::env::consts::OS)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            architecture: ArchitectureId::new(std::env::consts::ARCH)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
        },
        path_semantics: local_path_semantics(),
    })
}

#[cfg(windows)]
fn local_path_semantics() -> PathSemantics {
    PathSemantics { style: PathStyle::Windows, encoding: PathEncoding::Utf8 }
}

#[cfg(unix)]
fn local_path_semantics() -> PathSemantics {
    PathSemantics { style: PathStyle::Posix, encoding: PathEncoding::UnixBytes }
}

fn provider_contract_manifest_selected(
    compatibility: Option<&NegotiatedC2ControlCompatibility>,
) -> bool {
    compatibility.is_some_and(|compatibility| {
        compatibility.capabilities.iter().any(|capability| {
            capability.as_str() == C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY
        })
    })
}

fn provider_text_is_legacy(value: &str) -> bool {
    AgentId::new(value).is_ok_and(|provider| provider_id_is_legacy(&provider))
}

fn retain_legacy_provider_status(status: &mut StatusResponse) {
    for inventory in status.nodes.values_mut().filter_map(|node| node.inventory.as_mut()) {
        inventory.enabled_providers.retain(provider_id_is_legacy);
        inventory.provider_runtime_statuses
            .retain(|runtime| provider_id_is_legacy(runtime.provider()));
        inventory.provider_contracts
            .retain(|contract| provider_id_is_legacy(&contract.provider));
        inventory.provider_adapter_contracts
            .retain(|contract| provider_id_is_legacy(&contract.provider));
        for workspace in inventory.workspaces.values_mut() {
            workspace.sessions.retain(|session| provider_text_is_legacy(&session.agent_id));
            workspace.session_count = workspace.sessions.len();
            workspace.sessions_truncated = false;
        }
        inventory.session_count = inventory.workspaces.values()
            .map(|workspace| workspace.sessions.len())
            .sum();
        inventory.sessions_truncated = false;
        inventory.managed_sessions
            .retain(|record| provider_id_is_legacy(&record.provider));
        inventory.managed_session_count = inventory.managed_sessions.len();
        inventory.managed_sessions_truncated = false;
    }
}

fn request_targets_unavailable_provider(
    request: &crate::protocol::RoutedNodeRequest,
    status: &StatusResponse,
) -> bool {
    let node_id = &request.route.node_id;
    match &request.request {
        NodeRequest::Resume { session, .. }
        | NodeRequest::Prompt { session, .. }
        | NodeRequest::Paste { session, .. }
        | NodeRequest::Input { session, .. }
        | NodeRequest::TerminalBytes { session, .. }
        | NodeRequest::TerminalControl { session, .. }
        | NodeRequest::Resize { session, .. }
        | NodeRequest::Interrupt { session }
        | NodeRequest::Stop { session, .. }
        | NodeRequest::Remove { session } => {
            !status_address_is_legacy(status, node_id, session)
        }
        NodeRequest::RenameSessionRecord { record_id, .. }
        | NodeRequest::ResumeSessionRecord { record_id, .. }
        | NodeRequest::ForgetSessionRecord { record_id } => {
            !status_record_is_legacy(status, node_id, record_id)
        }
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::Spawn { .. }
        | NodeRequest::Shutdown => false,
    }
}

fn status_address_is_legacy(
    status: &StatusResponse,
    node_id: &NodeId,
    address: &gate4agent_node_protocol::SessionAddress,
) -> bool {
    let Some(inventory) = status.nodes.get(node_id).and_then(|node| node.inventory.as_ref()) else {
        return false;
    };
    let mut matched = false;
    let mut all_legacy = true;
    if let Some(workspace) = inventory.workspaces.get(&address.workspace_id) {
        for session in workspace.sessions.iter().filter(|session| {
            session.instance_id == address.session.instance_id
                && session.generation == address.session.generation
        }) {
            matched = true;
            all_legacy &= provider_text_is_legacy(&session.agent_id);
        }
    }
    for record in inventory.managed_sessions.iter()
        .filter(|record| record.active_session.as_ref() == Some(address))
    {
        matched = true;
        all_legacy &= provider_id_is_legacy(&record.provider);
    }
    matched && all_legacy
}

fn status_record_is_legacy(
    status: &StatusResponse,
    node_id: &NodeId,
    record_id: &gate4agent_node_protocol::SessionRecordId,
) -> bool {
    status.nodes.get(node_id)
        .and_then(|node| node.inventory.as_ref())
        .and_then(|inventory| {
            inventory.managed_sessions.iter().find(|record| &record.record_id == record_id)
        })
        .is_some_and(|record| provider_id_is_legacy(&record.provider))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyFrameProjection {
    Write,
    Drop,
    Reject,
}

fn project_server_frame_for_legacy_provider_ids(
    frame: &mut C2ServerFrame,
    status: &StatusResponse,
) -> LegacyFrameProjection {
    match frame {
        C2ServerFrame::Hello(hello) => {
            retain_legacy_provider_status(&mut hello.status);
            LegacyFrameProjection::Write
        }
        C2ServerFrame::Topology(topology) => {
            retain_legacy_topology(topology);
            LegacyFrameProjection::Write
        }
        C2ServerFrame::Reply(reply) => {
            let Ok(routed) = reply.result.as_mut() else {
                return LegacyFrameProjection::Write;
            };
            let Ok(response) = routed.response.as_mut() else {
                return LegacyFrameProjection::Write;
            };
            if project_legacy_response(response, &routed.node_id, status) {
                LegacyFrameProjection::Write
            } else {
                LegacyFrameProjection::Reject
            }
        }
        C2ServerFrame::Event(event) => {
            let node_id = event.node_id.clone();
            if project_legacy_event(&mut event.event, &node_id, status, None) {
                LegacyFrameProjection::Write
            } else {
                LegacyFrameProjection::Drop
            }
        }
        C2ServerFrame::Challenge(_) | C2ServerFrame::Rejected(_) => {
            LegacyFrameProjection::Write
        }
    }
}

fn retain_legacy_topology(topology: &mut C2Topology) {
    for node in &mut topology.nodes {
        node.provider_contracts
            .retain(|contract| provider_id_is_legacy(&contract.provider));
        node.provider_adapter_contracts
            .retain(|contract| provider_id_is_legacy(&contract.provider));
        node.provider_runtime_statuses
            .retain(|runtime| provider_id_is_legacy(runtime.provider()));
    }
}

fn retain_legacy_workspace(workspace: &mut crate::protocol::C2WorkspaceSnapshot) {
    workspace.sessions.retain(|session| provider_id_is_legacy(&session.agent_id));
}

fn retain_legacy_snapshot(snapshot: &mut crate::protocol::C2NodeSnapshot) {
    snapshot.enabled_providers.retain(provider_id_is_legacy);
    snapshot.provider_runtime_statuses
        .retain(|runtime| provider_id_is_legacy(runtime.provider()));
    for workspace in &mut snapshot.workspaces {
        retain_legacy_workspace(workspace);
    }
    snapshot.session_records
        .retain(|record| provider_id_is_legacy(&record.provider));
}

fn project_legacy_response(
    response: &mut C2NodeResponse,
    node_id: &NodeId,
    status: &StatusResponse,
) -> bool {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => {
            retain_legacy_snapshot(snapshot);
            true
        }
        C2NodeResponse::Resync { snapshot, events, .. } => {
            let source_snapshot = snapshot.clone();
            events.retain_mut(|event| {
                project_legacy_event(
                    &mut event.event,
                    node_id,
                    status,
                    Some(&source_snapshot),
                )
            });
            retain_legacy_snapshot(snapshot);
            true
        }
        C2NodeResponse::SessionRecordUpdated { record }
        | C2NodeResponse::SessionRecordResumed { record, .. } => {
            provider_id_is_legacy(&record.provider)
        }
        C2NodeResponse::WorkspaceRegistered { workspace }
        | C2NodeResponse::WorktreeCreated { workspace, .. } => {
            retain_legacy_workspace(workspace);
            true
        }
        C2NodeResponse::WorkspaceInspected { .. }
        | C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::WorkspaceUnregistered { .. }
        | C2NodeResponse::WorktreeRemoved { .. }
        | C2NodeResponse::Accepted
        | C2NodeResponse::ShuttingDown => true,
    }
}

fn project_legacy_event(
    event: &mut C2NodeEvent,
    node_id: &NodeId,
    status: &StatusResponse,
    snapshot: Option<&crate::protocol::C2NodeSnapshot>,
) -> bool {
    match event {
        C2NodeEvent::Control { address, .. } => {
            address_is_legacy(status, node_id, snapshot, address)
        }
        C2NodeEvent::WorkspaceAdded { workspace } => {
            retain_legacy_workspace(workspace);
            true
        }
        C2NodeEvent::SessionRecordUpserted { record } => {
            provider_id_is_legacy(&record.provider)
        }
        C2NodeEvent::SessionRecordRemoved { record_id } => {
            record_is_legacy(status, node_id, snapshot, record_id)
        }
        C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::ResyncRequired { .. } => true,
    }
}

fn address_is_legacy(
    status: &StatusResponse,
    node_id: &NodeId,
    snapshot: Option<&crate::protocol::C2NodeSnapshot>,
    address: &gate4agent_node_protocol::SessionAddress,
) -> bool {
    let status_match = status_address_is_legacy(status, node_id, address);
    let snapshot_match = snapshot.is_some_and(|snapshot| {
        snapshot.workspaces.iter()
            .find(|workspace| workspace.workspace_id == address.workspace_id)
            .is_some_and(|workspace| {
                workspace.sessions.iter().any(|session| {
                    session.instance_id == address.session.instance_id
                        && session.generation == address.session.generation
                        && provider_id_is_legacy(&session.agent_id)
                })
            })
            || snapshot.session_records.iter().any(|record| {
                record.active_session.as_ref() == Some(address)
                    && provider_id_is_legacy(&record.provider)
            })
    });
    status_match || snapshot_match
}

fn record_is_legacy(
    status: &StatusResponse,
    node_id: &NodeId,
    snapshot: Option<&crate::protocol::C2NodeSnapshot>,
    record_id: &gate4agent_node_protocol::SessionRecordId,
) -> bool {
    status_record_is_legacy(status, node_id, record_id)
        || snapshot.is_some_and(|snapshot| {
            snapshot.session_records.iter().any(|record| {
                &record.record_id == record_id && provider_id_is_legacy(&record.provider)
            })
        })
}

fn clear_provider_contract_manifests(status: &mut StatusResponse) {
    for inventory in status.nodes.values_mut().filter_map(|node| node.inventory.as_mut()) {
        inventory.provider_contracts.clear();
        inventory.provider_adapter_contracts.clear();
    }
}

fn clear_provider_runtime_statuses(status: &mut StatusResponse) {
    for inventory in status.nodes.values_mut().filter_map(|node| node.inventory.as_mut()) {
        inventory.provider_runtime_statuses.clear();
    }
}

fn clear_server_frame_provider_runtime_status(frame: &mut C2ServerFrame) {
    let C2ServerFrame::Reply(reply) = frame else {
        return;
    };
    let Ok(routed) = reply.result.as_mut() else {
        return;
    };
    let Ok(response) = routed.response.as_mut() else {
        return;
    };
    match response {
        C2NodeResponse::Snapshot { snapshot, .. }
        | C2NodeResponse::Resync { snapshot, .. } => {
            snapshot.provider_runtime_statuses.clear();
        }
        C2NodeResponse::WorkspaceInspected { .. }
        | C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::SessionRecordUpdated { .. }
        | C2NodeResponse::SessionRecordResumed { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::WorkspaceRegistered { .. }
        | C2NodeResponse::WorkspaceUnregistered { .. }
        | C2NodeResponse::WorktreeCreated { .. }
        | C2NodeResponse::WorktreeRemoved { .. }
        | C2NodeResponse::Accepted
        | C2NodeResponse::ShuttingDown => {}
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::protocol::{
        AdapterContractRevision, AdapterFamily, AdapterId, C2ClientHello, C2GitSnapshot,
        C2WorkspaceInspection, C2_API_VERSION, NodeFreshness, ObservedNode, OpaqueHostPath,
        ProviderAdapterContractSupport, ProviderContractRevision, ProviderContractSupport,
        RepositoryPath, SlimManagedSessionRecord, WorkspaceFileContent, WorkspaceFileRead,
    };
    use gate4agent_node_protocol::{
        GitStatusEntry, ManagedSessionState, SessionAddress, SessionKey, SessionMode,
        SessionRecordId, WorkspaceEntry, WorkspaceEntryKind, WorkspaceId,
    };
    use gate4agent_types::{AgentInstanceId, SessionGeneration, TerminalSize};
    use tokio::io::AsyncReadExt;

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

    fn status_with_provider_contract_manifest(
        incarnation_id: NodeIncarnationId,
    ) -> StatusResponse {
        let mut value = status(NodeTransportState::Online, Some(incarnation_id));
        let observed = value.nodes.get_mut(&NodeId::new("node-a").unwrap()).unwrap();
        let mut inventory = SlimNodeInventory::from_snapshot(&NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![AgentId::new("codex").unwrap()],
            provider_runtime_statuses: crate::protocol::ProviderRuntimeStatuses::default(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
        });
        inventory.provider_contracts = vec![ProviderContractSupport {
            provider: AgentId::new("codex").unwrap(),
            revision: ProviderContractRevision::new("old-contract").unwrap(),
        }];
        inventory.provider_adapter_contracts = vec![ProviderAdapterContractSupport {
            provider: AgentId::new("codex").unwrap(),
            family: AdapterFamily::PtySemantic,
            adapter_id: AdapterId::new("codex").unwrap(),
            revision: AdapterContractRevision::new("old-adapter-contract").unwrap(),
        }];
        observed.inventory = Some(inventory);
        value
    }

    fn address(instance_id: u64) -> SessionAddress {
        SessionAddress {
            workspace_id: WorkspaceId::new("repo").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(instance_id),
                generation: SessionGeneration(1),
            },
        }
    }

    fn managed_record(
        record_id: &str,
        provider: &str,
        active_session: Option<SessionAddress>,
    ) -> SlimManagedSessionRecord {
        SlimManagedSessionRecord {
            record_id: SessionRecordId::new(record_id).unwrap(),
            display_name: record_id.to_owned(),
            display_name_truncated: false,
            provider: AgentId::new(provider).unwrap(),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Live,
            workspace_id: WorkspaceId::new("repo").unwrap(),
            active_session,
            provider_identity_present: false,
            updated_at_unix_ms: 1,
        }
    }

    #[test]
    fn c2_control_server_selects_authenticated_path_opt_ins() {
        let support = c2_control_compatibility_support().unwrap();

        assert_eq!(
            support.protocol_versions,
            ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION).unwrap(),
        );
        assert_eq!(
            support.capabilities,
            vec![
                CapabilityId::new(C2_COMPATIBILITY_METADATA_CAPABILITY).unwrap(),
                CapabilityId::new(C2_OPAQUE_UNIX_PATH_CAPABILITY).unwrap(),
                CapabilityId::new(C2_REPOSITORY_PATH_CAPABILITY).unwrap(),
                CapabilityId::new(C2_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_ID_OPEN_CAPABILITY).unwrap(),
            ],
        );
        assert_eq!(support.host.operating_system.as_str(), "windows");
        assert_eq!(support.host.architecture.as_str(), std::env::consts::ARCH);
        assert_eq!(support.path_semantics.style, PathStyle::Windows);
        assert_eq!(support.path_semantics.encoding, PathEncoding::Utf8);

        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION).unwrap(),
            capabilities: support.capabilities.clone(),
            state_schema: None,
        };
        let selected = support.negotiate(&C2ClientHello::negotiating(
            [7; crate::protocol::C2_AUTH_NONCE_BYTES],
            offer,
        )).unwrap();
        assert_eq!(selected.capabilities, support.capabilities);

        let legacy = support.negotiate(&C2ClientHello::new(
            [8; crate::protocol::C2_AUTH_NONCE_BYTES],
        )).unwrap();
        assert!(legacy.capabilities.is_empty());
    }

    #[test]
    fn legacy_provider_projection_preserves_legacy_bytes_and_filters_open_ids() {
        let mut status = status_with_provider_contract_manifest(
            NodeIncarnationId::from_bytes([11; 16]),
        );
        let inventory = status.nodes.values_mut().next().unwrap().inventory.as_mut().unwrap();
        inventory.enabled_providers = ["claude", "codex", "kimi", "qwen-code"]
            .into_iter()
            .map(|provider| AgentId::new(provider).unwrap())
            .collect();
        inventory.provider_runtime_statuses = crate::protocol::ProviderRuntimeStatuses::new(
            ["claude", "codex", "kimi", "qwen-code"].into_iter().map(|provider| {
                crate::protocol::ProviderRuntimeStatus::raw_passthrough(
                    AgentId::new(provider).unwrap(),
                    None,
                )
            }),
        ).unwrap();
        inventory.provider_contracts.push(ProviderContractSupport {
            provider: AgentId::new("qwen-code").unwrap(),
            revision: ProviderContractRevision::new("open-contract").unwrap(),
        });
        inventory.provider_adapter_contracts.push(ProviderAdapterContractSupport {
            provider: AgentId::new("qwen-code").unwrap(),
            family: AdapterFamily::PtySemantic,
            adapter_id: AdapterId::new("qwen-code").unwrap(),
            revision: AdapterContractRevision::new("open-adapter-contract").unwrap(),
        });
        inventory.managed_sessions = vec![
            managed_record("claude-record", "claude", None),
            managed_record("codex-record", "codex", None),
            managed_record("kimi-record", "kimi", None),
            managed_record("qwen-record", "qwen-code", None),
        ];
        inventory.managed_session_count = inventory.managed_sessions.len();

        retain_legacy_provider_status(&mut status);

        let inventory = status.nodes.values().next().unwrap().inventory.as_ref().unwrap();
        assert_eq!(
            inventory.enabled_providers.iter().map(AgentId::as_str).collect::<Vec<_>>(),
            vec!["claude", "codex", "kimi"],
        );
        assert_eq!(
            inventory.provider_runtime_statuses.iter()
                .map(|runtime| runtime.provider().as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "kimi"],
        );
        assert_eq!(
            inventory.managed_sessions.iter()
                .map(|record| record.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "kimi"],
        );
        let json = serde_json::to_string(inventory).unwrap();
        assert!(json.contains(r#""enabled_providers":["claude","codex","kimi"]"#));
        assert!(!json.contains("qwen-code"));
    }

    #[test]
    fn legacy_provider_gate_blocks_open_and_unknown_session_mutations_before_dispatch() {
        let incarnation = NodeIncarnationId::from_bytes([12; 16]);
        let mut status = status_with_provider_contract_manifest(incarnation);
        let inventory = status.nodes.values_mut().next().unwrap().inventory.as_mut().unwrap();
        inventory.managed_sessions = vec![
            managed_record("legacy-record", "codex", Some(address(1))),
            managed_record("open-record", "qwen-code", Some(address(2))),
        ];
        inventory.managed_session_count = inventory.managed_sessions.len();
        let routed = |request| crate::protocol::RoutedNodeRequest {
            route: crate::protocol::NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: incarnation,
            },
            request,
        };

        assert!(!request_targets_unavailable_provider(
            &routed(NodeRequest::Interrupt { session: address(1) }),
            &status,
        ));
        assert!(request_targets_unavailable_provider(
            &routed(NodeRequest::Interrupt { session: address(2) }),
            &status,
        ));
        assert!(request_targets_unavailable_provider(
            &routed(NodeRequest::Interrupt { session: address(3) }),
            &status,
        ));
        assert!(!request_targets_unavailable_provider(
            &routed(NodeRequest::ForgetSessionRecord {
                record_id: SessionRecordId::new("legacy-record").unwrap(),
            }),
            &status,
        ));
        assert!(request_targets_unavailable_provider(
            &routed(NodeRequest::ForgetSessionRecord {
                record_id: SessionRecordId::new("open-record").unwrap(),
            }),
            &status,
        ));
        assert!(request_targets_unavailable_provider(
            &routed(NodeRequest::ForgetSessionRecord {
                record_id: SessionRecordId::new("unknown-record").unwrap(),
            }),
            &status,
        ));
    }

    #[test]
    fn legacy_provider_gate_blocks_open_spawn_before_dispatch() {
        let request = NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("repo").unwrap(),
            provider: AgentId::new("qwen-code").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 40, columns: 120 },
            initial_prompt: None,
        };
        let capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            provider_runtime_status: true,
            provider_ids_open: false,
        };
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure { code: C2RelayFailureCode::RequestForbidden, .. })
        ));
    }

    #[test]
    fn cross_incarnation_snapshot_never_inherits_stale_provider_contract_manifest() {
        let old_incarnation = NodeIncarnationId::from_bytes([3; 16]);
        let new_incarnation = NodeIncarnationId::from_bytes([4; 16]);
        let observed = status_with_provider_contract_manifest(old_incarnation)
            .nodes
            .remove(&NodeId::new("node-a").unwrap())
            .unwrap();

        let same = provider_contract_manifest_for_snapshot(&observed, old_incarnation);
        assert_eq!(same.provider_contracts.len(), 1);
        assert_eq!(same.provider_adapter_contracts.len(), 1);
        let restarted = provider_contract_manifest_for_snapshot(&observed, new_incarnation);
        assert!(restarted.provider_contracts.is_empty());
        assert!(restarted.provider_adapter_contracts.is_empty());
    }

    #[test]
    fn legacy_c2_client_receives_no_runtime_status() {
        let incarnation = NodeIncarnationId::from_bytes([6; 16]);
        let mut status = status_with_provider_contract_manifest(incarnation);
        status
            .nodes
            .values_mut()
            .next()
            .unwrap()
            .inventory
            .as_mut()
            .unwrap()
            .provider_runtime_statuses = crate::protocol::ProviderRuntimeStatuses::new([
                crate::protocol::ProviderRuntimeStatus::raw_passthrough(
                    AgentId::new("codex").unwrap(),
                    Some(crate::protocol::ProviderRuntimeVersion::new("0.147.0").unwrap()),
                ),
            ])
            .unwrap();

        clear_provider_runtime_statuses(&mut status);
        assert!(status
            .nodes
            .values()
            .next()
            .unwrap()
            .inventory
            .as_ref()
            .unwrap()
            .provider_runtime_statuses
            .is_empty());
        let topology = C2Topology::from_status_with_capabilities(&status, false, false);
        assert!(topology.nodes[0].provider_runtime_statuses.is_empty());
        assert!(!serde_json::to_string(&topology)
            .unwrap()
            .contains("provider_runtime_statuses"));
    }

    #[test]
    fn n_minus_one_control_hello_keeps_exact_legacy_empty_manifest_shape() {
        #[derive(serde::Serialize)]
        struct LegacyHello<'a> {
            protocol_version: u16,
            connection_id: u64,
            status: &'a StatusResponse,
        }

        let mut status = status_with_provider_contract_manifest(
            NodeIncarnationId::from_bytes([5; 16]),
        );
        clear_provider_contract_manifests(&mut status);
        let inventory = status.nodes.values().next().unwrap().inventory.as_ref().unwrap();
        assert!(inventory.provider_contracts.is_empty());
        assert!(inventory.provider_adapter_contracts.is_empty());
        let hello = C2Hello {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            connection_id: 9,
            status: status.clone(),
            compatibility: None,
        };
        let legacy = LegacyHello {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            connection_id: 9,
            status: &status,
        };
        assert_eq!(
            serde_json::to_vec(&hello).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
        );
        let json = serde_json::to_string(&hello).unwrap();
        assert!(!json.contains("provider_contracts"));
        assert!(!json.contains("provider_adapter_contracts"));
    }

    fn repository_path(value: &str) -> RepositoryPath {
        RepositoryPath::utf8(value.to_owned()).unwrap()
    }

    fn tagged_repository_path() -> RepositoryPath {
        RepositoryPath::unix_bytes(vec![b's', b'r', b'c', b'/', 0xff]).unwrap()
    }

    fn repository_path_frame(
        entry_path: RepositoryPath,
        status_path: RepositoryPath,
        previous_path: Option<RepositoryPath>,
    ) -> C2ServerFrame {
        C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(2),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                response: Ok(C2NodeResponse::WorkspaceInspected {
                    inspection: C2WorkspaceInspection {
                        workspace_id: gate4agent_node_protocol::WorkspaceId::new("repo").unwrap(),
                        entries: vec![WorkspaceEntry {
                            relative_path: entry_path,
                            kind: WorkspaceEntryKind::File,
                        }],
                        tree_truncated: false,
                        git: C2GitSnapshot {
                            is_repository: true,
                            branch: Some("main".to_owned()),
                            status: vec![GitStatusEntry {
                                index_status: " ".to_owned(),
                                worktree_status: "M".to_owned(),
                                path: status_path,
                                previous_path,
                            }],
                            recent_commits: Vec::new(),
                            worktrees: Vec::new(),
                            truncated: false,
                            diagnostic_present: false,
                        },
                    },
                }),
            }),
        })
    }

    #[test]
    fn legacy_control_repository_path_gate_covers_entries_status_and_previous_path() {
        let utf8 = || repository_path("src/main.rs");
        let tagged = tagged_repository_path;
        for frame in [
            repository_path_frame(tagged(), utf8(), None),
            repository_path_frame(utf8(), tagged(), None),
            repository_path_frame(utf8(), utf8(), Some(tagged())),
        ] {
            assert!(server_frame_contains_unix_repository_path(&frame));
        }
        assert!(!server_frame_contains_unix_repository_path(
            &repository_path_frame(utf8(), utf8(), Some(utf8())),
        ));
    }

    #[tokio::test]
    async fn control_writer_fails_closed_before_tagged_repository_path_write() {
        let frame = repository_path_frame(
            tagged_repository_path(),
            repository_path("src/main.rs"),
            None,
        );
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender.send(queued(frame, &budget).unwrap()).await.unwrap();
        drop(sender);
        let (disconnect, _disconnected) = watch::channel(false);
        control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            NegotiatedPathCapabilities {
                opaque_host_paths: false,
                repository_paths: false,
                workspace_file_read: false,
                provider_runtime_status: false,
                provider_ids_open: false,
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    fn workspace_file_frame(path: RepositoryPath) -> C2ServerFrame {
        C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(3),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                response: Ok(C2NodeResponse::WorkspaceFileRead {
                    file: WorkspaceFileRead {
                        workspace_id: gate4agent_node_protocol::WorkspaceId::new("repo").unwrap(),
                        path,
                        content: WorkspaceFileContent::Utf8 {
                            text: "hello\n".to_owned(),
                            byte_len: 6,
                        },
                    },
                }),
            }),
        })
    }

    #[test]
    fn c2_workspace_file_read_gate_requires_file_and_tagged_path_capabilities() {
        let utf8_request = NodeRequest::ReadWorkspaceFile {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("repo").unwrap(),
            path: repository_path("src/lib.rs"),
        };
        let tagged_request = NodeRequest::ReadWorkspaceFile {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("repo").unwrap(),
            path: tagged_repository_path(),
        };
        let no_file_read = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: false,
            provider_runtime_status: true,
            provider_ids_open: true,
        };
        assert!(matches!(
            unnegotiated_request_failure(&utf8_request, no_file_read),
            Some(C2RelayFailure { code: C2RelayFailureCode::RequestForbidden, .. })
        ));
        let no_repository_path = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: false,
            workspace_file_read: true,
            provider_runtime_status: true,
            provider_ids_open: true,
        };
        assert!(matches!(
            unnegotiated_request_failure(&tagged_request, no_repository_path),
            Some(C2RelayFailure { code: C2RelayFailureCode::RequestForbidden, .. })
        ));
        let all = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            provider_runtime_status: true,
            provider_ids_open: true,
        };
        assert!(unnegotiated_request_failure(&tagged_request, all).is_none());
        assert!(server_frame_contains_workspace_file_read(
            &workspace_file_frame(repository_path("src/lib.rs")),
        ));
        assert!(server_frame_contains_unix_repository_path(
            &workspace_file_frame(tagged_repository_path()),
        ));
    }

    #[tokio::test]
    async fn control_writer_fails_closed_before_unnegotiated_workspace_file_read_write() {
        let frame = workspace_file_frame(repository_path("src/lib.rs"));
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender.send(queued(frame, &budget).unwrap()).await.unwrap();
        drop(sender);
        let (disconnect, _disconnected) = watch::channel(false);
        control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            NegotiatedPathCapabilities {
                opaque_host_paths: true,
                repository_paths: true,
                workspace_file_read: false,
                provider_runtime_status: true,
                provider_ids_open: true,
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[test]
    fn legacy_control_path_gate_rejects_tagged_requests_and_responses() {
        let path = OpaqueHostPath::unix_bytes(vec![b'/', 0xff]).unwrap();
        let request = NodeRequest::RegisterWorkspace {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("opaque").unwrap(),
            root: path.clone(),
        };
        assert!(node_request_contains_opaque_unix_path(&request));

        let frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(1),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                response: Ok(C2NodeResponse::WorktreeRemoved {
                    target_root: path,
                    workspace_id: None,
                }),
            }),
        });
        assert!(server_frame_contains_opaque_unix_path(&frame));
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
