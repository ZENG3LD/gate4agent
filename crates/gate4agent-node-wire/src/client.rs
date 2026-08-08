use gate4agent_node_protocol::{
    production_node_client_compatibility_offer,
    read_json_frame_limited_body_timeout, validate_provider_contract_manifest,
    write_json_frame_limited, CapabilityId,
    ClientAuthentication, ClientCompatibilityOffer, ClientFrame, ClientHello, ClientRole,
    FrameError, NegotiatedNodeCompatibility, NodeEvent, NodeEventEnvelope, NodeFailure, NodeHello,
    NodeId, NodeRequest, NodeResponse, NodeSnapshot, RequestEnvelope,
    ServerChallenge, ServerFrame,
    MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    NODE_AUTH_NONCE_BYTES,
    NODE_COMPATIBILITY_METADATA_CAPABILITY, NODE_OPAQUE_UNIX_PATH_CAPABILITY,
    NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, NODE_REPOSITORY_PATH_CAPABILITY,
    NODE_WORKSPACE_FILE_READ_CAPABILITY,
    NODE_PROTOCOL_VERSION,
};
use crate::{
    connect_local_stream, negotiated_auth_proof, proofs_match, random_nonce, AuthDirection,
    LocalClientStream,
};
#[cfg(test)]
use gate4agent_node_protocol::{
    NodeIncarnationId, ProtocolRange, StateSchemaSupport, NODE_INCARNATION_ID_BYTES,
    NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V2,
};
#[cfg(test)]
use crate::auth_proof;
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
#[cfg(feature = "fixture")]
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

const AUTH_FRAME_TIMEOUT_MS: u64 = 5_000;
const FRAME_BODY_TIMEOUT_MS: u64 = 5_000;

pub struct LocalNodeClient {
    pipe: LocalClientStream,
    hello: NodeHello,
    opaque_unix_paths_enabled: bool,
    repository_paths_enabled: bool,
    negotiated_capabilities: Vec<CapabilityId>,
    next_request_id: u64,
    pending_events: VecDeque<NodeEventEnvelope>,
}

