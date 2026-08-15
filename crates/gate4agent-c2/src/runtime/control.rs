use super::*;
use crate::protocol::{
    c2_auth_transcript, c2_bound_auth_transcript, provider_id_is_legacy, AgentId, ArchitectureId, C2AuthDirection, C2ClientAuthentication,
    C2ClientFrame, C2ControlCompatibilitySupport, C2Hello, C2ReplyEnvelope,
    C2ServerChallenge, C2ServerFrame, C2Topology, CapabilityId, ClientCompatibilityOffer,
    HostDescriptor, NegotiatedC2ControlCompatibility,
    OperatingSystemId, PathEncoding, PathSemantics, PathStyle, ProtocolRange,
    C2_COMPATIBILITY_METADATA_CAPABILITY, C2_CONTROL_PROTOCOL_VERSION,
    C2_OPAQUE_UNIX_PATH_CAPABILITY, C2_REPOSITORY_PATH_CAPABILITY,
    C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
    C2_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
    C2_HISTORY_CONTEXT_PACK_CAPABILITY,
    C2_NATIVE_SESSION_CATALOG_CAPABILITY, C2_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY,
    C2_NATIVE_SESSION_INDEX_CAPABILITY, C2_NATIVE_SESSION_PREVIEW_CAPABILITY,
    C2_HOST_DIRECTORY_BROWSE_CAPABILITY,
    C2_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY,
    C2_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY,
    C2_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
    C2_SESSION_TASK_CORRELATION_CAPABILITY,
    C2_OBSERVATION_EVENTS_CAPABILITY,
    C2_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    C2_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY,
    C2_HARNESS_MCP_READ_PROXY_CAPABILITY,
    C2_PROVIDER_ID_OPEN_CAPABILITY,
    C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
    C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, C2_PROVIDER_RUNTIME_STATUS_CAPABILITY,
    C2_SPAWN_PROFILE_REVISION_CAPABILITY,
    C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
    C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
    C2_GIT_READ_CAPABILITY, C2_WORKSPACE_FILE_READ_CAPABILITY,
    C2_WORKSPACE_FILE_WRITE_CAPABILITY,
    C2_WORKSPACE_ENTRY_CREATE_CAPABILITY,
    C2_WORKTREE_SELECTION_CAPABILITY,
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

#[derive(Clone, Copy, Default)]
struct NegotiatedPathCapabilities {
    opaque_host_paths: bool,
    repository_paths: bool,
    workspace_file_read: bool,
    workspace_file_write: bool,
    workspace_entry_create: bool,
    git_read: bool,
    host_directory_browse: bool,
    standalone_workspace_lifecycle: bool,
    provider_session_reference_index: bool,
    provider_runtime_status: bool,
    provider_ids_open: bool,
    spawn_spec_defaults_overrides: bool,
    spawn_profile_revision: bool,
    worktree_selection: bool,
    managed_worktree_lifecycle: bool,
    child_environment_profile: bool,
    session_bundle_materialization: bool,
    history_context_pack: bool,
    native_session_catalog: bool,
    native_session_catalog_paging: bool,
    native_session_index: bool,
    native_session_preview: bool,
    terminal_frame_events: bool,
    agent_progress_snapshot: bool,
    session_task_correlation: bool,
    observation_events: bool,
    observation_managed_target: bool,
    observation_workflow_detail: bool,
    delivery_bundle_v2_stage_commit: bool,
    harness_mcp_read_proxy: bool,
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
    project_status_observation_support(
        &mut hello_status,
        path_capabilities.observation_events,
        path_capabilities.observation_managed_target,
        path_capabilities.observation_workflow_detail,
    );
    if !path_capabilities.provider_ids_open {
        retain_legacy_provider_status(&mut hello_status);
    }
    if !path_capabilities.child_environment_profile
        && status_contains_child_environment_profile(&hello_status)
    {
        hub.detach(connection_id);
        return Ok(());
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
    let mut last_topology = C2Topology::from_status_with_projection(
        &hello_status,
        include_provider_contracts,
        include_provider_runtime_status,
        path_capabilities.observation_events,
        path_capabilities.observation_managed_target,
        path_capabilities.observation_workflow_detail,
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
                    C2Topology::from_status_with_projection(
                        &projected,
                        include_provider_contracts,
                        include_provider_runtime_status,
                        path_capabilities.observation_events,
                        path_capabilities.observation_managed_target,
                        path_capabilities.observation_workflow_detail,
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
            observed.observation_support = snapshot.observation_support;
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
        project_server_frame_launch_inventory(
            &mut frame,
            path_capabilities.spawn_spec_defaults_overrides,
            path_capabilities.managed_worktree_lifecycle
                && path_capabilities.worktree_selection,
            path_capabilities.session_bundle_materialization,
        );
        if !path_capabilities.terminal_frame_events {
            match server_frame_terminal_frame_payload(&frame) {
                TerminalFramePayload::DirectEvent => {
                    budget.fetch_sub(queued.bytes, Ordering::AcqRel);
                    continue;
                }
                TerminalFramePayload::NestedReply => {
                    budget.fetch_sub(queued.bytes, Ordering::AcqRel);
                    break;
                }
                TerminalFramePayload::None => {}
            }
        }
        if !project_server_frame_observation_events(
            &mut frame,
            path_capabilities.observation_events,
            path_capabilities.observation_managed_target,
            path_capabilities.observation_workflow_detail,
        )
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            continue;
        }
        if !path_capabilities.harness_mcp_read_proxy
            && server_frame_contains_harness_mcp_proxy(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.spawn_spec_defaults_overrides
            && server_frame_contains_spawn_spec_response(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.spawn_profile_revision
            && server_frame_contains_spawn_spec_response(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.worktree_selection
            && server_frame_contains_worktree_selection_response(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.managed_worktree_lifecycle
            && server_frame_contains_managed_worktree(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.child_environment_profile
            && server_frame_contains_child_environment_profile(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.session_bundle_materialization
            && server_frame_contains_session_bundle_materialization(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.history_context_pack
            && !project_server_frame_without_history_context_pack(&mut frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.session_task_correlation {
            strip_session_task_bindings_from_server_frame(&mut frame);
        }
        if !path_capabilities.host_directory_browse
            && server_frame_contains_host_directory_browse(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.standalone_workspace_lifecycle
            && server_frame_contains_standalone_workspace_lifecycle(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.provider_session_reference_index
            && server_frame_contains_provider_session_reference_index(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.native_session_index
            && server_frame_contains_native_session_index(&frame)
        {
            budget.fetch_sub(queued.bytes, Ordering::AcqRel);
            break;
        }
        if !path_capabilities.provider_runtime_status {
            clear_server_frame_provider_runtime_status(&mut frame);
        }
        if !path_capabilities.agent_progress_snapshot {
            clear_server_frame_agent_progress(&mut frame);
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
            || (!path_capabilities.workspace_file_write
                && server_frame_contains_workspace_file_write(&frame))
            || (!path_capabilities.workspace_entry_create
                && server_frame_contains_workspace_entry_create(&frame))
            || (!path_capabilities.git_read
                && server_frame_contains_git_read(&frame))
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

fn project_server_frame_launch_inventory(
    frame: &mut C2ServerFrame,
    include_spawn_profiles: bool,
    include_managed_worktree_profiles: bool,
    include_bundles: bool,
) {
    let project_snapshot = |snapshot: &mut crate::protocol::C2NodeSnapshot| {
        project_c2_snapshot_launch_inventory(
            snapshot,
            include_spawn_profiles,
            include_managed_worktree_profiles,
            include_bundles,
        );
    };
    let project_slim = |inventory: &mut crate::protocol::SlimNodeInventory| {
        if !include_managed_worktree_profiles {
            for workspace in inventory.workspaces.values_mut() {
                workspace.worktree_service_mode = None;
                workspace.managed_worktree_profiles = None;
            }
        }
        if let Some(launch) = inventory.launch_inventory.as_mut() {
            if !include_spawn_profiles {
                launch.spawn_profiles = None;
            }
            if !include_bundles {
                launch.bundles = None;
            }
            if launch.spawn_profiles.is_none() && launch.bundles.is_none() {
                inventory.launch_inventory = None;
            }
        }
    };

    match frame {
        C2ServerFrame::Hello(hello) => {
            for node in hello.status.nodes.values_mut() {
                if let Some(inventory) = node.inventory.as_mut() {
                    project_slim(inventory);
                }
            }
        }
        C2ServerFrame::Reply(reply) => {
            let Ok(routed) = reply.result.as_mut() else { return };
            let Ok(response) = routed.response.as_mut() else { return };
            match response {
                C2NodeResponse::Snapshot { snapshot, .. } => project_snapshot(snapshot),
                C2NodeResponse::Resync { snapshot, events, .. } => {
                    project_snapshot(snapshot);
                    if !include_managed_worktree_profiles {
                        for event in events {
                            if let C2NodeEvent::WorkspaceAdded { workspace } = &mut event.event {
                                workspace.worktree_service_mode = None;
                                workspace.managed_worktree_profiles = None;
                            }
                        }
                    }
                }
                C2NodeResponse::WorkspaceRegistered { workspace }
                | C2NodeResponse::StandaloneWorkspaceCreated { workspace }
                | C2NodeResponse::WorktreeCreated { workspace, .. }
                    if !include_managed_worktree_profiles =>
                {
                    workspace.worktree_service_mode = None;
                    workspace.managed_worktree_profiles = None;
                }
                _ => {}
            }
        }
        C2ServerFrame::Event(event) => {
            if !include_managed_worktree_profiles {
                if let C2NodeEvent::WorkspaceAdded { workspace } = &mut event.event {
                    workspace.worktree_service_mode = None;
                    workspace.managed_worktree_profiles = None;
                }
            }
        }
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => {}
    }
}

fn project_c2_snapshot_launch_inventory(
    snapshot: &mut crate::protocol::C2NodeSnapshot,
    include_spawn_profiles: bool,
    include_managed_worktree_profiles: bool,
    include_bundles: bool,
) {
    if !include_managed_worktree_profiles {
        for workspace in &mut snapshot.workspaces {
            workspace.worktree_service_mode = None;
            workspace.managed_worktree_profiles = None;
        }
    }
    if let Some(inventory) = snapshot.launch_inventory.as_mut() {
        if !include_spawn_profiles {
            inventory.spawn_profiles = None;
        }
        if !include_bundles {
            inventory.bundles = None;
        }
        if inventory.spawn_profiles.is_none() && inventory.bundles.is_none() {
            snapshot.launch_inventory = None;
        }
    }
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
        workspace_file_write: selected_has(C2_WORKSPACE_FILE_WRITE_CAPABILITY),
        workspace_entry_create: selected_has(C2_WORKSPACE_ENTRY_CREATE_CAPABILITY),
        git_read: selected_has(C2_GIT_READ_CAPABILITY),
        host_directory_browse: selected_has(C2_HOST_DIRECTORY_BROWSE_CAPABILITY),
        standalone_workspace_lifecycle:
            selected_has(C2_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY),
        provider_session_reference_index:
            selected_has(C2_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY),
        provider_runtime_status: selected_has(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY),
        provider_ids_open: selected_has(C2_PROVIDER_ID_OPEN_CAPABILITY),
        spawn_spec_defaults_overrides:
            selected_has(C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY),
        spawn_profile_revision: selected_has(C2_SPAWN_PROFILE_REVISION_CAPABILITY),
        worktree_selection: selected_has(C2_WORKTREE_SELECTION_CAPABILITY),
        managed_worktree_lifecycle:
            selected_has(C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY),
        child_environment_profile:
            selected_has(C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY),
        session_bundle_materialization:
            selected_has(C2_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY),
        history_context_pack: selected_has(C2_HISTORY_CONTEXT_PACK_CAPABILITY),
        native_session_catalog: selected_has(C2_NATIVE_SESSION_CATALOG_CAPABILITY),
        native_session_catalog_paging:
            selected_has(C2_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY),
        native_session_index: selected_has(C2_NATIVE_SESSION_INDEX_CAPABILITY),
        native_session_preview: selected_has(C2_NATIVE_SESSION_PREVIEW_CAPABILITY),
        terminal_frame_events: selected_has(C2_TERMINAL_FRAME_EVENTS_CAPABILITY),
        agent_progress_snapshot: selected_has(C2_AGENT_PROGRESS_SNAPSHOT_CAPABILITY),
        session_task_correlation: selected_has(C2_SESSION_TASK_CORRELATION_CAPABILITY),
        observation_events: selected_has(C2_OBSERVATION_EVENTS_CAPABILITY),
        observation_managed_target: selected_has(C2_OBSERVATION_EVENTS_CAPABILITY)
            && selected_has(C2_OBSERVATION_MANAGED_TARGET_CAPABILITY),
        observation_workflow_detail: selected_has(C2_OBSERVATION_EVENTS_CAPABILITY)
            && selected_has(C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY),
        delivery_bundle_v2_stage_commit:
            selected_has(C2_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY),
        harness_mcp_read_proxy: selected_has(C2_HARNESS_MCP_READ_PROXY_CAPABILITY),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalFramePayload {
    None,
    DirectEvent,
    NestedReply,
}

fn server_frame_terminal_frame_payload(frame: &C2ServerFrame) -> TerminalFramePayload {
    match frame {
        C2ServerFrame::Reply(reply) if reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(|response| {
                match response {
                    C2NodeResponse::Resync { events, .. } => events.iter().any(|event| {
                        c2_event_is_terminal_frame(&event.event)
                    }),
                    C2NodeResponse::Snapshot { .. }
                    | C2NodeResponse::Armed { .. }
                    | C2NodeResponse::Spawned { .. }
                    | C2NodeResponse::Activated { .. }
                    | C2NodeResponse::Aborted { .. }
                    | C2NodeResponse::ReplyChunkAccepted { .. }
                    | C2NodeResponse::CallRejected { .. }
                    | C2NodeResponse::DeliveryStageBegun { .. }
                    | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
                    | C2NodeResponse::DeliveryCommitted { .. }
                    | C2NodeResponse::DeliveryStageAborted { .. }
                    | C2NodeResponse::WorkspaceInspected { .. }
                    | C2NodeResponse::HostDirectoriesBrowsed { .. }
                    | C2NodeResponse::WorkspaceFileRead { .. }
                    | C2NodeResponse::WorkspaceFileWritten { .. }
                    | C2NodeResponse::WorkspaceFileCreated { .. }
                    | C2NodeResponse::WorkspaceDirectoryCreated { .. }
                    | C2NodeResponse::GitHistoryRead { .. }
                    | C2NodeResponse::GitDiffRead { .. }
                    | C2NodeResponse::Controller { .. }
                    | C2NodeResponse::SpawnAccepted { .. }
                    | C2NodeResponse::SpawnSpecAccepted { .. }
                    | C2NodeResponse::ManagedWorktreeSpawnAccepted { .. }
                    | C2NodeResponse::ManagedWorktreeCleanup { .. }
                    | C2NodeResponse::SessionRecordUpdated { .. }
                    | C2NodeResponse::ProviderSessionIndexed { .. }
                    | C2NodeResponse::NativeSessionIndexed { .. }
                    | C2NodeResponse::SessionRecordResumed { .. }
                    | C2NodeResponse::SessionRecordForgotten { .. }
                    | C2NodeResponse::NativeSessionsCataloged { .. }
                    | C2NodeResponse::NativeSessionsPaged { .. }
                    | C2NodeResponse::NativeSessionPreviewed { .. }
                    | C2NodeResponse::SessionRecordPreviewed { .. }
                    | C2NodeResponse::HistoryDiscovered { .. }
                    | C2NodeResponse::HistoryLoaded { .. }
                    | C2NodeResponse::ContextPackExported { .. }
                    | C2NodeResponse::ContextPackForgotten { .. }
                    | C2NodeResponse::WorkspaceRegistered { .. }
                    | C2NodeResponse::StandaloneWorkspaceCreated { .. }
                    | C2NodeResponse::WorkspaceUnregistered { .. }
                    | C2NodeResponse::WorktreeCreated { .. }
                    | C2NodeResponse::WorktreeRemoved { .. }
                    | C2NodeResponse::Accepted
                    | C2NodeResponse::ShuttingDown => false,
                }
            })
        }) => TerminalFramePayload::NestedReply,
        C2ServerFrame::Event(event) if c2_event_is_terminal_frame(&event.event) => {
            TerminalFramePayload::DirectEvent
        }
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Reply(_)
        | C2ServerFrame::Event(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => TerminalFramePayload::None,
    }
}

fn c2_event_is_terminal_frame(event: &C2NodeEvent) -> bool {
    match event {
        C2NodeEvent::TerminalFrame { .. } => true,
        C2NodeEvent::HarnessMcpReadCall { .. }
        | C2NodeEvent::Control { .. }
        | C2NodeEvent::Observation { .. }
        | C2NodeEvent::ManagedObservation { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceAdded { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::SessionRecordUpserted { .. }
        | C2NodeEvent::SessionRecordRemoved { .. }
        | C2NodeEvent::ManagedWorktreeUpserted { .. }
        | C2NodeEvent::ManagedWorktreeRemoved { .. }
        | C2NodeEvent::ResyncRequired { .. } => false,
    }
}

fn server_frame_contains_harness_mcp_proxy(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            match &routed.response {
                Ok(C2NodeResponse::Resync { events, .. }) => events.iter().any(|event| {
                    event.event.requires_harness_mcp_proxy_capability()
                }),
                Ok(response) => response.requires_harness_mcp_proxy_capability(),
                Err(failure) => failure.requires_harness_mcp_proxy_capability(),
            }
        }),
        C2ServerFrame::Event(event) => event.event.requires_harness_mcp_proxy_capability(),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn unnegotiated_request_failure(
    request: &NodeRequest,
    capabilities: NegotiatedPathCapabilities,
) -> Option<C2RelayFailure> {
    if !request.harness_mcp_contract_is_valid_at(unix_ms()) {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "invalid harness MCP proxy request",
            None,
        ));
    }
    if !request.history_context_pack_contract_is_valid() {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "invalid history context pack request",
            None,
        ));
    }
    if !request.native_session_catalog_contract_is_valid() {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "invalid native session catalog request",
            None,
        ));
    }
    if !request.native_session_preview_contract_is_valid() {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "invalid native session preview request",
            None,
        ));
    }
    if matches!(request, NodeRequest::IndexNativeSession { selection, .. }
        if selection.route.scope
            != gate4agent_node_protocol::NativeSessionCatalogScope::Workspace)
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "external native sessions must be registered as workspaces before indexing",
            None,
        ));
    }
    let required_capability_available = match request.required_capability() {
        None => true,
        Some(C2_WORKSPACE_FILE_READ_CAPABILITY) => capabilities.workspace_file_read,
        Some(C2_WORKSPACE_FILE_WRITE_CAPABILITY) => capabilities.workspace_file_write,
        Some(C2_WORKSPACE_ENTRY_CREATE_CAPABILITY) => capabilities.workspace_entry_create,
        Some(C2_GIT_READ_CAPABILITY) => capabilities.git_read,
        Some(C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY) => {
            capabilities.spawn_spec_defaults_overrides
        }
        Some(C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY) => {
            capabilities.managed_worktree_lifecycle
        }
        Some(C2_HISTORY_CONTEXT_PACK_CAPABILITY) => capabilities.history_context_pack,
        Some(C2_NATIVE_SESSION_CATALOG_CAPABILITY) => capabilities.native_session_catalog,
        Some(C2_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY) => {
            capabilities.native_session_catalog_paging
        }
        Some(C2_NATIVE_SESSION_INDEX_CAPABILITY) => capabilities.native_session_index,
        Some(C2_NATIVE_SESSION_PREVIEW_CAPABILITY) => capabilities.native_session_preview,
        Some(C2_HOST_DIRECTORY_BROWSE_CAPABILITY) => capabilities.host_directory_browse,
        Some(C2_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY) => {
            capabilities.standalone_workspace_lifecycle
        }
        Some(C2_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY) => {
            capabilities.provider_session_reference_index
        }
        Some(C2_SESSION_TASK_CORRELATION_CAPABILITY) => {
            capabilities.session_task_correlation
        }
        Some(C2_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY) => {
            capabilities.delivery_bundle_v2_stage_commit
        }
        Some(C2_HARNESS_MCP_READ_PROXY_CAPABILITY) => capabilities.harness_mcp_read_proxy,
        Some(_) => false,
    };
    if !required_capability_available {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "request capability was not negotiated with C2",
            None,
        ));
    }
    if request.requires_spawn_spec_defaults_overrides_capability()
        && !capabilities.spawn_spec_defaults_overrides
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "spawn spec capability was not negotiated with C2",
            None,
        ));
    }
    if request.requires_spawn_profile_revision_capability()
        && !capabilities.spawn_profile_revision
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "spawn profile revision capability was not negotiated with C2",
            None,
        ));
    }
    if request.requires_worktree_selection_capability()
        && !capabilities.worktree_selection
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "worktree selection capability was not negotiated with C2",
            None,
        ));
    }
    if c2_request_requires_child_environment_profile_capability(request)
        && !capabilities.child_environment_profile
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "child environment profile capability was not negotiated with C2",
            None,
        ));
    }
    if request.requires_session_bundle_materialization_capability()
        && !capabilities.session_bundle_materialization
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "session bundle materialization capability was not negotiated with C2",
            None,
        ));
    }
    if request.requires_history_context_pack_capability()
        && !capabilities.history_context_pack
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "history context pack capability was not negotiated with C2",
            None,
        ));
    }
    if !capabilities.provider_ids_open
        && (matches!(request, NodeRequest::Spawn { provider, .. } if !provider_id_is_legacy(provider))
            || matches!(request, NodeRequest::IndexProviderSession { provider, .. } if !provider_id_is_legacy(provider))
            || matches!(request, NodeRequest::CatalogNativeSessions { route, .. } if !provider_id_is_legacy(&route.provider))
            || matches!(request, NodeRequest::PageNativeSessions { route, .. } if !provider_id_is_legacy(&route.provider))
            || matches!(request, NodeRequest::PreviewNativeSession { selection, .. } if !provider_id_is_legacy(&selection.route.provider))
            || matches!(request, NodeRequest::IndexNativeSession { selection, .. } if !provider_id_is_legacy(&selection.route.provider))
            || spawn_spec_requires_open_provider_capability(request)
            || matches!(request, NodeRequest::ForgetContextPack { .. }))
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "open provider IDs require negotiated C2 capability",
            None,
        ));
    }
    if !capabilities.repository_paths
        && (matches!(request, NodeRequest::ReadWorkspaceFile { path, .. }
            | NodeRequest::WriteWorkspaceFile { path, .. }
            | NodeRequest::CreateWorkspaceFile { path, .. }
            | NodeRequest::CreateWorkspaceDirectory { path, .. }
            if path.as_unix_bytes().is_some())
            || matches!(request, NodeRequest::ReadGitDiff { request, .. }
                if request.path.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())))
    {
        return Some(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "tagged repository paths require negotiated C2 capability",
            None,
        ));
    }
    None
}

