use gate4agent_c2_protocol::{
    c2_auth_transcript, c2_bound_auth_transcript, provider_id_is_legacy, C2AuthDirection, C2ClientAuthentication, C2ClientFrame,
    C2ClientHello, C2Hello, C2NodeEvent, C2NodeResponse, C2RelayFailure,
    C2RequestEnvelope, C2RequestId, C2ServerFrame, C2Topology, CapabilityId,
    ClientCompatibilityOffer, NegotiatedC2ControlCompatibility, NodeRequest, NodeRoute,
    ProtocolRange, RoutedNodeEvent, RoutedNodeRequest, RoutedNodeResponse,
    C2_COMPATIBILITY_METADATA_CAPABILITY, C2_CONTROL_PROTOCOL_VERSION,
    C2_OPAQUE_UNIX_PATH_CAPABILITY, C2_REPOSITORY_PATH_CAPABILITY,
    C2_PROVIDER_ID_OPEN_CAPABILITY,
    C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, C2_PROVIDER_RUNTIME_STATUS_CAPABILITY,
    C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
    C2_WORKSPACE_FILE_READ_CAPABILITY,
    C2_AUTH_NONCE_BYTES, MAX_C2_AUTH_FRAME_BYTES, MAX_C2_CLIENT_FRAME_BYTES, MAX_C2_HELLO_FRAME_BYTES,
    MAX_C2_SERVER_FRAME_BYTES,
};
use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, FrameError,
};
use gate4agent_node_wire::{
    connect_local_stream, local_hmac_sha256, proofs_match, random_nonce,
    LocalClientStream,
};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
const FRAME_BODY_DEADLINE: Duration = Duration::from_secs(5);
const RELAY_REPLY_HEADROOM: Duration = Duration::from_secs(5);
const COMMAND_CAPACITY: usize = 64;
const INBOUND_CAPACITY: usize = 2;
const WRITER_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 2;
const OPAQUE_UNIX_PATH_NOT_NEGOTIATED: &str =
    "opaque Unix paths require negotiated C2 capability";
const REPOSITORY_PATH_NOT_NEGOTIATED: &str =
    "tagged repository paths require negotiated C2 capability";
const WORKSPACE_FILE_READ_NOT_NEGOTIATED: &str =
    "workspace file reads require negotiated C2 capability";
const OPEN_PROVIDER_ID_NOT_NEGOTIATED: &str =
    "open provider IDs require negotiated C2 capability";
const TERMINAL_FRAME_EVENTS_NOT_NEGOTIATED: &str =
    "terminal frame events require negotiated C2 capability";

#[derive(Clone, Copy)]
struct NegotiatedPathCapabilities {
    opaque_host_paths: bool,
    repository_paths: bool,
    workspace_file_read: bool,
    provider_ids_open: bool,
    terminal_frame_events: bool,
}

#[derive(Clone)]
pub struct C2ControlHandle {
    commands: mpsc::Sender<ControlCommand>,
    hello: Arc<C2Hello>,
    topology: watch::Receiver<Arc<C2Topology>>,
    terminal_frame_events: bool,
}

impl C2ControlHandle {
    pub fn hello(&self) -> &C2Hello { &self.hello }

    pub fn current_topology(&self) -> Arc<C2Topology> { Arc::clone(&*self.topology.borrow()) }

    pub fn subscribe_topology(&self) -> watch::Receiver<Arc<C2Topology>> {
        self.topology.clone()
    }

    pub fn terminal_frame_events_enabled(&self) -> bool { self.terminal_frame_events }

    pub async fn request(
        &self,
        route: NodeRoute,
        request: NodeRequest,
    ) -> Result<RoutedNodeResponse, C2ControlError> {
        reject_unnegotiated_outbound_path(
            &request,
            negotiated_path_capabilities(self.hello.compatibility.as_ref()),
        )?;
        let deadline = control_request_deadline(&request);
        let (reply_tx, reply_rx) = oneshot::channel();
        timeout(deadline, async {
            self.commands.send(ControlCommand { route, request, reply: reply_tx })
                .await.map_err(|_| C2ControlError::Closed)?;
            reply_rx.await.map_err(|_| C2ControlError::Closed)?
        }).await.map_err(|_| C2ControlError::Closed)?
    }
}

fn control_request_deadline(request: &NodeRequest) -> Duration {
    let relay_deadline = match request {
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::RenameSessionRecord { .. }
        | NodeRequest::ForgetSessionRecord { .. } => Duration::from_secs(5),
        NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. } => Duration::from_secs(240),
        NodeRequest::Spawn { .. }
        | NodeRequest::Resume { .. }
        | NodeRequest::Stop { .. } => Duration::from_secs(15),
        NodeRequest::ResumeSessionRecord { .. } => Duration::from_secs(35),
        _ => Duration::from_secs(10),
    };
    relay_deadline + RELAY_REPLY_HEADROOM
}

pub struct C2EventReceiver {
    events: mpsc::Receiver<RoutedNodeEvent>,
}

impl C2EventReceiver {
    pub async fn recv(&mut self) -> Option<RoutedNodeEvent> { self.events.recv().await }
}

struct ControlCommand {
    route: NodeRoute,
    request: NodeRequest,
    reply: oneshot::Sender<Result<RoutedNodeResponse, C2ControlError>>,
}

enum OwnerInput {
    Frame(C2ServerFrame),
    Closed,
}