impl LocalNodeClient {
    pub async fn connect(
        endpoint: impl AsRef<Path>,
        expected_node_id: &NodeId,
        role: ClientRole,
        access_token: &str,
    ) -> Result<Self, NodeClientError> {
        let mut pipe = connect_local_stream(endpoint).await?;
        let client_nonce = random_nonce().map_err(NodeClientError::Authentication)?;
        let compatibility_offer = client_compatibility_offer()?;
        write_json_frame_limited(
            &mut pipe,
            &ClientFrame::Hello(ClientHello::negotiating(
                role,
                client_nonce,
                compatibility_offer.clone(),
            )),
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await?;
        let challenge = timeout(
            Duration::from_millis(AUTH_FRAME_TIMEOUT_MS),
            read_json_frame_limited_body_timeout(
                &mut pipe,
                MAX_NODE_HELLO_FRAME_BYTES,
                Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
            ),
        )
        .await
        .map_err(|_| NodeClientError::AuthenticationTimedOut)??;
        let ServerFrame::Challenge(challenge) = challenge else {
            return Err(NodeClientError::Protocol(
                "server did not return an authentication challenge".to_owned(),
            ));
        };
        if challenge.protocol_version != NODE_PROTOCOL_VERSION {
            return Err(NodeClientError::Protocol(
                "node protocol version mismatch".to_owned(),
            ));
        }
        let authentication = prepare_negotiated_authentication(
            &challenge,
            &compatibility_offer,
            role,
            &client_nonce,
            access_token,
        )?;
        write_json_frame_limited(
            &mut pipe,
            &ClientFrame::Authenticate(authentication),
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await?;
        let server_hello = timeout(
            Duration::from_millis(AUTH_FRAME_TIMEOUT_MS),
            read_json_frame_limited_body_timeout(
                &mut pipe,
                MAX_NODE_FRAME_BYTES,
                Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
            ),
        )
        .await
        .map_err(|_| NodeClientError::AuthenticationTimedOut)??;
        let ServerFrame::Hello(hello) = server_hello else {
            return Err(NodeClientError::Protocol(
                "server did not return hello".to_owned(),
            ));
        };
        if hello.protocol_version != NODE_PROTOCOL_VERSION {
            return Err(NodeClientError::Protocol(
                "node protocol version mismatch".to_owned(),
            ));
        }
        validate_authenticated_hello_compatibility(
            &compatibility_offer,
            challenge.compatibility.as_ref().ok_or_else(|| {
                NodeClientError::Protocol(
                    "node omitted the required authenticated compatibility selection".to_owned(),
                )
            })?,
            hello.compatibility.as_ref(),
        )?;
        let opaque_unix_paths_enabled = selected_supports_opaque_unix_paths(
            hello.compatibility.as_ref(),
        );
        let repository_paths_enabled = selected_supports_repository_paths(
            hello.compatibility.as_ref(),
        );
        let negotiated_capabilities = hello.compatibility.as_ref()
            .map(|compatibility| compatibility.capabilities.clone())
            .unwrap_or_default();
        ensure_node_hello_path_capability(&hello, opaque_unix_paths_enabled)?;
        if &hello.snapshot.node_id != expected_node_id {
            return Err(NodeClientError::Protocol(format!(
                "node identity mismatch: expected '{}', received '{}'",
                expected_node_id,
                hello.snapshot.node_id,
            )));
        }
        Ok(Self {
            pipe,
            hello,
            opaque_unix_paths_enabled,
            repository_paths_enabled,
            negotiated_capabilities,
            next_request_id: 1,
            pending_events: VecDeque::new(),
        })
    }

    pub fn hello(&self) -> &NodeHello {
        &self.hello
    }

    pub async fn send(&mut self, request: NodeRequest) -> Result<u64, NodeClientError> {
        let request_id = reserve_request_id(
            &mut self.next_request_id,
            &request,
            self.opaque_unix_paths_enabled,
            self.repository_paths_enabled,
            &self.negotiated_capabilities,
        )?;
        write_json_frame_limited(
            &mut self.pipe,
            &ClientFrame::Request(RequestEnvelope {
                request_id,
                request,
            }),
            MAX_NODE_CLIENT_FRAME_BYTES,
        )
        .await?;
        Ok(request_id)
    }

    pub async fn recv(&mut self) -> Result<ServerFrame, NodeClientError> {
        let frame = read_json_frame_limited_body_timeout(
            &mut self.pipe,
            MAX_NODE_FRAME_BYTES,
            Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
        )
        .await?;
        ensure_server_frame_required_capability(&frame, &self.negotiated_capabilities)?;
        ensure_server_frame_path_capability(
            &frame,
            self.opaque_unix_paths_enabled,
            self.repository_paths_enabled,
        )?;
        Ok(frame)
    }

    pub async fn request(&mut self, request: NodeRequest) -> Result<NodeResponse, NodeClientError> {
        let request_id = self.send(request).await?;
        loop {
            match self.recv().await? {
                ServerFrame::Reply(reply) if reply.request_id == request_id => {
                    return reply.result.map_err(NodeClientError::Node);
                }
                ServerFrame::Reply(reply) => {
                    return Err(NodeClientError::Protocol(format!(
                        "unexpected response id {} while waiting for {request_id}",
                        reply.request_id,
                    )));
                }
                ServerFrame::Event(event) => self.pending_events.push_back(event),
                ServerFrame::Challenge(_) => {
                    return Err(NodeClientError::Protocol(
                        "duplicate server challenge".to_owned(),
                    ));
                }
                ServerFrame::Hello(_) => {
                    return Err(NodeClientError::Protocol(
                        "duplicate server hello".to_owned(),
                    ));
                }
            }
        }
    }

    pub fn take_event(&mut self) -> Option<NodeEventEnvelope> {
        self.pending_events.pop_front()
    }

    #[cfg(feature = "fixture")]
    pub async fn send_malformed_json_frame_for_fixture(&mut self) -> Result<(), NodeClientError> {
        self.pipe.write_u32_le(1).await?;
        self.pipe.write_all(b"{").await?;
        self.pipe.flush().await?;
        Ok(())
    }
}

fn selected_supports_opaque_unix_paths(
    selected: Option<&NegotiatedNodeCompatibility>,
) -> bool {
    selected.is_some_and(|compatibility| {
        compatibility.capabilities.iter().any(|capability| {
            capability.as_str() == NODE_OPAQUE_UNIX_PATH_CAPABILITY
        })
    })
}

fn selected_supports_repository_paths(
    selected: Option<&NegotiatedNodeCompatibility>,
) -> bool {
    selected.is_some_and(|compatibility| {
        compatibility.capabilities.iter().any(|capability| {
            capability.as_str() == NODE_REPOSITORY_PATH_CAPABILITY
        })
    })
}

fn reserve_request_id(
    next_request_id: &mut u64,
    request: &NodeRequest,
    opaque_unix_paths_enabled: bool,
    repository_paths_enabled: bool,
    negotiated_capabilities: &[CapabilityId],
) -> Result<u64, NodeClientError> {
    ensure_node_request_required_capability(request, negotiated_capabilities)?;
    ensure_node_request_path_capability(
        request,
        opaque_unix_paths_enabled,
        repository_paths_enabled,
    )?;
    let request_id = *next_request_id;
    *next_request_id = next_request_id
        .checked_add(1)
        .ok_or(NodeClientError::RequestIdExhausted)?;
    Ok(request_id)
}

fn ensure_node_request_required_capability(
    request: &NodeRequest,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    if let Some(required) = request.required_capability() {
        if !negotiated_capabilities.iter().any(|capability| capability.as_str() == required) {
            return Err(NodeClientError::UnsupportedCapability(required.to_owned()));
        }
    }
    Ok(())
}

fn ensure_server_frame_required_capability(
    frame: &ServerFrame,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    let requires_workspace_file_read = matches!(
        frame,
        ServerFrame::Reply(reply)
            if matches!(reply.result.as_ref(), Ok(NodeResponse::WorkspaceFileRead { .. }))
    );
    if requires_workspace_file_read
        && !negotiated_capabilities.iter().any(|capability| {
            capability.as_str() == NODE_WORKSPACE_FILE_READ_CAPABILITY
        })
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_WORKSPACE_FILE_READ_CAPABILITY.to_owned(),
        ));
    }
    Ok(())
}

