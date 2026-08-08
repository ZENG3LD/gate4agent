#![cfg(unix)]

use gate4agent_node_protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, ArchitectureId,
    CapabilityId, ClientFrame, ClientHello, ClientRole, HostDescriptor, LocalTransportKind,
    NegotiatedNodeCompatibility, NodeCompatibilitySupport, NodeHello, NodeId, NodeIncarnationId,
    NodeRequest, NodeResponse, NodeSnapshot, OperatingSystemId, PathEncoding, PathSemantics,
    PathStyle, ProtocolRange, ResponseEnvelope, ServerChallenge, ServerFrame, StateSchemaSupport,
    MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    NODE_AUTH_NONCE_BYTES, NODE_COMPATIBILITY_METADATA_CAPABILITY, NODE_PROTOCOL_VERSION,
    NODE_STATE_SCHEMA_V2,
};
use gate4agent_node_wire::{
    negotiated_auth_proof, proofs_match, AuthDirection, LocalNodeClient, LocalServerStream,
    OwnerOnlyLocalListener,
};
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::timeout;

const ACCESS_TOKEN: &str = "unix-e2e-access-token";
const WRONG_ACCESS_TOKEN: &str = "unix-e2e-wrong-token";
const TEST_TIMEOUT: Duration = Duration::from_secs(2);
const FRAME_BODY_TIMEOUT: Duration = Duration::from_secs(1);
const NEGATIVE_OBSERVATION_TIMEOUT: Duration = Duration::from_millis(250);

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct PrivateSocketDir {
    path: PathBuf,
    endpoint: PathBuf,
    endpoint_lock: PathBuf,
}

impl PrivateSocketDir {
    fn new() -> Self {
        for _ in 0..100 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "gate4agent-node-wire-{}-{id}",
                std::process::id()
            ));
            let mut builder = DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    if fs::set_permissions(&path, Permissions::from_mode(0o700)).is_err() {
                        let _ = fs::remove_dir(&path);
                        panic!("secure test directory permissions could not be set");
                    }
                    let permissions = match fs::metadata(&path) {
                        Ok(metadata) => metadata.permissions().mode() & 0o777,
                        Err(_) => {
                            let _ = fs::remove_dir(&path);
                            panic!("secure test directory metadata could not be read");
                        }
                    };
                    assert_eq!(permissions, 0o700, "test directory must be private");
                    let endpoint = path.join("node.sock");
                    let endpoint_lock = path.join(".node.sock.gate4agent.lock");
                    return Self {
                        path,
                        endpoint,
                        endpoint_lock,
                    };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => panic!("secure test directory could not be created"),
            }
        }
        panic!("unique secure test directory could not be allocated");
    }

    fn endpoint(&self) -> &Path {
        &self.endpoint
    }
}

impl Drop for PrivateSocketDir {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.endpoint);
        let _ = fs::remove_file(&self.endpoint_lock);
        let _ = fs::remove_dir(&self.path);
    }
}

fn node_id() -> NodeId {
    NodeId::new("unix-wire-node").expect("fixed node identity is valid")
}

fn snapshot() -> NodeSnapshot {
    NodeSnapshot {
        node_id: node_id(),
        enabled_providers: Vec::new(),
        workspaces: Vec::new(),
        session_records: Vec::new(),
    }
}

fn negotiated_compatibility(hello: &ClientHello) -> NegotiatedNodeCompatibility {
    let offer = hello
        .compatibility
        .as_ref()
        .expect("production client must negotiate compatibility");
    let exact_protocol = ProtocolRange::exact(NODE_PROTOCOL_VERSION)
        .expect("active protocol range is valid");
    NodeCompatibilitySupport {
        protocol_versions: exact_protocol,
        capabilities: vec![
            CapabilityId::new(NODE_COMPATIBILITY_METADATA_CAPABILITY)
                .expect("fixed capability is valid"),
        ],
        host: HostDescriptor {
            operating_system: OperatingSystemId::new(std::env::consts::OS)
                .expect("fixed operating system is valid"),
            architecture: ArchitectureId::new(std::env::consts::ARCH)
                .expect("fixed architecture is valid"),
        },
        path_semantics: PathSemantics {
            style: PathStyle::Posix,
            encoding: PathEncoding::Utf8,
        },
        local_transport: LocalTransportKind::UnixDomainSocket,
        state_schema: StateSchemaSupport {
            versions: ProtocolRange::exact(NODE_STATE_SCHEMA_V2)
                .expect("active state schema range is valid"),
        },
        provider_contracts: Vec::new(),
        provider_adapter_contracts: Vec::new(),
    }
    .negotiate(NODE_PROTOCOL_VERSION, offer)
    .expect("fake node compatibility must match the production client offer")
}