pub async fn connect_local(
    endpoint: &str,
    token: &str,
) -> Result<(C2ControlHandle, C2EventReceiver), C2ControlError> {
    validate_endpoint(endpoint)?;
    validate_token(token)?;
    let mut pipe = connect_local_stream(endpoint).await?;
    let client_nonce = random_nonce().map_err(C2ControlError::Authentication)?;
    let compatibility_offer = client_compatibility_offer()?;
    timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ClientFrame::Hello(C2ClientHello::negotiating(
            client_nonce,
            compatibility_offer.clone(),
        )),
        MAX_C2_AUTH_FRAME_BYTES,
    )).await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let challenge = timeout(AUTH_DEADLINE, read_server_frame(&mut pipe, MAX_C2_AUTH_FRAME_BYTES))
        .await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let C2ServerFrame::Challenge(challenge) = challenge else {
        return Err(C2ControlError::Protocol("C2 did not return an authentication challenge".to_owned()));
    };
    if challenge.protocol_version != C2_CONTROL_PROTOCOL_VERSION {
        return Err(C2ControlError::Protocol("C2 control protocol version mismatch".to_owned()));
    }
    let selected = challenge.compatibility.as_ref().ok_or_else(|| {
        C2ControlError::Protocol(
            "C2 omitted the required authenticated compatibility selection".to_owned(),
        )
    })?;
    validate_selected_compatibility(
        &compatibility_offer,
        Some(selected),
    )?;
    let expected_server = c2_proof(
        token,
        C2AuthDirection::Server,
        &client_nonce,
        &challenge.server_nonce,
        Some((&compatibility_offer, selected)),
    )?;
    if !proofs_match(&challenge.server_proof, &expected_server) {
        return Err(C2ControlError::Authentication("C2 server proof mismatch".to_owned()));
    }
    let client_proof = c2_proof(
        token,
        C2AuthDirection::Client,
        &client_nonce,
        &challenge.server_nonce,
        Some((&compatibility_offer, selected)),
    )?;
    timeout(AUTH_DEADLINE, write_json_frame_limited(
        &mut pipe,
        &C2ClientFrame::Authenticate(C2ClientAuthentication { client_proof }),
        MAX_C2_AUTH_FRAME_BYTES,
    )).await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let hello = timeout(HELLO_DEADLINE, read_server_frame(&mut pipe, MAX_C2_HELLO_FRAME_BYTES))
        .await.map_err(|_| C2ControlError::AuthenticationTimedOut)??;
    let hello = match hello {
        C2ServerFrame::Hello(hello) if hello.protocol_version == C2_CONTROL_PROTOCOL_VERSION => hello,
        C2ServerFrame::Rejected(failure) =>
            return Err(C2ControlError::Relay(failure)),
        C2ServerFrame::Hello(_) =>
            return Err(C2ControlError::Protocol("C2 control protocol version mismatch".to_owned())),
        _ => return Err(C2ControlError::Protocol("C2 did not return hello".to_owned())),
    };
    if hello.compatibility.is_none() {
        return Err(C2ControlError::Protocol(
            "C2 omitted the authenticated compatibility selection from hello".to_owned(),
        ));
    }
    validate_selected_compatibility(&compatibility_offer, hello.compatibility.as_ref())?;
    if hello.compatibility != challenge.compatibility {
        return Err(C2ControlError::Protocol(
            "C2 compatibility selection changed after authentication".to_owned(),
        ));
    }
    let path_capabilities = negotiated_path_capabilities(hello.compatibility.as_ref());
    if !path_capabilities.provider_ids_open && status_has_open_provider_id(&hello.status) {
        return Err(C2ControlError::Protocol(
            OPEN_PROVIDER_ID_NOT_NEGOTIATED.to_owned(),
        ));
    }

    let (reader, writer) = tokio::io::split(pipe);
    let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
    let initial_topology = Arc::new(C2Topology::from_status(&hello.status));
    let (topology_tx, topology_rx) = watch::channel(initial_topology);
    let (writer_tx, writer_rx) = mpsc::channel(WRITER_CAPACITY);
    let (owner_tx, owner_rx) = mpsc::channel(INBOUND_CAPACITY);
    let reader_task = tokio::spawn(control_reader(reader, owner_tx.clone()));
    let writer_task = tokio::spawn(control_writer(writer, writer_rx, owner_tx));
    tokio::spawn(async move {
        control_owner(
            commands_rx,
            events_tx,
            topology_tx,
            writer_tx,
            owner_rx,
            path_capabilities,
        ).await;
        reader_task.abort();
        writer_task.abort();
    });
    Ok((C2ControlHandle {
        commands: commands_tx,
        hello: Arc::new(hello),
        topology: topology_rx,
        terminal_frame_events: path_capabilities.terminal_frame_events,
    }, C2EventReceiver { events: events_rx }))
}

fn client_compatibility_offer() -> Result<ClientCompatibilityOffer, C2ControlError> {
    Ok(ClientCompatibilityOffer {
        protocol_versions: ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION)
            .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
        capabilities: vec![
            CapabilityId::new(C2_COMPATIBILITY_METADATA_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_OPAQUE_UNIX_PATH_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_REPOSITORY_PATH_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_WORKSPACE_FILE_READ_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_PROVIDER_ID_OPEN_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
            CapabilityId::new(C2_TERMINAL_FRAME_EVENTS_CAPABILITY)
                .map_err(|error| C2ControlError::Protocol(error.to_string()))?,
        ],
        state_schema: None,
    })
}

fn validate_selected_compatibility(
    offer: &ClientCompatibilityOffer,
    selected: Option<&NegotiatedC2ControlCompatibility>,
) -> Result<(), C2ControlError> {
    if !offer.protocol_versions.contains(C2_CONTROL_PROTOCOL_VERSION) {
        return Err(C2ControlError::Protocol(
            "C2 compatibility offer excludes control protocol v2".to_owned(),
        ));
    }
    let Some(selected) = selected else {
        return Err(C2ControlError::Protocol(
            "C2 omitted the required authenticated compatibility selection".to_owned(),
        ));
    };
    if selected.protocol_version != C2_CONTROL_PROTOCOL_VERSION
        || !offer.protocol_versions.contains(selected.protocol_version)
    {
        return Err(C2ControlError::Protocol(
            "C2 selected a protocol version outside the client offer".to_owned(),
        ));
    }
    if selected
        .capabilities
        .iter()
        .any(|capability| !offer.capabilities.contains(capability))
    {
        return Err(C2ControlError::Protocol(
            "C2 selected a capability outside the client offer".to_owned(),
        ));
    }
    if !selected
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == C2_COMPATIBILITY_METADATA_CAPABILITY)
    {
        return Err(C2ControlError::Protocol(
            "C2 omitted the required compatibility metadata capability".to_owned(),
        ));
    }
    Ok(())
}

fn negotiated_path_capabilities(
    selected: Option<&NegotiatedC2ControlCompatibility>,
) -> NegotiatedPathCapabilities {
    let selected_has = |expected| {
        selected.is_some_and(|selected| {
            selected
                .capabilities
                .iter()
                .any(|capability| capability.as_str() == expected)
        })
    };
    NegotiatedPathCapabilities {
        opaque_host_paths: selected_has(C2_OPAQUE_UNIX_PATH_CAPABILITY),
        repository_paths: selected_has(C2_REPOSITORY_PATH_CAPABILITY),
        workspace_file_read: selected_has(C2_WORKSPACE_FILE_READ_CAPABILITY),
        provider_ids_open: selected_has(C2_PROVIDER_ID_OPEN_CAPABILITY),
        terminal_frame_events: selected_has(C2_TERMINAL_FRAME_EVENTS_CAPABILITY),
    }
}

fn reject_unnegotiated_outbound_path(
    request: &NodeRequest,
    capabilities: NegotiatedPathCapabilities,
) -> Result<(), C2ControlError> {
    if !capabilities.opaque_host_paths && node_request_has_unix_bytes(request) {
        return Err(C2ControlError::Protocol(
            OPAQUE_UNIX_PATH_NOT_NEGOTIATED.to_owned(),
        ));
    }
    if !capabilities.provider_ids_open
        && matches!(request, NodeRequest::Spawn { provider, .. } if !provider_id_is_legacy(provider))
    {
        return Err(C2ControlError::Protocol(
            OPEN_PROVIDER_ID_NOT_NEGOTIATED.to_owned(),
        ));
    }
    let required_capability_available = match request.required_capability() {
        None => true,
        Some(C2_WORKSPACE_FILE_READ_CAPABILITY) => capabilities.workspace_file_read,
        Some(_) => false,
    };
    if !required_capability_available {
        return Err(C2ControlError::Protocol(
            WORKSPACE_FILE_READ_NOT_NEGOTIATED.to_owned(),
        ));
    }
    if !capabilities.repository_paths && node_request_has_unix_repository_path(request) {
        return Err(C2ControlError::Protocol(
            REPOSITORY_PATH_NOT_NEGOTIATED.to_owned(),
        ));
    }
    Ok(())
}

