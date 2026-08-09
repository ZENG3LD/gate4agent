use gate4agent_node_protocol::{
    provider_id_is_legacy,
    production_node_client_compatibility_offer,
    read_json_frame_limited_body_timeout, validate_provider_contract_manifest,
    write_json_frame_limited, CapabilityId,
    ClientAuthentication, ClientCompatibilityOffer, ClientFrame, ClientHello, ClientRole,
    FrameError, NegotiatedNodeCompatibility, NodeEvent, NodeEventEnvelope, NodeFailure, NodeHello,
    NodeId, NodeRequest, NodeResponse, NodeSnapshot, RequestEnvelope, WorkspaceSnapshot,
    ServerChallenge, ServerFrame,
    MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    NODE_AUTH_NONCE_BYTES,
    NODE_COMPATIBILITY_METADATA_CAPABILITY, NODE_OPAQUE_UNIX_PATH_CAPABILITY,
    NODE_PROVIDER_ID_OPEN_CAPABILITY,
    NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, NODE_REPOSITORY_PATH_CAPABILITY,
    NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
    NODE_WORKSPACE_FILE_READ_CAPABILITY,
    NODE_PROTOCOL_VERSION,
};
use crate::{
    connect_local_stream, negotiated_auth_proof, proofs_match, random_nonce, AuthDirection,
};
#[cfg(test)]
use gate4agent_node_protocol::{
    NodeIncarnationId, ProtocolRange, StateSchemaSupport, NODE_INCARNATION_ID_BYTES,
    NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V3,
};
#[cfg(test)]
use crate::auth_proof;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;
#[cfg(feature = "fixture")]
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::timeout;

const AUTH_FRAME_TIMEOUT_MS: u64 = 5_000;
const FRAME_BODY_TIMEOUT_MS: u64 = 5_000;
const SERVER_FRAME_QUEUE_CAPACITY: usize = 1;
const PENDING_EVENTS_MAX: usize = 1_024;
const PENDING_EVENT_WIRE_BYTES_MAX: usize = 16 * 1024 * 1024;

trait NodeClientStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> NodeClientStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

struct CountingReader<R> {
    inner: R,
    bytes_read: usize,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, bytes_read: 0 }
    }

    fn take_bytes_read(&mut self) -> usize {
        let bytes_read = self.bytes_read;
        self.bytes_read = 0;
        bytes_read
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        match Pin::new(&mut self.inner).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                self.bytes_read = self
                    .bytes_read
                    .saturating_add(buffer.filled().len().saturating_sub(before));
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

struct ReceivedServerFrame {
    frame: ServerFrame,
    wire_bytes: usize,
}

struct PendingNodeEvent {
    envelope: NodeEventEnvelope,
    wire_bytes: usize,
}

struct AbortReaderOnDrop(AbortHandle);

impl Drop for AbortReaderOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_server_frame_reader(
    pipe: Box<dyn NodeClientStream>,
) -> (
    WriteHalf<Box<dyn NodeClientStream>>,
    mpsc::Receiver<Result<ReceivedServerFrame, FrameError>>,
    AbortReaderOnDrop,
) {
    let (reader, writer) = tokio::io::split(pipe);
    let mut reader = CountingReader::new(reader);
    let (frame_tx, frame_rx) = mpsc::channel(SERVER_FRAME_QUEUE_CAPACITY);
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = read_json_frame_limited_body_timeout(
                &mut reader,
                MAX_NODE_FRAME_BYTES,
                Duration::from_millis(FRAME_BODY_TIMEOUT_MS),
            )
            .await
            .map(|frame| ReceivedServerFrame {
                frame,
                wire_bytes: reader.take_bytes_read(),
            });
            if frame.is_err() {
                let _ = reader.take_bytes_read();
            }
            let terminal = frame.is_err();
            if frame_tx.send(frame).await.is_err() || terminal {
                break;
            }
        }
    });
    (
        writer,
        frame_rx,
        AbortReaderOnDrop(reader_task.abort_handle()),
    )
}