fn ensure_node_hello_path_capability(
    hello: &NodeHello,
    opaque_unix_paths_enabled: bool,
) -> Result<(), NodeClientError> {
    ensure_opaque_unix_path_capability(
        node_snapshot_contains_opaque_unix_path(&hello.snapshot),
        opaque_unix_paths_enabled,
    )
}

fn ensure_node_request_path_capability(
    request: &NodeRequest,
    opaque_unix_paths_enabled: bool,
    repository_paths_enabled: bool,
) -> Result<(), NodeClientError> {
    ensure_opaque_unix_path_capability(
        node_request_contains_opaque_unix_path(request),
        opaque_unix_paths_enabled,
    )?;
    ensure_repository_path_capability(
        node_request_contains_tagged_repository_path(request),
        repository_paths_enabled,
    )
}

fn ensure_server_frame_path_capability(
    frame: &ServerFrame,
    opaque_unix_paths_enabled: bool,
    repository_paths_enabled: bool,
) -> Result<(), NodeClientError> {
    ensure_opaque_unix_path_capability(
        server_frame_contains_opaque_unix_path(frame),
        opaque_unix_paths_enabled,
    )?;
    ensure_repository_path_capability(
        server_frame_contains_tagged_repository_path(frame),
        repository_paths_enabled,
    )
}

fn ensure_opaque_unix_path_capability(
    contains_opaque_unix_path: bool,
    opaque_unix_paths_enabled: bool,

) -> Result<(), NodeClientError> {
    if contains_opaque_unix_path && !opaque_unix_paths_enabled {
        return Err(NodeClientError::Protocol(
            "node sent or received opaque Unix path bytes without negotiating the capability"
                .to_owned(),
        ));
    }
    Ok(())
}

fn ensure_repository_path_capability(
    contains_tagged_repository_path: bool,
    repository_paths_enabled: bool,
) -> Result<(), NodeClientError> {
    if contains_tagged_repository_path && !repository_paths_enabled {
        return Err(NodeClientError::Protocol(
            "node sent tagged repository path bytes without negotiating the capability"
                .to_owned(),
        ));
    }
    Ok(())
}

fn node_request_contains_opaque_unix_path(request: &NodeRequest) -> bool {
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

fn node_request_contains_tagged_repository_path(request: &NodeRequest) -> bool {
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

fn server_frame_contains_opaque_unix_path(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Hello(hello) => node_snapshot_contains_opaque_unix_path(&hello.snapshot),
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(
            node_response_contains_opaque_unix_path,
        ),
        ServerFrame::Event(event) => node_event_contains_opaque_unix_path(&event.event),
        ServerFrame::Challenge(_) => false,
    }
}

fn server_frame_contains_tagged_repository_path(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(
            node_response_contains_tagged_repository_path,
        ),
        ServerFrame::Challenge(_) | ServerFrame::Hello(_) | ServerFrame::Event(_) => false,
    }
}