fn node_request_has_unix_repository_path(request: &NodeRequest) -> bool {
    match request {
        NodeRequest::ReadWorkspaceFile { path, .. } => path.as_unix_bytes().is_some(),
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::RegisterWorkspace { .. }
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::CreateWorktree { .. }
        | NodeRequest::RemoveWorktree { .. }
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

fn node_request_has_unix_bytes(request: &NodeRequest) -> bool {
    match request {
        NodeRequest::RegisterWorkspace { root, .. } => root.as_unix_bytes().is_some(),
        NodeRequest::CreateWorktree { target_root, .. }
        | NodeRequest::RemoveWorktree { target_root, .. } => {
            target_root.as_unix_bytes().is_some()
        }
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

fn routed_response_has_unix_bytes(response: &RoutedNodeResponse) -> bool {
    response
        .response
        .as_ref()
        .is_ok_and(c2_node_response_has_unix_bytes)
}

fn routed_response_has_terminal_frame_event(response: &RoutedNodeResponse) -> bool {
    response
        .response
        .as_ref()
        .is_ok_and(c2_node_response_has_terminal_frame_event)
}

fn c2_node_response_has_terminal_frame_event(response: &C2NodeResponse) -> bool {
    match response {
        C2NodeResponse::Resync { events, .. } => events
            .iter()
            .any(|event| c2_node_event_is_terminal_frame(&event.event)),
        C2NodeResponse::Snapshot { .. }
        | C2NodeResponse::WorkspaceInspected { .. }
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
        | C2NodeResponse::ShuttingDown => false,
    }
}

fn c2_node_event_is_terminal_frame(event: &C2NodeEvent) -> bool {
    match event {
        C2NodeEvent::TerminalFrame { .. } => true,
        C2NodeEvent::Control { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceAdded { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::SessionRecordUpserted { .. }
        | C2NodeEvent::SessionRecordRemoved { .. }
        | C2NodeEvent::ResyncRequired { .. } => false,
    }
}

fn c2_node_response_has_unix_bytes(response: &C2NodeResponse) -> bool {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => node_snapshot_has_unix_bytes(snapshot),
        C2NodeResponse::Resync { snapshot, events, .. } => {
            node_snapshot_has_unix_bytes(snapshot)
                || events
                    .iter()
                    .any(|envelope| c2_node_event_has_unix_bytes(&envelope.event))
        }
        C2NodeResponse::WorkspaceInspected { inspection } => inspection
            .git
            .worktrees
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
        C2NodeResponse::WorktreeRemoved { target_root, .. } => {
            target_root.as_unix_bytes().is_some()
        }
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

fn routed_response_has_unix_repository_path(response: &RoutedNodeResponse) -> bool {
    response
        .response
        .as_ref()
        .is_ok_and(c2_node_response_has_unix_repository_path)
}

fn c2_node_response_has_unix_repository_path(response: &C2NodeResponse) -> bool {
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
        C2NodeResponse::WorkspaceFileRead { file } => file.path.as_unix_bytes().is_some(),
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
}

fn routed_response_requires_workspace_file_read(response: &RoutedNodeResponse) -> bool {
    response.response.as_ref().is_ok_and(|response| {
        matches!(response, C2NodeResponse::WorkspaceFileRead { .. })
    })
}

fn node_snapshot_has_unix_bytes(snapshot: &gate4agent_c2_protocol::C2NodeSnapshot) -> bool {
    snapshot
        .workspaces
        .iter()
        .any(|workspace| workspace.canonical_root.as_unix_bytes().is_some())
}

fn routed_event_has_unix_bytes(event: &RoutedNodeEvent) -> bool {
    c2_node_event_has_unix_bytes(&event.event)
}

fn c2_node_event_has_unix_bytes(event: &C2NodeEvent) -> bool {
    match event {
        C2NodeEvent::WorkspaceAdded { workspace } => {
            workspace.canonical_root.as_unix_bytes().is_some()
        }
        C2NodeEvent::Control { .. }
        | C2NodeEvent::TerminalFrame { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::SessionRecordUpserted { .. }
        | C2NodeEvent::SessionRecordRemoved { .. }
        | C2NodeEvent::ResyncRequired { .. } => false,
    }
}

fn provider_text_is_legacy(value: &str) -> bool {
    gate4agent_c2_protocol::AgentId::new(value)
        .is_ok_and(|provider| provider_id_is_legacy(&provider))
}

fn status_has_open_provider_id(status: &gate4agent_c2_protocol::StatusResponse) -> bool {
    status.nodes.values().filter_map(|node| node.inventory.as_ref()).any(|inventory| {
        inventory.enabled_providers.iter().any(|provider| !provider_id_is_legacy(provider))
            || inventory.provider_runtime_statuses.iter()
                .any(|runtime| !provider_id_is_legacy(runtime.provider()))
            || inventory.provider_contracts.iter()
                .any(|contract| !provider_id_is_legacy(&contract.provider))
            || inventory.provider_adapter_contracts.iter()
                .any(|contract| !provider_id_is_legacy(&contract.provider))
            || inventory.workspaces.values().any(|workspace| {
                workspace.sessions.iter()
                    .any(|session| !provider_text_is_legacy(&session.agent_id))
            })
            || inventory.managed_sessions.iter()
                .any(|record| !provider_id_is_legacy(&record.provider))
    })
}

fn topology_has_open_provider_id(topology: &C2Topology) -> bool {
    topology.nodes.iter().any(|node| {
        node.provider_contracts.iter()
            .any(|contract| !provider_id_is_legacy(&contract.provider))
            || node.provider_adapter_contracts.iter()
                .any(|contract| !provider_id_is_legacy(&contract.provider))
            || node.provider_runtime_statuses.iter()
                .any(|runtime| !provider_id_is_legacy(runtime.provider()))
    })
}

fn workspace_has_open_provider_id(
    workspace: &gate4agent_c2_protocol::C2WorkspaceSnapshot,
) -> bool {
    workspace.sessions.iter()
        .any(|session| !provider_id_is_legacy(&session.agent_id))
}

fn snapshot_has_open_provider_id(
    snapshot: &gate4agent_c2_protocol::C2NodeSnapshot,
) -> bool {
    snapshot.enabled_providers.iter().any(|provider| !provider_id_is_legacy(provider))
        || snapshot.provider_runtime_statuses.iter()
            .any(|runtime| !provider_id_is_legacy(runtime.provider()))
        || snapshot.workspaces.iter().any(workspace_has_open_provider_id)
        || snapshot.session_records.iter()
            .any(|record| !provider_id_is_legacy(&record.provider))
}

fn c2_event_has_open_provider_id(event: &C2NodeEvent) -> bool {
    match event {
        C2NodeEvent::WorkspaceAdded { workspace } => workspace_has_open_provider_id(workspace),
        C2NodeEvent::SessionRecordUpserted { record } => {
            !provider_id_is_legacy(&record.provider)
        }
        C2NodeEvent::Control { .. }
        | C2NodeEvent::TerminalFrame { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::SessionRecordRemoved { .. }
        | C2NodeEvent::ResyncRequired { .. } => false,
    }
}

fn routed_event_has_open_provider_id(event: &RoutedNodeEvent) -> bool {
    c2_event_has_open_provider_id(&event.event)
}

fn c2_node_response_has_open_provider_id(response: &C2NodeResponse) -> bool {
    match response {
        C2NodeResponse::Snapshot { snapshot, .. } => snapshot_has_open_provider_id(snapshot),
        C2NodeResponse::Resync { snapshot, events, .. } => {
            snapshot_has_open_provider_id(snapshot)
                || events.iter().any(|event| c2_event_has_open_provider_id(&event.event))
        }
        C2NodeResponse::SessionRecordUpdated { record }
        | C2NodeResponse::SessionRecordResumed { record, .. } => {
            !provider_id_is_legacy(&record.provider)
        }
        C2NodeResponse::WorkspaceRegistered { workspace }
        | C2NodeResponse::WorktreeCreated { workspace, .. } => {
            workspace_has_open_provider_id(workspace)
        }
        C2NodeResponse::WorkspaceInspected { .. }
        | C2NodeResponse::WorkspaceFileRead { .. }
        | C2NodeResponse::Controller { .. }
        | C2NodeResponse::SpawnAccepted { .. }
        | C2NodeResponse::SessionRecordForgotten { .. }
        | C2NodeResponse::WorkspaceUnregistered { .. }
        | C2NodeResponse::WorktreeRemoved { .. }
        | C2NodeResponse::Accepted
        | C2NodeResponse::ShuttingDown => false,
    }
}

fn routed_response_has_open_provider_id(response: &RoutedNodeResponse) -> bool {
    response.response.as_ref().is_ok_and(c2_node_response_has_open_provider_id)
}

async fn read_server_frame(
    pipe: &mut LocalClientStream,
    limit: usize,
) -> Result<C2ServerFrame, C2ControlError> {
    Ok(read_json_frame_limited_body_timeout(pipe, limit, FRAME_BODY_DEADLINE).await?)
}

async fn control_reader<R>(mut reader: R, owner: mpsc::Sender<OwnerInput>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        match read_json_frame_limited_body_timeout(
            &mut reader,
            MAX_C2_SERVER_FRAME_BYTES,
            FRAME_BODY_DEADLINE,
        ).await {
            Ok(frame) => if owner.send(OwnerInput::Frame(frame)).await.is_err() { return; },
            Err(_) => { let _ = owner.send(OwnerInput::Closed).await; return; }
        }
    }
}

async fn control_writer<W>(
    mut writer: W,
    mut frames: mpsc::Receiver<C2ClientFrame>,
    owner: mpsc::Sender<OwnerInput>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(frame) = frames.recv().await {
        if !matches!(timeout(FRAME_BODY_DEADLINE, write_json_frame_limited(
            &mut writer,
            &frame,
            MAX_C2_CLIENT_FRAME_BYTES,
        )).await, Ok(Ok(()))) {
            break;
        }
    }
    let _ = owner.send(OwnerInput::Closed).await;
}

async fn control_owner(
    mut commands: mpsc::Receiver<ControlCommand>,
    events: mpsc::Sender<RoutedNodeEvent>,
    topology: watch::Sender<Arc<C2Topology>>,
    writer: mpsc::Sender<C2ClientFrame>,
    mut incoming: mpsc::Receiver<OwnerInput>,
    path_capabilities: NegotiatedPathCapabilities,
) {
    let mut next_request_id = 1_u64;
    let mut pending = BTreeMap::new();
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if let Err(error) = reject_unnegotiated_outbound_path(
                    &command.request,
                    path_capabilities,
                ) {
                    let _ = command.reply.send(Err(error));
                    continue;
                }
                let request_id = C2RequestId(next_request_id);
                let Some(next) = next_request_id.checked_add(1) else {
                    let _ = command.reply.send(Err(C2ControlError::RequestIdExhausted));
                    break;
                };
                next_request_id = next;
                let frame = C2ClientFrame::Request(C2RequestEnvelope {
                    request_id,
                    request: RoutedNodeRequest { route: command.route, request: command.request },
                });
                pending.insert(request_id, command.reply);
                if writer.send(frame).await.is_err() { break; }
            }
            input = incoming.recv() => {
                match input {
                    Some(OwnerInput::Frame(C2ServerFrame::Reply(reply))) => {
                        let Some(waiter) = pending.remove(&reply.request_id) else { break; };
                        if !path_capabilities.terminal_frame_events
                            && reply
                                .result
                                .as_ref()
                                .is_ok_and(routed_response_has_terminal_frame_event)
                        {
                            let _ = waiter.send(Err(C2ControlError::Protocol(
                                TERMINAL_FRAME_EVENTS_NOT_NEGOTIATED.to_owned(),
                            )));
                            break;
                        }
                        if !path_capabilities.provider_ids_open
                            && reply
                                .result
                                .as_ref()
                                .is_ok_and(routed_response_has_open_provider_id)
                        {
                            let _ = waiter.send(Err(C2ControlError::Protocol(
                                OPEN_PROVIDER_ID_NOT_NEGOTIATED.to_owned(),
                            )));
                            break;
                        }
                        if !path_capabilities.opaque_host_paths
                            && reply
                                .result
                                .as_ref()
                                .is_ok_and(routed_response_has_unix_bytes)
                        {
                            let _ = waiter.send(Err(C2ControlError::Protocol(
                                OPAQUE_UNIX_PATH_NOT_NEGOTIATED.to_owned(),
                            )));
                            break;
                        }
                        if !path_capabilities.repository_paths
                            && reply
                                .result
                                .as_ref()
                                .is_ok_and(routed_response_has_unix_repository_path)
                        {
                            let _ = waiter.send(Err(C2ControlError::Protocol(
                                REPOSITORY_PATH_NOT_NEGOTIATED.to_owned(),
                            )));
                            break;
                        }
                        if !path_capabilities.workspace_file_read
                            && reply
                                .result
                                .as_ref()
                                .is_ok_and(routed_response_requires_workspace_file_read)
                        {
                            let _ = waiter.send(Err(C2ControlError::Protocol(
                                WORKSPACE_FILE_READ_NOT_NEGOTIATED.to_owned(),
                            )));
                            break;
                        }
                        let _ = waiter.send(reply.result.map_err(C2ControlError::Relay));
                    }
                    Some(OwnerInput::Frame(C2ServerFrame::Event(event))) => {
                        if !path_capabilities.terminal_frame_events
                            && c2_node_event_is_terminal_frame(&event.event)
                        {
                            break;
                        }
                        if !path_capabilities.provider_ids_open
                            && routed_event_has_open_provider_id(&event)
                        {
                            break;
                        }
                        if !path_capabilities.opaque_host_paths
                            && routed_event_has_unix_bytes(&event)
                        {
                            break;
                        }
                        let terminal_frame = c2_node_event_is_terminal_frame(&event.event);
                        match events.try_send(event) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) if terminal_frame => {}
                            Err(mpsc::error::TrySendError::Full(_)
                                | mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Some(OwnerInput::Frame(C2ServerFrame::Topology(next))) => {
                        if !path_capabilities.provider_ids_open
                            && topology_has_open_provider_id(&next)
                        {
                            break;
                        }
                        if topology.borrow().as_ref() != &next {
                            topology.send_replace(Arc::new(next));
                        }
                    }
                    Some(OwnerInput::Frame(C2ServerFrame::Challenge(_) | C2ServerFrame::Hello(_) | C2ServerFrame::Rejected(_)))
                        | Some(OwnerInput::Closed) | None => break,
                }
            }
        }
    }
    for (_, waiter) in pending { let _ = waiter.send(Err(C2ControlError::Closed)); }
}

fn c2_proof(
    token: &str,
    direction: C2AuthDirection,
    client_nonce: &[u8; C2_AUTH_NONCE_BYTES],
    server_nonce: &[u8; C2_AUTH_NONCE_BYTES],
    compatibility: Option<(&ClientCompatibilityOffer, &NegotiatedC2ControlCompatibility)>,
) -> Result<[u8; 32], C2ControlError> {
    let transcript = match compatibility {
        Some((offer, selected)) => c2_bound_auth_transcript(
            direction,
            client_nonce,
            server_nonce,
            offer,
            selected,
        ).map_err(|error| C2ControlError::Authentication(error.to_string()))?,
        None => c2_auth_transcript(direction, client_nonce, server_nonce),
    };
    local_hmac_sha256(token.as_bytes(), &transcript)
        .map_err(C2ControlError::Authentication)
}

#[cfg(windows)]
fn validate_endpoint(endpoint: &str) -> Result<(), C2ControlError> {
    if !endpoint.starts_with(r"\\.\pipe\") || endpoint.len() <= r"\\.\pipe\".len() || endpoint.len() > 1024 {
        return Err(C2ControlError::InvalidEndpoint);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_endpoint(endpoint: &str) -> Result<(), C2ControlError> {
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    let path = Path::new(endpoint);
    if endpoint.is_empty() || path.as_os_str().as_bytes().len() > 103 || !path.is_absolute()
        || path.file_name().is_none()
    {
        return Err(C2ControlError::InvalidEndpoint);
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), C2ControlError> {
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(C2ControlError::InvalidToken);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum C2ControlError {
    #[cfg_attr(windows, error("C2 control endpoint is not a bounded local named pipe"))]
    #[cfg_attr(unix, error("C2 control endpoint is not a bounded absolute local endpoint"))]
    InvalidEndpoint,
    #[error("C2 token must contain 1..=4096 visible ASCII bytes without whitespace")]
    InvalidToken,
    #[error("C2 control I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("C2 authentication timed out")]
    AuthenticationTimedOut,
    #[error("C2 authentication failed: {0}")]
    Authentication(String),
    #[error("C2 control protocol failed: {0}")]
    Protocol(String),
    #[error("C2 relay rejected request: {0:?}")]
    Relay(C2RelayFailure),
    #[error("C2 control connection closed")]
    Closed,
    #[error("C2 request ID space exhausted")]
    RequestIdExhausted,
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use gate4agent_c2_protocol::{
        ArchitectureId, C2GitSnapshot, C2WorkspaceInspection, C2WorkspaceSnapshot,
        C2ServerChallenge, HostDescriptor, NodeCursor, NodeId, OpaqueHostPath,
        OperatingSystemId, PathEncoding, PathSemantics, PathStyle, RepositoryPath,
        WorkspaceFileContent, WorkspaceFileRead,
    };
    use gate4agent_node_protocol::{
        GitStatusEntry, NodeIncarnationId, SessionMode, WorkspaceEntry, WorkspaceEntryKind,
        WorkspaceId,
    };
    use gate4agent_types::{
        AgentInstanceId, SessionGeneration, TerminalFrame, TerminalSize,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::net::windows::named_pipe::ServerOptions;

    fn unique_control_endpoint() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            r"\\.\pipe\gate4agent-c2-client-strict-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )
    }

    fn event(sequence: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                sequence,
            },
            event: gate4agent_c2_protocol::C2NodeEvent::ResyncRequired {
                oldest_available_sequence: sequence,
            },
        }))
    }

    fn terminal_event(sequence: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                sequence,
            },
            event: terminal_node_event(sequence),
        }))
    }

    fn terminal_node_event(sequence: u64) -> C2NodeEvent {
        C2NodeEvent::TerminalFrame {
            address: gate4agent_node_protocol::SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: gate4agent_node_protocol::SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                },
            },
            frame: TerminalFrame {
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
            },
        }
    }

    fn route() -> NodeRoute {
        NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        }
    }

    fn no_path_capabilities() -> NegotiatedPathCapabilities {
        NegotiatedPathCapabilities {
            opaque_host_paths: false,
            repository_paths: false,
            workspace_file_read: false,
            provider_ids_open: false,
            terminal_frame_events: false,
        }
    }

    fn all_path_capabilities() -> NegotiatedPathCapabilities {
        NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: true,
            provider_ids_open: true,
            terminal_frame_events: true,
        }
    }

    fn reply(request_id: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Reply(gate4agent_c2_protocol::C2ReplyEnvelope {
            request_id: C2RequestId(request_id),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(gate4agent_c2_protocol::C2NodeResponse::Accepted),
            }),
        }))
    }

    fn terminal_resync_reply(request_id: u64, sequence: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Reply(gate4agent_c2_protocol::C2ReplyEnvelope {
            request_id: C2RequestId(request_id),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(C2NodeResponse::Resync {
                    event_sequence: sequence,
                    snapshot: gate4agent_c2_protocol::C2NodeSnapshot {
                        node_id: NodeId::new("node-a").unwrap(),
                        enabled_providers: Vec::new(),
                        provider_runtime_statuses: Default::default(),
                        workspaces: Vec::new(),
                        session_records: Vec::new(),
                    },
                    events: vec![gate4agent_c2_protocol::C2NodeEventEnvelope {
                        sequence,
                        event: terminal_node_event(sequence),
                    }],
                }),
            }),
        }))
    }

    fn unix_path() -> OpaqueHostPath {
        OpaqueHostPath::unix_bytes(vec![b'/', b's', b'r', b'v', b'/', 0xff]).unwrap()
    }

    fn unix_path_reply(request_id: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Reply(gate4agent_c2_protocol::C2ReplyEnvelope {
            request_id: C2RequestId(request_id),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(C2NodeResponse::WorktreeRemoved {
                    target_root: unix_path(),
                    workspace_id: None,
                }),
            }),
        }))
    }

    fn repository_path(value: &str) -> RepositoryPath {
        RepositoryPath::utf8(value.to_owned()).unwrap()
    }

    fn unix_repository_path(value: &[u8]) -> RepositoryPath {
        RepositoryPath::unix_bytes(value.to_vec()).unwrap()
    }

    fn repository_inspection_response(
        entry_path: RepositoryPath,
        status_path: RepositoryPath,
        previous_path: Option<RepositoryPath>,
    ) -> C2NodeResponse {
        C2NodeResponse::WorkspaceInspected {
            inspection: C2WorkspaceInspection {
                workspace_id: WorkspaceId::new("foreign").unwrap(),
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
        }
    }

    fn unix_repository_path_reply(request_id: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Reply(gate4agent_c2_protocol::C2ReplyEnvelope {
            request_id: C2RequestId(request_id),
            result: Ok(RoutedNodeResponse {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                response: Ok(repository_inspection_response(
                    unix_repository_path(b"src/\xff"),
                    repository_path("src/main.rs"),
                    None,
                )),
            }),
        }))
    }

    fn unix_path_event(sequence: u64) -> OwnerInput {
        OwnerInput::Frame(C2ServerFrame::Event(RoutedNodeEvent {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                sequence,
            },
            event: C2NodeEvent::WorkspaceAdded {
                workspace: C2WorkspaceSnapshot {
                    workspace_id: WorkspaceId::new("foreign").unwrap(),
                    canonical_root: unix_path(),
                    sessions: Vec::new(),
                },
            },
        }))
    }

    #[test]
    fn c2_control_client_offer_is_exact_v2_with_authenticated_opt_ins() {
        let offer = client_compatibility_offer().unwrap();

        assert_eq!(
            offer.protocol_versions,
            ProtocolRange::exact(C2_CONTROL_PROTOCOL_VERSION).unwrap(),
        );
        assert_eq!(
            offer.capabilities,
            vec![
                CapabilityId::new(C2_COMPATIBILITY_METADATA_CAPABILITY).unwrap(),
                CapabilityId::new(C2_OPAQUE_UNIX_PATH_CAPABILITY).unwrap(),
                CapabilityId::new(C2_REPOSITORY_PATH_CAPABILITY).unwrap(),
                CapabilityId::new(C2_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_RUNTIME_STATUS_CAPABILITY).unwrap(),
                CapabilityId::new(C2_PROVIDER_ID_OPEN_CAPABILITY).unwrap(),
                CapabilityId::new(C2_TERMINAL_FRAME_EVENTS_CAPABILITY).unwrap(),
            ],
        );
        assert_eq!(offer.state_schema, None);
        assert!(matches!(
            validate_selected_compatibility(&offer, None),
            Err(C2ControlError::Protocol(_)),
        ));
    }

    #[tokio::test]
    async fn control_owner_fails_closed_on_unnegotiated_terminal_frame_event() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx,
            events_tx,
            topology_tx,
            writer_tx,
            incoming_rx,
            no_path_capabilities(),
        ));

        incoming_tx.send(terminal_event(8)).await.unwrap();
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();

        assert!(commands_tx.is_closed());
        assert!(matches!(
            events_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected),
        ));
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_terminal_frame_resync_reply_and_closes() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx,
            events_tx,
            topology_tx,
            writer_tx,
            incoming_rx,
            no_path_capabilities(),
        ));
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Resync { after_sequence: 7 },
            reply: reply_tx,
        }).await.unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));

        incoming_tx.send(terminal_resync_reply(1, 8)).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), reply_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == TERMINAL_FRAME_EVENTS_NOT_NEGOTIATED
        ));
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();

        assert!(commands_tx.is_closed());
        assert!(matches!(
            events_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected),
        ));
    }

    #[tokio::test]
    async fn saturated_terminal_event_channel_does_not_block_following_reply() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx,
            events_tx,
            topology_tx,
            writer_tx,
            incoming_rx,
            all_path_capabilities(),
        ));

        incoming_tx.send(terminal_event(8)).await.unwrap();
        incoming_tx.send(terminal_event(9)).await.unwrap();
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Snapshot,
            reply: reply_tx,
        }).await.unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        incoming_tx.send(reply(1)).await.unwrap();

        let response = timeout(Duration::from_secs(1), reply_rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(response.response, Ok(C2NodeResponse::Accepted)));
        assert!(matches!(
            events_rx.try_recv(),
            Ok(RoutedNodeEvent {
                event: C2NodeEvent::TerminalFrame { .. },
                ..
            })
        ));

        drop(commands_tx);
        drop(incoming_tx);
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
    }

    #[test]
    fn c2_repository_path_gate_covers_entries_status_and_previous_path() {
        let utf8 = || repository_path("src/main.rs");
        let tagged = || unix_repository_path(b"src/\xff");
        for response in [
            repository_inspection_response(tagged(), utf8(), None),
            repository_inspection_response(utf8(), tagged(), None),
            repository_inspection_response(utf8(), utf8(), Some(tagged())),
        ] {
            assert!(c2_node_response_has_unix_repository_path(&response));
        }
        assert!(!c2_node_response_has_unix_repository_path(
            &repository_inspection_response(utf8(), utf8(), Some(utf8())),
        ));
    }

    #[test]
    fn c2_control_client_rejects_selection_outside_offer() {
        let offer = client_compatibility_offer().unwrap();
        let selected = NegotiatedC2ControlCompatibility {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION + 1,
            capabilities: offer.capabilities.clone(),
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
        };

        assert!(matches!(
            validate_selected_compatibility(&offer, Some(&selected)),
            Err(C2ControlError::Protocol(_)),
        ));
    }

    #[test]
    fn c2_control_client_requires_authenticated_compatibility_metadata() {
        let offer = client_compatibility_offer().unwrap();
        let selected = NegotiatedC2ControlCompatibility {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![
                CapabilityId::new(C2_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
            ],
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
        };

        assert!(matches!(
            validate_selected_compatibility(&offer, Some(&selected)),
            Err(C2ControlError::Protocol(_)),
        ));
    }

    #[tokio::test]
    async fn negotiating_c2_client_rejects_hmac_valid_legacy_challenge_before_authenticate() {
        let endpoint = unique_control_endpoint();
        let token = "strict-negotiation-token";
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&endpoint)
            .unwrap();
        let server_task = tokio::spawn({
            let token = token.to_owned();
            async move {
                server.connect().await.unwrap();
                let frame = read_json_frame_limited_body_timeout::<_, C2ClientFrame>(
                    &mut server,
                    MAX_C2_AUTH_FRAME_BYTES,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                let C2ClientFrame::Hello(hello) = frame else {
                    panic!("client did not send hello");
                };
                assert!(hello.compatibility.is_some());
                let server_nonce = [7; C2_AUTH_NONCE_BYTES];
                let server_proof = c2_proof(
                    &token,
                    C2AuthDirection::Server,
                    &hello.client_nonce,
                    &server_nonce,
                    None,
                )
                .unwrap();
                write_json_frame_limited(
                    &mut server,
                    &C2ServerFrame::Challenge(C2ServerChallenge {
                        protocol_version: C2_CONTROL_PROTOCOL_VERSION,
                        server_nonce,
                        server_proof,
                        compatibility: None,
                    }),
                    MAX_C2_AUTH_FRAME_BYTES,
                )
                .await
                .unwrap();
                timeout(
                    Duration::from_secs(1),
                    read_json_frame_limited_body_timeout::<_, C2ClientFrame>(
                        &mut server,
                        MAX_C2_AUTH_FRAME_BYTES,
                        Duration::from_secs(1),
                    ),
                )
                .await
            }
        });

        let error = match connect_local(&endpoint, token).await {
            Ok(_) => panic!("negotiated client accepted a legacy challenge"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            C2ControlError::Protocol(ref message)
                if message.contains("omitted the required authenticated compatibility selection")
        ));
        match server_task.await.unwrap() {
            Ok(Ok(frame)) => panic!("client sent a post-challenge frame: {frame:?}"),
            Ok(Err(_)) | Err(_) => {}
        }
    }

    #[test]
    fn c2_control_bound_proof_rejects_tampered_selection() {
        let offer = client_compatibility_offer().unwrap();
        let selected = NegotiatedC2ControlCompatibility {
            protocol_version: C2_CONTROL_PROTOCOL_VERSION,
            capabilities: offer.capabilities.clone(),
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
        };
        let expected = c2_proof(
            "test-token",
            C2AuthDirection::Server,
            &[1; C2_AUTH_NONCE_BYTES],
            &[2; C2_AUTH_NONCE_BYTES],
            Some((&offer, &selected)),
        ).unwrap();
        let mut tampered = selected;
        tampered.path_semantics.style = PathStyle::Posix;
        let received = c2_proof(
            "test-token",
            C2AuthDirection::Server,
            &[1; C2_AUTH_NONCE_BYTES],
            &[2; C2_AUTH_NONCE_BYTES],
            Some((&offer, &tampered)),
        ).unwrap();

        assert!(!proofs_match(&received, &expected));
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_unix_path_before_write() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (_incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let request = NodeRequest::RegisterWorkspace {
            workspace_id: WorkspaceId::new("foreign").unwrap(),
            root: unix_path(),
        };
        assert!(reject_unnegotiated_outbound_path(&request, all_path_capabilities()).is_ok());
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx
            .send(ControlCommand { route: route(), request, reply: reply_tx })
            .await
            .unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), reply_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == OPAQUE_UNIX_PATH_NOT_NEGOTIATED
        ));
        assert!(writer_rx.try_recv().is_err());
        drop(commands_tx);
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_open_provider_spawn_before_write() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (_incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Spawn {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                provider: gate4agent_c2_protocol::AgentId::new("qwen-code").unwrap(),
                mode: SessionMode::Pty,
                terminal_size: TerminalSize { rows: 40, columns: 120 },
                initial_prompt: None,
            },
            reply: reply_tx,
        }).await.unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), reply_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == OPEN_PROVIDER_ID_NOT_NEGOTIATED
        ));
        assert!(writer_rx.try_recv().is_err());
        drop(commands_tx);
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_open_provider_reply_and_closes() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Snapshot,
            reply: reply_tx,
        }).await.unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        incoming_tx.send(OwnerInput::Frame(C2ServerFrame::Reply(
            gate4agent_c2_protocol::C2ReplyEnvelope {
                request_id: C2RequestId(1),
                result: Ok(RoutedNodeResponse {
                    node_id: NodeId::new("node-a").unwrap(),
                    incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
                    response: Ok(C2NodeResponse::Snapshot {
                        event_sequence: 1,
                        controller: None,
                        snapshot: gate4agent_c2_protocol::C2NodeSnapshot {
                            node_id: NodeId::new("node-a").unwrap(),
                            enabled_providers: vec![
                                gate4agent_c2_protocol::AgentId::new("qwen-code").unwrap(),
                            ],
                            provider_runtime_statuses: Default::default(),
                            workspaces: Vec::new(),
                            session_records: Vec::new(),
                        },
                    }),
                }),
            },
        ))).await.unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), reply_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == OPEN_PROVIDER_ID_NOT_NEGOTIATED
        ));
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(commands_tx.is_closed());
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_workspace_file_read_before_write_and_stays_healthy() {
        let (commands_tx, commands_rx) = mpsc::channel(2);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let (read_tx, read_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::ReadWorkspaceFile {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                path: repository_path("src/lib.rs"),
            },
            reply: read_tx,
        }).await.unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), read_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == WORKSPACE_FILE_READ_NOT_NEGOTIATED
        ));
        assert!(writer_rx.try_recv().is_err());

        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Snapshot,
            reply: snapshot_tx,
        }).await.unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        incoming_tx.send(reply(1)).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), snapshot_rx).await.unwrap().unwrap(),
            Ok(RoutedNodeResponse { response: Ok(C2NodeResponse::Accepted), .. })
        ));

        drop(commands_tx);
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
    }

    #[test]
    fn tagged_workspace_file_read_requires_both_file_and_repository_capabilities() {
        let request = NodeRequest::ReadWorkspaceFile {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            path: unix_repository_path(b"src/\xff"),
        };
        let missing_repository = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: false,
            workspace_file_read: true,
            provider_ids_open: true,
            terminal_frame_events: true,
        };
        assert!(matches!(
            reject_unnegotiated_outbound_path(&request, missing_repository),
            Err(C2ControlError::Protocol(ref message))
                if message == REPOSITORY_PATH_NOT_NEGOTIATED
        ));
        let missing_file_read = NegotiatedPathCapabilities {
            opaque_host_paths: true,
            repository_paths: true,
            workspace_file_read: false,
            provider_ids_open: true,
            terminal_frame_events: true,
        };
        assert!(matches!(
            reject_unnegotiated_outbound_path(&request, missing_file_read),
            Err(C2ControlError::Protocol(ref message))
                if message == WORKSPACE_FILE_READ_NOT_NEGOTIATED
        ));
        assert!(reject_unnegotiated_outbound_path(
            &request,
            all_path_capabilities(),
        ).is_ok());
    }

    #[test]
    fn workspace_file_read_response_is_gated_and_path_checked() {
        let response = RoutedNodeResponse {
            node_id: NodeId::new("node-a").unwrap(),
            incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            response: Ok(C2NodeResponse::WorkspaceFileRead {
                file: WorkspaceFileRead {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    path: unix_repository_path(b"src/\xff"),
                    content: WorkspaceFileContent::NonUtf8 { byte_len: 3 },
                },
            }),
        };
        assert!(routed_response_requires_workspace_file_read(&response));
        assert!(routed_response_has_unix_repository_path(&response));
        assert!(!routed_response_has_unix_bytes(&response));
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_unix_path_reply_and_closes() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx
            .send(ControlCommand {
                route: route(),
                request: NodeRequest::Snapshot,
                reply: reply_tx,
            })
            .await
            .unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        incoming_tx.send(unix_path_reply(1)).await.unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), reply_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == OPAQUE_UNIX_PATH_NOT_NEGOTIATED
        ));
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(commands_tx.is_closed());
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_tagged_repository_path_reply_and_closes() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx
            .send(ControlCommand {
                route: route(),
                request: NodeRequest::InspectWorkspace {
                    workspace_id: WorkspaceId::new("foreign").unwrap(),
                },
                reply: reply_tx,
            })
            .await
            .unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        incoming_tx.send(unix_repository_path_reply(1)).await.unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), reply_rx).await.unwrap().unwrap(),
            Err(C2ControlError::Protocol(ref message))
                if message == REPOSITORY_PATH_NOT_NEGOTIATED
        ));
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(commands_tx.is_closed());
    }

    #[tokio::test]
    async fn control_owner_rejects_unnegotiated_unix_path_event_and_closes() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let (topology_tx, _topology_rx) =
            watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        incoming_tx.send(unix_path_event(1)).await.unwrap();

        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(events_rx.try_recv().is_err());
        assert!(commands_tx.is_closed());
    }

    #[tokio::test]
    async fn late_reply_after_timed_waiter_does_not_desynchronize_following_request() {
        let (commands_tx, commands_rx) = mpsc::channel(2);
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (writer_tx, mut writer_rx) = mpsc::channel(2);
        let (incoming_tx, incoming_rx) = mpsc::channel(2);
        let (topology_tx, _topology_rx) = watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));

        let (expired_tx, expired_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Snapshot,
            reply: expired_tx,
        }).await.unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        drop(expired_rx);
        incoming_tx.send(reply(1)).await.unwrap();

        let (live_tx, live_rx) = oneshot::channel();
        commands_tx.send(ControlCommand {
            route: route(),
            request: NodeRequest::Snapshot,
            reply: live_tx,
        }).await.unwrap();
        assert!(matches!(writer_rx.recv().await, Some(C2ClientFrame::Request(_))));
        incoming_tx.send(reply(2)).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), live_rx).await.unwrap().unwrap(),
            Ok(RoutedNodeResponse { response: Ok(gate4agent_c2_protocol::C2NodeResponse::Accepted), .. })
        ));

        drop(incoming_tx);
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn slow_event_consumer_closes_control_owner_without_silent_drop() {
        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, _events_rx) = mpsc::channel(EVENT_CAPACITY);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(4);
        let (topology_tx, _topology_rx) = watch::channel(Arc::new(C2Topology { nodes: Vec::new() }));
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));

        incoming_tx.send(event(1)).await.unwrap();
        incoming_tx.send(event(2)).await.unwrap();
        incoming_tx.send(event(3)).await.unwrap();
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(commands_tx.is_closed());
    }

    #[tokio::test]
    async fn c2_client_topology_replaces_old_incarnation_status() {
        use gate4agent_c2_protocol::{C2TopologyNode, NodeTransportState};

        let (commands_tx, commands_rx) = mpsc::channel(1);
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (incoming_tx, incoming_rx) = mpsc::channel(1);
        let old_runtime_status = gate4agent_c2_protocol::ProviderRuntimeStatuses::new([
            gate4agent_c2_protocol::ProviderRuntimeStatus::raw_passthrough(
                gate4agent_c2_protocol::AgentId::new("claude").unwrap(),
                Some(gate4agent_c2_protocol::ProviderRuntimeVersion::new("1.0.0").unwrap()),
            ),
        ])
        .unwrap();
        let offline = Arc::new(C2Topology { nodes: vec![C2TopologyNode {
            node_id: NodeId::new("node-a").unwrap(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            transport: NodeTransportState::Offline,
            current_incarnation_id: Some(NodeIncarnationId::from_bytes([8; 16])),
            provider_contracts: Vec::new(),
            provider_adapter_contracts: Vec::new(),
            provider_runtime_statuses: old_runtime_status,
        }] });
        let (topology_tx, mut topology_rx) = watch::channel(offline);
        let owner = tokio::spawn(control_owner(
            commands_rx, events_tx, topology_tx, writer_tx, incoming_rx, no_path_capabilities(),
        ));
        let incarnation_id = NodeIncarnationId::from_bytes([9; 16]);
        incoming_tx.send(OwnerInput::Frame(C2ServerFrame::Topology(C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: NodeId::new("node-a").unwrap(),
                endpoint: r"\\.\pipe\node-a".to_owned(),
                transport: NodeTransportState::Online,
                current_incarnation_id: Some(incarnation_id),
                provider_contracts: Vec::new(),
                provider_adapter_contracts: Vec::new(),
                provider_runtime_statuses: gate4agent_c2_protocol::ProviderRuntimeStatuses::new([
                    gate4agent_c2_protocol::ProviderRuntimeStatus::raw_passthrough(
                        gate4agent_c2_protocol::AgentId::new("codex").unwrap(),
                        Some(gate4agent_c2_protocol::ProviderRuntimeVersion::new("2.0.0").unwrap()),
                    ),
                ])
                .unwrap(),
            }],
        }))).await.unwrap();

        timeout(Duration::from_secs(1), topology_rx.changed()).await.unwrap().unwrap();
        assert_eq!(topology_rx.borrow().nodes[0].current_incarnation_id, Some(incarnation_id));
        let topology = topology_rx.borrow().clone();
        let statuses = &topology.nodes[0].provider_runtime_statuses;
        assert_eq!(statuses.as_slice().len(), 1);
        assert_eq!(
            statuses.as_slice()[0].provider(),
            &gate4agent_c2_protocol::AgentId::new("codex").unwrap(),
        );
        assert_eq!(statuses.as_slice()[0].version().unwrap().as_str(), "2.0.0");
        assert!(events_rx.try_recv().is_err());
        drop(incoming_tx);
        timeout(Duration::from_secs(1), owner).await.unwrap().unwrap();
        assert!(commands_tx.is_closed());
    }
}
