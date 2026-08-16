use gate4agent_node_protocol::{
    provider_id_is_legacy,
    production_node_client_compatibility_offer,
    read_json_frame_limited_body_timeout, validate_provider_contract_manifest,
    write_json_frame_limited, CapabilityId,
    ClientAuthentication, ClientCompatibilityOffer, ClientFrame, ClientHello, ClientRole,
    FrameError, HarnessMcpLocalReplyV1, HarnessMcpLocalRequestV1, HarnessMcpLocalToken,
    HarnessReadHostErrorV1, HarnessReadRequestV1, HarnessReadResponseV1,
    NegotiatedNodeCompatibility, NodeEvent, NodeEventEnvelope, NodeFailure,
    NodeFailureCode, NodeHello, NodeId, NodeRequest, NodeResponse, NodeSnapshot, RequestEnvelope,
    WorkspaceSnapshot,
    ServerChallenge, ServerFrame,
    MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES, MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES,
    MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    NODE_AUTH_NONCE_BYTES,
    CAPABILITY_HOST_DIRECTORY_BROWSE_V1,
    NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
    NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY,
    NODE_OBSERVATION_EVENTS_CAPABILITY,
    NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
    NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
    NODE_HARNESS_MCP_READ_PROXY_CAPABILITY,
    NODE_NATIVE_SESSION_CATALOG_CAPABILITY, NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY,
    NODE_NATIVE_SESSION_INDEX_CAPABILITY, NODE_NATIVE_SESSION_PREVIEW_CAPABILITY,
    NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY,
    NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
    NODE_COMPATIBILITY_METADATA_CAPABILITY, NODE_OPAQUE_UNIX_PATH_CAPABILITY,
    NODE_PROVIDER_ID_OPEN_CAPABILITY,
    NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY,
    NODE_SESSION_TASK_CORRELATION_CAPABILITY,
    NODE_PROVIDER_CONTRACT_MANIFEST_CAPABILITY, NODE_REPOSITORY_PATH_CAPABILITY,
    NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
    NODE_SPAWN_PROFILE_REVISION_CAPABILITY,
    NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
    NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
    NODE_GIT_READ_CAPABILITY, NODE_WORKSPACE_FILE_READ_CAPABILITY,
    NODE_WORKSPACE_FILE_WRITE_CAPABILITY,
    NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY,
    NODE_WORKTREE_SELECTION_CAPABILITY,
    NODE_PROTOCOL_VERSION,
};
use crate::{
    connect_local_stream, negotiated_auth_proof, proofs_match, random_nonce, AuthDirection,
};
#[cfg(test)]
use gate4agent_node_protocol::{
    NodeIncarnationId, ProtocolRange, StateSchemaSupport, NODE_INCARNATION_ID_BYTES,
    NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V8,
};
#[cfg(test)]
use crate::auth_proof;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
const LOCAL_SESSION_HARNESS_MCP_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct LocalSessionHarnessMcpClient {
    endpoint: PathBuf,
    token: HarnessMcpLocalToken,
}

impl LocalSessionHarnessMcpClient {
    pub fn new(
        endpoint: impl AsRef<Path>,
        token: HarnessMcpLocalToken,
    ) -> Result<Self, LocalSessionHarnessMcpError> {
        let endpoint = endpoint.as_ref();
        if endpoint.as_os_str().is_empty() {
            return Err(LocalSessionHarnessMcpError::Unavailable);
        }
        Ok(Self { endpoint: endpoint.to_path_buf(), token })
    }

    pub fn send(
        &self,
        request: HarnessReadRequestV1,
    ) -> Result<HarnessReadResponseV1, LocalSessionHarnessMcpError> {
        request.validate().map_err(|_| LocalSessionHarnessMcpError::InvalidRequest)?;
        let envelope = HarnessMcpLocalRequestV1 {
            version: 1,
            token: self.token.clone(),
            request,
        };
        envelope.validate().map_err(|_| LocalSessionHarnessMcpError::InvalidRequest)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|_| LocalSessionHarnessMcpError::Unavailable)?;
        runtime.block_on(async {
            timeout(LOCAL_SESSION_HARNESS_MCP_DEADLINE, async {
                let mut stream = connect_local_stream(&self.endpoint)
                    .await
                    .map_err(map_harness_mcp_io_error)?;
                write_json_frame_limited(
                    &mut stream,
                    &envelope,
                    MAX_HARNESS_MCP_LOCAL_REQUEST_BYTES,
                ).await.map_err(map_harness_mcp_frame_error)?;
                use tokio::io::AsyncWriteExt as _;
                stream.shutdown().await.map_err(map_harness_mcp_io_error)?;
                let reply: HarnessMcpLocalReplyV1 = read_json_frame_limited_body_timeout(
                    &mut stream,
                    MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES,
                    LOCAL_SESSION_HARNESS_MCP_DEADLINE,
                ).await.map_err(map_harness_mcp_frame_error)?;
                reply.validate().map_err(|_| LocalSessionHarnessMcpError::InvalidResponse)?;
                match reply {
                    HarnessMcpLocalReplyV1::Ok { response } => Ok(response),
                    HarnessMcpLocalReplyV1::Error {
                        error: HarnessReadHostErrorV1::Unauthorized,
                    } => Err(LocalSessionHarnessMcpError::Unauthorized),
                    HarnessMcpLocalReplyV1::Error { error } => {
                        Err(LocalSessionHarnessMcpError::Host(error))
                    }
                }
            }).await.map_err(|_| LocalSessionHarnessMcpError::Deadline)?
        })
    }
}

fn map_harness_mcp_io_error(error: io::Error) -> LocalSessionHarnessMcpError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => LocalSessionHarnessMcpError::Unauthorized,
        io::ErrorKind::TimedOut => LocalSessionHarnessMcpError::Deadline,
        _ => LocalSessionHarnessMcpError::Unavailable,
    }
}

fn map_harness_mcp_frame_error(error: FrameError) -> LocalSessionHarnessMcpError {
    match error {
        FrameError::PrefixTimedOut | FrameError::BodyTimedOut { .. } => {
            LocalSessionHarnessMcpError::Deadline
        }
        _ => LocalSessionHarnessMcpError::InvalidResponse,
    }
}