fn node_response_contains_tagged_repository_path(response: &NodeResponse) -> bool {
    match response {
        NodeResponse::WorkspaceInspected { inspection } => {
            inspection.entries.iter().any(|entry| {
                entry.relative_path.as_unix_bytes().is_some()
            }) || inspection.git.status.iter().any(|status| {
                status.path.as_unix_bytes().is_some()
                    || status.previous_path.as_ref().is_some_and(|path| {
                        path.as_unix_bytes().is_some()
                    })
            })
        }
        NodeResponse::WorkspaceFileRead { file } => file.path.as_unix_bytes().is_some(),
        NodeResponse::Snapshot { .. }
        | NodeResponse::Resync { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SessionRecordUpdated { .. }
        | NodeResponse::SessionRecordResumed { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceRegistered { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::WorktreeCreated { .. }
        | NodeResponse::WorktreeRemoved { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => false,
    }
}

fn node_response_contains_opaque_unix_path(response: &NodeResponse) -> bool {
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            node_snapshot_contains_opaque_unix_path(snapshot)
        }
        NodeResponse::Resync { snapshot, events, .. } => {
            node_snapshot_contains_opaque_unix_path(snapshot)
                || events.iter().any(|event| {
                    node_event_contains_opaque_unix_path(&event.event)
                })
        }
        NodeResponse::WorkspaceInspected { inspection } => inspection.git.worktrees
            .iter()
            .any(|worktree| worktree.path.as_unix_bytes().is_some()),
        NodeResponse::WorkspaceFileRead { .. } => false,
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            record.canonical_root.as_unix_bytes().is_some()
        }
        NodeResponse::WorkspaceRegistered { workspace } => {
            workspace.canonical_root.as_unix_bytes().is_some()
        }
        NodeResponse::WorktreeCreated { worktree, workspace } => {
            worktree.path.as_unix_bytes().is_some()
                || workspace.canonical_root.as_unix_bytes().is_some()
        }
        NodeResponse::WorktreeRemoved { target_root, .. } => {
            target_root.as_unix_bytes().is_some()
        }
        NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => false,
    }
}

fn node_snapshot_contains_opaque_unix_path(snapshot: &NodeSnapshot) -> bool {
    snapshot.workspaces.iter().any(|workspace| {
        workspace.canonical_root.as_unix_bytes().is_some()
    }) || snapshot.session_records.iter().any(|record| {
        record.canonical_root.as_unix_bytes().is_some()
    })
}

fn node_event_contains_opaque_unix_path(event: &NodeEvent) -> bool {
    match event {
        NodeEvent::WorkspaceAdded { workspace } => {
            workspace.canonical_root.as_unix_bytes().is_some()
        }
        NodeEvent::SessionRecordUpserted { record } => {
            record.canonical_root.as_unix_bytes().is_some()
        }
        NodeEvent::Control { .. }
        | NodeEvent::ControllerChanged { .. }
        | NodeEvent::WorkspaceRemoved { .. }
        | NodeEvent::SessionRecordRemoved { .. }
        | NodeEvent::ResyncRequired { .. } => false,
    }
}

fn client_compatibility_offer() -> Result<ClientCompatibilityOffer, NodeClientError> {
    Ok(production_node_client_compatibility_offer())
}

fn prepare_negotiated_authentication(
    challenge: &ServerChallenge,
    offer: &ClientCompatibilityOffer,
    role: ClientRole,
    client_nonce: &[u8; NODE_AUTH_NONCE_BYTES],
    access_token: &str,
) -> Result<ClientAuthentication, NodeClientError> {
    let selected = challenge.compatibility.as_ref().ok_or_else(|| {
        NodeClientError::Protocol(
            "node omitted the required authenticated compatibility selection".to_owned(),
        )
    })?;
    validate_selected_compatibility(offer, selected)?;
    let expected_server_proof = negotiated_auth_proof(
        access_token.as_bytes(),
        AuthDirection::Server,
        role,
        client_nonce,
        &challenge.server_nonce,
        offer,
        selected,
    )
    .map_err(NodeClientError::Authentication)?;
    if !proofs_match(&challenge.server_proof, &expected_server_proof) {
        return Err(NodeClientError::Protocol(
            "server failed access-token proof".to_owned(),
        ));
    }
    let client_proof = negotiated_auth_proof(
        access_token.as_bytes(),
        AuthDirection::Client,
        role,
        client_nonce,
        &challenge.server_nonce,
        offer,
        selected,
    )
    .map_err(NodeClientError::Authentication)?;
    Ok(ClientAuthentication { client_proof })
}

fn validate_authenticated_hello_compatibility(
    offer: &ClientCompatibilityOffer,
    challenge: &NegotiatedNodeCompatibility,
    hello: Option<&NegotiatedNodeCompatibility>,
) -> Result<(), NodeClientError> {
    let hello = hello.ok_or_else(|| {
        NodeClientError::Protocol(
            "node hello omitted the required authenticated compatibility selection".to_owned(),
        )
    })?;
    if hello != challenge {
        return Err(NodeClientError::Protocol(
            "node compatibility selection changed during authentication".to_owned(),
        ));
    }
    validate_selected_compatibility(offer, hello)
}

fn validate_selected_compatibility(
    offer: &ClientCompatibilityOffer,
    selected: &NegotiatedNodeCompatibility,
) -> Result<(), NodeClientError> {
    if selected.protocol_version != NODE_PROTOCOL_VERSION {
        return Err(NodeClientError::Protocol(format!(
            "node selected protocol version {} for active wire protocol {}",
            selected.protocol_version,
            NODE_PROTOCOL_VERSION,
        )));
    }
    if !offer.protocol_versions.contains(selected.protocol_version) {
        return Err(NodeClientError::Protocol(format!(
            "node selected protocol version {} outside the client offer",
            selected.protocol_version,
        )));
    }
    if selected
        .capabilities
        .iter()
        .any(|capability| !offer.capabilities.contains(capability))
    {
        return Err(NodeClientError::Protocol(
            "node selected a capability outside the client offer".to_owned(),
        ));
    }
    if !selected.capabilities.iter().any(|capability| {
        capability.as_str() == NODE_COMPATIBILITY_METADATA_CAPABILITY
    }) {
        return Err(NodeClientError::Protocol(
            "node omitted the required compatibility metadata capability".to_owned(),
        ));
    }
    let provider_manifest_selected = selected.capabilities.iter().any(|capability| {
        capability.as_str() == NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY
    });
    if !provider_manifest_selected
        && (!selected.provider_contracts.is_empty()
            || !selected.provider_adapter_contracts.is_empty())
    {
        return Err(NodeClientError::Protocol(
            "node published a provider contract manifest without negotiating the capability"
                .to_owned(),
        ));
    }
    if provider_manifest_selected {
        validate_provider_contract_manifest(
            &selected.provider_contracts,
            &selected.provider_adapter_contracts,
        )
        .map_err(|error| NodeClientError::Protocol(error.to_string()))?;
    }
    if let Some(state_schema_version) = selected.state_schema_version {
        let Some(state_schema) = offer.state_schema else {
            return Err(NodeClientError::Protocol(
                "node selected a state schema that the client did not offer".to_owned(),
            ));
        };
        if !state_schema.versions.contains(state_schema_version) {
            return Err(NodeClientError::Protocol(format!(
                "node selected state schema version {state_schema_version} outside the client offer",
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum NodeClientError {
    #[error("local IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("node rejected request: {0:?}")]
    Node(NodeFailure),
    #[error("node protocol failed: {0}")]
    Protocol(String),
    #[error("node capability was not negotiated: {0}")]
    UnsupportedCapability(String),
    #[error("node authentication frame was not received before the bounded deadline")]
    AuthenticationTimedOut,
    #[error("node authentication primitive failed: {0}")]
    Authentication(String),
    #[error("request id counter is exhausted")]
    RequestIdExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{
        AdapterContractRevision, AdapterFamily, AdapterId, AgentProvider, ArchitectureId,
        GitSnapshot, GitStatusEntry, GitWorktreeSnapshot, HostDescriptor, LocalTransportKind,
        ManagedSessionRecord, ManagedSessionState, NodeCompatibilitySupport, OpaqueHostPath,
        OperatingSystemId, PathEncoding, PathSemantics, PathStyle,
        ProviderAdapterContractSupport, ProviderContractRevision, ProviderContractSupport,
        RepositoryPath, ResponseEnvelope, SessionMode, SessionRecordId, WorkspaceEntry,
        WorkspaceEntryKind, WorkspaceFileContent, WorkspaceFileRead, WorkspaceId,
        WorkspaceInspection, WorkspaceSnapshot,
    };

    fn negotiated_fixture() -> (ClientCompatibilityOffer, NegotiatedNodeCompatibility) {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![CapabilityId::new(NODE_COMPATIBILITY_METADATA_CAPABILITY).unwrap()],
            state_schema: Some(StateSchemaSupport {
                versions: ProtocolRange::exact(1).unwrap(),
            }),
        };
        let support = NodeCompatibilitySupport {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![CapabilityId::new(NODE_COMPATIBILITY_METADATA_CAPABILITY).unwrap()],
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
            local_transport: LocalTransportKind::WindowsNamedPipe,
            state_schema: StateSchemaSupport {
                versions: ProtocolRange::exact(1).unwrap(),
            },
            provider_contracts: Vec::new(),
            provider_adapter_contracts: Vec::new(),
        };
        let selected = support.negotiate(NODE_PROTOCOL_VERSION, &offer).unwrap();
        (offer, selected)
    }

    fn unix_path() -> OpaqueHostPath {
        OpaqueHostPath::unix_bytes(vec![b'/', b's', b'r', b'v', b'/', 0xff]).unwrap()
    }

    fn utf8_path() -> OpaqueHostPath {
        OpaqueHostPath::utf8(r"C:\repo".to_owned()).unwrap()
    }

    fn tagged_repository_path(value: &[u8]) -> RepositoryPath {
        RepositoryPath::unix_bytes(value.to_vec()).unwrap()
    }

    fn utf8_repository_path(value: &str) -> RepositoryPath {
        RepositoryPath::utf8(value.to_owned()).unwrap()
    }

    fn workspace_with_path(canonical_root: OpaqueHostPath) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            canonical_root,
            sessions: Vec::new(),
        }
    }

    fn session_record_with_path(canonical_root: OpaqueHostPath) -> ManagedSessionRecord {
        ManagedSessionRecord {
            record_id: SessionRecordId::new("session-a").unwrap(),
            display_name: "session a".to_owned(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            canonical_root,
            provider_session: None,
            active_session: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
            last_error: None,
        }
    }

    fn worktree_with_path(path: OpaqueHostPath) -> GitWorktreeSnapshot {
        GitWorktreeSnapshot {
            path,
            head: "abcdef".to_owned(),
            branch: Some("main".to_owned()),
            is_bare: false,
            is_main: true,
            locked: false,
            lock_reason: None,
            prunable: false,
            prunable_reason: None,
            workspace_id: Some(WorkspaceId::new("workspace-a").unwrap()),
        }
    }

    fn empty_snapshot() -> NodeSnapshot {
        NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
        }
    }

    fn hello_with_snapshot(snapshot: NodeSnapshot) -> NodeHello {
        NodeHello {
            protocol_version: NODE_PROTOCOL_VERSION,
            incarnation_id: NodeIncarnationId::from_bytes([3; NODE_INCARNATION_ID_BYTES]),
            connection_id: 7,
            role: ClientRole::Operator,
            event_sequence: 0,
            controller: None,
            snapshot,
            compatibility: None,
        }
    }

    fn response_frame(response: NodeResponse) -> ServerFrame {
        ServerFrame::Reply(ResponseEnvelope {
            request_id: 1,
            result: Ok(response),
        })
    }

    #[test]
    fn client_offer_accepts_durable_state_schema_v1_through_v2() {
        let offer = client_compatibility_offer().unwrap();
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_OPAQUE_UNIX_PATH_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_REPOSITORY_PATH_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap(),
        ));
        assert_eq!(
            offer.state_schema.unwrap().versions,
            ProtocolRange::new(NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V2).unwrap(),
        );
    }

    #[test]
    fn hmac_valid_legacy_challenge_is_rejected_before_authenticate() {
        let offer = client_compatibility_offer().unwrap();
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let access_token = "strict-negotiation-token";
        let server_proof = auth_proof(
            access_token.as_bytes(),
            AuthDirection::Server,
            ClientRole::Observer,
            &client_nonce,
            &server_nonce,
        )
        .unwrap();
        let challenge = ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
            compatibility: None,
        };

        let result = prepare_negotiated_authentication(
            &challenge,
            &offer,
            ClientRole::Observer,
            &client_nonce,
            access_token,
        );
        assert!(matches!(
            result,
            Err(NodeClientError::Protocol(message))
                if message.contains("required authenticated compatibility selection")
        ));
    }

    #[test]
    fn hmac_valid_selection_without_compatibility_metadata_is_rejected() {
        let (offer, mut selected) = negotiated_fixture();
        selected.capabilities.clear();
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let access_token = "strict-negotiation-token";
        let server_proof = negotiated_auth_proof(
            access_token.as_bytes(),
            AuthDirection::Server,
            ClientRole::Observer,
            &client_nonce,
            &server_nonce,
            &offer,
            &selected,
        )
        .unwrap();
        let challenge = ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
            compatibility: Some(selected),
        };

        let result = prepare_negotiated_authentication(
            &challenge,
            &offer,
            ClientRole::Observer,
            &client_nonce,
            access_token,
        );
        assert!(matches!(
            result,
            Err(NodeClientError::Protocol(message))
                if message.contains("required compatibility metadata capability")
        ));
    }

    #[test]
    fn selected_manifest_requires_capability_and_valid_provider_linkage() {
        let (offer, mut selected) = negotiated_fixture();
        selected.provider_contracts.push(ProviderContractSupport {
            provider: AgentProvider::Codex,
            revision: ProviderContractRevision::new("codex.2026-08").unwrap(),
        });
        assert!(matches!(
            validate_selected_compatibility(&offer, &selected),
            Err(NodeClientError::Protocol(message))
                if message.contains("without negotiating the capability")
        ));

        let mut offer = offer;
        let manifest_capability =
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap();
        offer.capabilities.push(manifest_capability.clone());
        selected.capabilities.push(manifest_capability);
        selected.provider_adapter_contracts.push(ProviderAdapterContractSupport {
            provider: AgentProvider::Claude,
            family: AdapterFamily::PtySemantic,
            adapter_id: AdapterId::new("claude-code").unwrap(),
            revision: AdapterContractRevision::new("pty-semantic-v1").unwrap(),
        });
        assert!(matches!(
            validate_selected_compatibility(&offer, &selected),
            Err(NodeClientError::Protocol(message))
                if message.contains("has no provider contract")
        ));
    }

    #[test]
    fn authenticated_manifest_revision_tampering_is_rejected() {
        let (mut offer, mut selected) = negotiated_fixture();
        let manifest_capability =
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap();
        offer.capabilities.push(manifest_capability.clone());
        selected.capabilities.push(manifest_capability);
        selected.provider_contracts.push(ProviderContractSupport {
            provider: AgentProvider::Codex,
            revision: ProviderContractRevision::new("codex.2026-08").unwrap(),
        });
        selected.provider_adapter_contracts.push(ProviderAdapterContractSupport {
            provider: AgentProvider::Codex,
            family: AdapterFamily::PtySemantic,
            adapter_id: AdapterId::new("codex-cli").unwrap(),
            revision: AdapterContractRevision::new("pty-semantic-v1").unwrap(),
        });
        let client_nonce = [3; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [7; NODE_AUTH_NONCE_BYTES];
        let access_token = "strict-negotiation-token";
        let server_proof = negotiated_auth_proof(
            access_token.as_bytes(),
            AuthDirection::Server,
            ClientRole::Observer,
            &client_nonce,
            &server_nonce,
            &offer,
            &selected,
        )
        .unwrap();
        selected.provider_adapter_contracts[0].revision =
            AdapterContractRevision::new("pty-semantic-v2").unwrap();
        let challenge = ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
            compatibility: Some(selected),
        };
        assert!(matches!(
            prepare_negotiated_authentication(
                &challenge,
                &offer,
                ClientRole::Observer,
                &client_nonce,
                access_token,
            ),
            Err(NodeClientError::Protocol(message))
                if message.contains("server failed access-token proof")
        ));
    }

    #[test]
    fn node_hello_requires_the_same_nonempty_compatibility_selection() {
        let (offer, selected) = negotiated_fixture();
        assert!(validate_authenticated_hello_compatibility(
            &offer,
            &selected,
            None,
        )
        .is_err());

        let mut changed = selected.clone();
        changed.path_semantics.style = PathStyle::Posix;
        assert!(validate_authenticated_hello_compatibility(
            &offer,
            &selected,
            Some(&changed),
        )
        .is_err());
        assert!(validate_authenticated_hello_compatibility(
            &offer,
            &selected,
            Some(&selected),
        )
        .is_ok());
    }

    #[test]
    fn opaque_unix_path_gate_requires_explicit_authenticated_selection() {
        let (_, mut selected) = negotiated_fixture();
        assert!(!selected_supports_opaque_unix_paths(None));
        assert!(!selected_supports_opaque_unix_paths(Some(&selected)));

        selected.capabilities.push(
            CapabilityId::new(NODE_OPAQUE_UNIX_PATH_CAPABILITY).unwrap(),
        );
        assert!(selected_supports_opaque_unix_paths(Some(&selected)));
    }

    #[test]
    fn repository_path_gate_requires_explicit_authenticated_selection() {
        let (_, mut selected) = negotiated_fixture();
        assert!(!selected_supports_repository_paths(None));
        assert!(!selected_supports_repository_paths(Some(&selected)));

        selected.capabilities.push(
            CapabilityId::new(NODE_REPOSITORY_PATH_CAPABILITY).unwrap(),
        );
        assert!(selected_supports_repository_paths(Some(&selected)));
    }

    #[test]
    fn unnegotiated_workspace_file_read_is_rejected_before_consuming_a_request_id() {
        let request = NodeRequest::ReadWorkspaceFile {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            path: utf8_repository_path("src/lib.rs"),
        };
        let mut next_request_id = 41;
        let no_capabilities = Vec::new();
        let error = reserve_request_id(
            &mut next_request_id,
            &request,
            false,
            false,
            &no_capabilities,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NodeClientError::UnsupportedCapability(capability)
                if capability == NODE_WORKSPACE_FILE_READ_CAPABILITY
        ));
        assert_eq!(next_request_id, 41);

        let file_read_capabilities = vec![
            CapabilityId::new(NODE_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
        ];
        let request_id = reserve_request_id(
            &mut next_request_id,
            &request,
            false,
            false,
            &file_read_capabilities,
        )
        .unwrap();
        assert_eq!(request_id, 41);
        assert_eq!(next_request_id, 42);
    }

    #[test]
    fn unnegotiated_workspace_file_response_is_rejected_before_exposure() {
        let frame = response_frame(NodeResponse::WorkspaceFileRead {
            file: WorkspaceFileRead {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                path: utf8_repository_path("src/lib.rs"),
                content: WorkspaceFileContent::Utf8 {
                    text: "hello".to_owned(),
                    byte_len: 5,
                },
            },
        });
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        let capabilities = vec![
            CapabilityId::new(NODE_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
        ];
        assert!(ensure_server_frame_required_capability(&frame, &capabilities).is_ok());
    }

    #[test]
    fn malicious_legacy_hello_with_unix_path_is_rejected_before_exposure() {
        let mut snapshot = empty_snapshot();
        snapshot.workspaces.push(workspace_with_path(unix_path()));
        let hello = hello_with_snapshot(snapshot);

        assert!(ensure_node_hello_path_capability(&hello, false).is_err());
        assert!(ensure_server_frame_path_capability(
            &ServerFrame::Hello(hello),
            false,
            false,
        )
        .is_err());
    }

    #[test]
    fn legacy_utf8_hello_and_payloads_remain_accepted() {
        let mut snapshot = empty_snapshot();
        snapshot.workspaces.push(workspace_with_path(utf8_path()));
        let hello = hello_with_snapshot(snapshot);

        assert!(ensure_node_hello_path_capability(&hello, false).is_ok());
        assert!(ensure_node_request_path_capability(
            &NodeRequest::RegisterWorkspace {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                root: utf8_path(),
            },
            false,
            false,
        )
        .is_ok());
        assert!(ensure_server_frame_path_capability(
            &response_frame(NodeResponse::WorkspaceRegistered {
                workspace: workspace_with_path(utf8_path()),
            }),
            false,
            false,
        )
        .is_ok());
    }

    #[test]
    fn outbound_guard_covers_every_path_bearing_request_variant() {
        let requests = [
            NodeRequest::RegisterWorkspace {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                root: unix_path(),
            },
            NodeRequest::CreateWorktree {
                source_workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                workspace_id: WorkspaceId::new("workspace-b").unwrap(),
                target_root: unix_path(),
                branch: "feature/a".to_owned(),
                base: None,
            },
            NodeRequest::RemoveWorktree {
                source_workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                target_root: unix_path(),
            },
        ];

        for request in requests {
            assert!(ensure_node_request_path_capability(&request, false, false).is_err());
            assert!(ensure_node_request_path_capability(&request, true, false).is_ok());
        }
        assert!(ensure_node_request_path_capability(&NodeRequest::Snapshot, false, false).is_ok());

        let tagged_file_read = NodeRequest::ReadWorkspaceFile {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            path: tagged_repository_path(b"src/\xff"),
        };
        assert!(ensure_node_request_path_capability(&tagged_file_read, false, false).is_err());
        assert!(ensure_node_request_path_capability(&tagged_file_read, false, true).is_ok());
    }

    #[test]
    fn inbound_guard_covers_path_bearing_response_variants() {
        let mut snapshot = empty_snapshot();
        snapshot.workspaces.push(workspace_with_path(unix_path()));
        let inspection = WorkspaceInspection {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            entries: Vec::new(),
            tree_truncated: false,
            git: GitSnapshot {
                is_repository: true,
                branch: Some("main".to_owned()),
                status: Vec::new(),
                recent_commits: Vec::new(),
                worktrees: vec![worktree_with_path(unix_path())],
                truncated: false,
                diagnostic: None,
            },
        };
        let responses = vec![
            NodeResponse::Snapshot {
                event_sequence: 1,
                controller: None,
                snapshot: snapshot.clone(),
            },
            NodeResponse::Resync {
                event_sequence: 1,
                snapshot: empty_snapshot(),
                events: vec![NodeEventEnvelope {
                    sequence: 1,
                    event: NodeEvent::WorkspaceAdded {
                        workspace: workspace_with_path(unix_path()),
                    },
                }],
            },
            NodeResponse::WorkspaceInspected { inspection },
            NodeResponse::SessionRecordUpdated {
                record: session_record_with_path(unix_path()),
            },
            NodeResponse::WorkspaceRegistered {
                workspace: workspace_with_path(unix_path()),
            },
            NodeResponse::WorktreeCreated {
                worktree: worktree_with_path(unix_path()),
                workspace: workspace_with_path(utf8_path()),
            },
            NodeResponse::WorktreeRemoved {
                target_root: unix_path(),
                workspace_id: None,
            },
        ];

        for response in responses {
            let frame = response_frame(response);
            assert!(ensure_server_frame_path_capability(&frame, false, false).is_err());
            assert!(ensure_server_frame_path_capability(&frame, true, false).is_ok());
        }
    }

    #[test]
    fn inbound_guard_covers_path_bearing_event_variants() {
        let events = [
            NodeEvent::WorkspaceAdded {
                workspace: workspace_with_path(unix_path()),
            },
            NodeEvent::SessionRecordUpserted {
                record: session_record_with_path(unix_path()),
            },
        ];

        for event in events {
            let frame = ServerFrame::Event(NodeEventEnvelope {
                sequence: 1,
                event,
            });
            assert!(ensure_server_frame_path_capability(&frame, false, false).is_err());
            assert!(ensure_server_frame_path_capability(&frame, true, false).is_ok());
        }
    }

    #[test]
    fn inbound_guard_covers_every_tagged_repository_path_location() {
        let inspections = [
            WorkspaceInspection {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                entries: vec![WorkspaceEntry {
                    relative_path: tagged_repository_path(b"src/\xff"),
                    kind: WorkspaceEntryKind::File,
                }],
                tree_truncated: false,
                git: GitSnapshot {
                    is_repository: false,
                    branch: None,
                    status: Vec::new(),
                    recent_commits: Vec::new(),
                    worktrees: Vec::new(),
                    truncated: false,
                    diagnostic: None,
                },
            },
            WorkspaceInspection {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                entries: Vec::new(),
                tree_truncated: false,
                git: GitSnapshot {
                    is_repository: true,
                    branch: None,
                    status: vec![GitStatusEntry {
                        index_status: "M".to_owned(),
                        worktree_status: " ".to_owned(),
                        path: tagged_repository_path(b"src/\xff"),
                        previous_path: None,
                    }],
                    recent_commits: Vec::new(),
                    worktrees: Vec::new(),
                    truncated: false,
                    diagnostic: None,
                },
            },
            WorkspaceInspection {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                entries: Vec::new(),
                tree_truncated: false,
                git: GitSnapshot {
                    is_repository: true,
                    branch: None,
                    status: vec![GitStatusEntry {
                        index_status: "R".to_owned(),
                        worktree_status: " ".to_owned(),
                        path: utf8_repository_path("src/new.rs"),
                        previous_path: Some(tagged_repository_path(b"src/\xff")),
                    }],
                    recent_commits: Vec::new(),
                    worktrees: Vec::new(),
                    truncated: false,
                    diagnostic: None,
                },
            },
        ];

        for inspection in inspections {
            let frame = response_frame(NodeResponse::WorkspaceInspected { inspection });
            assert!(ensure_server_frame_path_capability(&frame, false, false).is_err());
            assert!(ensure_server_frame_path_capability(&frame, false, true).is_ok());
        }

        let file_frame = response_frame(NodeResponse::WorkspaceFileRead {
            file: WorkspaceFileRead {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                path: tagged_repository_path(b"src/\xff"),
                content: WorkspaceFileContent::NonUtf8 { byte_len: 3 },
            },
        });
        assert!(ensure_server_frame_path_capability(&file_frame, false, false).is_err());
        assert!(ensure_server_frame_path_capability(&file_frame, false, true).is_ok());
    }

    #[test]
    fn legacy_utf8_repository_paths_remain_accepted_without_capability() {
        let inspection = WorkspaceInspection {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            entries: vec![WorkspaceEntry {
                relative_path: utf8_repository_path("src/lib.rs"),
                kind: WorkspaceEntryKind::File,
            }],
            tree_truncated: false,
            git: GitSnapshot {
                is_repository: true,
                branch: None,
                status: vec![GitStatusEntry {
                    index_status: "R".to_owned(),
                    worktree_status: " ".to_owned(),
                    path: utf8_repository_path("src/new.rs"),
                    previous_path: Some(utf8_repository_path("src/old.rs")),
                }],
                recent_commits: Vec::new(),
                worktrees: Vec::new(),
                truncated: false,
                diagnostic: None,
            },
        };
        let frame = response_frame(NodeResponse::WorkspaceInspected { inspection });

        assert!(ensure_server_frame_path_capability(&frame, false, false).is_ok());
    }

}
