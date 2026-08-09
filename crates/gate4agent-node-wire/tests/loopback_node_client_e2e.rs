use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, ArchitectureId,
    CapabilityId, ClientFrame, ClientRole, HostDescriptor, LocalTransportKind,
    NodeCompatibilitySupport, NodeHello, NodeId, NodeIncarnationId, NodeRequest, NodeResponse,
    NodeSnapshot, OperatingSystemId, PathEncoding, PathSemantics, PathStyle,
    ProtocolRange, ProviderRuntimeStatuses, ResponseEnvelope, ServerChallenge, ServerFrame,
    StateSchemaSupport, MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES,
    MAX_NODE_HELLO_FRAME_BYTES, NODE_COMPATIBILITY_METADATA_CAPABILITY,
    NODE_PROTOCOL_VERSION, NODE_STATE_SCHEMA_V2,
};
use gate4agent_node_wire::{
    negotiated_auth_proof, proofs_match, AuthDirection, LocalNodeClient, NodeClientError,
};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const ACCESS_TOKEN: &str = "loopback-node-token";
const TEST_TIMEOUT: Duration = Duration::from_secs(2);

fn node_id() -> NodeId {
    NodeId::new("loopback-wire-node").expect("fixed node identity is valid")
}

fn snapshot() -> NodeSnapshot {
    NodeSnapshot {
        node_id: node_id(),
        enabled_providers: Vec::new(),
        provider_runtime_statuses: ProviderRuntimeStatuses::default(),
        workspaces: Vec::new(),
        session_records: Vec::new(),
    }
}

async fn read_client_frame(stream: &mut TcpStream) -> ClientFrame {
    timeout(
        TEST_TIMEOUT,
        read_json_frame_limited_body_timeout(
            stream,
            MAX_NODE_CLIENT_FRAME_BYTES,
            TEST_TIMEOUT,
        ),
    )
    .await
    .expect("client frame deadline")
    .expect("valid client frame")
}

async fn write_server_frame(stream: &mut TcpStream, frame: &ServerFrame, limit: usize) {
    timeout(TEST_TIMEOUT, write_json_frame_limited(stream, frame, limit))
        .await
        .expect("server frame deadline")
        .expect("valid server frame");
}

#[tokio::test(flavor = "current_thread")]
async fn loopback_node_client_uses_exact_node_v8_handshake_and_snapshot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("loopback bind");
    let address = listener.local_addr().expect("loopback address");
    let expected_snapshot = snapshot();
    let server_snapshot = expected_snapshot.clone();

    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("loopback accept");
        assert!(peer.ip().is_loopback());
        let ClientFrame::Hello(hello) = read_client_frame(&mut stream).await else {
            panic!("first frame must be client hello");
        };
        assert_eq!(hello.protocol_version, NODE_PROTOCOL_VERSION);
        assert_eq!(hello.role, ClientRole::Operator);
        let offer = hello.compatibility.as_ref().expect("compatibility offer");
        let compatibility = NodeCompatibilitySupport {
            protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
            capabilities: vec![CapabilityId::new(NODE_COMPATIBILITY_METADATA_CAPABILITY).unwrap()],
            host: HostDescriptor {
                operating_system: OperatingSystemId::new(std::env::consts::OS).unwrap(),
                architecture: ArchitectureId::new(std::env::consts::ARCH).unwrap(),
            },
            path_semantics: PathSemantics { style: PathStyle::Posix, encoding: PathEncoding::Utf8 },
            local_transport: LocalTransportKind::UnixDomainSocket,
            state_schema: StateSchemaSupport {
                versions: ProtocolRange::exact(NODE_STATE_SCHEMA_V2).unwrap(),
            },
            provider_contracts: Vec::new(),
            provider_adapter_contracts: Vec::new(),
        }
        .negotiate(NODE_PROTOCOL_VERSION, offer)
        .expect("compatible Node v8 selection");
        let server_nonce = [0x42; 32];
        let server_proof = negotiated_auth_proof(
            ACCESS_TOKEN.as_bytes(),
            AuthDirection::Server,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
            offer,
            &compatibility,
        )
        .expect("server proof");
        write_server_frame(
            &mut stream,
            &ServerFrame::Challenge(ServerChallenge {
                protocol_version: NODE_PROTOCOL_VERSION,
                server_nonce,
                server_proof,
                compatibility: Some(compatibility.clone()),
            }),
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await;
        let ClientFrame::Authenticate(authentication) = read_client_frame(&mut stream).await else {
            panic!("second frame must authenticate");
        };
        let expected_client_proof = negotiated_auth_proof(
            ACCESS_TOKEN.as_bytes(),
            AuthDirection::Client,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
            offer,
            &compatibility,
        )
        .expect("client proof");
        assert!(proofs_match(&authentication.client_proof, &expected_client_proof));
        write_server_frame(
            &mut stream,
            &ServerFrame::Hello(NodeHello {
                protocol_version: NODE_PROTOCOL_VERSION,
                incarnation_id: NodeIncarnationId::from_bytes([0x24; 16]),
                connection_id: 9,
                role: hello.role,
                event_sequence: 3,
                controller: None,
                snapshot: server_snapshot.clone(),
                compatibility: Some(compatibility),
            }),
            MAX_NODE_FRAME_BYTES,
        )
        .await;
        let ClientFrame::Request(request) = read_client_frame(&mut stream).await else {
            panic!("authenticated client must request snapshot");
        };
        assert_eq!(request.request, NodeRequest::Snapshot);
        write_server_frame(
            &mut stream,
            &ServerFrame::Reply(ResponseEnvelope {
                request_id: request.request_id,
                result: Ok(NodeResponse::Snapshot {
                    event_sequence: 3,
                    controller: None,
                    snapshot: server_snapshot,
                }),
            }),
            MAX_NODE_FRAME_BYTES,
        )
        .await;
    });

    let mut client = timeout(
        TEST_TIMEOUT,
        LocalNodeClient::connect_loopback(
            address,
            &node_id(),
            ClientRole::Operator,
            ACCESS_TOKEN,
        ),
    )
    .await
    .expect("connect deadline")
    .expect("Node v8 loopback handshake");
    assert_eq!(client.hello().snapshot, expected_snapshot);
    let response = client.request(NodeRequest::Snapshot).await.expect("snapshot response");
    assert!(matches!(response, NodeResponse::Snapshot { event_sequence: 3, .. }));
    server.await.expect("mock node task");
}

#[tokio::test(flavor = "current_thread")]
async fn loopback_node_client_rejects_nonloopback_before_connect() {
    let endpoint = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9);
    let error = match LocalNodeClient::connect_loopback(
        endpoint,
        &node_id(),
        ClientRole::Operator,
        ACCESS_TOKEN,
    )
    .await {
        Ok(_) => panic!("non-loopback address must be rejected before I/O"),
        Err(error) => error,
    };
    assert!(matches!(error, NodeClientError::Io(error) if error.kind() == io::ErrorKind::InvalidInput));
}