#[derive(Debug, Error)]
pub enum LocalSessionHarnessMcpError {
    #[error("local session harness MCP request was unauthorized")]
    Unauthorized,
    #[error("local session harness MCP endpoint is unavailable")]
    Unavailable,
    #[error("local session harness MCP request is invalid")]
    InvalidRequest,
    #[error("local session harness MCP response is invalid")]
    InvalidResponse,
    #[error("local session harness MCP deadline exceeded")]
    Deadline,
    #[error("local session harness MCP host rejected the request: {0:?}")]
    Host(HarnessReadHostErrorV1),
}

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
        ensure_node_hello_environment_profile_capability(
            &hello,
            &negotiated_capabilities,
        )?;
        ensure_node_hello_bundle_materialization_capability(
            &hello,
            &negotiated_capabilities,
        )?;
        ensure_node_hello_history_context_pack_capability(
            &hello,
            &negotiated_capabilities,
        )?;
        ensure_node_snapshot_agent_progress_capability(
            &hello.snapshot,
            &negotiated_capabilities,
        )?;
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
        self.recv_received_for_request(None).await
    }

    async fn recv_received_for_request(
        &mut self,
        expected_request: Option<&NodeRequest>,
    ) -> Result<ReceivedServerFrame, NodeClientError> {
        let received = self.frame_rx.recv().await.ok_or_else(|| {
            NodeClientError::Protocol("node frame reader closed".to_owned())
        })??;
        let frame = &received.frame;
        ensure_server_frame_required_capability_for_request(
            frame,
            &self.negotiated_capabilities,
            expected_request,
        )?;
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
        let expected_request = request.clone();
        let request_id = self.send(request).await?;
        loop {
            let received = self
                .recv_received_for_request(Some(&expected_request))
                .await?;
            match received.frame {
                ServerFrame::Reply(reply) if reply.request_id == request_id => {
                    validate_provider_session_index_response(&expected_request, &reply.result)?;
                    validate_native_session_response(&expected_request, &reply.result)?;
                    validate_workspace_content_response(&expected_request, &reply.result)?;
                    validate_session_task_response(&expected_request, &reply.result)?;
                    validate_delivery_response(&expected_request, &reply.result)?;
                    validate_harness_mcp_response(&expected_request, &reply.result)?;
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
    let now_unix_ms = current_unix_ms()?;
    if !request.harness_mcp_contract_is_valid_at(now_unix_ms) {
        return Err(NodeClientError::Protocol(
            "invalid harness MCP proxy request".to_owned(),
        ));
    }
    if !request.history_context_pack_contract_is_valid() {
        return Err(NodeClientError::Protocol(
            "invalid history context pack request".to_owned(),
        ));
    }
    if !request.native_session_catalog_contract_is_valid() {
        return Err(NodeClientError::Protocol(
            "invalid native session catalog request".to_owned(),
        ));
    }
    if !request.native_session_preview_contract_is_valid() {
        return Err(NodeClientError::Protocol(
            "invalid native session preview request".to_owned(),
        ));
    }
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

fn current_unix_ms() -> Result<u64, NodeClientError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NodeClientError::Protocol("system clock precedes Unix epoch".to_owned()))?
        .as_millis()
        .try_into()
        .map_err(|_| NodeClientError::Protocol("system clock exceeds protocol range".to_owned()))
}

fn validate_provider_session_index_response(
    expected: &NodeRequest,
    response: &Result<NodeResponse, NodeFailure>,
) -> Result<(), NodeClientError> {
    let matches = match (expected, response) {
        (
            NodeRequest::IndexProviderSession {
                workspace_id,
                provider,
                identity,
                ..
            },
            Ok(NodeResponse::ProviderSessionIndexed { record }),
        ) => {
            &record.workspace_id == workspace_id
                && &record.provider == provider
                && record.provider_session.as_ref() == Some(identity)
        }
        (NodeRequest::IndexProviderSession { .. }, Err(_)) => true,
        (NodeRequest::IndexProviderSession { .. }, Ok(_)) => false,
        (_, Ok(NodeResponse::ProviderSessionIndexed { .. })) => false,
        _ => true,
    };
    if matches {
        Ok(())
    } else {
        Err(NodeClientError::Protocol(
            "node provider session index response does not match the request".to_owned(),
        ))
    }
}

fn validate_native_session_response(
    expected: &NodeRequest,
    response: &Result<NodeResponse, NodeFailure>,
) -> Result<(), NodeClientError> {
    let matches = match (expected, response) {
        (
            NodeRequest::CatalogNativeSessions { route, .. },
            Ok(response @ NodeResponse::NativeSessionsCataloged {
                route: echoed_route,
                ..
            }),
        ) => echoed_route == route && response.native_session_catalog_contract_is_valid(),
        (
            NodeRequest::PageNativeSessions {
                route,
                window,
                catalog_revision,
                ..
            },
            Ok(response @ NodeResponse::NativeSessionsPaged {
                route: echoed_route,
                page,
            }),
        ) => {
            echoed_route == route
                && page.window == *window
                && page.revision == *catalog_revision
                && response.native_session_catalog_contract_is_valid()
        }
        (
            NodeRequest::PreviewNativeSession { selection, .. },
            Ok(response @ NodeResponse::NativeSessionPreviewed {
                selection: echoed_selection,
                ..
            }),
        ) => {
            echoed_selection == selection
                && response.native_session_preview_contract_is_valid()
        }
        (
            NodeRequest::IndexNativeSession { selection, .. },
            Ok(response @ NodeResponse::NativeSessionIndexed {
                selection: echoed_selection,
                record,
            }),
        ) => {
            echoed_selection == selection
                && selection.route.scope
                == gate4agent_node_protocol::NativeSessionCatalogScope::Workspace
                && selection.route.workspace_id.as_ref() == Some(&record.workspace_id)
                && selection.route.provider == record.provider
                && response.native_session_index_contract_is_valid()
        }
        (
            NodeRequest::CatalogNativeSessions { .. }
            | NodeRequest::PageNativeSessions { .. }
            | NodeRequest::PreviewNativeSession { .. }
            | NodeRequest::IndexNativeSession { .. },
            Err(_),
        ) => true,
        (
            NodeRequest::CatalogNativeSessions { .. }
            | NodeRequest::PageNativeSessions { .. }
            | NodeRequest::PreviewNativeSession { .. }
            | NodeRequest::IndexNativeSession { .. },
            Ok(_),
        ) => false,
        (
            _,
            Ok(
                NodeResponse::NativeSessionsCataloged { .. }
                | NodeResponse::NativeSessionsPaged { .. }
                | NodeResponse::NativeSessionPreviewed { .. }
                | NodeResponse::NativeSessionIndexed { .. },
            ),
        ) => false,
        _ => true,
    };
    if matches {
        Ok(())
    } else {
        Err(NodeClientError::Protocol(
            "node native session response does not match the request".to_owned(),
        ))
    }
}

fn validate_workspace_content_response(
    expected: &NodeRequest,
    response: &Result<NodeResponse, NodeFailure>,
) -> Result<(), NodeClientError> {
    let matches = match (expected, response) {
        (
            NodeRequest::ReadWorkspaceFile { workspace_id, path },
            Ok(NodeResponse::WorkspaceFileRead { file }),
        ) => &file.workspace_id == workspace_id && &file.path == path,
        (
            NodeRequest::WriteWorkspaceFile { workspace_id, path, text, .. },
            Ok(NodeResponse::WorkspaceFileWritten { file }),
        ) => {
            &file.workspace_id == workspace_id
                && &file.path == path
                && matches!(
                    &file.content,
                    gate4agent_node_protocol::WorkspaceFileContent::Utf8 {
                        text: written,
                        byte_len,
                    } if written == text
                        && u32::try_from(text.len()).ok() == Some(*byte_len)
                )
        }
        (
            NodeRequest::CreateWorkspaceFile { workspace_id, path },
            Ok(NodeResponse::WorkspaceFileCreated { file }),
        ) => {
            &file.workspace_id == workspace_id
                && &file.path == path
                && file.revision.is_some()
                && matches!(
                    &file.content,
                    gate4agent_node_protocol::WorkspaceFileContent::Utf8 {
                        text,
                        byte_len: 0,
                    } if text.is_empty()
                )
        }
        (
            NodeRequest::CreateWorkspaceDirectory { workspace_id, path },
            Ok(NodeResponse::WorkspaceDirectoryCreated {
                workspace_id: actual_workspace_id,
                entry,
            }),
        ) => {
            actual_workspace_id == workspace_id
                && &entry.relative_path == path
                && entry.kind == gate4agent_node_protocol::WorkspaceEntryKind::Directory
        }
        (
            NodeRequest::ReadGitHistory { workspace_id, .. },
            Ok(NodeResponse::GitHistoryRead { workspace_id: actual, .. }),
        ) => actual == workspace_id,
        (
            NodeRequest::ReadGitDiff { workspace_id, request },
            Ok(NodeResponse::GitDiffRead { workspace_id: actual, diff }),
        ) => actual == workspace_id && diff.mode == request.mode && diff.path == request.path,
        (
            NodeRequest::ReadWorkspaceFile { .. }
            | NodeRequest::WriteWorkspaceFile { .. }
            | NodeRequest::CreateWorkspaceFile { .. }
            | NodeRequest::CreateWorkspaceDirectory { .. }
            | NodeRequest::ReadGitHistory { .. }
            | NodeRequest::ReadGitDiff { .. },
            Err(_),
        ) => true,
        (
            NodeRequest::ReadWorkspaceFile { .. }
            | NodeRequest::WriteWorkspaceFile { .. }
            | NodeRequest::CreateWorkspaceFile { .. }
            | NodeRequest::CreateWorkspaceDirectory { .. }
            | NodeRequest::ReadGitHistory { .. }
            | NodeRequest::ReadGitDiff { .. },
            Ok(_),
        ) => false,
        (
            _,
            Ok(
                NodeResponse::WorkspaceFileRead { .. }
                | NodeResponse::WorkspaceFileWritten { .. }
                | NodeResponse::WorkspaceFileCreated { .. }
                | NodeResponse::WorkspaceDirectoryCreated { .. }
                | NodeResponse::GitHistoryRead { .. }
                | NodeResponse::GitDiffRead { .. },
            ),
        ) => false,
        _ => true,
    };
    if matches {
        Ok(())
    } else {
        Err(NodeClientError::Protocol(
            "node workspace content response does not match the request".to_owned(),
        ))
    }
}

fn validate_session_task_response(
    expected: &NodeRequest,
    response: &Result<NodeResponse, NodeFailure>,
) -> Result<(), NodeClientError> {
    let matches = match (expected, response) {
        (
            NodeRequest::SetSessionTask {
                record_id,
                expected_revision,
                target,
            },
            Ok(NodeResponse::SessionRecordUpdated { record }),
        ) => session_task_record_matches(record, record_id, *expected_revision, target),
        (NodeRequest::SetSessionTask { .. }, Err(_)) => true,
        (NodeRequest::SetSessionTask { .. }, Ok(_)) => false,
        _ => true,
    };
    if matches {
        Ok(())
    } else {
        Err(NodeClientError::Protocol(
            "node session task response does not match the request".to_owned(),
        ))
    }
}

fn validate_delivery_response(
    expected: &NodeRequest,
    response: &Result<NodeResponse, NodeFailure>,
) -> Result<(), NodeClientError> {
    let matches = match (expected, response) {
        (
            NodeRequest::BeginDeliveryStage { manifest },
            Ok(NodeResponse::DeliveryStageBegun {
                manifest_digest, ..
            }),
        ) => manifest_digest == &manifest.manifest_digest,
        (
            NodeRequest::PutDeliveryBlobChunk {
                stage_id,
                blob_digest,
                offset,
                chunk_hex,
            },
            Ok(NodeResponse::DeliveryBlobChunkAccepted {
                stage_id: actual_stage_id,
                blob_digest: actual_blob_digest,
                next_offset,
            }),
        ) => {
            actual_stage_id == stage_id
                && actual_blob_digest == blob_digest
                && offset
                    .checked_add(chunk_hex.raw_len() as u64)
                    .is_some_and(|expected_offset| expected_offset == *next_offset)
        }
        (
            NodeRequest::CommitDeliveryStage { .. },
            Ok(NodeResponse::DeliveryCommitted { .. }),
        ) => true,
        (
            NodeRequest::AbortDeliveryStage { stage_id },
            Ok(NodeResponse::DeliveryStageAborted {
                stage_id: actual_stage_id,
            }),
        ) => actual_stage_id == stage_id,
        (
            NodeRequest::BeginDeliveryStage { .. }
            | NodeRequest::PutDeliveryBlobChunk { .. }
            | NodeRequest::CommitDeliveryStage { .. }
            | NodeRequest::AbortDeliveryStage { .. },
            Err(_),
        ) => true,
        (
            NodeRequest::BeginDeliveryStage { .. }
            | NodeRequest::PutDeliveryBlobChunk { .. }
            | NodeRequest::CommitDeliveryStage { .. }
            | NodeRequest::AbortDeliveryStage { .. },
            Ok(_),
        ) => false,
        (
            _,
            Ok(
                NodeResponse::DeliveryStageBegun { .. }
                | NodeResponse::DeliveryBlobChunkAccepted { .. }
                | NodeResponse::DeliveryCommitted { .. }
                | NodeResponse::DeliveryStageAborted { .. },
            ),
        ) => false,
        _ => true,
    };
    if matches {
        Ok(())
    } else {
        Err(NodeClientError::Protocol(
            "node delivery response does not match the request".to_owned(),
        ))
    }
}

fn validate_harness_mcp_response(
    expected: &NodeRequest,
    response: &Result<NodeResponse, NodeFailure>,
) -> Result<(), NodeClientError> {
    use NodeRequest as Request;
    use NodeResponse as Response;
    let matches = match (expected, response) {
        (Request::ArmHarnessMcpReservation { reservation_id, activation_digest, expires_at_unix_ms, .. },
            Ok(Response::Armed { reservation_id: echoed_id, activation_digest: echoed_digest, expires_at_unix_ms: echoed_expiry })) =>
            reservation_id == echoed_id && activation_digest == echoed_digest
                && expires_at_unix_ms == echoed_expiry,
        (Request::SpawnSpecWithHarnessMcp { reservation_id, activation_digest, .. },
            Ok(Response::Spawned { reservation_id: echoed_id, activation_digest: echoed_digest, receipt })) =>
            reservation_id == echoed_id && activation_digest == echoed_digest
                && receipt.harness_mcp_proxy.as_ref().is_some_and(|proxy| {
                    &proxy.reservation_id == reservation_id
                        && &proxy.activation_digest == activation_digest
                }),
        (Request::ActivateHarnessMcpReservation { reservation_id, activation_digest, record_id, session },
            Ok(Response::Activated { reservation_id: echoed_id, activation_digest: echoed_digest, record_id: echoed_record, session: echoed_session })) =>
            reservation_id == echoed_id && activation_digest == echoed_digest
                && record_id == echoed_record && session == echoed_session,
        (Request::AbortHarnessMcpReservation { reservation_id, activation_digest },
            Ok(Response::Aborted { reservation_id: echoed_id, activation_digest: echoed_digest })) =>
            reservation_id == echoed_id && activation_digest == echoed_digest,
        (Request::PutHarnessMcpReplyChunk { reservation_id, activation_digest, record_id, session, call_id, offset, final_chunk, chunk_hex },
            Ok(Response::ReplyChunkAccepted { reservation_id: echoed_id, activation_digest: echoed_digest, record_id: echoed_record, session: echoed_session, call_id: echoed_call, next_offset, completed })) =>
            reservation_id == echoed_id && activation_digest == echoed_digest
                && record_id == echoed_record && session == echoed_session && call_id == echoed_call
                && offset.checked_add(u32::try_from(chunk_hex.raw_len()).unwrap_or(u32::MAX))
                    == Some(*next_offset)
                && completed == final_chunk,
        (Request::RejectHarnessMcpCall { reservation_id, activation_digest, record_id, session, call_id, .. },
            Ok(Response::CallRejected { reservation_id: echoed_id, activation_digest: echoed_digest, record_id: echoed_record, session: echoed_session, call_id: echoed_call })) =>
            reservation_id == echoed_id && activation_digest == echoed_digest
                && record_id == echoed_record && session == echoed_session && call_id == echoed_call,
        (request, Err(_)) if request.required_capability()
            == Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY) => true,
        (request, Ok(_)) if request.required_capability()
            == Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY) => false,
        (_, Ok(response)) if response.requires_harness_mcp_proxy_capability() => false,
        _ => true,
    };
    if matches { Ok(()) } else {
        Err(NodeClientError::Protocol(
            "node harness MCP proxy response does not match the request".to_owned(),
        ))
    }
}

fn session_task_record_matches(
    record: &gate4agent_node_protocol::ManagedSessionRecord,
    record_id: &gate4agent_node_protocol::SessionRecordId,
    expected_revision: u64,
    target: &gate4agent_node_protocol::SessionTaskTargetV1,
) -> bool {
    if &record.record_id != record_id { return false; }
    let next_revision = expected_revision.checked_add(1);
    match target {
        gate4agent_node_protocol::SessionTaskTargetV1::New => record
            .task_binding
            .as_ref()
            .is_some_and(|binding| {
                Some(binding.revision) == next_revision && binding.task_id.is_some()
            }),
        gate4agent_node_protocol::SessionTaskTargetV1::Existing { task_id } => record
            .task_binding
            .as_ref()
            .is_some_and(|binding| {
                (binding.revision == expected_revision
                    || Some(binding.revision) == next_revision)
                    && binding.task_id.as_ref() == Some(task_id)
            }),
        gate4agent_node_protocol::SessionTaskTargetV1::Clear => match &record.task_binding {
            None => expected_revision == 0,
            Some(binding) => binding.task_id.is_none()
                && (binding.revision == expected_revision
                    || Some(binding.revision) == next_revision),
        },
    }
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
    if request.requires_spawn_spec_defaults_overrides_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY.to_owned(),
        ));
    }
    if request.requires_spawn_profile_revision_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_SPAWN_PROFILE_REVISION_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_SPAWN_PROFILE_REVISION_CAPABILITY.to_owned(),
        ));
    }
    if request.requires_worktree_selection_capability()
        && !negotiated_capabilities.iter().any(|capability| {
            capability.as_str() == NODE_WORKTREE_SELECTION_CAPABILITY
        })
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_WORKTREE_SELECTION_CAPABILITY.to_owned(),
        ));
    }
    if request.requires_child_environment_profile_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY.to_owned(),
        ));
    }
    if request.requires_session_bundle_materialization_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY.to_owned(),
        ));
    }
    if request.requires_history_context_pack_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_HISTORY_CONTEXT_PACK_CAPABILITY.to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn ensure_server_frame_required_capability(
    frame: &ServerFrame,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    ensure_server_frame_required_capability_for_request(
        frame,
        negotiated_capabilities,
        None,
    )
}