pub struct LocalNodeClient {
    writer: WriteHalf<Box<dyn NodeClientStream>>,
    frame_rx: mpsc::Receiver<Result<ReceivedServerFrame, FrameError>>,
    _reader_abort: AbortReaderOnDrop,
    hello: NodeHello,
    opaque_unix_paths_enabled: bool,
    repository_paths_enabled: bool,
    open_provider_ids_enabled: bool,
    terminal_frame_events_enabled: bool,
    negotiated_capabilities: Vec<CapabilityId>,
    next_request_id: u64,
    pending_events: VecDeque<PendingNodeEvent>,
    pending_event_wire_bytes: usize,
}

impl LocalNodeClient {
    pub async fn connect(
        endpoint: impl AsRef<Path>,
        expected_node_id: &NodeId,
        role: ClientRole,
        access_token: &str,
    ) -> Result<Self, NodeClientError> {
        let pipe = connect_local_stream(endpoint).await?;
        Self::connect_stream(Box::new(pipe), expected_node_id, role, access_token).await
    }

    pub async fn connect_loopback(
        endpoint: SocketAddr,
        expected_node_id: &NodeId,
        role: ClientRole,
        access_token: &str,
    ) -> Result<Self, NodeClientError> {
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(NodeClientError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "node TCP endpoint must be loopback with a nonzero port",
            )));
        }
        let stream = TcpStream::connect(endpoint).await?;
        Self::connect_stream(Box::new(stream), expected_node_id, role, access_token).await
    }

    async fn connect_stream(
        mut pipe: Box<dyn NodeClientStream>,
        expected_node_id: &NodeId,
        role: ClientRole,
        access_token: &str,
    ) -> Result<Self, NodeClientError> {
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
        let open_provider_ids_enabled = selected_supports_open_provider_ids(
            hello.compatibility.as_ref(),
        );
        let terminal_frame_events_enabled = selected_supports_terminal_frame_events(
            hello.compatibility.as_ref(),
        );
        ensure_node_hello_path_capability(&hello, opaque_unix_paths_enabled)?;
        ensure_node_hello_provider_capability(&hello, open_provider_ids_enabled)?;
        if &hello.snapshot.node_id != expected_node_id {
            return Err(NodeClientError::Protocol(format!(
                "node identity mismatch: expected '{}', received '{}'",
                expected_node_id,
                hello.snapshot.node_id,
            )));
        }
        let (writer, frame_rx, reader_abort) = start_server_frame_reader(pipe);
        Ok(Self {
            writer,
            frame_rx,
            _reader_abort: reader_abort,
            hello,
            opaque_unix_paths_enabled,
            repository_paths_enabled,
            open_provider_ids_enabled,
            terminal_frame_events_enabled,
            negotiated_capabilities,
            next_request_id: 1,
            pending_events: VecDeque::new(),
            pending_event_wire_bytes: 0,
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
            self.open_provider_ids_enabled,
            &self.negotiated_capabilities,
        )?;
        write_json_frame_limited(
            &mut self.writer,
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
        Ok(self.recv_received().await?.frame)
    }

    async fn recv_received(&mut self) -> Result<ReceivedServerFrame, NodeClientError> {
        let received = self.frame_rx.recv().await.ok_or_else(|| {
            NodeClientError::Protocol("node frame reader closed".to_owned())
        })??;
        let frame = &received.frame;
        ensure_server_frame_required_capability(frame, &self.negotiated_capabilities)?;
        ensure_server_frame_terminal_capability(
            frame,
            self.terminal_frame_events_enabled,
        )?;
        ensure_server_frame_path_capability(
            frame,
            self.opaque_unix_paths_enabled,
            self.repository_paths_enabled,
        )?;
        ensure_server_frame_provider_capability(frame, self.open_provider_ids_enabled)?;
        Ok(received)
    }

    pub async fn request(&mut self, request: NodeRequest) -> Result<NodeResponse, NodeClientError> {
        let request_id = self.send(request).await?;
        loop {
            let received = self.recv_received().await?;
            match received.frame {
                ServerFrame::Reply(reply) if reply.request_id == request_id => {
                    return reply.result.map_err(NodeClientError::Node);
                }
                ServerFrame::Reply(reply) => {
                    return Err(NodeClientError::Protocol(format!(
                        "unexpected response id {} while waiting for {request_id}",
                        reply.request_id,
                    )));
                }
                ServerFrame::Event(event) => queue_pending_event_bounded(
                    &mut self.pending_events,
                    &mut self.pending_event_wire_bytes,
                    event,
                    received.wire_bytes,
                    PENDING_EVENTS_MAX,
                    PENDING_EVENT_WIRE_BYTES_MAX,
                )?,
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
        let pending = self.pending_events.pop_front()?;
        self.pending_event_wire_bytes = self
            .pending_event_wire_bytes
            .checked_sub(pending.wire_bytes)
            .expect("pending node event wire byte accounting diverged");
        Some(pending.envelope)
    }

    #[cfg(feature = "fixture")]
    pub async fn send_malformed_json_frame_for_fixture(&mut self) -> Result<(), NodeClientError> {
        self.writer.write_u32_le(1).await?;
        self.writer.write_all(b"{").await?;
        self.writer.flush().await?;
        Ok(())
    }
}

fn queue_pending_event_bounded(
    pending: &mut VecDeque<PendingNodeEvent>,
    pending_wire_bytes: &mut usize,
    event: NodeEventEnvelope,
    wire_bytes: usize,
    max_events: usize,
    max_wire_bytes: usize,
) -> Result<(), NodeClientError> {
    let replacement = if let NodeEvent::TerminalFrame { address, .. } = &event.event {
        pending.iter().position(|current| {
            matches!(
                &current.envelope.event,
                NodeEvent::TerminalFrame {
                    address: current_address,
                    ..
                } if current_address == address
            )
        })
    } else {
        None
    };
    let replaced_wire_bytes = replacement
        .and_then(|index| pending.get(index))
        .map(|pending| pending.wire_bytes)
        .unwrap_or(0);
    let retained_wire_bytes = pending_wire_bytes
        .checked_sub(replaced_wire_bytes)
        .expect("pending node event wire byte accounting diverged");
    let next_wire_bytes = retained_wire_bytes
        .checked_add(wire_bytes)
        .ok_or_else(|| {
            NodeClientError::Protocol(
                "pending node event wire byte accounting overflowed".to_owned(),
            )
        })?;
    if next_wire_bytes > max_wire_bytes {
        return Err(NodeClientError::Protocol(
            "pending node events exceeded the bounded wire byte capacity".to_owned(),
        ));
    }
    if replacement.is_none() && pending.len() >= max_events {
        return Err(NodeClientError::Protocol(
            "pending node events exceeded the bounded event capacity".to_owned(),
        ));
    }
    if let Some(index) = replacement {
        pending.remove(index);
    }
    pending.push_back(PendingNodeEvent {
        envelope: event,
        wire_bytes,
    });
    *pending_wire_bytes = next_wire_bytes;
    Ok(())
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

fn selected_supports_open_provider_ids(
    selected: Option<&NegotiatedNodeCompatibility>,
) -> bool {
    selected.is_some_and(|compatibility| {
        compatibility.capabilities.iter().any(|capability| {
            capability.as_str() == NODE_PROVIDER_ID_OPEN_CAPABILITY
        })
    })
}

fn selected_supports_terminal_frame_events(
    selected: Option<&NegotiatedNodeCompatibility>,
) -> bool {
    selected.is_some_and(|compatibility| {
        compatibility.capabilities.iter().any(|capability| {
            capability.as_str() == NODE_TERMINAL_FRAME_EVENTS_CAPABILITY
        })
    })
}

fn reserve_request_id(
    next_request_id: &mut u64,
    request: &NodeRequest,
    opaque_unix_paths_enabled: bool,
    repository_paths_enabled: bool,
    open_provider_ids_enabled: bool,
    negotiated_capabilities: &[CapabilityId],
) -> Result<u64, NodeClientError> {
    ensure_node_request_required_capability(request, negotiated_capabilities)?;
    ensure_node_request_path_capability(
        request,
        opaque_unix_paths_enabled,
        repository_paths_enabled,
    )?;
    ensure_node_request_provider_capability(request, open_provider_ids_enabled)?;
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

fn ensure_server_frame_terminal_capability(
    frame: &ServerFrame,
    terminal_frame_events_enabled: bool,
) -> Result<(), NodeClientError> {
    let contains_terminal_frame_event = match frame {
        ServerFrame::Event(NodeEventEnvelope {
            event: NodeEvent::TerminalFrame { .. },
            ..
        }) => true,
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|response| {
            matches!(response, NodeResponse::Resync { events, .. }
                if events.iter().any(|event| {
                    matches!(&event.event, NodeEvent::TerminalFrame { .. })
                }))
        }),
        ServerFrame::Challenge(_) | ServerFrame::Hello(_) | ServerFrame::Event(_) => false,
    };
    if contains_terminal_frame_event && !terminal_frame_events_enabled {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_TERMINAL_FRAME_EVENTS_CAPABILITY.to_owned(),
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

fn ensure_node_hello_provider_capability(
    hello: &NodeHello,
    open_provider_ids_enabled: bool,
) -> Result<(), NodeClientError> {
    ensure_inbound_open_provider_capability(
        node_snapshot_contains_open_provider_id(&hello.snapshot),
        open_provider_ids_enabled,
    )
}

fn ensure_node_request_provider_capability(
    request: &NodeRequest,
    open_provider_ids_enabled: bool,
) -> Result<(), NodeClientError> {
    if let NodeRequest::Spawn { provider, .. } = request {
        ensure_outbound_provider_id_capability(provider, open_provider_ids_enabled)?;
    }
    Ok(())
}

fn ensure_outbound_provider_id_capability(
    provider: &gate4agent_node_protocol::AgentId,
    open_provider_ids_enabled: bool,
) -> Result<(), NodeClientError> {
    if !provider_id_is_legacy(provider) && !open_provider_ids_enabled {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_PROVIDER_ID_OPEN_CAPABILITY.to_owned(),
        ));
    }
    Ok(())
}

fn ensure_server_frame_provider_capability(
    frame: &ServerFrame,
    open_provider_ids_enabled: bool,
) -> Result<(), NodeClientError> {
    ensure_inbound_open_provider_capability(
        server_frame_contains_open_provider_id(frame),
        open_provider_ids_enabled,
    )
}

fn ensure_inbound_open_provider_capability(
    contains_open_provider_id: bool,
    open_provider_ids_enabled: bool,
) -> Result<(), NodeClientError> {
    if contains_open_provider_id && !open_provider_ids_enabled {
        return Err(NodeClientError::Protocol(
            "node sent an open provider ID without negotiating the capability".to_owned(),
        ));
    }
    Ok(())
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

fn server_frame_contains_open_provider_id(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Hello(hello) => node_snapshot_contains_open_provider_id(&hello.snapshot),
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(
            node_response_contains_open_provider_id,
        ),
        ServerFrame::Event(event) => node_event_contains_open_provider_id(&event.event),
        ServerFrame::Challenge(_) => false,
    }
}

fn node_response_contains_open_provider_id(response: &NodeResponse) -> bool {
    match response {
        NodeResponse::Snapshot { snapshot, .. } => {
            node_snapshot_contains_open_provider_id(snapshot)
        }
        NodeResponse::Resync { snapshot, events, .. } => {
            node_snapshot_contains_open_provider_id(snapshot)
                || events.iter().any(|event| {
                    node_event_contains_open_provider_id(&event.event)
                })
        }
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            !provider_id_is_legacy(&record.provider)
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::WorktreeCreated { workspace, .. } => {
            workspace_contains_open_provider_id(workspace)
        }
        NodeResponse::WorkspaceInspected { .. }
        | NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::WorkspaceUnregistered { .. }
        | NodeResponse::WorktreeRemoved { .. }
        | NodeResponse::Accepted
        | NodeResponse::ShuttingDown => false,
    }
}

fn node_snapshot_contains_open_provider_id(snapshot: &NodeSnapshot) -> bool {
    snapshot.enabled_providers.iter().any(|provider| {
        !provider_id_is_legacy(provider)
    }) || snapshot.provider_runtime_statuses.iter().any(|status| {
        !provider_id_is_legacy(status.provider())
    }) || snapshot.session_records.iter().any(|record| {
        !provider_id_is_legacy(&record.provider)
    }) || snapshot.workspaces.iter().any(workspace_contains_open_provider_id)
}

fn workspace_contains_open_provider_id(workspace: &WorkspaceSnapshot) -> bool {
    workspace.sessions.iter().any(|session| {
        !provider_id_is_legacy(&session.agent_id)
    })
}

fn node_event_contains_open_provider_id(event: &NodeEvent) -> bool {
    match event {
        NodeEvent::WorkspaceAdded { workspace } => {
            workspace_contains_open_provider_id(workspace)
        }
        NodeEvent::SessionRecordUpserted { record } => {
            !provider_id_is_legacy(&record.provider)
        }
        NodeEvent::Control { .. }
        | NodeEvent::TerminalFrame { .. }
        | NodeEvent::ControllerChanged { .. }
        | NodeEvent::WorkspaceRemoved { .. }
        | NodeEvent::SessionRecordRemoved { .. }
        | NodeEvent::ResyncRequired { .. } => false,
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
        | NodeEvent::TerminalFrame { .. }
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
    let open_provider_ids_selected = selected.capabilities.iter().any(|capability| {
        capability.as_str() == NODE_PROVIDER_ID_OPEN_CAPABILITY
    });
    if !open_provider_ids_selected
        && (selected.provider_contracts.iter().any(|contract| {
            !provider_id_is_legacy(&contract.provider)
        }) || selected.provider_adapter_contracts.iter().any(|contract| {
            !provider_id_is_legacy(&contract.provider)
        }))
    {
        return Err(NodeClientError::Protocol(
            "node published an open provider ID without negotiating the capability".to_owned(),
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
        AdapterContractRevision, AdapterFamily, AdapterId, AgentId, ArchitectureId,
        GitSnapshot, GitStatusEntry, GitWorktreeSnapshot, HostDescriptor, LocalTransportKind,
        ManagedSessionRecord, ManagedSessionState, NodeCompatibilitySupport, OpaqueHostPath,
        OperatingSystemId, PathEncoding, PathSemantics, PathStyle,
        ProviderAdapterContractSupport, ProviderContractRevision, ProviderContractSupport,
        ProviderRuntimeStatus, ProviderRuntimeStatuses, RepositoryPath, ResponseEnvelope,
        SessionAddress, SessionKey, SessionMode, SessionRecordId, WorkspaceEntry,
        WorkspaceEntryKind, WorkspaceFileContent, WorkspaceFileRead, WorkspaceId,
        WorkspaceInspection, WorkspaceSnapshot,
    };
    use gate4agent_types::{
        AgentInstanceId, CapabilitySnapshot, ForegroundSnapshot, HistorySnapshot,
        ProviderSnapshot, ResumeSnapshot, SessionGeneration, SessionSnapshot, SessionStatus,
        TerminalFrame, TerminalSize, TransportKind,
    };

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

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

    fn session_snapshot(provider: &str) -> SessionSnapshot {
        SessionSnapshot {
            instance_id: AgentInstanceId(7),
            agent_id: agent(provider),
            transport: TransportKind::Pty,
            generation: SessionGeneration(2),
            status: SessionStatus::Running,
            pending_operation: None,
            pending_input: None,
            process_id: None,
            terminal_size: None,
            terminal_frame: None,
            terminal_stale: None,
            session_options: None,
            capabilities: CapabilitySnapshot::default(),
            history: HistorySnapshot::default(),
            resume: ResumeSnapshot::default(),
            foreground: ForegroundSnapshot::default(),
            provider: ProviderSnapshot::default(),
        }
    }

    fn session_record_with_path(canonical_root: OpaqueHostPath) -> ManagedSessionRecord {
        ManagedSessionRecord {
            record_id: SessionRecordId::new("session-a").unwrap(),
            display_name: "session a".to_owned(),
            provider: agent("claude"),
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
            provider_runtime_statuses: ProviderRuntimeStatuses::default(),
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
    fn client_offer_accepts_open_provider_ids_and_durable_state_schema_v1_through_v3() {
        let offer = client_compatibility_offer().unwrap();
        assert_eq!(
            offer.protocol_versions,
            ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
        );
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
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_PROVIDER_ID_OPEN_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_TERMINAL_FRAME_EVENTS_CAPABILITY).unwrap(),
        ));
        assert_eq!(
            offer.state_schema.unwrap().versions,
            ProtocolRange::new(NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V3).unwrap(),
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
            provider: agent("codex"),
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
            provider: agent("claude"),
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
            provider: agent("codex"),
            revision: ProviderContractRevision::new("codex.2026-08").unwrap(),
        });
        selected.provider_adapter_contracts.push(ProviderAdapterContractSupport {
            provider: agent("codex"),
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
    fn open_provider_gate_requires_explicit_authenticated_selection() {
        let (_, mut selected) = negotiated_fixture();
        assert!(!selected_supports_open_provider_ids(None));
        assert!(!selected_supports_open_provider_ids(Some(&selected)));

        selected.capabilities.push(
            CapabilityId::new(NODE_PROVIDER_ID_OPEN_CAPABILITY).unwrap(),
        );
        assert!(selected_supports_open_provider_ids(Some(&selected)));
    }

    #[test]
    fn inbound_terminal_frame_events_require_authenticated_selection() {
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(2),
            },
        };
        let event = ServerFrame::Event(NodeEventEnvelope {
            sequence: 11,
            event: NodeEvent::TerminalFrame {
                address,
                frame: TerminalFrame {
                    sequence: 3,
                    size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    cursor_row: 4,
                    cursor_column: 5,
                    contents: "ready".to_owned(),
                    formatted: vec![1, 2, 3],
                    scrollback_formatted: Vec::new(),
                    alternate_screen: false,
                    mouse_protocol_enabled: false,
                    mouse_protocol_encoding: Default::default(),
                },
            },
        });

        assert!(matches!(
            ensure_server_frame_terminal_capability(&event, false),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_TERMINAL_FRAME_EVENTS_CAPABILITY
        ));
        assert!(ensure_server_frame_terminal_capability(&event, true).is_ok());

        let (_, mut selected) = negotiated_fixture();
        assert!(!selected_supports_terminal_frame_events(Some(&selected)));
        selected.capabilities.push(
            CapabilityId::new(NODE_TERMINAL_FRAME_EVENTS_CAPABILITY).unwrap(),
        );
        assert!(selected_supports_terminal_frame_events(Some(&selected)));
    }

    #[tokio::test]
    async fn cancelled_recv_preserves_the_next_complete_server_frame() {
        let (client_stream, mut server_stream) = tokio::io::duplex(4_096);
        let (writer, frame_rx, reader_abort) =
            start_server_frame_reader(Box::new(client_stream));
        let mut client = LocalNodeClient {
            writer,
            frame_rx,
            _reader_abort: reader_abort,
            hello: hello_with_snapshot(empty_snapshot()),
            opaque_unix_paths_enabled: false,
            repository_paths_enabled: false,
            open_provider_ids_enabled: false,
            terminal_frame_events_enabled: false,
            negotiated_capabilities: Vec::new(),
            next_request_id: 1,
            pending_events: VecDeque::new(),
            pending_event_wire_bytes: 0,
        };

        tokio::select! {
            _ = async { tokio::task::yield_now().await } => {}
            result = client.recv() => panic!("empty receive completed unexpectedly: {result:?}"),
        }

        let expected = ServerFrame::Event(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::ControllerChanged { controller: None },
        });
        write_json_frame_limited(
            &mut server_stream,
            &expected,
            MAX_NODE_FRAME_BYTES,
        )
        .await
        .unwrap();
        assert_eq!(client.recv().await.unwrap(), expected);
    }

    #[test]
    fn pending_events_coalesce_exact_terminal_address_and_enforce_wire_byte_bound() {
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(2),
            },
        };
        let terminal_event = |event_sequence, address: SessionAddress, frame_sequence| {
            NodeEventEnvelope {
                sequence: event_sequence,
                event: NodeEvent::TerminalFrame {
                    address,
                    frame: TerminalFrame {
                        sequence: frame_sequence,
                        size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        cursor_row: 0,
                        cursor_column: 0,
                        contents: format!("frame-{frame_sequence}"),
                        formatted: vec![frame_sequence as u8],
                        scrollback_formatted: Vec::new(),
                        alternate_screen: false,
                        mouse_protocol_enabled: false,
                        mouse_protocol_encoding: Default::default(),
                    },
                },
            }
        };
        let mut pending = VecDeque::new();
        let mut wire_bytes = 0;
        queue_pending_event_bounded(
            &mut pending,
            &mut wire_bytes,
            terminal_event(1, address.clone(), 1),
            4,
            4,
            9,
        )
        .unwrap();
        queue_pending_event_bounded(
            &mut pending,
            &mut wire_bytes,
            NodeEventEnvelope {
                sequence: 2,
                event: NodeEvent::ControllerChanged { controller: None },
            },
            1,
            4,
            9,
        )
        .unwrap();
        queue_pending_event_bounded(
            &mut pending,
            &mut wire_bytes,
            terminal_event(3, address.clone(), 2),
            5,
            4,
            9,
        )
        .unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(wire_bytes, 6);
        assert_eq!(pending[0].envelope.sequence, 2);
        assert_eq!(pending[1].envelope.sequence, 3);

        let mut next_generation = address.clone();
        next_generation.session.generation = SessionGeneration(3);
        queue_pending_event_bounded(
            &mut pending,
            &mut wire_bytes,
            terminal_event(4, next_generation, 1),
            3,
            4,
            9,
        )
        .unwrap();
        assert_eq!(pending.len(), 3);
        assert_eq!(wire_bytes, 9);
        let mut third_address = address.clone();
        third_address.session.instance_id = AgentInstanceId(8);
        assert!(matches!(
            queue_pending_event_bounded(
                &mut pending,
                &mut wire_bytes,
                terminal_event(5, third_address, 1),
                1,
                4,
                9,
            ),
            Err(NodeClientError::Protocol(message))
                if message.contains("wire byte capacity")
        ));
        assert_eq!(pending.len(), 3);
        assert_eq!(wire_bytes, 9);

        let mut regular = VecDeque::new();
        let mut regular_bytes = 0;
        for sequence in 1..=2 {
            queue_pending_event_bounded(
                &mut regular,
                &mut regular_bytes,
                NodeEventEnvelope {
                    sequence,
                    event: NodeEvent::ControllerChanged { controller: None },
                },
                1,
                2,
                8,
            )
            .unwrap();
        }
        assert!(matches!(
            queue_pending_event_bounded(
                &mut regular,
                &mut regular_bytes,
                NodeEventEnvelope {
                    sequence: 3,
                    event: NodeEvent::ControllerChanged { controller: None },
                },
                1,
                2,
                8,
            ),
            Err(NodeClientError::Protocol(message))
                if message.contains("event capacity")
        ));
        assert_eq!(
            regular
                .iter()
                .map(|pending| pending.envelope.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2],
        );
    }

    #[test]
    fn outbound_open_provider_gate_preserves_all_legacy_ids() {
        for provider in [agent("claude"), agent("codex"), agent("kimi")] {
            assert!(ensure_outbound_provider_id_capability(&provider, false).is_ok());
        }
        let error = ensure_outbound_provider_id_capability(&agent("qwen"), false)
            .unwrap_err();
        assert!(matches!(
            error,
            NodeClientError::UnsupportedCapability(capability)
                if capability == NODE_PROVIDER_ID_OPEN_CAPABILITY
        ));
        assert!(ensure_outbound_provider_id_capability(&agent("qwen"), true).is_ok());
    }

    #[test]
    fn inbound_open_provider_payloads_require_the_authenticated_capability() {
        let mut snapshot = empty_snapshot();
        snapshot.enabled_providers.push(agent("qwen"));
        assert!(ensure_node_hello_provider_capability(
            &hello_with_snapshot(snapshot.clone()),
            false,
        )
        .is_err());
        assert!(ensure_node_hello_provider_capability(
            &hello_with_snapshot(snapshot),
            true,
        )
        .is_ok());

        let mut runtime_snapshot = empty_snapshot();
        runtime_snapshot.provider_runtime_statuses = ProviderRuntimeStatuses::new([
            ProviderRuntimeStatus::unavailable(agent("grok")),
        ])
        .unwrap();
        assert!(ensure_node_hello_provider_capability(
            &hello_with_snapshot(runtime_snapshot),
            false,
        )
        .is_err());

        let mut record = session_record_with_path(utf8_path());
        record.provider = agent("qwen");
        let reply = response_frame(NodeResponse::SessionRecordUpdated {
            record: record.clone(),
        });
        assert!(ensure_server_frame_provider_capability(&reply, false).is_err());
        let event = ServerFrame::Event(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::SessionRecordUpserted { record },
        });
        assert!(ensure_server_frame_provider_capability(&event, false).is_err());
        assert!(ensure_server_frame_provider_capability(&event, true).is_ok());

        let mut open_workspace = workspace_with_path(utf8_path());
        open_workspace.sessions.push(session_snapshot("qwen-code"));
        let mut nested_snapshot = empty_snapshot();
        nested_snapshot.workspaces.push(open_workspace.clone());
        let nested_frames = [
            ServerFrame::Hello(hello_with_snapshot(nested_snapshot)),
            ServerFrame::Event(NodeEventEnvelope {
                sequence: 2,
                event: NodeEvent::WorkspaceAdded {
                    workspace: open_workspace.clone(),
                },
            }),
            response_frame(NodeResponse::WorkspaceRegistered {
                workspace: open_workspace.clone(),
            }),
            response_frame(NodeResponse::WorktreeCreated {
                worktree: worktree_with_path(utf8_path()),
                workspace: open_workspace,
            }),
        ];
        for frame in nested_frames {
            assert!(ensure_server_frame_provider_capability(&frame, false).is_err());
            assert!(ensure_server_frame_provider_capability(&frame, true).is_ok());
        }

        let mut legacy_workspace = workspace_with_path(utf8_path());
        legacy_workspace.sessions.push(session_snapshot("claude"));
        let legacy_frame = response_frame(NodeResponse::WorkspaceRegistered {
            workspace: legacy_workspace,
        });
        assert!(ensure_server_frame_provider_capability(&legacy_frame, false).is_ok());
    }

    #[test]
    fn selected_open_provider_manifest_requires_the_open_id_capability() {
        let (mut offer, mut selected) = negotiated_fixture();
        let manifest_capability =
            CapabilityId::new(NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY).unwrap();
        offer.capabilities.push(manifest_capability.clone());
        selected.capabilities.push(manifest_capability);
        selected.provider_contracts.push(ProviderContractSupport {
            provider: agent("qwen"),
            revision: ProviderContractRevision::new("qwen.2026-08").unwrap(),
        });
        assert!(matches!(
            validate_selected_compatibility(&offer, &selected),
            Err(NodeClientError::Protocol(message))
                if message.contains("open provider ID")
        ));

        let open_capability = CapabilityId::new(NODE_PROVIDER_ID_OPEN_CAPABILITY).unwrap();
        offer.capabilities.push(open_capability.clone());
        selected.capabilities.push(open_capability);
        assert!(validate_selected_compatibility(&offer, &selected).is_ok());
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