fn c2_request_requires_child_environment_profile_capability(request: &NodeRequest) -> bool {
    let spec = match request {
        NodeRequest::SpawnSpec { spec } => spec,
        NodeRequest::SpawnManagedWorktree { request } => &request.spawn_spec,
        _ => return false,
    };
    !matches!(
        &spec.overrides.environment_profile_id,
        gate4agent_node_protocol::SpawnOverride::Clear
    )
}

fn spawn_spec_requires_open_provider_capability(request: &NodeRequest) -> bool {
    let spec = match request {
        NodeRequest::SpawnSpec { spec } => spec,
        NodeRequest::ArmHarnessMcpReservation { spawn_spec: spec, .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { spec, .. } => spec,
        NodeRequest::SpawnManagedWorktree { request } => &request.spawn_spec,
        _ => return false,
    };
    match &spec.overrides.provider {
        gate4agent_node_protocol::SpawnOverride::Set { value } => {
            !provider_id_is_legacy(value)
        }
        gate4agent_node_protocol::SpawnOverride::Inherit
        | gate4agent_node_protocol::SpawnOverride::Clear => true,
    }
}

fn node_request_contains_opaque_unix_path(request: &NodeRequest) -> bool {
    match request {
        NodeRequest::RegisterWorkspace { root, .. }
        | NodeRequest::CreateStandaloneWorkspace { root, .. } => root.as_unix_bytes().is_some(),
        NodeRequest::BrowseHostDirectories { directory, after } => {
            directory.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
                || after.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
        }
        NodeRequest::CreateWorktree { target_root, .. }
        | NodeRequest::RemoveWorktree { target_root, .. } => target_root.as_unix_bytes().is_some(),
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::Spawn { .. }
        | NodeRequest::SpawnSpec { .. }
        | NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::CleanupManagedWorktree { .. }
        | NodeRequest::Resume { .. }
        | NodeRequest::RenameSessionRecord { .. }
        | NodeRequest::SetSessionTask { .. }
        | NodeRequest::IndexProviderSession { .. }
        | NodeRequest::IndexNativeSession { .. }
        | NodeRequest::ResumeSessionRecord { .. }
        | NodeRequest::ForgetSessionRecord { .. }
        | NodeRequest::CatalogNativeSessions { .. }
        | NodeRequest::PageNativeSessions { .. }
        | NodeRequest::PreviewNativeSession { .. }
        | NodeRequest::PreviewSessionRecord { .. }
        | NodeRequest::DiscoverHistory { .. }
        | NodeRequest::LoadHistory { .. }
        | NodeRequest::ExportContextPack { .. }
        | NodeRequest::ForgetContextPack { .. }
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

fn server_frame_contains_host_directory_browse(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            match &routed.response {
                Ok(response) => response.requires_host_directory_browse_capability(),
                Err(failure) => failure.requires_host_directory_browse_capability(),
            }
        }),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Event(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn server_frame_contains_standalone_workspace_lifecycle(frame: &C2ServerFrame) -> bool {
    matches!(
        frame,
        C2ServerFrame::Reply(reply)
            if reply.result.as_ref().ok().is_some_and(|routed| matches!(
                routed.response,
                Ok(C2NodeResponse::StandaloneWorkspaceCreated { .. })
            ))
    )
}

fn server_frame_contains_provider_session_reference_index(frame: &C2ServerFrame) -> bool {
    matches!(
        frame,
        C2ServerFrame::Reply(reply)
            if reply.result.as_ref().ok().is_some_and(|routed| matches!(
                routed.response,
                Ok(C2NodeResponse::ProviderSessionIndexed { .. })
            ))
    )
}

fn server_frame_contains_native_session_index(frame: &C2ServerFrame) -> bool {
    matches!(
        frame,
        C2ServerFrame::Reply(reply)
            if reply.result.as_ref().ok().is_some_and(|routed| matches!(
                routed.response,
                Ok(C2NodeResponse::NativeSessionIndexed { .. })
            ))
    )
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
                    C2NodeResponse::WorkspaceFileRead { file }
                    | C2NodeResponse::WorkspaceFileWritten { file }
                    | C2NodeResponse::WorkspaceFileCreated { file } => {
                        file.path.as_unix_bytes().is_some()
                    }
                    C2NodeResponse::WorkspaceDirectoryCreated { entry, .. } => {
                        entry.relative_path.as_unix_bytes().is_some()
                    }
                    C2NodeResponse::GitDiffRead { diff, .. } => diff
                        .path
                        .as_ref()
                        .is_some_and(|path| path.as_unix_bytes().is_some()),
                    C2NodeResponse::Snapshot { .. }
                    | C2NodeResponse::Resync { .. }
                    | C2NodeResponse::Armed { .. }
                    | C2NodeResponse::Spawned { .. }
                    | C2NodeResponse::Activated { .. }
                    | C2NodeResponse::Aborted { .. }
                    | C2NodeResponse::ReplyChunkAccepted { .. }
                    | C2NodeResponse::CallRejected { .. }
                    | C2NodeResponse::DeliveryStageBegun { .. }
                    | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
                    | C2NodeResponse::DeliveryCommitted { .. }
                    | C2NodeResponse::DeliveryStageAborted { .. }
                    | C2NodeResponse::HostDirectoriesBrowsed { .. }
                    | C2NodeResponse::GitHistoryRead { .. }
                    | C2NodeResponse::Controller { .. }
                    | C2NodeResponse::SpawnAccepted { .. }
                    | C2NodeResponse::SpawnSpecAccepted { .. }
                    | C2NodeResponse::ManagedWorktreeSpawnAccepted { .. }
                    | C2NodeResponse::ManagedWorktreeCleanup { .. }
                    | C2NodeResponse::SessionRecordUpdated { .. }
                    | C2NodeResponse::ProviderSessionIndexed { .. }
                    | C2NodeResponse::NativeSessionIndexed { .. }
                    | C2NodeResponse::SessionRecordResumed { .. }
                    | C2NodeResponse::SessionRecordForgotten { .. }
                    | C2NodeResponse::NativeSessionsCataloged { .. }
                    | C2NodeResponse::NativeSessionsPaged { .. }
                    | C2NodeResponse::NativeSessionPreviewed { .. }
                    | C2NodeResponse::SessionRecordPreviewed { .. }
                    | C2NodeResponse::HistoryDiscovered { .. }
                    | C2NodeResponse::HistoryLoaded { .. }
                    | C2NodeResponse::ContextPackExported { .. }
                    | C2NodeResponse::ContextPackForgotten { .. }
                    | C2NodeResponse::WorkspaceRegistered { .. }
                    | C2NodeResponse::StandaloneWorkspaceCreated { .. }
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

fn server_frame_contains_workspace_file_write(frame: &C2ServerFrame) -> bool {
    matches!(frame, C2ServerFrame::Reply(reply)
        if reply.result.as_ref().ok().is_some_and(|routed|
            matches!(routed.response, Ok(C2NodeResponse::WorkspaceFileWritten { .. }))))
}

fn server_frame_contains_workspace_entry_create(frame: &C2ServerFrame) -> bool {
    matches!(frame, C2ServerFrame::Reply(reply)
        if reply.result.as_ref().ok().is_some_and(|routed|
            routed.response.as_ref().is_ok_and(
                C2NodeResponse::requires_workspace_entry_create_capability,
            )))
}

fn server_frame_contains_git_read(frame: &C2ServerFrame) -> bool {
    matches!(frame, C2ServerFrame::Reply(reply)
        if reply.result.as_ref().ok().is_some_and(|routed|
            matches!(routed.response,
                Ok(C2NodeResponse::GitHistoryRead { .. } | C2NodeResponse::GitDiffRead { .. }))))
}

fn server_frame_contains_spawn_spec_response(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(|response| {
                matches!(response,
                    C2NodeResponse::SpawnSpecAccepted { .. }
                    | C2NodeResponse::Spawned { .. }
                    | C2NodeResponse::ManagedWorktreeSpawnAccepted { .. }
                )
            })
        }),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Event(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn server_frame_contains_worktree_selection_response(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(|response| {
                matches!(response,
                    C2NodeResponse::SpawnSpecAccepted { receipt }
                        | C2NodeResponse::Spawned { receipt, .. }
                    if receipt.target.worktree_id.is_some())
                    || c2_response_contains_managed_worktree(response)
            })
        }),
        C2ServerFrame::Hello(hello) => hello.status.nodes.values().any(|node| {
            node.inventory.as_ref().is_some_and(|inventory| {
                !inventory.managed_worktrees.is_empty()
            })
        }),
        C2ServerFrame::Event(event) => c2_event_is_managed_worktree(&event.event),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn server_frame_contains_managed_worktree(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed
                .response
                .as_ref()
                .ok()
                .is_some_and(c2_response_contains_managed_worktree)
        }),
        C2ServerFrame::Hello(hello) => hello.status.nodes.values().any(|node| {
            node.inventory.as_ref().is_some_and(|inventory| {
                !inventory.managed_worktrees.is_empty()
            })
        }),
        C2ServerFrame::Event(event) => c2_event_is_managed_worktree(&event.event),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn server_frame_contains_child_environment_profile(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Hello(hello) => {
            status_contains_child_environment_profile(&hello.status)
        }
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(
                C2NodeResponse::requires_child_environment_profile_capability,
            )
        }),
        C2ServerFrame::Event(event) => event
            .event
            .requires_child_environment_profile_capability(),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn status_contains_child_environment_profile(status: &StatusResponse) -> bool {
    status.nodes.values().any(|node| {
        node.inventory.as_ref().is_some_and(|inventory| {
            inventory.managed_sessions.iter().any(|record| {
                record.environment_profile.is_some()
            })
        })
    })
}

fn server_frame_contains_session_bundle_materialization(frame: &C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Hello(hello) => {
            status_contains_session_bundle_materialization(&hello.status)
        }
        C2ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|routed| {
            routed.response.as_ref().ok().is_some_and(
                C2NodeResponse::requires_session_bundle_materialization_capability,
            )
        }),
        C2ServerFrame::Event(event) => event
            .event
            .requires_session_bundle_materialization_capability(),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => false,
    }
}