fn ensure_server_frame_required_capability_for_request(
    frame: &ServerFrame,
    negotiated_capabilities: &[CapabilityId],
    expected_request: Option<&NodeRequest>,
) -> Result<(), NodeClientError> {
    if matches!(frame, ServerFrame::Event(event)
        if !event.event.harness_mcp_contract_is_valid_at(current_unix_ms()?))
    {
        return Err(NodeClientError::Protocol(
            "node sent an invalid harness MCP read call".to_owned(),
        ));
    }
    if matches!(frame, ServerFrame::Reply(reply)
        if reply.result.as_ref().is_ok_and(|response| {
            !response.native_session_catalog_contract_is_valid()
        }))
    {
        return Err(NodeClientError::Protocol(
            "node sent an invalid native session catalog response".to_owned(),
        ));
    }
    if matches!(frame, ServerFrame::Reply(reply)
        if reply.result.as_ref().is_ok_and(|response| {
            response.requires_native_session_preview_capability()
                && !response.native_session_preview_contract_is_valid()
        }))
    {
        return Err(NodeClientError::Protocol(
            "node sent an invalid native session preview response".to_owned(),
        ));
    }
    if matches!(frame, ServerFrame::Reply(reply)
        if reply.result.as_ref().is_ok_and(|response| {
            response.requires_native_session_index_capability()
                && !response.native_session_index_contract_is_valid()
        }))
    {
        return Err(NodeClientError::Protocol(
            "node sent an invalid native session index response".to_owned(),
        ));
    }
    let required = match frame {
        ServerFrame::Reply(reply) => match reply.result.as_ref() {
            Ok(NodeResponse::DeliveryStageBegun { .. })
            | Ok(NodeResponse::DeliveryBlobChunkAccepted { .. })
            | Ok(NodeResponse::DeliveryCommitted { .. })
            | Ok(NodeResponse::DeliveryStageAborted { .. }) => {
                Some(NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY)
            }
            Err(NodeFailure {
                code:
                    NodeFailureCode::DeliveryManifestInvalid
                    | NodeFailureCode::UnknownDeliveryStage
                    | NodeFailureCode::DeliveryStageConflict
                    | NodeFailureCode::DeliveryBlobUnexpected
                    | NodeFailureCode::DeliveryChunkOutOfOrder
                    | NodeFailureCode::DeliveryBlobDigestMismatch
                    | NodeFailureCode::DeliveryBundleDigestMismatch
                    | NodeFailureCode::DeliveryStageIncomplete
                    | NodeFailureCode::DeliveryStageStorageFailed,
                ..
            }) => Some(NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY),
            Ok(response) if response.requires_harness_mcp_proxy_capability() => {
                Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY)
            }
            Err(NodeFailure { code, .. }) if matches!(code,
                NodeFailureCode::HarnessMcpUnavailable
                    | NodeFailureCode::ReservationNotFound
                    | NodeFailureCode::ReservationConflict
                    | NodeFailureCode::ReservationExpired
                    | NodeFailureCode::BindingMismatch
                    | NodeFailureCode::NotActivated
                    | NodeFailureCode::CallNotFound
                    | NodeFailureCode::ChunkOutOfOrder
                    | NodeFailureCode::ResponseTooLarge) => {
                Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY)
            }
            Ok(NodeResponse::WorkspaceFileRead { .. }) => {
                Some(NODE_WORKSPACE_FILE_READ_CAPABILITY)
            }
            Ok(NodeResponse::WorkspaceFileWritten { .. }) => {
                Some(NODE_WORKSPACE_FILE_WRITE_CAPABILITY)
            }
            Ok(NodeResponse::WorkspaceFileCreated { .. })
            | Ok(NodeResponse::WorkspaceDirectoryCreated { .. }) => {
                Some(NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY)
            }
            Ok(NodeResponse::GitHistoryRead { .. }) | Ok(NodeResponse::GitDiffRead { .. }) => {
                Some(NODE_GIT_READ_CAPABILITY)
            }
            Ok(NodeResponse::HostDirectoriesBrowsed { .. }) => {
                Some(CAPABILITY_HOST_DIRECTORY_BROWSE_V1)
            }
            Ok(NodeResponse::StandaloneWorkspaceCreated { .. }) => {
                Some(NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY)
            }
            Ok(NodeResponse::ProviderSessionIndexed { .. }) => {
                Some(NODE_PROVIDER_SESSION_REFERENCE_INDEX_CAPABILITY)
            }
            Ok(NodeResponse::NativeSessionIndexed { .. }) => {
                Some(NODE_NATIVE_SESSION_INDEX_CAPABILITY)
            }
            Ok(NodeResponse::NativeSessionsCataloged { .. }) => {
                Some(NODE_NATIVE_SESSION_CATALOG_CAPABILITY)
            }
            Ok(NodeResponse::NativeSessionsPaged { .. }) => {
                Some(NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY)
            }
            Err(NodeFailure {
                code: NodeFailureCode::StaleNativeSessionCatalog,
                ..
            }) => Some(NODE_NATIVE_SESSION_CATALOG_PAGING_CAPABILITY),
            Ok(NodeResponse::NativeSessionPreviewed { .. })
            | Ok(NodeResponse::SessionRecordPreviewed { .. }) => {
                Some(NODE_NATIVE_SESSION_PREVIEW_CAPABILITY)
            }
            Ok(NodeResponse::SessionRecordUpdated { .. })
                if matches!(expected_request, Some(NodeRequest::SetSessionTask { .. })) => {
                Some(NODE_SESSION_TASK_CORRELATION_CAPABILITY)
            }
            Ok(response) if response.requires_spawn_spec_defaults_overrides_capability() => {
                Some(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY)
            }
            _ => None,
        },
        ServerFrame::Event(event) if event.event.requires_harness_mcp_proxy_capability() => {
            Some(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY)
        }
        _ => None,
    };
    if let Some(required) = required {
        if !negotiated_capabilities
            .iter()
            .any(|capability| capability.as_str() == required)
        {
            return Err(NodeClientError::UnsupportedCapability(required.to_owned()));
        }
    }
    if matches!(frame, ServerFrame::Reply(reply)
        if reply.result.as_ref().is_ok_and(|response|
            response.requires_spawn_profile_revision_capability()))
        && !has_capability(
            negotiated_capabilities,
            NODE_SPAWN_PROFILE_REVISION_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_SPAWN_PROFILE_REVISION_CAPABILITY.to_owned(),
        ));
    }
    let contains_managed_worktree = server_frame_contains_managed_worktree(frame);
    if contains_managed_worktree
        && !has_capability(
            negotiated_capabilities,
            NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
        )
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY.to_owned(),
        ));
    }
    let requires_worktree_selection = contains_managed_worktree || match frame {
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(
            NodeResponse::requires_worktree_selection_capability,
        ),
        _ => false,
    };
    if requires_worktree_selection
        && !negotiated_capabilities.iter().any(|capability| {
            capability.as_str() == NODE_WORKTREE_SELECTION_CAPABILITY
        })
    {
        return Err(NodeClientError::UnsupportedCapability(
            NODE_WORKTREE_SELECTION_CAPABILITY.to_owned(),
        ));
    }
    let contains_environment_profile = match frame {
        ServerFrame::Hello(hello) => hello
            .snapshot
            .requires_child_environment_profile_capability(),
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(
            NodeResponse::requires_child_environment_profile_capability,
        ),
        ServerFrame::Event(event) => event
            .event
            .requires_child_environment_profile_capability(),
        ServerFrame::Challenge(_) => false,
    };
    if contains_environment_profile
        && !has_capability(
            negotiated_capabilities,
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent child environment profile metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    let contains_bundle = match frame {
        ServerFrame::Hello(hello) => hello
            .snapshot
            .requires_session_bundle_materialization_capability(),
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(
            NodeResponse::requires_session_bundle_materialization_capability,
        ),
        ServerFrame::Event(event) => event
            .event
            .requires_session_bundle_materialization_capability(),
        ServerFrame::Challenge(_) => false,
    };
    if contains_bundle
        && !has_capability(
            negotiated_capabilities,
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent session bundle materialization metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    let contains_history_context_pack = match frame {
        ServerFrame::Hello(hello) => hello
            .snapshot
            .requires_history_context_pack_capability(),
        ServerFrame::Reply(reply) => {
            reply.result.as_ref().ok().is_some_and(
                NodeResponse::requires_history_context_pack_capability,
            ) || reply.result.as_ref().err().is_some_and(|failure| {
                matches!(
                    failure.code,
                    NodeFailureCode::UnknownContextPack
                        | NodeFailureCode::ContextPackBusy
                        | NodeFailureCode::ContextPackMaterializationFailed
                )
            })
        }
        ServerFrame::Event(event) => event
            .event
            .requires_history_context_pack_capability(),
        ServerFrame::Challenge(_) => false,
    };
    if contains_history_context_pack
        && !has_capability(
            negotiated_capabilities,
            NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent history context pack metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    let contains_session_task_binding = match frame {
        ServerFrame::Hello(hello) => hello
            .snapshot
            .session_records
            .iter()
            .any(|record| record.task_binding.is_some()),
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|response| {
            match response {
                NodeResponse::Snapshot { snapshot, .. } => snapshot
                    .session_records
                    .iter()
                    .any(|record| record.task_binding.is_some()),
                NodeResponse::Resync { snapshot, events, .. } => snapshot
                    .session_records
                    .iter()
                    .any(|record| record.task_binding.is_some())
                    || events.iter().any(|event| matches!(&event.event,
                        NodeEvent::SessionRecordUpserted { record }
                            if record.task_binding.is_some())),
                NodeResponse::SessionRecordUpdated { record }
                | NodeResponse::ProviderSessionIndexed { record }
                | NodeResponse::NativeSessionIndexed { record, .. }
                | NodeResponse::SessionRecordResumed { record, .. } => {
                    record.task_binding.is_some()
                }
                _ => false,
            }
        }),
        ServerFrame::Event(event) => matches!(&event.event,
            NodeEvent::SessionRecordUpserted { record } if record.task_binding.is_some()),
        ServerFrame::Challenge(_) => false,
    };
    if contains_session_task_binding
        && !has_capability(
            negotiated_capabilities,
            NODE_SESSION_TASK_CORRELATION_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent session task correlation metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    if server_frame_contains_agent_progress(frame)
        && !has_capability(
            negotiated_capabilities,
            NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent agent progress metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    if server_frame_contains_observation(frame)
        && !has_capability(
            negotiated_capabilities,
            NODE_OBSERVATION_EVENTS_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent observation events without negotiating the capability".to_owned(),
        ));
    }
    if server_frame_contains_managed_observation(frame)
        && !has_capability(
            negotiated_capabilities,
            NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent managed observation targets without negotiating the capability"
                .to_owned(),
        ));
    }
    if server_frame_contains_observation_workflow_detail(frame)
        && !has_capability(
            negotiated_capabilities,
            NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent observation workflow detail without negotiating the capability".to_owned(),
        ));
    }
    Ok(())
}

fn has_capability(capabilities: &[CapabilityId], required: &str) -> bool {
    capabilities
        .iter()
        .any(|capability| capability.as_str() == required)
}

fn ensure_node_snapshot_agent_progress_capability(
    snapshot: &NodeSnapshot,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    if !snapshot.agent_progress.is_empty()
        && !has_capability(
            negotiated_capabilities,
            NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent agent progress metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    Ok(())
}

fn server_frame_contains_agent_progress(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Hello(hello) => !hello.snapshot.agent_progress.is_empty(),
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|response| {
            match response {
                NodeResponse::Snapshot { snapshot, .. }
                | NodeResponse::Resync { snapshot, .. } => !snapshot.agent_progress.is_empty(),
                _ => false,
            }
        }),
        ServerFrame::Challenge(_) | ServerFrame::Event(_) => false,
    }
}

fn server_frame_contains_observation(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|response| {
            matches!(response, NodeResponse::Resync { events, .. }
                if events.iter().any(|event| {
                    event.event.requires_observation_events_capability()
                }))
        }),
        ServerFrame::Event(event) => event.event.requires_observation_events_capability(),
        ServerFrame::Challenge(_) | ServerFrame::Hello(_) => false,
    }
}

fn server_frame_contains_managed_observation(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|response| {
            matches!(response, NodeResponse::Resync { events, .. }
                if events.iter().any(|event| {
                    event
                        .event
                        .requires_observation_managed_target_capability()
                }))
        }),
        ServerFrame::Event(event) => event
            .event
            .requires_observation_managed_target_capability(),
        ServerFrame::Challenge(_) | ServerFrame::Hello(_) => false,
    }
}

fn server_frame_contains_observation_workflow_detail(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Reply(reply) => reply.result.as_ref().ok().is_some_and(|response| {
            matches!(response, NodeResponse::Resync { events, .. }
                if events.iter().any(|event| {
                    event.event.requires_observation_workflow_detail_capability()
                }))
        }),
        ServerFrame::Event(event) => event
            .event
            .requires_observation_workflow_detail_capability(),
        ServerFrame::Challenge(_) | ServerFrame::Hello(_) => false,
    }
}

fn server_frame_contains_managed_worktree(frame: &ServerFrame) -> bool {
    match frame {
        ServerFrame::Hello(hello) => node_snapshot_contains_managed_worktree(&hello.snapshot),
        ServerFrame::Reply(reply) => reply
            .result
            .as_ref()
            .ok()
            .is_some_and(node_response_contains_managed_worktree),
        ServerFrame::Event(event) => node_event_is_managed_worktree(&event.event),
        ServerFrame::Challenge(_) => false,
    }
}

fn node_response_contains_managed_worktree(response: &NodeResponse) -> bool {
    match response {
        NodeResponse::Snapshot { snapshot, .. } => node_snapshot_contains_managed_worktree(snapshot),
        NodeResponse::Resync { snapshot, events, .. } => {
            node_snapshot_contains_managed_worktree(snapshot)
                || events
                    .iter()
                    .any(|event| node_event_is_managed_worktree(&event.event))
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::StandaloneWorkspaceCreated { workspace }
        | NodeResponse::WorktreeCreated { workspace, .. } => {
            workspace_contains_managed_worktree_metadata(workspace)
        }
        NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. } => true,
        _ => false,
    }
}

fn node_event_is_managed_worktree(event: &NodeEvent) -> bool {
    match event {
        NodeEvent::WorkspaceAdded { workspace } => {
            workspace_contains_managed_worktree_metadata(workspace)
        }
        NodeEvent::ManagedWorktreeUpserted { .. }
        | NodeEvent::ManagedWorktreeRemoved { .. } => true,
        _ => false,
    }
}

fn workspace_contains_managed_worktree_metadata(workspace: &WorkspaceSnapshot) -> bool {
    workspace.worktree_service_mode.is_some()
        || workspace.managed_worktree_profiles.is_some()
}