async fn read_client_frame(stream: &mut LocalServerStream) -> ClientFrame {
    match timeout(
        TEST_TIMEOUT,
        read_json_frame_limited_body_timeout::<_, ClientFrame>(
            stream,
            MAX_NODE_CLIENT_FRAME_BYTES,
            FRAME_BODY_TIMEOUT,
        ),
    )
    .await
    {
        Ok(Ok(frame)) => frame,
        _ => panic!("fake node did not receive the expected client frame"),
    }
}

async fn write_server_frame(stream: &mut LocalServerStream, frame: &ServerFrame, limit: usize) {
    match timeout(TEST_TIMEOUT, write_json_frame_limited(stream, frame, limit)).await {
        Ok(Ok(())) => {}
        _ => panic!("fake node could not write the expected server frame"),
    }
}

async fn challenge_client(
    stream: &mut LocalServerStream,
    valid_server_proof: bool,
) -> (ClientHello, NegotiatedNodeCompatibility, [u8; NODE_AUTH_NONCE_BYTES]) {
    let ClientFrame::Hello(hello) = read_client_frame(stream).await else {
        panic!("first client frame must be hello");
    };
    assert_eq!(hello.protocol_version, NODE_PROTOCOL_VERSION);
    assert_eq!(hello.role, ClientRole::Observer);

    let selected = negotiated_compatibility(&hello);
    let server_nonce = [0x5a; NODE_AUTH_NONCE_BYTES];
    let mut server_proof = negotiated_auth_proof(
        ACCESS_TOKEN.as_bytes(),
        AuthDirection::Server,
        hello.role,
        &hello.client_nonce,
        &server_nonce,
        hello
            .compatibility
            .as_ref()
            .expect("negotiated offer must remain present"),
        &selected,
    )
    .expect("fake node proof generation must succeed");
    if !valid_server_proof {
        server_proof[0] ^= 1;
    }
    write_server_frame(
        stream,
        &ServerFrame::Challenge(ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce,
            server_proof,
            compatibility: Some(selected.clone()),
        }),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await;
    (hello, selected, server_nonce)
}

async fn accept_client(listener: &mut OwnerOnlyLocalListener) -> LocalServerStream {
    match timeout(TEST_TIMEOUT, listener.accept()).await {
        Ok(Ok(stream)) => stream,
        _ => panic!("fake node did not accept the local client"),
    }
}