fn status_contains_session_bundle_materialization(status: &StatusResponse) -> bool {
    status.nodes.values().any(|node| {
        node.inventory.as_ref().is_some_and(|inventory| {
            inventory.managed_sessions.iter().any(|record| record.bundle.is_some())
        })
    })
}

fn project_server_frame_without_history_context_pack(frame: &mut C2ServerFrame) -> bool {
    match frame {
        C2ServerFrame::Reply(reply) => {
            let Ok(routed) = reply.result.as_mut() else {
                return true;
            };
            match routed.response.as_mut() {
                Ok(response) => strip_history_context_pack_from_response(response),
                Err(failure) => !matches!(
                    failure.code,
                    NodeFailureCode::UnknownContextPack
                        | NodeFailureCode::ContextPackBusy
                        | NodeFailureCode::ContextPackMaterializationFailed
                ),
            }
        }
        C2ServerFrame::Event(event) => {
            strip_history_context_pack_from_event(&mut event.event);
            true
        }
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => true,
    }
}

fn project_server_frame_observation_events(
    frame: &mut C2ServerFrame,
    include_observations: bool,
    include_managed_targets: bool,
    include_workflow_detail: bool,
) -> bool {
    let visible = |event: &C2NodeEvent| {
        (!event.requires_observation_events_capability() || include_observations)
            && (!event.requires_observation_managed_target_capability()
                || include_managed_targets)
            && (!event.requires_observation_workflow_detail_capability()
                || include_workflow_detail)
    };
    match frame {
        C2ServerFrame::Event(event) => visible(&event.event),
        C2ServerFrame::Reply(reply) => {
            if let Ok(routed) = reply.result.as_mut() {
                if let Ok(response) = routed.response.as_mut() {
                    match response {
                        C2NodeResponse::Snapshot { snapshot, .. } => {
                            project_snapshot_observation_support(
                                snapshot,
                                include_observations,
                                include_managed_targets,
                                include_workflow_detail,
                            );
                        }
                        C2NodeResponse::Resync { snapshot, events, .. } => {
                            project_snapshot_observation_support(
                                snapshot,
                                include_observations,
                                include_managed_targets,
                                include_workflow_detail,
                            );
                            events.retain(|event| visible(&event.event));
                        }
                        _ => {}
                    }
                }
            }
            true
        }
        C2ServerFrame::Hello(hello) => {
            project_status_observation_support(
                &mut hello.status,
                include_observations,
                include_managed_targets,
                include_workflow_detail,
            );
            true
        }
        C2ServerFrame::Topology(topology) => {
            for node in &mut topology.nodes {
                node.observation_support = node.observation_support.and_then(|support| {
                    support.projected_for_downstream(
                        include_observations,
                        include_managed_targets,
                        include_workflow_detail,
                    )
                });
            }
            true
        }
        C2ServerFrame::Challenge(_) | C2ServerFrame::Rejected(_) => true,
    }
}

fn project_status_observation_support(
    status: &mut StatusResponse,
    include_observations: bool,
    include_managed_targets: bool,
    include_workflow_detail: bool,
) {
    for node in status.nodes.values_mut() {
        node.observation_support = node.observation_support.and_then(|support| {
            support.projected_for_downstream(
                include_observations,
                include_managed_targets,
                include_workflow_detail,
            )
        });
    }
}

fn project_snapshot_observation_support(
    snapshot: &mut crate::protocol::C2NodeSnapshot,
    include_observations: bool,
    include_managed_targets: bool,
    include_workflow_detail: bool,
) {
    snapshot.observation_support = snapshot.observation_support.and_then(|support| {
        support.projected_for_downstream(
            include_observations,
            include_managed_targets,
            include_workflow_detail,
        )
    });
}

fn strip_history_context_pack_from_response(response: &mut C2NodeResponse) -> bool {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => {
            strip_history_context_pack_from_snapshot(snapshot);
            true
        }
        C2NodeResponse::Resync { snapshot, events, .. } => {
            strip_history_context_pack_from_snapshot(snapshot);
            for event in events {
                strip_history_context_pack_from_event(&mut event.event);
            }
            true
        }
        C2NodeResponse::SpawnSpecAccepted { receipt }
        | C2NodeResponse::Spawned { receipt, .. } => {
            receipt.context_id = None;
            receipt.context = None;
            true
        }
        C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt } => {
            receipt.spawn.context_id = None;
            receipt.spawn.context = None;
            true
        }
        C2NodeResponse::SessionRecordUpdated { record }
        | C2NodeResponse::ProviderSessionIndexed { record }
        | C2NodeResponse::NativeSessionIndexed { record, .. }
        | C2NodeResponse::SessionRecordResumed { record, .. } => {
            strip_history_context_pack_from_record(record);
            true
        }
        C2NodeResponse::HistoryDiscovered { .. }
        | C2NodeResponse::HistoryLoaded { .. }
        | C2NodeResponse::ContextPackExported { .. }
        | C2NodeResponse::ContextPackForgotten { .. } => false,
        C2NodeResponse::Armed { .. }
        | C2NodeResponse::Activated { .. }
        | C2NodeResponse::Aborted { .. }
        | C2NodeResponse::ReplyChunkAccepted { .. }
        | C2NodeResponse::CallRejected { .. }
        | C2NodeResponse::WorkspaceInspected { .. }
        | C2NodeResponse::DeliveryStageBegun { .. }
        | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
        | C2NodeResponse::DeliveryCommitted { .. }
        | C2NodeResponse::DeliveryStageAborted { .. }
        | C2NodeResponse::HostDirectoriesBrowsed { .. }
        | C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::WorkspaceFileWritten { .. }
        | C2NodeResponse::WorkspaceFileCreated { .. }
        | C2NodeResponse::WorkspaceDirectoryCreated { .. }
        | C2NodeResponse::GitHistoryRead { .. }
        | C2NodeResponse::GitDiffRead { .. }
        | C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::ManagedWorktreeCleanup { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::NativeSessionsCataloged { .. }
        | C2NodeResponse::NativeSessionsPaged { .. }
        | C2NodeResponse::NativeSessionPreviewed { .. }
        | C2NodeResponse::SessionRecordPreviewed { .. }
        | C2NodeResponse::WorkspaceRegistered { .. }
        | C2NodeResponse::StandaloneWorkspaceCreated { .. }
        | C2NodeResponse::WorkspaceUnregistered { .. }
        | C2NodeResponse::WorktreeCreated { .. }
        | C2NodeResponse::WorktreeRemoved { .. }
        | C2NodeResponse::Accepted
        | C2NodeResponse::ShuttingDown => true,
    }
}