fn node_snapshot_contains_managed_worktree(snapshot: &NodeSnapshot) -> bool {
    !snapshot.managed_worktrees.is_empty()
        || snapshot
            .workspaces
            .iter()
            .any(workspace_contains_managed_worktree_metadata)
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

fn ensure_node_hello_environment_profile_capability(
    hello: &NodeHello,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    if hello
        .snapshot
        .requires_child_environment_profile_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent child environment profile metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    Ok(())
}

fn ensure_node_hello_bundle_materialization_capability(
    hello: &NodeHello,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    if hello
        .snapshot
        .requires_session_bundle_materialization_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent session bundle materialization metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    Ok(())
}

fn ensure_node_hello_history_context_pack_capability(
    hello: &NodeHello,
    negotiated_capabilities: &[CapabilityId],
) -> Result<(), NodeClientError> {
    if hello
        .snapshot
        .requires_history_context_pack_capability()
        && !has_capability(
            negotiated_capabilities,
            NODE_HISTORY_CONTEXT_PACK_CAPABILITY,
        )
    {
        return Err(NodeClientError::Protocol(
            "node sent history context pack metadata without negotiating the capability"
                .to_owned(),
        ));
    }
    Ok(())
}

fn ensure_node_request_provider_capability(
    request: &NodeRequest,
    open_provider_ids_enabled: bool,
) -> Result<(), NodeClientError> {
    match request {
        NodeRequest::Spawn { provider, .. } => {
            ensure_outbound_provider_id_capability(provider, open_provider_ids_enabled)?;
        }
        NodeRequest::IndexProviderSession { provider, .. } => {
            ensure_outbound_provider_id_capability(provider, open_provider_ids_enabled)?;
        }
        NodeRequest::CatalogNativeSessions { route, .. }
        | NodeRequest::PageNativeSessions { route, .. } => {
            ensure_outbound_provider_id_capability(&route.provider, open_provider_ids_enabled)?;
        }
        NodeRequest::PreviewNativeSession { selection, .. }
        | NodeRequest::IndexNativeSession { selection, .. } => {
            ensure_outbound_provider_id_capability(
                &selection.route.provider,
                open_provider_ids_enabled,
            )?;
        }
        NodeRequest::SpawnSpec { spec }
        | NodeRequest::ArmHarnessMcpReservation { spawn_spec: spec, .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { spec, .. }
        | NodeRequest::SpawnManagedWorktree {
            request: gate4agent_node_protocol::ManagedWorktreeSpawnRequest {
                spawn_spec: spec,
                ..
            },
        }
        | NodeRequest::SpawnManagedWorktreeV2 {
            request: gate4agent_node_protocol::ManagedWorktreeSpawnRequestV2 {
                spawn_spec: spec,
                ..
            },
        } => {
            if let gate4agent_node_protocol::SpawnOverride::Set { value: provider } =
                &spec.overrides.provider
            {
                ensure_outbound_provider_id_capability(provider, open_provider_ids_enabled)?;
            }
        }
        NodeRequest::ForgetContextPack { .. } => {
            if !open_provider_ids_enabled {
                return Err(NodeClientError::UnsupportedCapability(
                    NODE_PROVIDER_ID_OPEN_CAPABILITY.to_owned(),
                ));
            }
        }
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
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
        | NodeRequest::CleanupManagedWorktree { .. }
        | NodeRequest::Resume { .. }
        | NodeRequest::RenameSessionRecord { .. }
        | NodeRequest::SetSessionTask { .. }
        | NodeRequest::ResumeSessionRecord { .. }
        | NodeRequest::ForgetSessionRecord { .. }
        | NodeRequest::PreviewSessionRecord { .. }
        | NodeRequest::DiscoverHistory { .. }
        | NodeRequest::LoadHistory { .. }
        | NodeRequest::ExportContextPack { .. }
        | NodeRequest::Prompt { .. }
        | NodeRequest::Paste { .. }
        | NodeRequest::Input { .. }
        | NodeRequest::TerminalBytes { .. }
        | NodeRequest::TerminalControl { .. }
        | NodeRequest::Resize { .. }
        | NodeRequest::Interrupt { .. }
        | NodeRequest::Stop { .. }
        | NodeRequest::Remove { .. }
        | NodeRequest::Shutdown => {}
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
        NodeRequest::RegisterWorkspace { root, .. }
        | NodeRequest::CreateStandaloneWorkspace { root, .. } => {
            root.as_unix_bytes().is_some()
        }
        NodeRequest::BrowseHostDirectories { directory, after } => {
            directory.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
                || after.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
        }
        NodeRequest::CreateWorktree { target_root, .. }
        | NodeRequest::RemoveWorktree { target_root, .. } => {
            target_root.as_unix_bytes().is_some()
        }
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadWorkspaceFile { .. }
        | NodeRequest::WriteWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceFile { .. }
        | NodeRequest::CreateWorkspaceDirectory { .. }
        | NodeRequest::ReadGitHistory { .. }
        | NodeRequest::ReadGitDiff { .. }
        | NodeRequest::AcquireController { .. }
        | NodeRequest::ReleaseController
        | NodeRequest::UnregisterWorkspace { .. }
        | NodeRequest::Spawn { .. }
        | NodeRequest::SpawnSpec { .. }
        | NodeRequest::SpawnManagedWorktree { .. }
        | NodeRequest::SpawnManagedWorktreeV2 { .. }
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
        | NodeResponse::ProviderSessionIndexed { record }
        | NodeResponse::NativeSessionIndexed { record, .. }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            managed_session_record_contains_open_provider_id(record)
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::StandaloneWorkspaceCreated { workspace }
        | NodeResponse::WorktreeCreated { workspace, .. } => {
            workspace_contains_open_provider_id(workspace)
        }
        NodeResponse::SpawnSpecAccepted { receipt }
        | NodeResponse::Spawned { receipt, .. } => {
            resolved_spawn_receipt_contains_open_provider_id(receipt)
        }
        NodeResponse::ManagedWorktreeSpawnAccepted { receipt } => {
            resolved_spawn_receipt_contains_open_provider_id(&receipt.spawn)
        }
        NodeResponse::ContextPackExported { context } => {
            context_pack_contains_open_provider_id(context)
        }
        NodeResponse::NativeSessionsCataloged { route, .. }
        | NodeResponse::NativeSessionsPaged { route, .. } => {
            !provider_id_is_legacy(&route.provider)
        }
        NodeResponse::NativeSessionPreviewed { selection, .. } => {
            !provider_id_is_legacy(&selection.route.provider)
        }
        NodeResponse::Armed { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::WorkspaceInspected { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::HostDirectoriesBrowsed { .. }
        | NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::WorkspaceFileWritten { .. }
        | NodeResponse::WorkspaceFileCreated { .. }
        | NodeResponse::WorkspaceDirectoryCreated { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::GitDiffRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackForgotten { .. }
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
    }) || snapshot.session_records.iter().any(
        managed_session_record_contains_open_provider_id,
    ) || snapshot.workspaces.iter().any(workspace_contains_open_provider_id)
}

fn managed_session_record_contains_open_provider_id(
    record: &gate4agent_node_protocol::ManagedSessionRecord,
) -> bool {
    !provider_id_is_legacy(&record.provider)
        || record.context.as_ref().is_some_and(
            context_pack_contains_open_provider_id,
        )
}

fn resolved_spawn_receipt_contains_open_provider_id(
    receipt: &gate4agent_node_protocol::ResolvedSpawnReceipt,
) -> bool {
    !provider_id_is_legacy(&receipt.provider)
        || receipt.context.as_ref().is_some_and(
            context_pack_contains_open_provider_id,
        )
}

fn context_pack_contains_open_provider_id(
    context: &gate4agent_node_protocol::ResolvedContextPackReceipt,
) -> bool {
    !provider_id_is_legacy(&context.lineage.source_provider)
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
            managed_session_record_contains_open_provider_id(record)
        }
        NodeEvent::HarnessMcpReadCall { .. }
        | NodeEvent::Control { .. }
        | NodeEvent::Observation { .. }
        | NodeEvent::ManagedObservation { .. }
        | NodeEvent::TerminalFrame { .. }
        | NodeEvent::ControllerChanged { .. }
        | NodeEvent::WorkspaceRemoved { .. }
        | NodeEvent::SessionRecordRemoved { .. }
        | NodeEvent::ManagedWorktreeUpserted { .. }
        | NodeEvent::ManagedWorktreeRemoved { .. }
        | NodeEvent::ResyncRequired { .. } => false,
    }
}

fn node_request_contains_tagged_repository_path(request: &NodeRequest) -> bool {
    match request {
        NodeRequest::ReadWorkspaceFile { path, .. }
        | NodeRequest::WriteWorkspaceFile { path, .. }
        | NodeRequest::CreateWorkspaceFile { path, .. }
        | NodeRequest::CreateWorkspaceDirectory { path, .. } => path.as_unix_bytes().is_some(),
        NodeRequest::ReadGitDiff { request, .. } => request
            .path
            .as_ref()
            .is_some_and(|path| path.as_unix_bytes().is_some()),
        NodeRequest::Snapshot
        | NodeRequest::Resync { .. }
        | NodeRequest::ArmHarnessMcpReservation { .. }
        | NodeRequest::SpawnSpecWithHarnessMcp { .. }
        | NodeRequest::ActivateHarnessMcpReservation { .. }
        | NodeRequest::AbortHarnessMcpReservation { .. }
        | NodeRequest::PutHarnessMcpReplyChunk { .. }
        | NodeRequest::RejectHarnessMcpCall { .. }
        | NodeRequest::BeginDeliveryStage { .. }
        | NodeRequest::PutDeliveryBlobChunk { .. }
        | NodeRequest::CommitDeliveryStage { .. }
        | NodeRequest::AbortDeliveryStage { .. }
        | NodeRequest::BrowseHostDirectories { .. }
        | NodeRequest::InspectWorkspace { .. }
        | NodeRequest::ReadGitHistory { .. }
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
        | NodeRequest::SpawnManagedWorktreeV2 { .. }
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
        NodeResponse::WorkspaceFileRead { file }
        | NodeResponse::WorkspaceFileWritten { file }
        | NodeResponse::WorkspaceFileCreated { file } => file.path.as_unix_bytes().is_some(),
        NodeResponse::WorkspaceDirectoryCreated { entry, .. } => {
            entry.relative_path.as_unix_bytes().is_some()
        }
        NodeResponse::GitDiffRead { diff, .. } => diff
            .path
            .as_ref()
            .is_some_and(|path| path.as_unix_bytes().is_some()),
        NodeResponse::Snapshot { .. }
        | NodeResponse::Resync { .. }
        | NodeResponse::Armed { .. }
        | NodeResponse::Spawned { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::HostDirectoriesBrowsed { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SpawnSpecAccepted { .. }
        | NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::SessionRecordUpdated { .. }
        | NodeResponse::ProviderSessionIndexed { .. }
        | NodeResponse::NativeSessionIndexed { .. }
        | NodeResponse::SessionRecordResumed { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::NativeSessionsCataloged { .. }
        | NodeResponse::NativeSessionsPaged { .. }
        | NodeResponse::NativeSessionPreviewed { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackExported { .. }
        | NodeResponse::ContextPackForgotten { .. }
        | NodeResponse::WorkspaceRegistered { .. }
        | NodeResponse::StandaloneWorkspaceCreated { .. }
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
        NodeResponse::HostDirectoriesBrowsed { listing } => {
            listing.directory.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
                || listing.parent.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
                || listing.entries.iter().any(|entry| entry.path.as_unix_bytes().is_some())
                || listing.next_after.as_ref().is_some_and(|path| path.as_unix_bytes().is_some())
        }
        NodeResponse::WorkspaceFileRead { .. }
        | NodeResponse::WorkspaceFileWritten { .. }
        | NodeResponse::WorkspaceFileCreated { .. }
        | NodeResponse::WorkspaceDirectoryCreated { .. }
        | NodeResponse::GitHistoryRead { .. }
        | NodeResponse::GitDiffRead { .. } => false,
        NodeResponse::SessionRecordUpdated { record }
        | NodeResponse::ProviderSessionIndexed { record }
        | NodeResponse::NativeSessionIndexed { record, .. }
        | NodeResponse::SessionRecordResumed { record, .. } => {
            record.canonical_root.as_unix_bytes().is_some()
        }
        NodeResponse::WorkspaceRegistered { workspace }
        | NodeResponse::StandaloneWorkspaceCreated { workspace } => {
            workspace.canonical_root.as_unix_bytes().is_some()
        }
        NodeResponse::WorktreeCreated { worktree, workspace } => {
            worktree.path.as_unix_bytes().is_some()
                || workspace.canonical_root.as_unix_bytes().is_some()
        }
        NodeResponse::WorktreeRemoved { target_root, .. } => {
            target_root.as_unix_bytes().is_some()
        }
        NodeResponse::Armed { .. }
        | NodeResponse::Spawned { .. }
        | NodeResponse::Activated { .. }
        | NodeResponse::Aborted { .. }
        | NodeResponse::ReplyChunkAccepted { .. }
        | NodeResponse::CallRejected { .. }
        | NodeResponse::Controller { .. }
        | NodeResponse::DeliveryStageBegun { .. }
        | NodeResponse::DeliveryBlobChunkAccepted { .. }
        | NodeResponse::DeliveryCommitted { .. }
        | NodeResponse::DeliveryStageAborted { .. }
        | NodeResponse::SpawnAccepted { .. }
        | NodeResponse::SpawnSpecAccepted { .. }
        | NodeResponse::ManagedWorktreeSpawnAccepted { .. }
        | NodeResponse::ManagedWorktreeCleanup { .. }
        | NodeResponse::SessionRecordForgotten { .. }
        | NodeResponse::NativeSessionsCataloged { .. }
        | NodeResponse::NativeSessionsPaged { .. }
        | NodeResponse::NativeSessionPreviewed { .. }
        | NodeResponse::SessionRecordPreviewed { .. }
        | NodeResponse::HistoryDiscovered { .. }
        | NodeResponse::HistoryLoaded { .. }
        | NodeResponse::ContextPackExported { .. }
        | NodeResponse::ContextPackForgotten { .. }
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
        NodeEvent::HarnessMcpReadCall { .. }
        | NodeEvent::Control { .. }
        | NodeEvent::Observation { .. }
        | NodeEvent::ManagedObservation { .. }
        | NodeEvent::TerminalFrame { .. }
        | NodeEvent::ControllerChanged { .. }
        | NodeEvent::WorkspaceRemoved { .. }
        | NodeEvent::SessionRecordRemoved { .. }
        | NodeEvent::ManagedWorktreeUpserted { .. }
        | NodeEvent::ManagedWorktreeRemoved { .. }
        | NodeEvent::ResyncRequired { .. } => false,
    }
}

fn client_compatibility_offer() -> Result<ClientCompatibilityOffer, NodeClientError> {
    let mut offer = production_node_client_compatibility_offer();
    let history_context_pack =
        CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).map_err(|error| {
            NodeClientError::Protocol(error.to_string())
        })?;
    if !offer.capabilities.contains(&history_context_pack) {
        offer.capabilities.push(history_context_pack);
    }
    Ok(offer)
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
        AdapterContractRevision, AdapterFamily, AdapterId, AgentId, AgentProgressCurrentV1,
        AgentProgressV1, ArchitectureId,
        ContextPackLineageReceipt,
        GitSnapshot, GitStatusEntry, GitWorktreeSnapshot, HostDescriptor, LocalTransportKind,
        ManagedSessionRecord, ManagedSessionState, NodeCompatibilitySupport, OpaqueHostPath,
        ManagedWorktreeCleanupFailure, ManagedWorktreeLeaseId,
        ManagedWorktreeLeaseSnapshot, ManagedWorktreeLeaseState, ManagedWorktreeRetention,
        ManagedWorktreeSpawnReceipt, ManagedWorktreeSpawnRequest,
        HostDirectoryEntry, HostDirectoryListing, OperatingSystemId, PathEncoding,
        PathSemantics, PathStyle,
        ProviderAdapterContractSupport, ProviderContractRevision, ProviderContractSupport,
        ProviderRuntimeStatus, ProviderRuntimeStatuses, RepositoryPath, ResponseEnvelope,
        ResolvedBundleReceipt, ResolvedContextPackReceipt, ResolvedEnvironmentProfileReceipt,
        ResolvedSpawnReceipt,
        SessionAddress, SessionAgentProgress, SessionKey,
        SessionMode, SessionRecordId,
        SpawnBundleDigest, SpawnBundleId, SpawnBundleRevision, SpawnDeadlineMs,
        SpawnContextDigest, SpawnContextId,
        SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverrides,
        SpawnEnvironmentProfileId, SpawnEnvironmentProfileRevision, SpawnProfileId,
        SpawnProfileRevision, SpawnPromptMetadata, SpawnRequiredCapabilities,
        SpawnResolutionProvenance, SpawnSpec, SpawnTarget, WorkspaceEntry,
        WorkspaceEntryKind, WorkspaceFileContent, WorkspaceFileRead, WorkspaceFileRevision,
        WorkspaceId,
        WorkspaceInspection, WorkspaceSnapshot,
        WorktreeProfileId, WorktreeProfileRevision, WorktreeServiceMode,
        ObservationEvidenceV1, ObservationKindV1, ObservationV1,
    };

    #[test]
    fn harness_mcp_wire_capability_and_exact_response_correlation() {
        let reservation_id = gate4agent_node_protocol::HarnessMcpReservationId::new(
            format!("hmcpres_{}", "a".repeat(24)),
        ).unwrap();
        let activation_digest = gate4agent_node_protocol::HarnessMcpActivationDigest::new(
            format!("sha256:{}", "b".repeat(64)),
        ).unwrap();
        let request = NodeRequest::AbortHarnessMcpReservation {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
        };
        assert!(matches!(
            ensure_node_request_required_capability(&request, &[]),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_HARNESS_MCP_READ_PROXY_CAPABILITY
        ));
        let capability = CapabilityId::new(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY).unwrap();
        assert!(ensure_node_request_required_capability(
            &request,
            std::slice::from_ref(&capability),
        ).is_ok());
        let exact = Ok(NodeResponse::Aborted {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
        });
        assert!(validate_harness_mcp_response(&request, &exact).is_ok());
        let mismatch = Ok(NodeResponse::Aborted {
            reservation_id: gate4agent_node_protocol::HarnessMcpReservationId::new(
                format!("hmcpres_{}", "c".repeat(24)),
            ).unwrap(),
            activation_digest,
        });
        assert!(validate_harness_mcp_response(&request, &mismatch).is_err());
        let frame = ServerFrame::Reply(ResponseEnvelope {
            request_id: 1,
            result: exact,
        });
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(&frame, &[capability]).is_ok());
    }
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
            worktree_service_mode: None,
            managed_worktree_profiles: None,
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
            environment_profile: None,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
            last_error: None,
        }
    }

    #[test]
    fn session_task_capability_and_idempotent_response_correlation_are_fail_closed() {
        let task_id = gate4agent_node_protocol::TaskId::from_nonce([7; 12]);
        let request = NodeRequest::SetSessionTask {
            record_id: SessionRecordId::new("session-a").unwrap(),
            expected_revision: 7,
            target: gate4agent_node_protocol::SessionTaskTargetV1::Existing {
                task_id: task_id.clone(),
            },
        };
        assert!(ensure_node_request_required_capability(&request, &[]).is_err());
        let capability = CapabilityId::new(NODE_SESSION_TASK_CORRELATION_CAPABILITY).unwrap();
        assert!(ensure_node_request_required_capability(
            &request,
            std::slice::from_ref(&capability),
        ).is_ok());

        let mut record = session_record_with_path(utf8_path());
        record.task_binding = Some(gate4agent_node_protocol::SessionTaskBindingV1 {
            revision: 7,
            task_id: Some(task_id.clone()),
            changed_at_unix_ms: 2,
        });
        assert!(validate_session_task_response(
            &request,
            &Ok(NodeResponse::SessionRecordUpdated { record: record.clone() }),
        ).is_ok());
        record.task_binding.as_mut().unwrap().revision = 8;
        assert!(validate_session_task_response(
            &request,
            &Ok(NodeResponse::SessionRecordUpdated { record: record.clone() }),
        ).is_ok());
        record.task_binding.as_mut().unwrap().revision = 9;
        assert!(validate_session_task_response(
            &request,
            &Ok(NodeResponse::SessionRecordUpdated { record: record.clone() }),
        ).is_err());

        let clear = NodeRequest::SetSessionTask {
            record_id: SessionRecordId::new("session-a").unwrap(),
            expected_revision: 0,
            target: gate4agent_node_protocol::SessionTaskTargetV1::Clear,
        };
        record.task_binding = None;
        assert!(validate_session_task_response(
            &clear,
            &Ok(NodeResponse::SessionRecordUpdated { record: record.clone() }),
        ).is_ok());

        let new = NodeRequest::SetSessionTask {
            record_id: SessionRecordId::new("session-a").unwrap(),
            expected_revision: 7,
            target: gate4agent_node_protocol::SessionTaskTargetV1::New,
        };
        record.task_binding = Some(gate4agent_node_protocol::SessionTaskBindingV1 {
            revision: 7,
            task_id: Some(task_id),
            changed_at_unix_ms: 2,
        });
        assert!(validate_session_task_response(
            &new,
            &Ok(NodeResponse::SessionRecordUpdated { record: record.clone() }),
        ).is_err());
        record.task_binding.as_mut().unwrap().revision = 8;
        let frame = response_frame(NodeResponse::SessionRecordUpdated { record: record.clone() });
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(&frame, &[capability]).is_ok());
        assert!(validate_session_task_response(
            &new,
            &Ok(NodeResponse::SessionRecordUpdated { record }),
        ).is_ok());
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
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            agent_progress: Vec::new(),
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

    #[test]
    fn agent_progress_requires_negotiated_capability_on_hello_snapshot_and_reply() {
        let mut snapshot = empty_snapshot();
        snapshot.agent_progress.push(SessionAgentProgress {
            address: SessionAddress {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                },
            },
            progress: AgentProgressV1 {
                provider_sequence: 3,
                activity: gate4agent_types::ProviderActivity::Idle,
                completed_turns: 0,
                usage: None,
                current: AgentProgressCurrentV1::Idle,
                active_tool_labels: Vec::new(),
                active_tool_count: 0,
                attention: None,
                subagent_count: 0,
                last_event_kind: None,
                gap_count: 0,
                stale: false,
                truncated: false,
            },
        });
        let capabilities = Vec::new();
        assert!(matches!(
            ensure_node_snapshot_agent_progress_capability(&snapshot, &capabilities),
            Err(NodeClientError::Protocol(message))
                if message.contains("without negotiating the capability")
        ));
        let frame = response_frame(NodeResponse::Snapshot {
            event_sequence: 0,
            controller: None,
            snapshot: snapshot.clone(),
        });
        assert!(matches!(
            ensure_server_frame_required_capability(&frame, &capabilities),
            Err(NodeClientError::Protocol(message))
                if message.contains("without negotiating the capability")
        ));
        let negotiated = vec![
            CapabilityId::new(NODE_AGENT_PROGRESS_SNAPSHOT_CAPABILITY).unwrap(),
        ];
        assert!(ensure_node_snapshot_agent_progress_capability(
            &snapshot,
            &negotiated,
        )
        .is_ok());
        assert!(ensure_server_frame_required_capability(&frame, &negotiated).is_ok());
    }

    #[test]
    fn observation_events_require_negotiated_capabilities() {
        let address = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(2),
            },
        };
        let frame = ServerFrame::Event(NodeEventEnvelope {
            sequence: 4,
            event: NodeEvent::Observation {
                address: address.clone(),
                observation: ObservationV1 {
                    source_sequence: 3,
                    observed_at_unix_ms: Some(2),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                    kind: ObservationKindV1::Working,
                    truncated: false,
                },
            },
        });
        let base = CapabilityId::new(NODE_OBSERVATION_EVENTS_CAPABILITY).unwrap();
        let detail = CapabilityId::new(NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY).unwrap();
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(
            &frame,
            std::slice::from_ref(&base),
        )
        .is_ok());

        let detailed = ServerFrame::Event(NodeEventEnvelope {
            sequence: 5,
            event: NodeEvent::Observation {
                address,
                observation: ObservationV1 {
                    source_sequence: 4,
                    observed_at_unix_ms: Some(3),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                    kind: ObservationKindV1::Error {
                        detail: "provider-error".to_owned(),
                    },
                    truncated: false,
                },
            },
        });
        assert!(ensure_server_frame_required_capability(
            &detailed,
            std::slice::from_ref(&base),
        )
        .is_err());
        assert!(ensure_server_frame_required_capability(&detailed, &[base, detail]).is_ok());
    }

    #[test]
    fn managed_observation_requires_managed_target_capability() {
        let frame = ServerFrame::Event(NodeEventEnvelope {
            sequence: 4,
            event: NodeEvent::ManagedObservation {
                record_id: gate4agent_node_protocol::SessionRecordId::new("record-a").unwrap(),
                observation: ObservationV1 {
                    source_sequence: 3,
                    observed_at_unix_ms: Some(2),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                    kind: ObservationKindV1::Working,
                    truncated: false,
                },
            },
        });
        let base = CapabilityId::new(NODE_OBSERVATION_EVENTS_CAPABILITY).unwrap();
        let managed =
            CapabilityId::new(NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY).unwrap();
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(
            &frame,
            std::slice::from_ref(&base),
        )
        .is_err());
        assert!(ensure_server_frame_required_capability(
            &frame,
            std::slice::from_ref(&managed),
        )
        .is_err());
        assert!(ensure_server_frame_required_capability(&frame, &[base, managed]).is_ok());
    }

    fn response_frame(response: NodeResponse) -> ServerFrame {
        ServerFrame::Reply(ResponseEnvelope {
            request_id: 1,
            result: Ok(response),
        })
    }

    fn spawn_spec_request() -> NodeRequest {
        let mut overrides = SpawnOverrides::default();
        overrides.context_id = gate4agent_node_protocol::SpawnOverride::Clear;
        NodeRequest::SpawnSpec {
            spec: SpawnSpec {
                target: SpawnTarget {
                    node_id: NodeId::new("node-a").unwrap(),
                    workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                    worktree_id: None,
                },
                profile_id: SpawnProfileId::new("default").unwrap(),
                expected_profile_revision:
                    SpawnProfileRevision::new("default.r1").unwrap(),
                overrides,
                deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
                idempotency_key: SpawnIdempotencyKey::new("request-a").unwrap(),
                required_capabilities: SpawnRequiredCapabilities::default(),
            },
        }
    }

    fn spawn_spec_receipt() -> ResolvedSpawnReceipt {
        ResolvedSpawnReceipt {
            incarnation_id: NodeIncarnationId::from_bytes([3; NODE_INCARNATION_ID_BYTES]),
            session: SessionAddress {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(9),
                    generation: SessionGeneration(1),
                },
            },
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("default").unwrap(),
            profile_revision: SpawnProfileRevision::new("default.r1").unwrap(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            prompt: SpawnPromptMetadata {
                present: false,
                byte_len: 0,
            },
            bundle_id: None,
            bundle: None,
            context_id: None,
            context: None,
            environment_profile: None,
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("request-a").unwrap(),
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

    fn context_pack_receipt() -> ResolvedContextPackReceipt {
        ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-a").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "c".repeat(64)))
                .unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("node-a").unwrap(),
                source_session: SessionAddress {
                    workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(7),
                        generation: SessionGeneration(2),
                    },
                },
                source_provider: agent("claude"),
            },
            source_message_count: 4,
            retained_message_count: 3,
            byte_len: 128,
            truncated: true,
        }
    }

    #[test]
    fn native_session_response_correlation_is_fail_closed() {
        let route = gate4agent_node_protocol::NativeSessionCatalogRoute::workspace(
            WorkspaceId::new("workspace-a").unwrap(),
            agent("codex"),
        );
        let selection = gate4agent_node_protocol::NativeSessionSelection {
            route: route.clone(),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 70,
            selection_id: "selection-7".to_owned(),
        };

        let catalog = NodeRequest::CatalogNativeSessions {
            route: route.clone(),
            limit: 10,
        };
        assert!(validate_native_session_response(
            &catalog,
            &Ok(NodeResponse::NativeSessionsCataloged {
                route: route.clone(),
                entries: Vec::new(),
                summary: None,
            }),
        )
        .is_ok());
        let wrong_route = gate4agent_node_protocol::NativeSessionCatalogRoute::workspace(
            WorkspaceId::new("workspace-b").unwrap(),
            agent("codex"),
        );
        assert!(validate_native_session_response(
            &catalog,
            &Ok(NodeResponse::NativeSessionsCataloged {
                route: wrong_route,
                entries: Vec::new(),
                summary: None,
            }),
        )
        .is_err());

        let page = NodeRequest::PageNativeSessions {
            route: route.clone(),
            window: gate4agent_node_protocol::NativeSessionCatalogWindow::Recent,
            catalog_revision: 7,
            recent_cutoff_unix_ms: 70,
            after_selection_id: None,
            limit: 10,
        };
        assert!(validate_native_session_response(
            &page,
            &Ok(NodeResponse::NativeSessionsPaged {
                route: route.clone(),
                page: gate4agent_node_protocol::NativeSessionCatalogPage {
                    window: gate4agent_node_protocol::NativeSessionCatalogWindow::Recent,
                    revision: 8,
                    entries: Vec::new(),
                    next_after_selection_id: None,
                    remaining_count: 0,
                    has_more: false,
                },
            }),
        )
        .is_err());

        let preview = NodeRequest::PreviewNativeSession {
            selection: selection.clone(),
            message_limit: 10,
        };
        let mut wrong_selection = selection.clone();
        wrong_selection.catalog_revision = 8;
        assert!(validate_native_session_response(
            &preview,
            &Ok(NodeResponse::NativeSessionPreviewed {
                selection: wrong_selection,
                preview: gate4agent_node_protocol::SessionRecordPreview {
                    title: None,
                    modified_at_unix_ms: None,
                    model: None,
                    message_count: 0,
                    message_count_exact: true,
                    completed_turn_count: None,
                    total_tokens: None,
                    truncated: false,
                    messages: Vec::new(),
                },
            }),
        )
        .is_err());

        let index = NodeRequest::IndexNativeSession {
            selection: selection.clone(),
            display_name: "Indexed".to_owned(),
        };
        let mut record = session_record_with_path(utf8_path());
        record.provider = agent("codex");
        assert!(validate_native_session_response(
            &index,
            &Ok(NodeResponse::NativeSessionIndexed {
                selection: selection.clone(),
                record: record.clone(),
            }),
        )
        .is_ok());
        assert!(validate_native_session_response(
            &index,
            &Ok(NodeResponse::ProviderSessionIndexed {
                record: record.clone(),
            }),
        )
        .is_err());
        let mut wrong_selection = selection.clone();
        wrong_selection.catalog_revision = 8;
        assert!(validate_native_session_response(
            &index,
            &Ok(NodeResponse::NativeSessionIndexed {
                selection: wrong_selection,
                record: record.clone(),
            }),
        )
        .is_err());
        let mut wrong_record = record.clone();
        wrong_record.provider = agent("claude");
        assert!(validate_native_session_response(
            &index,
            &Ok(NodeResponse::NativeSessionIndexed {
                selection: selection.clone(),
                record: wrong_record,
            }),
        )
        .is_err());

        let external_index = NodeRequest::IndexNativeSession {
            selection: gate4agent_node_protocol::NativeSessionSelection {
                route: gate4agent_node_protocol::NativeSessionCatalogRoute::unregistered(
                    agent("codex"),
                ),
                ..selection
            },
            display_name: "External".to_owned(),
        };
        assert!(validate_native_session_response(
            &external_index,
            &Ok(NodeResponse::NativeSessionIndexed {
                selection: match &external_index {
                    NodeRequest::IndexNativeSession { selection, .. } => selection.clone(),
                    _ => unreachable!(),
                },
                record,
            }),
        )
        .is_err());
    }

    #[test]
    fn client_offer_accepts_open_provider_ids_and_durable_state_schema_v1_through_v8() {
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
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap(),
        ));
        assert!(offer.capabilities.contains(
            &CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap(),
        ));
        assert_eq!(
            offer.state_schema.unwrap().versions,
            ProtocolRange::new(NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V8).unwrap(),
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
    fn spawn_spec_capability_is_bound_to_authentication_proof() {
        let (mut offer, mut selected) = negotiated_fixture();
        let capability =
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap();
        offer.capabilities.push(capability.clone());
        selected.capabilities.push(capability);
        let client_nonce = [5; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [8; NODE_AUTH_NONCE_BYTES];
        let access_token = "spawn-spec-auth-token";
        let server_proof = negotiated_auth_proof(
            access_token.as_bytes(),
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &offer,
            &selected,
        )
        .unwrap();
        selected.capabilities.retain(|candidate| {
            candidate.as_str() != NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
        });
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
                ClientRole::Operator,
                &client_nonce,
                access_token,
            ),
            Err(NodeClientError::Protocol(message))
                if message.contains("server failed access-token proof")
        ));
    }

    #[test]
    fn history_context_pack_capability_is_bound_to_authentication_proof() {
        let (mut offer, mut selected) = negotiated_fixture();
        let capability = CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap();
        offer.capabilities.push(capability.clone());
        selected.capabilities.push(capability);
        let client_nonce = [6; NODE_AUTH_NONCE_BYTES];
        let server_nonce = [9; NODE_AUTH_NONCE_BYTES];
        let access_token = "history-context-auth-token";
        let server_proof = negotiated_auth_proof(
            access_token.as_bytes(),
            AuthDirection::Server,
            ClientRole::Operator,
            &client_nonce,
            &server_nonce,
            &offer,
            &selected,
        )
        .unwrap();
        selected.capabilities.retain(|candidate| {
            candidate.as_str() != NODE_HISTORY_CONTEXT_PACK_CAPABILITY
        });
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
                ClientRole::Operator,
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

        let mut context = context_pack_receipt();
        context.lineage.source_provider = agent("qwen");
        let context_frame = response_frame(NodeResponse::ContextPackExported { context });
        assert!(ensure_server_frame_provider_capability(&context_frame, false).is_err());
        assert!(ensure_server_frame_provider_capability(&context_frame, true).is_ok());
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
                revision: None,
            },
        });
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        let capabilities = vec![
            CapabilityId::new(NODE_WORKSPACE_FILE_READ_CAPABILITY).unwrap(),
        ];
        assert!(ensure_server_frame_required_capability(&frame, &capabilities).is_ok());
    }

    #[test]
    fn workspace_entry_create_is_capability_gated_and_response_correlated_fail_closed() {
        let workspace_id = WorkspaceId::new("workspace-a").unwrap();
        let file_path = utf8_repository_path("src/new.rs");
        let file_request = NodeRequest::CreateWorkspaceFile {
            workspace_id: workspace_id.clone(),
            path: file_path.clone(),
        };
        let mut next_request_id = 73;
        let error = reserve_request_id(
            &mut next_request_id,
            &file_request,
            false,
            false,
            false,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NodeClientError::UnsupportedCapability(capability)
                if capability == NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY
        ));
        assert_eq!(next_request_id, 73);

        let capabilities = vec![
            CapabilityId::new(NODE_WORKSPACE_ENTRY_CREATE_CAPABILITY).unwrap(),
        ];
        assert_eq!(
            reserve_request_id(
                &mut next_request_id,
                &file_request,
                false,
                false,
                false,
                &capabilities,
            )
            .unwrap(),
            73,
        );

        let file = WorkspaceFileRead {
            workspace_id: workspace_id.clone(),
            path: file_path.clone(),
            content: WorkspaceFileContent::Utf8 {
                text: String::new(),
                byte_len: 0,
            },
            revision: Some(WorkspaceFileRevision::new(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_owned(),
            ).unwrap()),
        };
        let file_frame = response_frame(NodeResponse::WorkspaceFileCreated {
            file: file.clone(),
        });
        assert!(ensure_server_frame_required_capability(&file_frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(&file_frame, &capabilities).is_ok());
        assert!(validate_workspace_content_response(
            &file_request,
            &Ok(NodeResponse::WorkspaceFileCreated { file: file.clone() }),
        ).is_ok());

        let mut wrong_content = file;
        wrong_content.content = WorkspaceFileContent::Utf8 {
            text: "not-empty".to_owned(),
            byte_len: 9,
        };
        assert!(validate_workspace_content_response(
            &file_request,
            &Ok(NodeResponse::WorkspaceFileCreated { file: wrong_content }),
        ).is_err());

        let directory_path = utf8_repository_path("src/new");
        let directory_request = NodeRequest::CreateWorkspaceDirectory {
            workspace_id: workspace_id.clone(),
            path: directory_path.clone(),
        };
        let directory_response = NodeResponse::WorkspaceDirectoryCreated {
            workspace_id,
            entry: WorkspaceEntry {
                relative_path: directory_path,
                kind: WorkspaceEntryKind::Directory,
            },
        };
        assert!(validate_workspace_content_response(
            &directory_request,
            &Ok(directory_response),
        ).is_ok());
        let tagged_directory_request = NodeRequest::CreateWorkspaceDirectory {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            path: tagged_repository_path(b"src/\xff"),
        };
        assert!(ensure_node_request_path_capability(
            &tagged_directory_request,
            false,
            false,
        ).is_err());
        assert!(ensure_node_request_path_capability(
            &tagged_directory_request,
            false,
            true,
        ).is_ok());
        let tagged_directory_frame = response_frame(
            NodeResponse::WorkspaceDirectoryCreated {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                entry: WorkspaceEntry {
                    relative_path: tagged_repository_path(b"src/\xff"),
                    kind: WorkspaceEntryKind::Directory,
                },
            },
        );
        assert!(ensure_server_frame_path_capability(
            &tagged_directory_frame,
            false,
            false,
        ).is_err());
        assert!(ensure_server_frame_path_capability(
            &tagged_directory_frame,
            false,
            true,
        ).is_ok());
        assert!(validate_workspace_content_response(
            &directory_request,
            &Ok(NodeResponse::WorkspaceFileCreated {
                file: WorkspaceFileRead {
                    workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                    path: utf8_repository_path("src/new"),
                    content: WorkspaceFileContent::Utf8 {
                        text: String::new(),
                        byte_len: 0,
                    },
                    revision: None,
                },
            }),
        ).is_err());
    }

    #[test]
    fn ordinary_startup_snapshot_is_not_rejected_as_native_session_preview() {
        let frame = response_frame(NodeResponse::Snapshot {
            event_sequence: 0,
            controller: None,
            snapshot: empty_snapshot(),
        });

        assert!(ensure_server_frame_required_capability(&frame, &[]).is_ok());
    }

    #[test]
    fn host_directory_browse_requires_capability_and_preserves_opaque_paths() {
        let unix = OpaqueHostPath::unix_bytes(b"/srv/\xff".to_vec()).unwrap();
        let request = NodeRequest::BrowseHostDirectories {
            directory: None,
            after: Some(unix.clone()),
        };
        assert!(ensure_node_request_required_capability(&request, &[]).is_err());
        assert!(node_request_contains_opaque_unix_path(&request));
        let capability = CapabilityId::new(CAPABILITY_HOST_DIRECTORY_BROWSE_V1).unwrap();
        assert!(ensure_node_request_required_capability(
            &request,
            &[capability.clone()],
        ).is_ok());

        let frame = response_frame(NodeResponse::HostDirectoriesBrowsed {
            listing: HostDirectoryListing {
                directory: Some(unix.clone()),
                parent: None,
                entries: vec![HostDirectoryEntry {
                    path: unix,
                    display_name: "opaque".to_owned(),
                    is_link: false,
                }],
                next_after: None,
                incomplete: false,
            },
        });
        assert!(ensure_server_frame_required_capability(&frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(&frame, &[capability]).is_ok());
        assert!(server_frame_contains_opaque_unix_path(&frame));
    }

    #[test]
    fn standalone_workspace_lifecycle_is_exactly_capability_and_path_gated() {
        let root = OpaqueHostPath::unix_bytes(b"/srv/standalone".to_vec()).unwrap();
        let request = NodeRequest::CreateStandaloneWorkspace {
            workspace_id: WorkspaceId::new("standalone").unwrap(),
            root: root.clone(),
            initial_branch: Some("main".to_owned()),
        };
        let capability = CapabilityId::new(
            NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY,
        ).unwrap();

        assert!(matches!(
            ensure_node_request_required_capability(&request, &[]),
            Err(NodeClientError::UnsupportedCapability(required))
                if required == NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY
        ));
        assert!(ensure_node_request_required_capability(
            &request,
            std::slice::from_ref(&capability),
        ).is_ok());
        assert!(node_request_contains_opaque_unix_path(&request));

        let frame = response_frame(NodeResponse::StandaloneWorkspaceCreated {
            workspace: workspace_with_path(root),
        });
        assert!(matches!(
            ensure_server_frame_required_capability(&frame, &[]),
            Err(NodeClientError::UnsupportedCapability(required))
                if required == NODE_STANDALONE_WORKSPACE_LIFECYCLE_CAPABILITY
        ));
        assert!(ensure_server_frame_required_capability(&frame, &[capability]).is_ok());
        assert!(server_frame_contains_opaque_unix_path(&frame));
    }

    #[test]
    fn spawn_profile_revision_capability_is_advertised_and_gates_all_spawn_spec_paths() {
        let offer = client_compatibility_offer().unwrap();
        let profile_revision =
            CapabilityId::new(NODE_SPAWN_PROFILE_REVISION_CAPABILITY).unwrap();
        assert!(offer.capabilities.contains(&profile_revision));

        let NodeRequest::SpawnSpec { spec } = spawn_spec_request() else {
            unreachable!("spawn spec helper changed variant");
        };
        let reservation_id = gate4agent_node_protocol::HarnessMcpReservationId::new(
            format!("hmcpres_{}", "a".repeat(24)),
        ).unwrap();
        let activation_digest = gate4agent_node_protocol::HarnessMcpActivationDigest::new(
            format!("sha256:{}", "b".repeat(64)),
        ).unwrap();
        let requests = [
            NodeRequest::SpawnSpec { spec: spec.clone() },
            NodeRequest::SpawnManagedWorktree {
                request: ManagedWorktreeSpawnRequest {
                    spawn_spec: spec.clone(),
                    worktree_profile_id: WorktreeProfileId::new("review").unwrap(),
                },
            },
            NodeRequest::ArmHarnessMcpReservation {
                reservation_id: reservation_id.clone(),
                activation_digest: activation_digest.clone(),
                spawn_spec: spec.clone(),
                expires_at_unix_ms: 10_000,
            },
            NodeRequest::SpawnSpecWithHarnessMcp {
                reservation_id,
                activation_digest,
                spec,
                deadline_unix_ms: 10_000,
            },
        ];
        let mut capabilities = vec![
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap(),
            CapabilityId::new(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY).unwrap(),
            CapabilityId::new(NODE_HARNESS_MCP_READ_PROXY_CAPABILITY).unwrap(),
        ];
        for request in &requests {
            assert!(matches!(
                ensure_node_request_required_capability(request, &capabilities),
                Err(NodeClientError::UnsupportedCapability(capability))
                    if capability == NODE_SPAWN_PROFILE_REVISION_CAPABILITY
            ));
        }
        capabilities.push(profile_revision.clone());
        for request in &requests {
            let result = ensure_node_request_required_capability(request, &capabilities);
            assert!(!matches!(
                result,
                Err(NodeClientError::UnsupportedCapability(capability))
                    if capability == NODE_SPAWN_PROFILE_REVISION_CAPABILITY
            ));
        }

        let frame = response_frame(NodeResponse::SpawnSpecAccepted {
            receipt: spawn_spec_receipt(),
        });
        capabilities.retain(|capability| capability != &profile_revision);
        assert!(matches!(
            ensure_server_frame_required_capability(&frame, &capabilities),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_SPAWN_PROFILE_REVISION_CAPABILITY
        ));
        capabilities.push(profile_revision);
        assert!(ensure_server_frame_required_capability(&frame, &capabilities).is_ok());
    }

    #[test]
    fn unnegotiated_spawn_spec_request_and_receipt_fail_closed() {
        let request = spawn_spec_request();
        let mut next_request_id = 73;
        let error = reserve_request_id(
            &mut next_request_id,
            &request,
            false,
            false,
            false,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            NodeClientError::UnsupportedCapability(capability)
                if capability == NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
        ));
        assert_eq!(next_request_id, 73);

        let capabilities = vec![
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap(),
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap(),
        ];
        assert_eq!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &capabilities,
            )
            .unwrap(),
            73,
        );
        assert_eq!(next_request_id, 74);

        let frame = response_frame(NodeResponse::SpawnSpecAccepted {
            receipt: spawn_spec_receipt(),
        });
        assert!(matches!(
            ensure_server_frame_required_capability(&frame, &[]),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
        ));
        assert!(ensure_server_frame_required_capability(&frame, &capabilities).is_ok());

        let mut worktree_request = spawn_spec_request();
        let NodeRequest::SpawnSpec { spec } = &mut worktree_request else {
            unreachable!("spawn spec helper changed variant");
        };
        spec.target.worktree_id = Some(WorkspaceId::new("review-tree").unwrap());
        let mut worktree_request_id = 81;
        assert!(matches!(
            reserve_request_id(
                &mut worktree_request_id,
                &worktree_request,
                false,
                false,
                false,
                &capabilities,
            ),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_WORKTREE_SELECTION_CAPABILITY
        ));
        assert_eq!(worktree_request_id, 81);
        let mut worktree_capabilities = capabilities.clone();
        worktree_capabilities.push(
            CapabilityId::new(NODE_WORKTREE_SELECTION_CAPABILITY).unwrap(),
        );
        assert_eq!(
            reserve_request_id(
                &mut worktree_request_id,
                &worktree_request,
                false,
                false,
                false,
                &worktree_capabilities,
            )
            .unwrap(),
            81,
        );

        let mut worktree_receipt = spawn_spec_receipt();
        worktree_receipt.target.worktree_id =
            Some(WorkspaceId::new("review-tree").unwrap());
        let worktree_frame = response_frame(NodeResponse::SpawnSpecAccepted {
            receipt: worktree_receipt,
        });
        assert!(matches!(
            ensure_server_frame_required_capability(
                &worktree_frame,
                &capabilities,
            ),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_WORKTREE_SELECTION_CAPABILITY
        ));
        assert!(ensure_server_frame_required_capability(
            &worktree_frame,
            &worktree_capabilities,
        )
        .is_ok());
    }

    #[test]
    fn child_environment_profile_is_gated_before_write_and_on_recursive_read() {
        let mut request = spawn_spec_request();
        let NodeRequest::SpawnSpec { spec } = &mut request else {
            unreachable!("spawn spec helper changed variant");
        };
        spec.overrides.environment_profile_id =
            gate4agent_node_protocol::SpawnOverride::Set {
                value: SpawnEnvironmentProfileId::new("local-default").unwrap(),
            };
        let spawn_capability =
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap();
        let bundle_capability =
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap();
        let mut next_request_id = 91;
        assert!(matches!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &[spawn_capability.clone(), bundle_capability.clone()],
            ),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY
        ));
        assert_eq!(next_request_id, 91);

        let mut cleared = spawn_spec_request();
        let NodeRequest::SpawnSpec { spec } = &mut cleared else {
            unreachable!("spawn spec helper changed variant");
        };
        spec.overrides.environment_profile_id =
            gate4agent_node_protocol::SpawnOverride::Clear;
        let mut clear_request_id = 101;
        assert_eq!(
            reserve_request_id(
                &mut clear_request_id,
                &cleared,
                false,
                false,
                false,
                &[spawn_capability.clone(), bundle_capability.clone()],
            )
            .unwrap(),
            101,
        );

        let environment_capability =
            CapabilityId::new(NODE_CHILD_ENVIRONMENT_PROFILE_CAPABILITY).unwrap();
        assert_eq!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &[
                    spawn_capability.clone(),
                    environment_capability.clone(),
                    bundle_capability,
                ],
            )
            .unwrap(),
            91,
        );

        let mut receipt = spawn_spec_receipt();
        receipt.environment_profile = Some(ResolvedEnvironmentProfileReceipt {
            profile_id: SpawnEnvironmentProfileId::new("local-default").unwrap(),
            profile_revision: SpawnEnvironmentProfileRevision::new("local-default.r1")
                .unwrap(),
        });
        let frame = response_frame(NodeResponse::SpawnSpecAccepted { receipt });
        assert!(matches!(
            ensure_server_frame_required_capability(&frame, &[spawn_capability.clone()]),
            Err(NodeClientError::Protocol(ref message))
                if message.contains("child environment profile metadata")
        ));
        assert!(ensure_server_frame_required_capability(
            &frame,
            &[spawn_capability, environment_capability],
        )
        .is_ok());
    }

    #[test]
    fn session_bundle_materialization_is_gated_before_write_and_on_recursive_read() {
        let spawn_capability =
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap();
        let bundle_capability =
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap();
        let request = spawn_spec_request();
        let mut next_request_id = 111;
        assert!(matches!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &[spawn_capability.clone()],
            ),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY
        ));
        assert_eq!(next_request_id, 111);
        assert_eq!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &[spawn_capability.clone(), bundle_capability.clone()],
            )
            .unwrap(),
            111,
        );

        let mut cleared = spawn_spec_request();
        let NodeRequest::SpawnSpec { spec } = &mut cleared else {
            unreachable!("spawn spec helper changed variant");
        };
        spec.overrides.bundle_id = gate4agent_node_protocol::SpawnOverride::Clear;
        let mut clear_request_id = 121;
        assert_eq!(
            reserve_request_id(
                &mut clear_request_id,
                &cleared,
                false,
                false,
                false,
                &[spawn_capability.clone()],
            )
            .unwrap(),
            121,
        );

        let mut receipt = spawn_spec_receipt();
        let bundle_id = SpawnBundleId::new("review-bundle").unwrap();
        receipt.bundle_id = Some(bundle_id.clone());
        receipt.bundle = Some(ResolvedBundleReceipt {
            id: bundle_id,
            revision: SpawnBundleRevision::new("review-bundle.r1").unwrap(),
            digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
        });
        let frame = response_frame(NodeResponse::SpawnSpecAccepted { receipt });
        assert!(matches!(
            ensure_server_frame_required_capability(&frame, &[spawn_capability.clone()]),
            Err(NodeClientError::Protocol(ref message))
                if message.contains("session bundle materialization metadata")
        ));
        assert!(ensure_server_frame_required_capability(
            &frame,
            &[spawn_capability, bundle_capability],
        )
        .is_ok());
    }

    #[test]
    fn history_context_pack_requests_and_responses_fail_closed() {
        let session = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(2),
            },
        };
        let context_id = SpawnContextId::new("context-a").unwrap();
        let requests = [
            NodeRequest::DiscoverHistory {
                session: session.clone(),
                limit: 8,
            },
            NodeRequest::LoadHistory {
                session: session.clone(),
                candidate_id: "candidate-a".to_owned(),
            },
            NodeRequest::ExportContextPack {
                session: session.clone(),
            },
            NodeRequest::ForgetContextPack {
                context_id: context_id.clone(),
            },
        ];
        let capability = CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap();
        for request in requests {
            assert!(matches!(
                ensure_node_request_required_capability(&request, &[]),
                Err(NodeClientError::UnsupportedCapability(ref required))
                    if required == NODE_HISTORY_CONTEXT_PACK_CAPABILITY
            ));
            assert!(ensure_node_request_required_capability(
                &request,
                std::slice::from_ref(&capability),
            )
            .is_ok());
        }

        let mut next_request_id = 41;
        let invalid = NodeRequest::DiscoverHistory {
            session: session.clone(),
            limit: 0,
        };
        assert!(matches!(
            reserve_request_id(
                &mut next_request_id,
                &invalid,
                false,
                false,
                true,
                std::slice::from_ref(&capability),
            ),
            Err(NodeClientError::Protocol(ref message))
                if message == "invalid history context pack request"
        ));
        assert_eq!(next_request_id, 41);

        let forget = NodeRequest::ForgetContextPack {
            context_id: context_id.clone(),
        };
        assert!(matches!(
            reserve_request_id(
                &mut next_request_id,
                &forget,
                false,
                false,
                false,
                std::slice::from_ref(&capability),
            ),
            Err(NodeClientError::UnsupportedCapability(ref required))
                if required == NODE_PROVIDER_ID_OPEN_CAPABILITY
        ));
        assert_eq!(next_request_id, 41);
        assert_eq!(
            reserve_request_id(
                &mut next_request_id,
                &forget,
                false,
                false,
                true,
                std::slice::from_ref(&capability),
            )
            .unwrap(),
            41,
        );

        let responses = [
            NodeResponse::HistoryDiscovered {
                session: session.clone(),
                candidates: Vec::new(),
            },
            NodeResponse::HistoryLoaded {
                session,
                session_id: "provider-session-a".to_owned(),
                message_count: 4,
                completed_turn_count: None,
            },
            NodeResponse::ContextPackExported {
                context: context_pack_receipt(),
            },
            NodeResponse::ContextPackForgotten { context_id },
        ];
        for response in responses {
            let frame = response_frame(response);
            assert!(matches!(
                ensure_server_frame_required_capability(&frame, &[]),
                Err(NodeClientError::Protocol(ref message))
                    if message.contains("history context pack metadata")
            ));
            assert!(ensure_server_frame_required_capability(
                &frame,
                std::slice::from_ref(&capability),
            )
            .is_ok());
        }

        for code in [
            NodeFailureCode::UnknownContextPack,
            NodeFailureCode::ContextPackBusy,
            NodeFailureCode::ContextPackMaterializationFailed,
        ] {
            let frame = ServerFrame::Reply(ResponseEnvelope {
                request_id: 1,
                result: Err(NodeFailure {
                    code,
                    message: "typed F7 failure".to_owned(),
                }),
            });
            assert!(matches!(
                ensure_server_frame_required_capability(&frame, &[]),
                Err(NodeClientError::Protocol(ref message))
                    if message.contains("history context pack metadata")
            ));
            assert!(ensure_server_frame_required_capability(
                &frame,
                std::slice::from_ref(&capability),
            )
            .is_ok());
        }
    }

    #[test]
    fn history_context_pack_nested_metadata_fails_closed() {
        let history_capability =
            CapabilityId::new(NODE_HISTORY_CONTEXT_PACK_CAPABILITY).unwrap();
        let spawn_capability =
            CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap();
        let bundle_capability =
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap();
        let mut request = spawn_spec_request();
        let NodeRequest::SpawnSpec { spec } = &mut request else {
            unreachable!("spawn spec helper changed variant");
        };
        spec.overrides.context_id = gate4agent_node_protocol::SpawnOverride::Set {
            value: SpawnContextId::new("context-a").unwrap(),
        };
        assert!(matches!(
            ensure_node_request_required_capability(
                &request,
                &[spawn_capability.clone(), bundle_capability.clone()],
            ),
            Err(NodeClientError::UnsupportedCapability(ref required))
                if required == NODE_HISTORY_CONTEXT_PACK_CAPABILITY
        ));
        assert!(ensure_node_request_required_capability(
            &request,
            &[
                spawn_capability.clone(),
                bundle_capability,
                history_capability.clone(),
            ],
        )
        .is_ok());

        let context = context_pack_receipt();
        let mut receipt = spawn_spec_receipt();
        receipt.context_id = Some(context.id.clone());
        receipt.context = Some(context.clone());
        let receipt_frame = response_frame(NodeResponse::SpawnSpecAccepted { receipt });
        assert!(matches!(
            ensure_server_frame_required_capability(
                &receipt_frame,
                std::slice::from_ref(&spawn_capability),
            ),
            Err(NodeClientError::Protocol(ref message))
                if message.contains("history context pack metadata")
        ));
        assert!(ensure_server_frame_required_capability(
            &receipt_frame,
            &[spawn_capability, history_capability.clone()],
        )
        .is_ok());

        let mut record = session_record_with_path(utf8_path());
        record.context_id = Some(context.id.clone());
        record.context = Some(context);
        let event_frame = ServerFrame::Event(NodeEventEnvelope {
            sequence: 1,
            event: NodeEvent::SessionRecordUpserted {
                record: record.clone(),
            },
        });
        assert!(ensure_server_frame_required_capability(&event_frame, &[]).is_err());
        assert!(ensure_server_frame_required_capability(
            &event_frame,
            std::slice::from_ref(&history_capability),
        )
        .is_ok());

        let mut snapshot = empty_snapshot();
        snapshot.session_records.push(record);
        let hello = hello_with_snapshot(snapshot);
        assert!(ensure_node_hello_history_context_pack_capability(&hello, &[]).is_err());
        assert!(ensure_node_hello_history_context_pack_capability(
            &hello,
            std::slice::from_ref(&history_capability),
        )
        .is_ok());
    }

    #[test]
    fn managed_worktree_partial_capability_intersections_fail_closed() {
        let NodeRequest::SpawnSpec { spec } = spawn_spec_request() else {
            unreachable!("spawn spec helper changed variant");
        };
        let request = NodeRequest::SpawnManagedWorktree {
            request: ManagedWorktreeSpawnRequest {
                spawn_spec: spec,
                worktree_profile_id: WorktreeProfileId::new("review").unwrap(),
            },
        };
        let managed = CapabilityId::new(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY).unwrap();
        let spawn = CapabilityId::new(NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY).unwrap();
        let worktree = CapabilityId::new(NODE_WORKTREE_SELECTION_CAPABILITY).unwrap();
        let bundle =
            CapabilityId::new(NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY).unwrap();
        let mut next_request_id = 91;
        assert!(matches!(
            reserve_request_id(&mut next_request_id, &request, false, false, false, &[]),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY
        ));
        assert!(matches!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &[managed.clone()],
            ),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
        ));
        assert!(matches!(
            reserve_request_id(
                &mut next_request_id,
                &request,
                false,
                false,
                false,
                &[managed.clone(), spawn.clone()],
            ),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_WORKTREE_SELECTION_CAPABILITY
        ));
        assert!(matches!(reserve_request_id(
            &mut next_request_id,
            &request,
            false,
            false,
            false,
            &[managed.clone(), spawn.clone(), worktree.clone()],
        ), Err(NodeClientError::UnsupportedCapability(capability))
            if capability == NODE_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY));
        assert!(reserve_request_id(
            &mut next_request_id,
            &request,
            false,
            false,
            false,
            &[managed.clone(), spawn.clone(), worktree.clone(), bundle],
        )
        .is_ok());

        let cleanup = NodeRequest::CleanupManagedWorktree {
            lease_id: ManagedWorktreeLeaseId::new("lease-a").unwrap(),
        };
        assert!(ensure_node_request_required_capability(&cleanup, &[managed.clone()]).is_err());
        assert!(ensure_node_request_required_capability(
            &cleanup,
            &[managed.clone(), worktree.clone()],
        )
        .is_ok());

        let event = ServerFrame::Event(NodeEventEnvelope {
            sequence: 4,
            event: NodeEvent::ManagedWorktreeRemoved {
                lease_id: ManagedWorktreeLeaseId::new("lease-a").unwrap(),
            },
        });
        assert!(ensure_server_frame_required_capability(&event, &[managed.clone()]).is_err());
        assert!(ensure_server_frame_required_capability(&event, &[worktree.clone()]).is_err());
        assert!(ensure_server_frame_required_capability(
            &event,
            &[managed.clone(), worktree.clone()],
        )
        .is_ok());

        let mut spawn_receipt = spawn_spec_receipt();
        let workspace_id = WorkspaceId::new("managed-a").unwrap();
        spawn_receipt.target.worktree_id = Some(workspace_id.clone());
        spawn_receipt.session.workspace_id = workspace_id.clone();
        let receipt = ManagedWorktreeSpawnReceipt {
            spawn: spawn_receipt,
            lease: ManagedWorktreeLeaseSnapshot {
                lease_id: ManagedWorktreeLeaseId::new("lease-a").unwrap(),
                source_workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                workspace_id,
                profile_id: WorktreeProfileId::new("review").unwrap(),
                profile_revision: WorktreeProfileRevision::new("review.r1").unwrap(),
                retention: ManagedWorktreeRetention::RemoveWhenReleased,
                state: ManagedWorktreeLeaseState::InUse,
                active_session_count: 1,
                managed_record_count: 1,
                cleanup_failure: None::<ManagedWorktreeCleanupFailure>,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 2,
            },
        };
        let reply = response_frame(NodeResponse::ManagedWorktreeSpawnAccepted { receipt });
        assert!(ensure_server_frame_required_capability(
            &reply,
            &[managed.clone(), worktree.clone()],
        )
        .is_err());
        assert!(ensure_server_frame_required_capability(
            &reply,
            &[managed, worktree, spawn],
        )
        .is_ok());
    }

    #[test]
    fn worktree_service_mode_metadata_requires_managed_worktree_capabilities() {
        let managed = CapabilityId::new(NODE_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY).unwrap();
        let worktree = CapabilityId::new(NODE_WORKTREE_SELECTION_CAPABILITY).unwrap();
        let mut workspace = workspace_with_path(utf8_path());
        workspace.worktree_service_mode = Some(WorktreeServiceMode::Manual);
        let reply = response_frame(NodeResponse::WorkspaceRegistered {
            workspace: workspace.clone(),
        });

        assert!(ensure_server_frame_required_capability(&reply, &[]).is_err());
        assert!(ensure_server_frame_required_capability(
            &reply,
            std::slice::from_ref(&managed),
        )
        .is_err());
        assert!(ensure_server_frame_required_capability(
            &reply,
            std::slice::from_ref(&worktree),
        )
        .is_err());
        assert!(ensure_server_frame_required_capability(
            &reply,
            &[managed.clone(), worktree.clone()],
        )
        .is_ok());

        let event = ServerFrame::Event(NodeEventEnvelope {
            sequence: 5,
            event: NodeEvent::WorkspaceAdded { workspace },
        });
        assert!(ensure_server_frame_required_capability(&event, &[]).is_err());
        assert!(ensure_server_frame_required_capability(
            &event,
            &[managed.clone(), worktree.clone()],
        )
        .is_ok());

        let mut snapshot = empty_snapshot();
        let mut profile_only = workspace_with_path(utf8_path());
        profile_only.managed_worktree_profiles = Some(
            gate4agent_node_protocol::WorktreeProfileInventory {
                profiles: Vec::new(),
            },
        );
        snapshot.workspaces.push(profile_only);
        let hello = ServerFrame::Hello(hello_with_snapshot(snapshot));
        assert!(ensure_server_frame_required_capability(&hello, &[]).is_err());
        assert!(ensure_server_frame_required_capability(
            &hello,
            &[managed.clone(), worktree.clone()],
        )
        .is_ok());

        let legacy = response_frame(NodeResponse::WorkspaceRegistered {
            workspace: workspace_with_path(utf8_path()),
        });
        assert!(ensure_server_frame_required_capability(&legacy, &[]).is_ok());
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
                managed_worktree: None,
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
                oldest_available_sequence: 1,
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
                    managed_worktree: None,
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
                    managed_worktree: None,
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
                    managed_worktree: None,
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
                revision: None,
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
                managed_worktree: None,
                truncated: false,
                diagnostic: None,
            },
        };
        let frame = response_frame(NodeResponse::WorkspaceInspected { inspection });

        assert!(ensure_server_frame_path_capability(&frame, false, false).is_ok());
    }

    #[test]
    fn delivery_wire_capability_and_exact_reply_validation() {
        use gate4agent_node_protocol::{
            DeliveryBlobChunkHexV1, DeliveryBlobDigestV1, DeliveryManifestDigestV2,
            DeliveryStageId,
        };

        let stage_id = DeliveryStageId::new(format!(
            "delivery-stage-{}",
            "1".repeat(32),
        ))
        .unwrap();
        let digest = DeliveryBlobDigestV1::new(format!("sha256:{}", "a".repeat(64)))
            .unwrap();
        let request = NodeRequest::PutDeliveryBlobChunk {
            stage_id: stage_id.clone(),
            blob_digest: digest.clone(),
            offset: 7,
            chunk_hex: DeliveryBlobChunkHexV1::new("00ff").unwrap(),
        };
        let mut next_request_id = 1;
        assert!(matches!(
            reserve_request_id(&mut next_request_id, &request, false, false, false, &[]),
            Err(NodeClientError::UnsupportedCapability(capability))
                if capability == NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY
        ));
        let capability = CapabilityId::new(
            NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY,
        )
        .unwrap();
        assert!(reserve_request_id(
            &mut next_request_id,
            &request,
            false,
            false,
            false,
            std::slice::from_ref(&capability),
        )
        .is_ok());

        let exact = Ok(NodeResponse::DeliveryBlobChunkAccepted {
            stage_id: stage_id.clone(),
            blob_digest: digest.clone(),
            next_offset: 9,
        });
        assert!(validate_delivery_response(&request, &exact).is_ok());
        let wrong_offset = Ok(NodeResponse::DeliveryBlobChunkAccepted {
            stage_id: stage_id.clone(),
            blob_digest: digest.clone(),
            next_offset: 8,
        });
        assert!(validate_delivery_response(&request, &wrong_offset).is_err());
        let unexpected = Ok(NodeResponse::DeliveryStageBegun {
            stage_id,
            manifest_digest: DeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "b".repeat(64),
            ))
            .unwrap(),
            missing_blobs: vec![digest],
        });
        assert!(validate_delivery_response(&request, &unexpected).is_err());
        assert!(ensure_server_frame_required_capability(
            &response_frame(unexpected.unwrap()),
            &[],
        )
        .is_err());
    }

}