async fn bind_listener(endpoint: &Path) -> OwnerOnlyLocalListener {
    match timeout(TEST_TIMEOUT, OwnerOnlyLocalListener::bind(endpoint)).await {
        Ok(Ok(listener)) => listener,
        _ => panic!("fake node could not bind its private local socket"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn local_node_client_authenticates_and_correlates_snapshot_over_real_uds() {
    let socket_dir = PrivateSocketDir::new();
    let mut listener = bind_listener(socket_dir.endpoint()).await;
    let expected_snapshot = snapshot();
    let server_snapshot = expected_snapshot.clone();

    let server = tokio::spawn(async move {
        let mut stream = accept_client(&mut listener).await;
        let (hello, compatibility, server_nonce) = challenge_client(&mut stream, true).await;
        let ClientFrame::Authenticate(authentication) = read_client_frame(&mut stream).await else {
            panic!("second client frame must authenticate the challenge");
        };
        let expected_client_proof = negotiated_auth_proof(
            ACCESS_TOKEN.as_bytes(),
            AuthDirection::Client,
            hello.role,
            &hello.client_nonce,
            &server_nonce,
            hello
                .compatibility
                .as_ref()
                .expect("negotiated offer must remain present"),
            &compatibility,
        )
        .expect("fake node proof generation must succeed");
        assert!(proofs_match(
            &authentication.client_proof,
            &expected_client_proof
        ));

        write_server_frame(
            &mut stream,
            &ServerFrame::Hello(NodeHello {
                protocol_version: NODE_PROTOCOL_VERSION,
                incarnation_id: NodeIncarnationId::from_bytes([0x24; 16]),
                connection_id: 41,
                role: hello.role,
                event_sequence: 7,
                controller: None,
                snapshot: server_snapshot.clone(),
                compatibility: Some(compatibility),
            }),
            MAX_NODE_FRAME_BYTES,
        )
        .await;

        let ClientFrame::Request(request) = read_client_frame(&mut stream).await else {
            panic!("authenticated client must send a request");
        };
        assert_eq!(request.request, NodeRequest::Snapshot);
        write_server_frame(
            &mut stream,
            &ServerFrame::Reply(ResponseEnvelope {
                request_id: request.request_id,
                result: Ok(NodeResponse::Snapshot {
                    event_sequence: 7,
                    controller: None,
                    snapshot: server_snapshot,
                }),
            }),
            MAX_NODE_FRAME_BYTES,
        )
        .await;
        request.request_id
    });

    let connected = timeout(
        TEST_TIMEOUT,
        LocalNodeClient::connect(
            socket_dir.endpoint(),
            &node_id(),
            ClientRole::Observer,
            ACCESS_TOKEN,
        ),
    )
    .await;
    let mut client = match connected {
        Ok(Ok(client)) => client,
        _ => panic!("production local client did not complete UDS authentication"),
    };
    assert_eq!(client.hello().snapshot.node_id, node_id());
    assert_eq!(client.hello().snapshot, expected_snapshot);

    let response = match timeout(TEST_TIMEOUT, client.request(NodeRequest::Snapshot)).await {
        Ok(Ok(response)) => response,
        _ => panic!("production local client did not receive the snapshot response"),
    };
    assert_eq!(
        response,
        NodeResponse::Snapshot {
            event_sequence: 7,
            controller: None,
            snapshot: expected_snapshot,
        }
    );
    let request_id = match timeout(TEST_TIMEOUT, server).await {
        Ok(Ok(request_id)) => request_id,
        _ => panic!("fake node did not complete the authenticated exchange"),
    };
    assert_eq!(request_id, 1, "first request must retain its correlation ID");
}

async fn rejected_handshake_sends_no_request(valid_server_proof: bool, client_token: &'static str) {
    let socket_dir = PrivateSocketDir::new();
    let mut listener = bind_listener(socket_dir.endpoint()).await;
    let server = tokio::spawn(async move {
        let mut stream = accept_client(&mut listener).await;
        challenge_client(&mut stream, valid_server_proof).await;
        match timeout(
            NEGATIVE_OBSERVATION_TIMEOUT,
            read_json_frame_limited_body_timeout::<_, ClientFrame>(
                &mut stream,
                MAX_NODE_CLIENT_FRAME_BYTES,
                NEGATIVE_OBSERVATION_TIMEOUT,
            ),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(_)) | Err(_) => false,
        }
    });

    let connected = timeout(
        TEST_TIMEOUT,
        LocalNodeClient::connect(
            socket_dir.endpoint(),
            &node_id(),
            ClientRole::Observer,
            client_token,
        ),
    )
    .await;
    assert!(matches!(connected, Ok(Err(_))), "authentication must fail closed");
    let post_challenge_frame_observed = match timeout(TEST_TIMEOUT, server).await {
        Ok(Ok(observed)) => observed,
        _ => panic!("fake node did not finish the rejected handshake"),
    };
    assert!(
        !post_challenge_frame_observed,
        "rejected server authentication must send no subsequent client frame"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn local_node_client_rejects_wrong_server_proof_before_request() {
    rejected_handshake_sends_no_request(false, ACCESS_TOKEN).await;
}

#[tokio::test(flavor = "current_thread")]
async fn local_node_client_rejects_wrong_token_before_request() {
    rejected_handshake_sends_no_request(true, WRONG_ACCESS_TOKEN).await;
}