fn strip_history_context_pack_from_snapshot(snapshot: &mut crate::protocol::C2NodeSnapshot) {
    for record in &mut snapshot.session_records {
        strip_history_context_pack_from_record(record);
    }
}

fn strip_history_context_pack_from_event(event: &mut C2NodeEvent) {
    if let C2NodeEvent::SessionRecordUpserted { record } = event {
        strip_history_context_pack_from_record(record);
    }
}

fn strip_history_context_pack_from_record(record: &mut crate::protocol::C2ManagedSessionRecord) {
    record.context_id = None;
    record.context = None;
}

fn strip_session_task_bindings_from_server_frame(frame: &mut C2ServerFrame) {
    match frame {
        C2ServerFrame::Reply(reply) => {
            if let Ok(routed) = reply.result.as_mut() {
                if let Ok(response) = routed.response.as_mut() {
                    strip_session_task_bindings_from_response(response);
                }
            }
        }
        C2ServerFrame::Event(event) => strip_session_task_binding_from_event(&mut event.event),
        C2ServerFrame::Challenge(_)
        | C2ServerFrame::Hello(_)
        | C2ServerFrame::Topology(_)
        | C2ServerFrame::Rejected(_) => {}
    }
}

fn strip_session_task_bindings_from_response(response: &mut C2NodeResponse) {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => {
            strip_session_task_bindings_from_snapshot(snapshot);
        }
        C2NodeResponse::Resync { snapshot, events, .. } => {
            strip_session_task_bindings_from_snapshot(snapshot);
            for event in events {
                strip_session_task_binding_from_event(&mut event.event);
            }
        }
        C2NodeResponse::SessionRecordUpdated { record }
        | C2NodeResponse::ProviderSessionIndexed { record }
        | C2NodeResponse::NativeSessionIndexed { record, .. }
        | C2NodeResponse::SessionRecordResumed { record, .. } => {
            record.task_binding = None;
        }
        _ => {}
    }
}

fn strip_session_task_bindings_from_snapshot(snapshot: &mut crate::protocol::C2NodeSnapshot) {
    for record in &mut snapshot.session_records {
        record.task_binding = None;
    }
}

fn strip_session_task_binding_from_event(event: &mut C2NodeEvent) {
    if let C2NodeEvent::SessionRecordUpserted { record } = event {
        record.task_binding = None;
    }
}

fn c2_response_contains_managed_worktree(response: &C2NodeResponse) -> bool {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => !snapshot.managed_worktrees.is_empty(),
        C2NodeResponse::Resync { snapshot, events, .. } => {
            !snapshot.managed_worktrees.is_empty()
                || events
                    .iter()
                    .any(|event| c2_event_is_managed_worktree(&event.event))
        }
        C2NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | C2NodeResponse::ManagedWorktreeCleanup { .. } => true,
        _ => false,
    }
}

fn c2_event_is_managed_worktree(event: &C2NodeEvent) -> bool {
    matches!(
        event,
        C2NodeEvent::ManagedWorktreeUpserted { .. }
            | C2NodeEvent::ManagedWorktreeRemoved { .. }
    )
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
        C2NodeResponse::HostDirectoriesBrowsed { listing } => {
            listing.directory.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
                || listing.parent.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
                || listing.entries.iter().any(|entry| entry.path.as_unix_bytes().is_some())
                || listing.next_after.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
        }
        C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::WorkspaceFileWritten { .. }
        | C2NodeResponse::WorkspaceFileCreated { .. }
        | C2NodeResponse::WorkspaceDirectoryCreated { .. }
        | C2NodeResponse::GitHistoryRead { .. }
        | C2NodeResponse::GitDiffRead { .. }
        | C2NodeResponse::DeliveryStageBegun { .. }
        | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
        | C2NodeResponse::DeliveryCommitted { .. }
        | C2NodeResponse::DeliveryStageAborted { .. } => false,
        C2NodeResponse::WorkspaceRegistered { workspace }
        | C2NodeResponse::StandaloneWorkspaceCreated { workspace } => {
            workspace.canonical_root.as_unix_bytes().is_some()
        }
        C2NodeResponse::WorktreeCreated { worktree, workspace } => {
            worktree.path.as_unix_bytes().is_some()
                || workspace.canonical_root.as_unix_bytes().is_some()
        }
        C2NodeResponse::WorktreeRemoved { target_root, .. } => target_root.as_unix_bytes().is_some(),
        C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::SpawnSpecAccepted { .. }
        | C2NodeResponse::Armed { .. }
        | C2NodeResponse::Spawned { .. }
        | C2NodeResponse::Activated { .. }
        | C2NodeResponse::Aborted { .. }
        | C2NodeResponse::ReplyChunkAccepted { .. }
        | C2NodeResponse::CallRejected { .. }
        | C2NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | C2NodeResponse::ManagedWorktreeCleanup { .. }
        | C2NodeResponse::SessionRecordUpdated { .. }
        | C2NodeResponse::ProviderSessionIndexed { .. }
        | C2NodeResponse::NativeSessionIndexed { .. }
        | C2NodeResponse::SessionRecordResumed { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::NativeSessionsCataloged { .. }
        | C2NodeResponse::NativeSessionsPaged { .. }
        | C2NodeResponse::NativeSessionPreviewed { .. }
        | C2NodeResponse::SessionRecordPreviewed { .. }
        | C2NodeResponse::HistoryDiscovered { .. }
        | C2NodeResponse::HistoryLoaded { .. }
        | C2NodeResponse::ContextPackExported { .. }
        | C2NodeResponse::ContextPackForgotten { .. }
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
        C2NodeEvent::HarnessMcpReadCall { .. }
        | C2NodeEvent::Control { .. }
        | C2NodeEvent::Observation { .. }
        | C2NodeEvent::ManagedObservation { .. }
        | C2NodeEvent::TerminalFrame { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::SessionRecordUpserted { .. }
        | C2NodeEvent::SessionRecordRemoved { .. }
        | C2NodeEvent::ManagedWorktreeUpserted { .. }
        | C2NodeEvent::ManagedWorktreeRemoved { .. }
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
    if matches!(&request.request, NodeRequest::SpawnSpec { spec }
        if spec.target.node_id != request.route.node_id)
        || matches!(&request.request, NodeRequest::SpawnManagedWorktree { request: managed }
            if managed.spawn_spec.target.node_id != request.route.node_id)
    {
        return DispatchStart::Immediate(Err(relay_failure(
            C2RelayFailureCode::RequestForbidden,
            "spawn target node does not match C2 route",
            None,
        )));
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
            CapabilityId::new(C2_WORKSPACE_FILE_WRITE_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_WORKSPACE_ENTRY_CREATE_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_GIT_READ_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_ID_OPEN_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_SPAWN_PROFILE_REVISION_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_TERMINAL_FRAME_EVENTS_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_WORKTREE_SELECTION_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_HISTORY_CONTEXT_PACK_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_NATIVE_SESSION_CATALOG_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_NATIVE_SESSION_INDEX_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_NATIVE_SESSION_PREVIEW_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_HOST_DIRECTORY_BROWSE_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_AGENT_PROGRESS_SNAPSHOT_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_SESSION_TASK_CORRELATION_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_OBSERVATION_EVENTS_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_OBSERVATION_MANAGED_TARGET_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY)
                .map_err(|error| authentication_frame_error(error.to_string()))?,
            CapabilityId::new(C2_HARNESS_MCP_READ_PROXY_CAPABILITY)
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
        NodeRequest::ActivateHarnessMcpReservation { record_id, session, .. }
        | NodeRequest::PutHarnessMcpReplyChunk { record_id, session, .. }
        | NodeRequest::RejectHarnessMcpCall { record_id, session, .. } => {
            !status_address_is_legacy(status, node_id, session)
                || !status_record_is_legacy(status, node_id, record_id)
        }
        NodeRequest::Resume { session, .. }
        | NodeRequest::Prompt { session, .. }
        | NodeRequest::Paste { session, .. }
        | NodeRequest::Input { session, .. }
        | NodeRequest::TerminalBytes { session, .. }
        | NodeRequest::TerminalControl { session, .. }
        | NodeRequest::DiscoverHistory { session, .. }
        | NodeRequest::LoadHistory { session, .. }
        | NodeRequest::ExportContextPack { session }
        | NodeRequest::Resize { session, .. }
        | NodeRequest::Interrupt { session }
        | NodeRequest::Stop { session, .. }
        | NodeRequest::Remove { session } => {
            !status_address_is_legacy(status, node_id, session)
        }
        NodeRequest::RenameSessionRecord { record_id, .. }
        | NodeRequest::SetSessionTask { record_id, .. }
        | NodeRequest::ResumeSessionRecord { record_id, .. }
        | NodeRequest::ForgetSessionRecord { record_id } => {
            !status_record_is_legacy(status, node_id, record_id)
        }
        NodeRequest::PreviewSessionRecord { record_id, .. } => {
            !status_record_is_legacy(status, node_id, record_id)
        }
        NodeRequest::ForgetContextPack { .. } => true,
        NodeRequest::IndexProviderSession { provider, .. } => !provider_id_is_legacy(provider),
        NodeRequest::CatalogNativeSessions { route, .. }
        | NodeRequest::PageNativeSessions { route, .. } => {
            !provider_id_is_legacy(&route.provider)
        }
        NodeRequest::PreviewNativeSession { selection, .. }
        | NodeRequest::IndexNativeSession { selection, .. } => {
            !provider_id_is_legacy(&selection.route.provider)
        }
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::BrowseHostDirectories { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::CreateStandaloneWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
        | NodeRequest::Spawn { .. }
        | NodeRequest::SpawnSpec { .. }
        | NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::CleanupManagedWorktree { .. }
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
    snapshot.session_records.retain_mut(project_legacy_record_provider);
}

fn context_receipt_provider_is_legacy(
    context: &gate4agent_node_protocol::ResolvedContextPackReceipt,
) -> bool {
    provider_id_is_legacy(&context.lineage.source_provider)
}

fn project_legacy_record_provider(
    record: &mut crate::protocol::C2ManagedSessionRecord,
) -> bool {
    if !provider_id_is_legacy(&record.provider) {
        return false;
    }
    if record
        .context
        .as_ref()
        .is_some_and(|context| !context_receipt_provider_is_legacy(context))
    {
        strip_history_context_pack_from_record(record);
    }
    true
}

fn project_legacy_spawn_provider(receipt: &mut ResolvedSpawnReceipt) -> bool {
    if !provider_id_is_legacy(&receipt.provider) {
        return false;
    }
    if receipt
        .context
        .as_ref()
        .is_some_and(|context| !context_receipt_provider_is_legacy(context))
    {
        receipt.context_id = None;
        receipt.context = None;
    }
    true
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
        | C2NodeResponse::ProviderSessionIndexed { record }
        | C2NodeResponse::SessionRecordResumed { record, .. } => {
            project_legacy_record_provider(record)
        }
        C2NodeResponse::NativeSessionIndexed { selection, record } => {
            let record_is_legacy = project_legacy_record_provider(record);
            provider_id_is_legacy(&selection.route.provider) && record_is_legacy
        }
        C2NodeResponse::SpawnSpecAccepted { receipt }
        | C2NodeResponse::Spawned { receipt, .. } => {
            project_legacy_spawn_provider(receipt)
        }
        C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt } => {
            project_legacy_spawn_provider(&mut receipt.spawn)
        }
        C2NodeResponse::ContextPackExported { context } => {
            context_receipt_provider_is_legacy(context)
        }
        C2NodeResponse::NativeSessionsCataloged { route, .. }
        | C2NodeResponse::NativeSessionsPaged { route, .. } => {
            provider_id_is_legacy(&route.provider)
        }
        C2NodeResponse::NativeSessionPreviewed { selection, .. } => {
            provider_id_is_legacy(&selection.route.provider)
        }
        C2NodeResponse::WorkspaceRegistered { workspace }
        | C2NodeResponse::StandaloneWorkspaceCreated { workspace }
        | C2NodeResponse::WorktreeCreated { workspace, .. } => {
            retain_legacy_workspace(workspace);
            true
        }
        C2NodeResponse::Armed { .. }
        | C2NodeResponse::Activated { .. }
        | C2NodeResponse::Aborted { .. }
        | C2NodeResponse::ReplyChunkAccepted { .. }
        | C2NodeResponse::CallRejected { .. }
        | C2NodeResponse::WorkspaceInspected { .. }
        | C2NodeResponse::DeliveryStageBegun { .. }
        | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
        | C2NodeResponse::DeliveryCommitted { .. }
        | C2NodeResponse::DeliveryStageAborted { .. }
        | C2NodeResponse::HostDirectoriesBrowsed { .. }
        | C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::WorkspaceFileWritten { .. }
        | C2NodeResponse::WorkspaceFileCreated { .. }
        | C2NodeResponse::WorkspaceDirectoryCreated { .. }
        | C2NodeResponse::GitHistoryRead { .. }
        | C2NodeResponse::GitDiffRead { .. }
        | C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::ManagedWorktreeCleanup { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::SessionRecordPreviewed { .. }
        | C2NodeResponse::HistoryDiscovered { .. }
        | C2NodeResponse::HistoryLoaded { .. }
        | C2NodeResponse::ContextPackForgotten { .. }
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
        C2NodeEvent::HarnessMcpReadCall { record_id, session, .. } => {
            address_is_legacy(status, node_id, snapshot, session)
                && snapshot.map_or_else(
                    || status_record_is_legacy(status, node_id, record_id),
                    |snapshot| snapshot.session_records.iter().any(|record| {
                        &record.record_id == record_id && provider_id_is_legacy(&record.provider)
                    }),
                )
        }
        C2NodeEvent::Control { address, .. }
        | C2NodeEvent::Observation { address, .. }
        | C2NodeEvent::TerminalFrame { address, .. } => {
            address_is_legacy(status, node_id, snapshot, address)
        }
        C2NodeEvent::ManagedObservation { .. } => false,
        C2NodeEvent::WorkspaceAdded { workspace } => {
            retain_legacy_workspace(workspace);
            true
        }
        C2NodeEvent::SessionRecordUpserted { record } => {
            project_legacy_record_provider(record)
        }
        C2NodeEvent::SessionRecordRemoved { record_id } => {
            record_is_legacy(status, node_id, snapshot, record_id)
        }
        C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::ManagedWorktreeUpserted { .. }
        | C2NodeEvent::ManagedWorktreeRemoved { .. }
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
        | C2NodeResponse::DeliveryStageBegun { .. }
        | C2NodeResponse::DeliveryBlobChunkAccepted { .. }
        | C2NodeResponse::DeliveryCommitted { .. }
        | C2NodeResponse::DeliveryStageAborted { .. }
        | C2NodeResponse::HostDirectoriesBrowsed { .. }
        | C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::WorkspaceFileWritten { .. }
        | C2NodeResponse::WorkspaceFileCreated { .. }
        | C2NodeResponse::WorkspaceDirectoryCreated { .. }
        | C2NodeResponse::GitHistoryRead { .. }
        | C2NodeResponse::GitDiffRead { .. }
        | C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::SpawnSpecAccepted { .. }
        | C2NodeResponse::Armed { .. }
        | C2NodeResponse::Spawned { .. }
        | C2NodeResponse::Activated { .. }
        | C2NodeResponse::Aborted { .. }
        | C2NodeResponse::ReplyChunkAccepted { .. }
        | C2NodeResponse::CallRejected { .. }
        | C2NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | C2NodeResponse::ManagedWorktreeCleanup { .. }
        | C2NodeResponse::SessionRecordUpdated { .. }
        | C2NodeResponse::ProviderSessionIndexed { .. }
        | C2NodeResponse::NativeSessionIndexed { .. }
        | C2NodeResponse::SessionRecordResumed { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::NativeSessionsCataloged { .. }
        | C2NodeResponse::NativeSessionsPaged { .. }
        | C2NodeResponse::NativeSessionPreviewed { .. }
        | C2NodeResponse::SessionRecordPreviewed { .. }
        | C2NodeResponse::HistoryDiscovered { .. }
        | C2NodeResponse::HistoryLoaded { .. }
        | C2NodeResponse::ContextPackExported { .. }
        | C2NodeResponse::ContextPackForgotten { .. }
        | C2NodeResponse::WorkspaceRegistered { .. }
        | C2NodeResponse::StandaloneWorkspaceCreated { .. }
        | C2NodeResponse::WorkspaceUnregistered { .. }
        | C2NodeResponse::WorktreeCreated { .. }
        | C2NodeResponse::WorktreeRemoved { .. }
        | C2NodeResponse::Accepted
        | C2NodeResponse::ShuttingDown => {}
    }
}

fn clear_server_frame_agent_progress(frame: &mut C2ServerFrame) {
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
        | C2NodeResponse::Resync { snapshot, .. } => snapshot.agent_progress.clear(),
        _ => {}
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::protocol::{
        AdapterContractRevision, AdapterFamily, AdapterId, C2ClientHello, C2GitSnapshot,
        C2SessionSnapshot, C2SessionStatus, C2WorkspaceInspection, C2WorkspaceSnapshot,
        C2_API_VERSION, HostDirectoryEntry, HostDirectoryListing, NodeFreshness, ObservedNode,
        OpaqueHostPath,
        ProviderAdapterContractSupport, ProviderContractRevision, ProviderContractSupport,
        RepositoryPath, ResolvedSpawnReceipt, SlimManagedSessionRecord, SpawnDeadlineMs,
        SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverrides, SpawnProfileId,
        SpawnProfileRevision, SpawnPromptMetadata, SpawnRequiredCapabilities,
        SpawnResolutionProvenance, SpawnSpec, SpawnTarget, WorkspaceFileContent,
        WorkspaceFileRead,
    };

    #[test]
    fn harness_mcp_c2_control_gate_preserves_controller_semantics() {
        let request = NodeRequest::AbortHarnessMcpReservation {
            reservation_id: gate4agent_node_protocol::HarnessMcpReservationId::new(
                format!("hmcpres_{}", "a".repeat(24)),
            ).unwrap(),
            activation_digest: gate4agent_node_protocol::HarnessMcpActivationDigest::new(
                format!("sha256:{}", "b".repeat(64)),
            ).unwrap(),
        };
        assert!(unnegotiated_request_failure(
            &request,
            NegotiatedPathCapabilities::default(),
        ).is_some());
        let capabilities = NegotiatedPathCapabilities {
            harness_mcp_read_proxy: true,
            spawn_spec_defaults_overrides: true,
            ..NegotiatedPathCapabilities::default()
        };
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());
        assert!(!is_read_only_request(&request));
        assert!(c2_control_compatibility_support().unwrap().capabilities.iter().any(|capability| {
            capability.as_str() == C2_HARNESS_MCP_READ_PROXY_CAPABILITY
        }));
    }
    use gate4agent_node_protocol::{
        GitStatusEntry, ManagedSessionState, SessionAddress, SessionKey, SessionMode,
        SessionRecordId, WorkspaceEntry, WorkspaceEntryKind, WorkspaceId,
    };
    use gate4agent_types::{
        AgentInstanceId, ProviderActivity, SessionGeneration, TerminalFrame, TerminalSize,
        TransportKind,
    };
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
                observation_support: None,
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
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            agent_progress: Vec::new(),
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

    fn spawn_spec(node_id: &str) -> SpawnSpec {
        SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new(node_id).unwrap(),
                workspace_id: WorkspaceId::new("repo").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("default").unwrap(),
            expected_profile_revision: SpawnProfileRevision::new("r1").unwrap(),
            overrides: SpawnOverrides::default(),
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("spawn-1").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
        }
    }

    fn spawn_receipt() -> ResolvedSpawnReceipt {
        ResolvedSpawnReceipt {
            incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            session: address(7),
            target: spawn_spec("node-a").target,
            profile_id: SpawnProfileId::new("default").unwrap(),
            profile_revision: SpawnProfileRevision::new("r1").unwrap(),
            provider: AgentId::new("claude").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            prompt: SpawnPromptMetadata { present: false, byte_len: 0 },
            bundle_id: None,
            bundle: None,
            context_id: None,
            context: None,
            environment_profile: None,
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("spawn-1").unwrap(),
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
            harness_mcp_proxy: None,
        }
    }

    fn context_receipt() -> crate::protocol::ResolvedContextPackReceipt {
        crate::protocol::ResolvedContextPackReceipt {
            id: crate::protocol::SpawnContextId::new("context-1").unwrap(),
            digest: crate::protocol::SpawnContextDigest::new(format!(
                "sha256:{}",
                "a".repeat(64),
            ))
            .unwrap(),
            lineage: crate::protocol::ContextPackLineageReceipt {
                source_node_id: NodeId::new("node-a").unwrap(),
                source_session: address(7),
                source_provider: AgentId::new("claude").unwrap(),
            },
            source_message_count: 2,
            retained_message_count: 2,
            byte_len: 32,
            truncated: false,
        }
    }

    fn terminal_frame(sequence: u64) -> TerminalFrame {
        TerminalFrame {
            sequence,
            size: TerminalSize { rows: 24, columns: 80 },
            cursor_row: 1,
            cursor_column: 2,
            contents: "ready".to_owned(),
            formatted: b"ready".to_vec(),
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: Default::default(),
        }
    }

    fn c2_session(provider: &str, instance_id: u64) -> C2SessionSnapshot {
        C2SessionSnapshot {
            instance_id: AgentInstanceId(instance_id),
            agent_id: AgentId::new(provider).unwrap(),
            transport: TransportKind::Pty,
            generation: SessionGeneration(1),
            status: C2SessionStatus::Running,
            pending_operation: None,
            pending_input: None,
            process_id: Some(7),
            terminal_size: Some(TerminalSize { rows: 24, columns: 80 }),
            terminal_frame: Some(terminal_frame(1)),
            provider_activity: ProviderActivity::Idle,
            provider_interaction_pending: false,
            provider_identity_present: true,
        }
    }

    fn agent_progress(instance_id: u64) -> crate::protocol::SessionAgentProgress {
        crate::protocol::SessionAgentProgress {
            address: address(instance_id),
            progress: crate::protocol::AgentProgressV1 {
                provider_sequence: 9,
                activity: ProviderActivity::Working,
                completed_turns: 2,
                usage: None,
                current: crate::protocol::AgentProgressCurrentV1::Working,
                active_tool_labels: vec!["shell".to_owned()],
                active_tool_count: 1,
                attention: None,
                subagent_count: 0,
                last_event_kind: Some(crate::protocol::AgentProgressEventKindV1::ToolStarted),
                gap_count: 0,
                stale: false,
                truncated: false,
            },
        }
    }

    fn terminal_event_frame(instance_id: u64, sequence: u64) -> C2ServerFrame {
        C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                sequence,
            },
            event: C2NodeEvent::TerminalFrame {
                address: address(instance_id),
                frame: terminal_frame(sequence),
            },
        })
    }

    fn terminal_resync_frame(instance_id: u64, sequence: u64) -> C2ServerFrame {
        C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(4),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                response: Ok(C2NodeResponse::Resync {
                    event_sequence: sequence,
                    oldest_available_sequence: 1,
                    snapshot: crate::protocol::C2NodeSnapshot {
                        node_id: NodeId::new("node-a").unwrap(),
                        enabled_providers: Vec::new(),
                        provider_runtime_statuses: Default::default(),
                        workspaces: Vec::new(),
                        session_records: Vec::new(),
                        agent_progress: Vec::new(),
                        managed_worktrees: Vec::new(),
                        launch_inventory: None,
                        observation_support: None,
                    },
                    events: vec![crate::protocol::C2NodeEventEnvelope {
                        sequence,
                        event: C2NodeEvent::TerminalFrame {
                            address: address(instance_id),
                            frame: terminal_frame(sequence),
                        },
                    }],
                }),
            }),
        })
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
            environment_profile: None,
            bundle: None,
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
                CapabilityId::new(C2_WORKSPACE_FILE_WRITE_CAPABILITY).unwrap(),
                CapabilityId::new(C2_WORKSPACE_ENTRY_CREATE_CAPABILITY).unwrap(),
                CapabilityId::new(C2_GIT_READ_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_ID_OPEN_CAPABILITY).unwrap(),
                CapabilityId::new(C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap(),
                CapabilityId::new(C2_SPAWN_PROFILE_REVISION_CAPABILITY).unwrap(),
                CapabilityId::new(C2_TERMINAL_FRAME_EVENTS_CAPABILITY).unwrap(),
                CapabilityId::new(C2_WORKTREE_SELECTION_CAPABILITY).unwrap(),
                CapabilityId::new(C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY).unwrap(),
                CapabilityId::new(C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY).unwrap(),
                CapabilityId::new(C2_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap(),
                CapabilityId::new(C2_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap(),
                CapabilityId::new(C2_NATIVE_SESSION_CATALOG_CAPABILITY).unwrap(),
                CapabilityId::new(C2_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY).unwrap(),
                CapabilityId::new(C2_NATIVE_SESSION_INDEX_CAPABILITY).unwrap(),
                CapabilityId::new(C2_NATIVE_SESSION_PREVIEW_CAPABILITY).unwrap(),
                CapabilityId::new(C2_HOST_DIRECTORY_BROWSE_CAPABILITY).unwrap(),
                CapabilityId::new(C2_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY).unwrap(),
                CapabilityId::new(C2_AGENT_PROGRESS_SNAPSHOT_CAPABILITY).unwrap(),
                CapabilityId::new(C2_SESSION_TASK_CORRELATION_CAPABILITY).unwrap(),
                CapabilityId::new(C2_OBSERVATION_EVENTS_CAPABILITY).unwrap(),
                CapabilityId::new(C2_OBSERVATION_MANAGED_TARGET_CAPABILITY).unwrap(),
                CapabilityId::new(C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY).unwrap(),
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
    fn c2_downstream_agent_progress_is_stripped_from_snapshot_and_resync() {
        let mut resync = terminal_resync_frame(7, 11);
        let C2ServerFrame::Reply(reply) = &mut resync else { unreachable!() };
        let routed = reply.result.as_mut().unwrap();
        let response = routed.response.as_mut().unwrap();
        let C2NodeResponse::Resync { snapshot, .. } = response else { unreachable!() };
        snapshot.agent_progress = vec![agent_progress(7)];
        clear_server_frame_agent_progress(&mut resync);
        let C2ServerFrame::Reply(reply) = &resync else { unreachable!() };
        let C2NodeResponse::Resync { snapshot, .. } = reply
            .result
            .as_ref()
            .unwrap()
            .response
            .as_ref()
            .unwrap()
        else {
            unreachable!()
        };
        assert!(snapshot.agent_progress.is_empty());

        let mut snapshot_frame = terminal_resync_frame(7, 12);
        let C2ServerFrame::Reply(reply) = &mut snapshot_frame else { unreachable!() };
        let response = reply
            .result
            .as_mut()
            .unwrap()
            .response
            .as_mut()
            .unwrap();
        let C2NodeResponse::Resync { snapshot, .. } = response else { unreachable!() };
        snapshot.agent_progress = vec![agent_progress(7)];
        *response = C2NodeResponse::Snapshot {
            event_sequence: 12,
            controller: None,
            snapshot: snapshot.clone(),
        };
        clear_server_frame_agent_progress(&mut snapshot_frame);
        let C2ServerFrame::Reply(reply) = &snapshot_frame else { unreachable!() };
        let C2NodeResponse::Snapshot { snapshot, .. } = reply
            .result
            .as_ref()
            .unwrap()
            .response
            .as_ref()
            .unwrap()
        else {
            unreachable!()
        };
        assert!(snapshot.agent_progress.is_empty());
    }

    #[test]
    fn c2_observation_events_are_stripped_without_downstream_capability() {
        let observation = || C2NodeEvent::Observation {
            address: address(7),
            observation: crate::protocol::ObservationV1 {
                source_sequence: 3,
                observed_at_unix_ms: Some(5),
                evidence: crate::protocol::ObservationEvidenceV1::StructuredProvider,
                kind: crate::protocol::ObservationKindV1::Working,
                truncated: false,
            },
        };
        let mut direct = C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                sequence: 3,
            },
            event: observation(),
        });
        assert!(!project_server_frame_observation_events(
            &mut direct,
            false,
            false,
            false,
        ));

        let mut resync = terminal_resync_frame(7, 11);
        let C2ServerFrame::Reply(reply) = &mut resync else { unreachable!() };
        let C2NodeResponse::Resync { events, .. } = reply
            .result
            .as_mut()
            .unwrap()
            .response
            .as_mut()
            .unwrap()
        else {
            unreachable!()
        };
        events.push(crate::protocol::C2NodeEventEnvelope {
            sequence: 12,
            event: observation(),
        });
        assert!(project_server_frame_observation_events(
            &mut resync,
            false,
            false,
            false,
        ));
        let C2ServerFrame::Reply(reply) = resync else { unreachable!() };
        let C2NodeResponse::Resync { events, .. } = reply
            .result
            .unwrap()
            .response
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event, C2NodeEvent::TerminalFrame { .. }));

        let mut detail = C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                sequence: 13,
            },
            event: C2NodeEvent::Observation {
                address: address(7),
                observation: crate::protocol::ObservationV1 {
                    source_sequence: 4,
                    observed_at_unix_ms: Some(6),
                    evidence: crate::protocol::ObservationEvidenceV1::StructuredProvider,
                    kind: crate::protocol::ObservationKindV1::FileChanged {
                        path: Some("src/lib.rs".to_owned()),
                    },
                    truncated: false,
                },
            },
        });
        assert!(!project_server_frame_observation_events(
            &mut detail,
            true,
            false,
            false,
        ));
    }

    #[test]
    fn c2_managed_observation_is_capability_gated_and_legacy_stripped() {
        let managed = || C2NodeEvent::ManagedObservation {
            record_id: gate4agent_node_protocol::SessionRecordId::new("record-a").unwrap(),
            observation: crate::protocol::ObservationV1 {
                source_sequence: 3,
                observed_at_unix_ms: Some(5),
                evidence: crate::protocol::ObservationEvidenceV1::ManagedHook,
                kind: crate::protocol::ObservationKindV1::Working,
                truncated: false,
            },
        };
        let mut without_target = C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                sequence: 3,
            },
            event: managed(),
        });
        assert!(!project_server_frame_observation_events(
            &mut without_target,
            true,
            false,
            false,
        ));

        let mut selected = C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                sequence: 3,
            },
            event: managed(),
        });
        assert!(project_server_frame_observation_events(
            &mut selected,
            true,
            true,
            false,
        ));

        let mut legacy = managed();
        assert!(!project_legacy_event(
            &mut legacy,
            &NodeId::new("node-a").unwrap(),
            &status(NodeTransportState::Online, Some(NodeIncarnationId::from_bytes([9; 16]))),
            None,
        ));
    }

    #[test]
    fn c2_observation_support_projection_matrix_covers_hello_topology_snapshot_and_resync() {
        let full = crate::protocol::C2ObservationSupport {
            events: true,
            managed_target: true,
            workflow_detail: true,
        };
        let mut source_status = status(
            NodeTransportState::Online,
            Some(NodeIncarnationId::from_bytes([7; 16])),
        );
        source_status
            .nodes
            .values_mut()
            .next()
            .unwrap()
            .observation_support = Some(full);

        for (include_events, include_managed, include_detail, expected) in [
            (false, false, false, None),
            (
                true,
                false,
                false,
                Some(crate::protocol::C2ObservationSupport {
                    events: true,
                    managed_target: false,
                    workflow_detail: false,
                }),
            ),
            (
                true,
                true,
                false,
                Some(crate::protocol::C2ObservationSupport {
                    events: true,
                    managed_target: true,
                    workflow_detail: false,
                }),
            ),
            (
                true,
                false,
                true,
                Some(crate::protocol::C2ObservationSupport {
                    events: true,
                    managed_target: false,
                    workflow_detail: true,
                }),
            ),
            (true, true, true, Some(full)),
        ] {
            let mut hello = C2ServerFrame::Hello(C2Hello {
                protocol_version: C2_CONTROL_PROTOCOL_VERSION,
                connection_id: 1,
                status: source_status.clone(),
                compatibility: None,
            });
            assert!(project_server_frame_observation_events(
                &mut hello,
                include_events,
                include_managed,
                include_detail,
            ));
            let C2ServerFrame::Hello(hello) = hello else { unreachable!() };
            assert_eq!(
                hello.status.nodes.values().next().unwrap().observation_support,
                expected,
            );

            let mut topology = C2ServerFrame::Topology(C2Topology::from_status(&source_status));
            assert!(project_server_frame_observation_events(
                &mut topology,
                include_events,
                include_managed,
                include_detail,
            ));
            let C2ServerFrame::Topology(topology) = topology else { unreachable!() };
            assert_eq!(topology.nodes[0].observation_support, expected);

            let mut resync = terminal_resync_frame(7, 11);
            let C2ServerFrame::Reply(reply) = &mut resync else { unreachable!() };
            let C2NodeResponse::Resync { snapshot, .. } = reply
                .result
                .as_mut()
                .unwrap()
                .response
                .as_mut()
                .unwrap()
            else {
                unreachable!()
            };
            snapshot.observation_support = Some(full);
            let mut snapshot_frame = resync.clone();
            let C2ServerFrame::Reply(reply) = &mut snapshot_frame else { unreachable!() };
            let response = reply.result.as_mut().unwrap().response.as_mut().unwrap();
            let C2NodeResponse::Resync { event_sequence, snapshot, .. } = response else {
                unreachable!()
            };
            *response = C2NodeResponse::Snapshot {
                event_sequence: *event_sequence,
                controller: None,
                snapshot: snapshot.clone(),
            };

            for frame in [&mut snapshot_frame, &mut resync] {
                assert!(project_server_frame_observation_events(
                    frame,
                    include_events,
                    include_managed,
                    include_detail,
                ));
                let C2ServerFrame::Reply(reply) = frame else { unreachable!() };
                let support = match reply
                    .result
                    .as_ref()
                    .unwrap()
                    .response
                    .as_ref()
                    .unwrap()
                {
                    C2NodeResponse::Snapshot { snapshot, .. }
                    | C2NodeResponse::Resync { snapshot, .. } => snapshot.observation_support,
                    _ => unreachable!(),
                };
                assert_eq!(support, expected);
            }
        }
    }

    #[test]
    fn c2_downstream_task_bindings_are_stripped_from_resync_events_and_replies() {
        let record = crate::protocol::C2ManagedSessionRecord {
            record_id: SessionRecordId::new("record-task").unwrap(),
            display_name: "task".to_owned(),
            provider: AgentId::new("codex").unwrap(),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("repo").unwrap(),
            active_session: None,
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: Some(gate4agent_node_protocol::SessionTaskBindingV1 {
                revision: 1,
                task_id: Some(gate4agent_node_protocol::TaskId::from_nonce([1; 12])),
                changed_at_unix_ms: 1,
            }),
            provider_identity_present: false,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let mut resync = terminal_resync_frame(7, 11);
        let C2ServerFrame::Reply(reply) = &mut resync else { unreachable!() };
        let C2NodeResponse::Resync { snapshot, events, .. } = reply
            .result.as_mut().unwrap().response.as_mut().unwrap()
        else { unreachable!() };
        snapshot.session_records.push(record.clone());
        events.push(crate::protocol::C2NodeEventEnvelope {
            sequence: 12,
            event: C2NodeEvent::SessionRecordUpserted { record: record.clone() },
        });
        strip_session_task_bindings_from_server_frame(&mut resync);
        let C2ServerFrame::Reply(reply) = &resync else { unreachable!() };
        let C2NodeResponse::Resync { snapshot, events, .. } = reply
            .result.as_ref().unwrap().response.as_ref().unwrap()
        else { unreachable!() };
        assert!(snapshot.session_records[0].task_binding.is_none());
        assert!(matches!(&events.last().unwrap().event,
            C2NodeEvent::SessionRecordUpserted { record } if record.task_binding.is_none()));

        let mut direct = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(1),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
                response: Ok(C2NodeResponse::SessionRecordUpdated { record }),
            }),
        });
        strip_session_task_bindings_from_server_frame(&mut direct);
        let C2ServerFrame::Reply(reply) = direct else { unreachable!() };
        let C2NodeResponse::SessionRecordUpdated { record } =
            reply.result.unwrap().response.unwrap()
        else { unreachable!() };
        assert!(record.task_binding.is_none());
    }

    #[test]
    fn spawn_profile_revision_is_advertised_and_rejected_before_node_relay_on_all_spawn_paths() {
        let support = c2_control_compatibility_support().unwrap();
        assert!(support.capabilities.iter().any(|capability| {
            capability.as_str() == C2_SPAWN_PROFILE_REVISION_CAPABILITY
        }));

        let spec = spawn_spec("node-a");
        let reservation_id = gate4agent_node_protocol::HarnessMcpReservationId::new(
            format!("hmcpres_{}", "a".repeat(24)),
        ).unwrap();
        let activation_digest = gate4agent_node_protocol::HarnessMcpActivationDigest::new(
            format!("sha256:{}", "b".repeat(64)),
        ).unwrap();
        let deadline_unix_ms = unix_ms().saturating_add(60_000);
        let requests = [
            NodeRequest::SpawnSpec { spec: spec.clone() },
            NodeRequest::SpawnManagedWorktree {
                request: ManagedWorktreeSpawnRequest {
                    spawn_spec: spec.clone(),
                    worktree_profile_id:
                        gate4agent_node_protocol::WorktreeProfileId::new("review").unwrap(),
                },
            },
            NodeRequest::ArmHarnessMcpReservation {
                reservation_id: reservation_id.clone(),
                activation_digest: activation_digest.clone(),
                spawn_spec: spec.clone(),
                expires_at_unix_ms: deadline_unix_ms,
            },
            NodeRequest::SpawnSpecWithHarnessMcp {
                reservation_id,
                activation_digest,
                spec,
                deadline_unix_ms,
            },
        ];
        let capabilities = NegotiatedPathCapabilities {
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            spawn_profile_revision: false,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            session_bundle_materialization: true,
            harness_mcp_read_proxy: true,
            ..NegotiatedPathCapabilities::default()
        };
        for request in &requests {
            assert!(matches!(
                unnegotiated_request_failure(request, capabilities),
                Some(C2RelayFailure {
                    code: C2RelayFailureCode::RequestForbidden,
                    ref message,
                    ..
                }) if message ==
                    "spawn profile revision capability was not negotiated with C2"
            ));
        }
    }

    #[test]
    fn c2_spawn_spec_gate_rejects_unnegotiated_request() {
        let request = NodeRequest::SpawnSpec { spec: spawn_spec("node-a") };
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            workspace_file_write: true,
            workspace_entry_create: true,
            git_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            spawn_profile_revision: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());
        capabilities.provider_ids_open = false;
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "open provider IDs require negotiated C2 capability"
        ));
        let mut explicit_legacy = spawn_spec("node-a");
        explicit_legacy.overrides.provider =
            gate4agent_node_protocol::SpawnOverride::Set { value: AgentId::new("claude").unwrap() };
        assert!(unnegotiated_request_failure(
            &NodeRequest::SpawnSpec { spec: explicit_legacy },
            capabilities,
        ).is_none());
        capabilities.provider_ids_open = true;
        capabilities.spawn_spec_defaults_overrides = false;
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "request capability was not negotiated with C2"
        ));

        capabilities.spawn_spec_defaults_overrides = true;
        capabilities.worktree_selection = false;
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());
        let mut worktree_spec = spawn_spec("node-a");
        worktree_spec.target.worktree_id =
            Some(gate4agent_node_protocol::WorkspaceId::new("review-tree").unwrap());
        assert!(matches!(
            unnegotiated_request_failure(
                &NodeRequest::SpawnSpec { spec: worktree_spec },
                capabilities,
            ),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "worktree selection capability was not negotiated with C2"
        ));

        capabilities.worktree_selection = true;
        capabilities.child_environment_profile = false;
        let mut explicit_environment = spawn_spec("node-a");
        explicit_environment.overrides.environment_profile_id =
            gate4agent_node_protocol::SpawnOverride::Set {
                value: crate::protocol::SpawnEnvironmentProfileId::new("local-default")
                    .unwrap(),
            };
        assert!(matches!(
            unnegotiated_request_failure(
                &NodeRequest::SpawnSpec {
                    spec: explicit_environment,
                },
                capabilities,
            ),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "child environment profile capability was not negotiated with C2"
        ));
        let inherited_environment = spawn_spec("node-a");
        assert!(matches!(
            unnegotiated_request_failure(
                &NodeRequest::SpawnSpec {
                    spec: inherited_environment.clone(),
                },
                capabilities,
            ),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "child environment profile capability was not negotiated with C2"
        ));
        let managed_environment = NodeRequest::SpawnManagedWorktree {
            request: crate::protocol::ManagedWorktreeSpawnRequest {
                spawn_spec: inherited_environment,
                worktree_profile_id:
                    crate::protocol::WorktreeProfileId::new("review").unwrap(),
            },
        };
        assert!(matches!(
            unnegotiated_request_failure(&managed_environment, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "child environment profile capability was not negotiated with C2"
        ));
        let mut cleared_environment = spawn_spec("node-a");
        cleared_environment.overrides.environment_profile_id =
            gate4agent_node_protocol::SpawnOverride::Clear;
        assert!(unnegotiated_request_failure(
            &NodeRequest::SpawnSpec {
                spec: cleared_environment,
            },
            capabilities,
        )
        .is_none());
    }

    #[test]
    fn c2_session_bundle_gate_requires_inherit_or_set_but_not_clear() {
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: false,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        let inherited = spawn_spec("node-a");
        assert!(matches!(
            unnegotiated_request_failure(
                &NodeRequest::SpawnSpec {
                    spec: inherited.clone(),
                },
                capabilities,
            ),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "session bundle materialization capability was not negotiated with C2"
        ));

        let mut cleared = inherited;
        cleared.overrides.bundle_id = gate4agent_node_protocol::SpawnOverride::Clear;
        assert!(unnegotiated_request_failure(
            &NodeRequest::SpawnSpec { spec: cleared },
            capabilities,
        )
        .is_none());

        capabilities.session_bundle_materialization = true;
        assert!(unnegotiated_request_failure(
            &NodeRequest::SpawnSpec { spec: spawn_spec("node-a") },
            capabilities,
        )
        .is_none());
    }

    #[test]
    fn c2_history_context_pack_gate_and_legacy_projection_are_fail_closed() {
        let request = NodeRequest::DiscoverHistory {
            session: address(7),
            limit: 1,
        };
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: false,
            native_session_catalog: false,
            native_session_catalog_paging: false,
            native_session_index: false,
            native_session_preview: false,
            terminal_frame_events: true,
            ..Default::default()
        };
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "request capability was not negotiated with C2"
        ));
        capabilities.history_context_pack = true;
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());

        let invalid = NodeRequest::DiscoverHistory {
            session: address(7),
            limit: 0,
        };
        assert!(matches!(
            unnegotiated_request_failure(&invalid, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "invalid history context pack request"
        ));

        capabilities.provider_ids_open = false;
        let forget = NodeRequest::ForgetContextPack {
            context_id: gate4agent_node_protocol::SpawnContextId::new("context-a").unwrap(),
        };
        assert!(matches!(
            unnegotiated_request_failure(&forget, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ref message,
                ..
            }) if message == "open provider IDs require negotiated C2 capability"
        ));
        capabilities.provider_ids_open = true;
        assert!(unnegotiated_request_failure(&forget, capabilities).is_none());

        let context = context_receipt();
        let mut receipt = spawn_receipt();
        receipt.context_id = Some(context.id.clone());
        receipt.context = Some(context);
        let mut nested = C2NodeResponse::SpawnSpecAccepted { receipt };
        assert!(strip_history_context_pack_from_response(&mut nested));
        let C2NodeResponse::SpawnSpecAccepted { receipt } = nested else {
            unreachable!("constructed spawn response");
        };
        assert!(receipt.context_id.is_none());
        assert!(receipt.context.is_none());

        let mut standalone = C2NodeResponse::HistoryLoaded {
            session: address(7),
            session_id: "session-1".to_owned(),
            message_count: 2,
            completed_turn_count: None,
        };
        assert!(!strip_history_context_pack_from_response(&mut standalone));
        let mut failure = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(9),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Err(C2NodeFailure {
                    code: NodeFailureCode::UnknownContextPack,
                    message: "context pack unavailable".to_owned(),
                }),
            }),
        });
        assert!(!project_server_frame_without_history_context_pack(
            &mut failure,
        ));
    }

    #[test]
    fn managed_worktree_partial_capability_intersections_fail_closed() {
        let request = NodeRequest::SpawnManagedWorktree {
            request: crate::protocol::ManagedWorktreeSpawnRequest {
                spawn_spec: spawn_spec("node-a"),
                worktree_profile_id:
                    crate::protocol::WorktreeProfileId::new("review").unwrap(),
            },
        };
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: false,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        assert!(unnegotiated_request_failure(&request, capabilities).is_some());
        capabilities.managed_worktree_lifecycle = true;
        capabilities.worktree_selection = false;
        assert!(unnegotiated_request_failure(&request, capabilities).is_some());
        capabilities.worktree_selection = true;
        capabilities.spawn_spec_defaults_overrides = false;
        assert!(unnegotiated_request_failure(&request, capabilities).is_some());

        let cleanup = NodeRequest::CleanupManagedWorktree {
            lease_id: crate::protocol::ManagedWorktreeLeaseId::new("lease-a").unwrap(),
        };
        capabilities.spawn_spec_defaults_overrides = false;
        assert!(unnegotiated_request_failure(&cleanup, capabilities).is_none());
        capabilities.worktree_selection = false;
        assert!(unnegotiated_request_failure(&cleanup, capabilities).is_some());

        let event = C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                sequence: 1,
            },
            event: C2NodeEvent::ManagedWorktreeRemoved {
                lease_id: crate::protocol::ManagedWorktreeLeaseId::new("lease-a").unwrap(),
            },
        });
        assert!(server_frame_contains_managed_worktree(&event));
        assert!(server_frame_contains_worktree_selection_response(&event));
    }

    #[test]
    fn spawn_spec_route_target_mismatch_is_rejected_before_relay_lookup() {
        let incarnation_id = NodeIncarnationId::from_bytes([7; 16]);
        let (_status_tx, status_rx) = watch::channel(Arc::new(status(
            NodeTransportState::Online,
            Some(incarnation_id),
        )));
        let request = crate::protocol::RoutedNodeRequest {
            route: crate::protocol::NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: incarnation_id,
            },
            request: NodeRequest::SpawnSpec { spec: spawn_spec("node-b") },
        };
        let DispatchStart::Immediate(Err(failure)) = dispatch_start(
            1,
            request,
            &BTreeMap::new(),
            &status_rx,
        ) else {
            panic!("route mismatch reached relay lookup");
        };
        assert_eq!(failure.code, C2RelayFailureCode::RequestForbidden);
        assert_eq!(failure.message, "spawn target node does not match C2 route");
    }

    #[tokio::test]
    async fn host_directory_browse_is_capability_gated_and_routes_exact_incarnation() {
        let incarnation_id = NodeIncarnationId::from_bytes([21; 16]);
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        let request = NodeRequest::BrowseHostDirectories {
            directory: Some(OpaqueHostPath::utf8(r"C:\Users".to_owned()).unwrap()),
            after: Some(OpaqueHostPath::utf8(r"C:\Users\Public".to_owned()).unwrap()),
        };
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());
        capabilities.host_directory_browse = false;
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ..
            })
        ));
        assert!(node_request_contains_opaque_unix_path(
            &NodeRequest::BrowseHostDirectories {
                directory: Some(OpaqueHostPath::unix_bytes(b"/srv/\xff".to_vec()).unwrap()),
                after: None,
            },
        ));
        assert!(node_request_contains_opaque_unix_path(
            &NodeRequest::BrowseHostDirectories {
                directory: None,
                after: Some(OpaqueHostPath::unix_bytes(b"/srv/\xfe".to_vec()).unwrap()),
            },
        ));

        let (commands_tx, mut commands_rx) = mpsc::channel(1);
        let (releases_tx, _releases_rx) = mpsc::channel(1);
        let (force_disconnect, _force_disconnect_rx) = watch::channel(0);
        let relays = BTreeMap::from([(
            NodeId::new("node-a").unwrap(),
            RelayEndpoint {
                commands: commands_tx,
                releases: releases_tx,
                force_disconnect,
            },
        )]);
        let (_status_tx, status_rx) = watch::channel(Arc::new(status(
            NodeTransportState::Online,
            Some(incarnation_id),
        )));
        let routed = crate::protocol::RoutedNodeRequest {
            route: crate::protocol::NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: incarnation_id,
            },
            request: request.clone(),
        };
        assert!(matches!(dispatch_start(77, routed, &relays, &status_rx), DispatchStart::Pending(_)));
        let RelayCommand::Request {
            operator_connection_id,
            expected_incarnation_id,
            request: relayed,
            ..
        } = commands_rx.recv().await.unwrap();
        assert_eq!(operator_connection_id, 77);
        assert_eq!(expected_incarnation_id, incarnation_id);
        assert_eq!(relayed, request);

        let frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(9),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id,
                response: Ok(C2NodeResponse::HostDirectoriesBrowsed {
                    listing: HostDirectoryListing {
                        directory: None,
                        parent: None,
                        entries: vec![HostDirectoryEntry {
                            path: OpaqueHostPath::utf8(r"C:\Users".to_owned()).unwrap(),
                            display_name: "Users".to_owned(),
                            is_link: false,
                        }],
                        next_after: None,
                        incomplete: false,
                    },
                }),
            }),
        });
        assert!(server_frame_contains_host_directory_browse(&frame));
    }

    #[tokio::test]
    async fn standalone_workspace_is_capability_gated_routed_and_projected_exactly() {
        let incarnation_id = NodeIncarnationId::from_bytes([22; 16]);
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        let request = NodeRequest::CreateStandaloneWorkspace {
            workspace_id: WorkspaceId::new("standalone").unwrap(),
            root: OpaqueHostPath::utf8(r"C:\standalone".to_owned()).unwrap(),
            initial_branch: Some("main".to_owned()),
        };
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());
        capabilities.standalone_workspace_lifecycle = false;
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ..
            })
        ));
        assert!(node_request_contains_opaque_unix_path(
            &NodeRequest::CreateStandaloneWorkspace {
                workspace_id: WorkspaceId::new("opaque").unwrap(),
                root: OpaqueHostPath::unix_bytes(b"/srv/standalone".to_vec()).unwrap(),
                initial_branch: None,
            },
        ));

        let (commands_tx, mut commands_rx) = mpsc::channel(1);
        let (releases_tx, _releases_rx) = mpsc::channel(1);
        let (force_disconnect, _force_disconnect_rx) = watch::channel(0);
        let relays = BTreeMap::from([(
            NodeId::new("node-a").unwrap(),
            RelayEndpoint {
                commands: commands_tx,
                releases: releases_tx,
                force_disconnect,
            },
        )]);
        let (_status_tx, status_rx) = watch::channel(Arc::new(status(
            NodeTransportState::Online,
            Some(incarnation_id),
        )));
        let routed = crate::protocol::RoutedNodeRequest {
            route: crate::protocol::NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: incarnation_id,
            },
            request: request.clone(),
        };
        assert!(matches!(dispatch_start(78, routed, &relays, &status_rx), DispatchStart::Pending(_)));
        let RelayCommand::Request {
            operator_connection_id,
            expected_incarnation_id,
            request: relayed,
            ..
        } = commands_rx.recv().await.unwrap();
        assert_eq!(operator_connection_id, 78);
        assert_eq!(expected_incarnation_id, incarnation_id);
        assert_eq!(relayed, request);

        let workspace = C2WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("standalone").unwrap(),
            canonical_root: OpaqueHostPath::utf8(r"C:\standalone".to_owned()).unwrap(),
            sessions: Vec::new(),
            worktree_service_mode: Some(crate::protocol::WorktreeServiceMode::Manual),
            managed_worktree_profiles: Some(crate::protocol::WorktreeProfileInventory {
                profiles: Vec::new(),
            }),
        };
        let mut frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(10),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id,
                response: Ok(C2NodeResponse::StandaloneWorkspaceCreated {
                    workspace: workspace.clone(),
                }),
            }),
        });
        assert!(server_frame_contains_standalone_workspace_lifecycle(&frame));
        project_server_frame_launch_inventory(&mut frame, true, false, true);
        let C2ServerFrame::Reply(reply) = frame else { unreachable!() };
        let C2NodeResponse::StandaloneWorkspaceCreated { workspace: projected } =
            reply.result.unwrap().response.unwrap()
        else {
            unreachable!()
        };
        assert_eq!(projected.canonical_root, workspace.canonical_root);
        assert!(projected.worktree_service_mode.is_none());
        assert!(projected.managed_worktree_profiles.is_none());
    }

    #[tokio::test]
    async fn control_writer_rejects_unnegotiated_spawn_spec_reply_recursively() {
        let frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(3),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(C2NodeResponse::SpawnSpecAccepted {
                    receipt: spawn_receipt(),
                }),
            }),
        });
        assert!(server_frame_contains_spawn_spec_response(&frame));
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender.send(queued(frame, &budget).unwrap()).await.unwrap();
        let (disconnect, disconnected) = watch::channel(false);
        control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            NegotiatedPathCapabilities {
                opaque_host_paths: true,
                repository_paths: true,
                workspace_file_read: true,
                workspace_file_write: true,
                workspace_entry_create: true,
                git_read: true,
                host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
                provider_ids_open: true,
                spawn_spec_defaults_overrides: false,
                worktree_selection: true,
                managed_worktree_lifecycle: true,
                child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
                ..Default::default()
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        assert!(*disconnected.borrow());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn control_writer_rejects_unnegotiated_environment_receipt_recursively() {
        let mut receipt = spawn_receipt();
        receipt.environment_profile = Some(
            crate::protocol::ResolvedEnvironmentProfileReceipt {
                profile_id: crate::protocol::SpawnEnvironmentProfileId::new(
                    "local-default",
                )
                .unwrap(),
                profile_revision:
                    crate::protocol::SpawnEnvironmentProfileRevision::new(
                        "local-default.r1",
                    )
                    .unwrap(),
            },
        );
        let frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(4),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(C2NodeResponse::SpawnSpecAccepted { receipt }),
            }),
        });
        assert!(server_frame_contains_child_environment_profile(&frame));
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender.send(queued(frame, &budget).unwrap()).await.unwrap();
        let (disconnect, disconnected) = watch::channel(false);
        let capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: false,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            capabilities,
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        assert!(*disconnected.borrow());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn control_writer_rejects_unnegotiated_worktree_receipt_recursively() {
        let mut receipt = spawn_receipt();
        receipt.target.worktree_id =
            Some(gate4agent_node_protocol::WorkspaceId::new("review-tree").unwrap());
        let frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(4),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(C2NodeResponse::SpawnSpecAccepted { receipt }),
            }),
        });
        assert!(server_frame_contains_worktree_selection_response(&frame));
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender.send(queued(frame, &budget).unwrap()).await.unwrap();
        let (disconnect, disconnected) = watch::channel(false);
        control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            NegotiatedPathCapabilities {
                opaque_host_paths: true,
                repository_paths: true,
                workspace_file_read: true,
                host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
                provider_ids_open: true,
                spawn_spec_defaults_overrides: true,
                worktree_selection: false,
                managed_worktree_lifecycle: true,
                child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
                ..Default::default()
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        assert!(*disconnected.borrow());
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
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
    fn terminal_frame_legacy_projection_uses_exact_session_provider() {
        let node_id = NodeId::new("node-a").unwrap();
        let snapshot = crate::protocol::C2NodeSnapshot {
            node_id: node_id.clone(),
            enabled_providers: vec![AgentId::new("codex").unwrap()],
            agent_progress: Vec::new(),
            provider_runtime_statuses: Default::default(),
            workspaces: vec![C2WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("repo").unwrap(),
                canonical_root: OpaqueHostPath::utf8(r"C:\repo".to_owned()).unwrap(),
                sessions: vec![c2_session("codex", 1), c2_session("qwen-code", 2)],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            observation_support: None,
        };
        let empty_status = status(NodeTransportState::Offline, None);
        let mut legacy = C2NodeEvent::TerminalFrame {
            address: address(1),
            frame: terminal_frame(2),
        };
        let mut open = C2NodeEvent::TerminalFrame {
            address: address(2),
            frame: terminal_frame(2),
        };

        assert!(project_legacy_event(
            &mut legacy,
            &node_id,
            &empty_status,
            Some(&snapshot),
        ));
        assert!(!project_legacy_event(
            &mut open,
            &node_id,
            &empty_status,
            Some(&snapshot),
        ));
    }

    #[test]
    fn launch_inventory_projection_uses_existing_capability_components() {
        let inventory = crate::protocol::LaunchInventory {
            spawn_profiles: Some(vec![crate::protocol::SpawnProfileSummary {
                id: SpawnProfileId::new("default").unwrap(),
                revision: SpawnProfileRevision::new("v1").unwrap(),
            }]),
            bundles: Some(vec![crate::protocol::ResolvedBundleReceipt {
                id: crate::protocol::SpawnBundleId::new("review").unwrap(),
                revision: crate::protocol::SpawnBundleRevision::new("v1").unwrap(),
                digest: crate::protocol::SpawnBundleDigest::new(format!(
                    "sha256:{}",
                    "0".repeat(64),
                ))
                .unwrap(),
            }]),
        };
        let snapshot = crate::protocol::C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            agent_progress: Vec::new(),
            provider_runtime_statuses: Default::default(),
            workspaces: vec![C2WorkspaceSnapshot {
                workspace_id: WorkspaceId::new("repo").unwrap(),
                canonical_root: OpaqueHostPath::utf8(r"C:\repo".to_owned()).unwrap(),
                sessions: Vec::new(),
                worktree_service_mode: None,
                managed_worktree_profiles: Some(
                    crate::protocol::WorktreeProfileInventory {
                        profiles: vec![crate::protocol::ManagedWorktreeProfileSummary {
                            id: crate::protocol::WorktreeProfileId::new("review").unwrap(),
                            revision: crate::protocol::WorktreeProfileRevision::new("v1").unwrap(),
                            retention: crate::protocol::ManagedWorktreeRetention::Retain,
                        }],
                    },
                ),
            }],
            session_records: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: Some(inventory),
            observation_support: None,
        };
        let frame = |snapshot| C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(1),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
                response: Ok(C2NodeResponse::Snapshot {
                    event_sequence: 1,
                    controller: None,
                    snapshot,
                }),
            }),
        });

        let mut bundle_only = frame(snapshot.clone());
        project_server_frame_launch_inventory(&mut bundle_only, false, false, true);
        let C2ServerFrame::Reply(bundle_only) = bundle_only else { unreachable!() };
        let C2NodeResponse::Snapshot { snapshot: bundle_only, .. } =
            bundle_only.result.unwrap().response.unwrap()
        else {
            unreachable!()
        };
        assert!(bundle_only.workspaces[0].managed_worktree_profiles.is_none());
        let bundle_only = bundle_only.launch_inventory.unwrap();
        assert!(bundle_only.spawn_profiles.is_none());
        assert_eq!(bundle_only.bundles.unwrap().len(), 1);

        let mut legacy = frame(snapshot);
        project_server_frame_launch_inventory(&mut legacy, false, false, false);
        let C2ServerFrame::Reply(legacy) = legacy else { unreachable!() };
        let C2NodeResponse::Snapshot { snapshot: legacy, .. } =
            legacy.result.unwrap().response.unwrap()
        else {
            unreachable!()
        };
        assert!(legacy.launch_inventory.is_none());
        assert!(legacy.workspaces[0].managed_worktree_profiles.is_none());
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
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: false,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
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
                            managed_worktree: None,
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
                workspace_file_write: false,
                workspace_entry_create: false,
                git_read: false,
                host_directory_browse: false,
                standalone_workspace_lifecycle: false,
                provider_session_reference_index: false,
                provider_runtime_status: false,
                provider_ids_open: false,
                spawn_spec_defaults_overrides: false,
                worktree_selection: false,
                managed_worktree_lifecycle: false,
                child_environment_profile: false,
                session_bundle_materialization: false,
                history_context_pack: false,
                native_session_catalog: false,
                native_session_catalog_paging: false,
                native_session_index: false,
                native_session_preview: false,
                terminal_frame_events: false,
                ..Default::default()
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn control_writer_drops_unnegotiated_terminal_frame_event() {
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender
            .send(queued(terminal_event_frame(1, 8), &budget).unwrap())
            .await
            .unwrap();
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
                workspace_file_read: true,
                host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
                provider_ids_open: true,
                spawn_spec_defaults_overrides: true,
                worktree_selection: true,
                managed_worktree_lifecycle: true,
                child_environment_profile: true,
                session_bundle_materialization: true,
                history_context_pack: true,
                native_session_catalog: true,
                native_session_catalog_paging: true,
                native_session_index: true,
                native_session_preview: true,
                terminal_frame_events: false,
                ..Default::default()
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn control_writer_disconnects_on_unnegotiated_terminal_frame_resync_reply() {
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender
            .send(queued(terminal_resync_frame(1, 8), &budget).unwrap())
            .await
            .unwrap();
        assert_ne!(budget.load(Ordering::Acquire), 0);
        let (disconnect, disconnected) = watch::channel(false);
        let writer = tokio::spawn(control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            NegotiatedPathCapabilities {
                opaque_host_paths: true,
                repository_paths: true,
                workspace_file_read: true,
                host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
                provider_ids_open: true,
                spawn_spec_defaults_overrides: true,
                worktree_selection: true,
                managed_worktree_lifecycle: true,
                child_environment_profile: true,
                session_bundle_materialization: true,
                history_context_pack: true,
                native_session_catalog: true,
                native_session_catalog_paging: true,
                native_session_index: true,
                native_session_preview: true,
                terminal_frame_events: false,
                ..Default::default()
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ));

        timeout(Duration::from_secs(1), writer).await.unwrap().unwrap();
        assert!(*disconnected.borrow());
        assert!(sender.is_closed());
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
                        revision: None,
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
            workspace_file_write: false,
            workspace_entry_create: false,
            git_read: false,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        assert!(matches!(
            unnegotiated_request_failure(&utf8_request, no_file_read),
            Some(C2RelayFailure { code: C2RelayFailureCode::RequestForbidden, .. })
        ));
        let no_repository_path = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: false,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
        };
        assert!(matches!(
            unnegotiated_request_failure(&tagged_request, no_repository_path),
            Some(C2RelayFailure { code: C2RelayFailureCode::RequestForbidden, .. })
        ));
        let all = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            ..Default::default()
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
                host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
                provider_ids_open: true,
                spawn_spec_defaults_overrides: true,
                worktree_selection: true,
                managed_worktree_lifecycle: true,
                child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
                ..Default::default()
            },
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        ).await;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn workspace_entry_create_fails_closed_before_c2_relay_or_response_write() {
        let workspace_id = WorkspaceId::new("repo").unwrap();
        let path = repository_path("src/new");
        let request = NodeRequest::CreateWorkspaceDirectory {
            workspace_id: workspace_id.clone(),
            path: path.clone(),
        };
        let mut capabilities = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            workspace_file_write: true,
            workspace_entry_create: false,
            git_read: true,
            host_directory_browse: true,
            standalone_workspace_lifecycle: true,
            provider_session_reference_index: true,
            provider_runtime_status: true,
            provider_ids_open: true,
            spawn_spec_defaults_overrides: true,
            spawn_profile_revision: true,
            worktree_selection: true,
            managed_worktree_lifecycle: true,
            child_environment_profile: true,
            session_bundle_materialization: true,
            history_context_pack: true,
            native_session_catalog: true,
            native_session_catalog_paging: true,
            native_session_index: true,
            native_session_preview: true,
            terminal_frame_events: true,
            agent_progress_snapshot: true,
            session_task_correlation: true,
            observation_events: true,
            observation_managed_target: true,
            observation_workflow_detail: true,
            delivery_bundle_v2_stage_commit: true,
            harness_mcp_read_proxy: true,
        };
        assert!(matches!(
            unnegotiated_request_failure(&request, capabilities),
            Some(C2RelayFailure {
                code: C2RelayFailureCode::RequestForbidden,
                ..
            })
        ));
        capabilities.workspace_entry_create = true;
        assert!(unnegotiated_request_failure(&request, capabilities).is_none());

        let frame = C2ServerFrame::Reply(C2ReplyEnvelope {
            request_id: crate::protocol::C2RequestId(41),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([9; 16]),
                response: Ok(C2NodeResponse::WorkspaceDirectoryCreated {
                    workspace_id,
                    entry: gate4agent_node_protocol::WorkspaceEntry {
                        relative_path: path,
                        kind: gate4agent_node_protocol::WorkspaceEntryKind::Directory,
                    },
                }),
            }),
        });
        assert!(server_frame_contains_workspace_entry_create(&frame));
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (sender, receiver) = mpsc::channel(1);
        let budget = Arc::new(AtomicUsize::new(0));
        sender.send(queued(frame, &budget).unwrap()).await.unwrap();
        drop(sender);
        let (disconnect, _disconnected) = watch::channel(false);
        capabilities.workspace_entry_create = false;
        control_writer(
            writer,
            receiver,
            Arc::clone(&budget),
            disconnect,
            capabilities,
            watch::channel(Arc::new(status(NodeTransportState::Offline, None))).1,
        )
        .await;
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
