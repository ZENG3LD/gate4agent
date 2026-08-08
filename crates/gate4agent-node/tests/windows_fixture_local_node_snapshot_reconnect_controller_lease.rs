#![cfg(windows)]

use gate4agent_node::protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, AgentProvider, CapabilityId,
    ClientAuthentication, ClientCompatibilityOffer, ClientFrame, ClientHello, ClientRole,
    FrameError, LocalTransportKind, NodeEvent, NodeFailureCode, NodeId, NodeRequest, NodeResponse,
    NodeSnapshot, OpaqueHostPath, PathEncoding, PathStyle, ProtocolRange, ServerFrame,
    SessionAddress, SessionMode, WorkspaceId,
    MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES, MAX_NODE_TEXT_BYTES,
    NODE_COMPATIBILITY_METADATA_CAPABILITY, NODE_PROTOCOL_VERSION,
};
use gate4agent_node::{NodeServer, NodeServerConfig, NodeServerError, WorkspaceConfig};
use gate4agent_node_wire::{
    auth_proof, negotiated_auth_proof, proofs_match, random_nonce, AuthDirection,
    NamedPipeNodeClient, NodeClientError,
};
use gate4agent_types::{
    AdapterFamily, ControlEventKind, SessionGeneration, SessionStatus, TerminalControl,
    TerminalSize,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::process::{Child, Command};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, ServerOptions};
use tokio::time::{sleep, timeout, Duration};

fn endpoint() -> String {
    static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
    format!(
        r"\\.\pipe\gate4agent-node-e2e-{}-{nonce}-{sequence}",
        std::process::id()
    )
}

fn host_path(value: impl Into<String>) -> OpaqueHostPath {
    OpaqueHostPath::utf8(value.into()).unwrap()
}

fn expected_node_id() -> NodeId {
    NodeId::new("fixture-node").unwrap()
}

fn server_config(endpoint: &str, token: &str) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        NodeId::new("fixture-node").unwrap(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
}

fn two_workspace_server_config(endpoint: &str, token: &str) -> NodeServerConfig {
    let root = std::env::current_dir().unwrap();
    NodeServerConfig::new(
        endpoint,
        token,
        NodeId::new("fixture-node").unwrap(),
        [
            WorkspaceConfig::new(WorkspaceId::new("primary").unwrap(), &root).unwrap(),
            WorkspaceConfig::new(WorkspaceId::new("secondary").unwrap(), root.join("src"))
                .unwrap(),
        ],
    )
    .unwrap()
}

fn git_workspace_server_config(
    endpoint: &str,
    token: &str,
    root: &Path,
) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        NodeId::new("fixture-node").unwrap(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            root,
        )
        .unwrap()],
    )
    .unwrap()
}

fn all_sessions(snapshot: &NodeSnapshot) -> Vec<&gate4agent_types::SessionSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.sessions.iter())
        .collect()
}

fn addressed_session<'a>(
    snapshot: &'a NodeSnapshot,
    address: &SessionAddress,
) -> Option<&'a gate4agent_types::SessionSnapshot> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == address.workspace_id)?
        .sessions
        .iter()
        .find(|session| {
            session.instance_id == address.session.instance_id
                && session.generation == address.session.generation
        })
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct RemoveTestDirectoryOnDrop(PathBuf);

impl Drop for RemoveTestDirectoryOnDrop {
    fn drop(&mut self) {
        let safe_name = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-node-worktree-e2e-"));
        if safe_name {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn git_command(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_git_success(root: &Path, arguments: &[&str]) {
    let output = git_command(root, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

async fn raw_pipe_client(endpoint: &str) -> NamedPipeClient {
    for _ in 0..100 {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return client,
            Err(error)
                if matches!(error.kind(), std::io::ErrorKind::NotFound)
                    || error.raw_os_error() == Some(231) =>
            {
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("raw named-pipe connect failed: {error}"),
        }
    }
    panic!("raw named-pipe endpoint was not available");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_negotiating_client_receives_exact_v8_node_compatibility() {
    let endpoint = endpoint();
    let token = "fixture-compatibility-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());

    let client = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    let compatibility = client
        .hello()
        .compatibility
        .as_ref()
        .expect("new node omitted negotiated compatibility");
    assert_eq!(compatibility.protocol_version, NODE_PROTOCOL_VERSION);
    assert_eq!(
        compatibility
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>(),
        vec![
            "compatibility.metadata",
            "workspace-file-read-v1",
            "provider-contract-manifest-v1",
        ],
    );
    assert_eq!(compatibility.host.operating_system.as_str(), "windows");
    assert_eq!(compatibility.host.architecture.as_str(), std::env::consts::ARCH);
    assert_eq!(compatibility.path_semantics.style, PathStyle::Windows);
    assert_eq!(compatibility.path_semantics.encoding, PathEncoding::Utf8);
    assert_eq!(compatibility.local_transport, LocalTransportKind::WindowsNamedPipe);
    assert_eq!(compatibility.state_schema_version, Some(2));
    assert_eq!(compatibility.provider_contracts.len(), 1);
    assert_eq!(compatibility.provider_contracts[0].provider, AgentProvider::Claude);
    assert_eq!(compatibility.provider_contracts[0].revision.as_str(), "fixture-r1");
    assert!(compatibility.provider_adapter_contracts.is_empty());

    drop(client);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("compatibility node did not shut down")
        .expect("compatibility node task panicked")
        .expect("compatibility node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_production_named_pipe_negotiates_exact_provider_contract_manifest() {
    let endpoint = endpoint();
    let token = "production-provider-manifest-token";
    let server = NodeServer::new(server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());

    let client = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    let compatibility = client
        .hello()
        .compatibility
        .as_ref()
        .expect("production node omitted negotiated compatibility");
    assert_eq!(
        compatibility
            .provider_contracts
            .iter()
            .map(|contract| (contract.provider, contract.revision.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (AgentProvider::Claude, "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
            (AgentProvider::Codex, "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
            (AgentProvider::Kimi, "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42"),
        ]
    );
    assert_eq!(
        compatibility
            .provider_adapter_contracts
            .iter()
            .map(|contract| (
                contract.provider,
                contract.family,
                contract.adapter_id.as_str(),
                contract.revision.as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (AgentProvider::Claude, AdapterFamily::PtySemantic, "claude-code", "gate4agent-adapter/v1"),
            (AgentProvider::Claude, AdapterFamily::Pipe, "claude-code", "gate4agent-adapter/v1"),
            (AgentProvider::Claude, AdapterFamily::Hook, "claude-code", "gate4agent-adapter/v1"),
            (AgentProvider::Claude, AdapterFamily::ManagedHook, "claude", "gate4agent-managed-hooks/orca-d8629c4/v1"),
            (AgentProvider::Claude, AdapterFamily::OneShot, "claude", "gate4agent-inline/claude-code-2.1/v1"),
            (AgentProvider::Claude, AdapterFamily::History, "claude-code", "gate4agent-adapter/v1"),
            (AgentProvider::Claude, AdapterFamily::Resume, "claude-code", "gate4agent-adapter/v1"),
            (AgentProvider::Claude, AdapterFamily::SessionOptions, "claude-code", "gate4agent-session-options/orca-d8629c4/v1"),
            (AgentProvider::Codex, AdapterFamily::PtySemantic, "codex", "gate4agent-adapter/v1"),
            (AgentProvider::Codex, AdapterFamily::Pipe, "codex", "gate4agent-adapter/v1"),
            (AgentProvider::Codex, AdapterFamily::Hook, "codex", "gate4agent-adapter/v1"),
            (AgentProvider::Codex, AdapterFamily::ManagedHook, "codex", "gate4agent-managed-hooks/orca-d8629c4/v1"),
            (AgentProvider::Codex, AdapterFamily::OneShot, "codex", "gate4agent-inline/codex-cli-0.144/v1"),
            (AgentProvider::Codex, AdapterFamily::History, "codex", "gate4agent-adapter/v1"),
            (AgentProvider::Codex, AdapterFamily::Resume, "codex", "gate4agent-adapter/v1"),
            (AgentProvider::Codex, AdapterFamily::SessionOptions, "codex", "gate4agent-session-options/orca-d8629c4/v1"),
            (AgentProvider::Kimi, AdapterFamily::PtySemantic, "kimi", "gate4agent-adapter/v1"),
            (AgentProvider::Kimi, AdapterFamily::Pipe, "kimi", "gate4agent-adapter/v1"),
            (AgentProvider::Kimi, AdapterFamily::Hook, "kimi", "gate4agent-adapter/v1"),
            (AgentProvider::Kimi, AdapterFamily::ManagedHook, "kimi", "gate4agent-managed-hooks/orca-d8629c4/v1"),
            (AgentProvider::Kimi, AdapterFamily::OneShot, "kimi", "gate4agent-inline/kimi-code-0.31/v1"),
            (AgentProvider::Kimi, AdapterFamily::History, "kimi", "gate4agent-adapter/v1"),
            (AgentProvider::Kimi, AdapterFamily::Resume, "kimi", "gate4agent-adapter/v1"),
        ]
    );

    drop(client);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("production provider manifest node did not shut down")
        .expect("production provider manifest node task panicked")
        .expect("production provider manifest node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_raw_legacy_v8_hello_negotiates_without_optional_capabilities() {
    let endpoint = endpoint();
    let token = "fixture-legacy-compatibility-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut pipe = raw_pipe_client(&endpoint).await;
    let client_nonce = random_nonce().unwrap();

    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Hello(ClientHello::new(ClientRole::Observer, client_nonce)),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Challenge(challenge) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_HELLO_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("legacy client did not receive a challenge");
    };
    assert_eq!(challenge.protocol_version, NODE_PROTOCOL_VERSION);
    assert_eq!(challenge.compatibility, None);
    let expected_server_proof = auth_proof(
        token.as_bytes(),
        AuthDirection::Server,
        ClientRole::Observer,
        &client_nonce,
        &challenge.server_nonce,
    )
    .unwrap();
    assert!(proofs_match(&challenge.server_proof, &expected_server_proof));
    let client_proof = auth_proof(
        token.as_bytes(),
        AuthDirection::Client,
        ClientRole::Observer,
        &client_nonce,
        &challenge.server_nonce,
    )
    .unwrap();
    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Authenticate(ClientAuthentication { client_proof }),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Hello(hello) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("legacy client did not receive node hello");
    };
    assert_eq!(hello.protocol_version, NODE_PROTOCOL_VERSION);
    assert_eq!(hello.snapshot.node_id, expected_node_id());
    assert_eq!(hello.compatibility, None);

    drop(pipe);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("legacy compatibility node did not shut down")
        .expect("legacy compatibility node task panicked")
        .expect("legacy compatibility node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_negotiation_without_manifest_capability_returns_empty_manifests() {
    let endpoint = endpoint();
    let token = "fixture-no-manifest-capability-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut pipe = raw_pipe_client(&endpoint).await;
    let client_nonce = random_nonce().unwrap();
    let offer = ClientCompatibilityOffer {
        protocol_versions: ProtocolRange::exact(NODE_PROTOCOL_VERSION).unwrap(),
        capabilities: vec![CapabilityId::new(NODE_COMPATIBILITY_METADATA_CAPABILITY).unwrap()],
        state_schema: None,
    };

    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Hello(ClientHello::negotiating(
            ClientRole::Observer,
            client_nonce,
            offer.clone(),
        )),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Challenge(challenge) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_HELLO_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("client without manifest capability did not receive a challenge");
    };
    let selected = challenge
        .compatibility
        .as_ref()
        .expect("negotiating challenge omitted compatibility");
    assert!(selected.provider_contracts.is_empty());
    assert!(selected.provider_adapter_contracts.is_empty());
    let expected_server_proof = negotiated_auth_proof(
        token.as_bytes(),
        AuthDirection::Server,
        ClientRole::Observer,
        &client_nonce,
        &challenge.server_nonce,
        &offer,
        selected,
    )
    .unwrap();
    assert!(proofs_match(&challenge.server_proof, &expected_server_proof));
    let client_proof = negotiated_auth_proof(
        token.as_bytes(),
        AuthDirection::Client,
        ClientRole::Observer,
        &client_nonce,
        &challenge.server_nonce,
        &offer,
        selected,
    )
    .unwrap();
    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Authenticate(ClientAuthentication { client_proof }),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Hello(hello) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("client without manifest capability did not receive node hello");
    };
    let selected = hello
        .compatibility
        .as_ref()
        .expect("negotiating hello omitted compatibility");
    assert!(selected.provider_contracts.is_empty());
    assert!(selected.provider_adapter_contracts.is_empty());

    drop(pipe);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("no-manifest compatibility node did not shut down")
        .expect("no-manifest compatibility node task panicked")
        .expect("no-manifest compatibility node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_node_incarnation_is_stable_for_reconnect_and_changes_after_restart() {
    let endpoint = endpoint();
    let token = "fixture-incarnation-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());

    let first = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    let first_incarnation = first.hello().incarnation_id;
    assert_eq!(first.hello().event_sequence, 0);

    let second = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    assert_eq!(second.hello().incarnation_id, first_incarnation);
    assert_eq!(second.hello().event_sequence, first.hello().event_sequence);

    drop(first);
    drop(second);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let restarted = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let restarted_shutdown = restarted.shutdown_handle();
    let restarted_task = tokio::spawn(restarted.run());
    let reconnected = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    assert_ne!(reconnected.hello().incarnation_id, first_incarnation);
    assert_eq!(reconnected.hello().event_sequence, 0);

    drop(reconnected);
    restarted_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), restarted_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_extracted_named_pipe_client_preserves_auth_snapshot_events_and_controller_lease() {
    let endpoint = endpoint();
    let token = "fixture-control-token";
    let external = KillOnDrop(
        Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .spawn()
            .unwrap(),
    );
    let external_process_id = external.0.id();
    let server = NodeServer::new_fixture(two_workspace_server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());

    assert!(
        NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, "wrong-token")
            .await
            .is_err(),
        "client accepted a server proof made with another token"
    );
    let identity_error = match NamedPipeNodeClient::connect(
        &endpoint,
        &NodeId::new("another-node").unwrap(),
        ClientRole::Observer,
        token,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("checked connect accepted another node identity"),
    };
    assert!(matches!(identity_error, NodeClientError::Protocol(ref message) if message.contains("node identity mismatch")));

    let mut first = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token)
        .await
        .unwrap();
    assert_eq!(first.hello().protocol_version, NODE_PROTOCOL_VERSION);
    assert_eq!(first.hello().snapshot.node_id, NodeId::new("fixture-node").unwrap());
    assert_eq!(first.hello().snapshot.workspaces.len(), 2);
    assert!(all_sessions(&first.hello().snapshot).is_empty());
    assert!(all_sessions(&first.hello().snapshot)
        .iter()
        .all(|session| session.process_id != Some(external_process_id)));
    let NodeResponse::Controller { controller: Some(controller) } = first
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap()
    else {
        panic!("first operator did not acquire controller");
    };
    assert_eq!(controller.connection_id, first.hello().connection_id);
    let oversized_text = first
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: Some("x".repeat(MAX_NODE_TEXT_BYTES + 1)),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        oversized_text,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::InvalidRequest
    ));
    let unknown_workspace = first
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("unknown").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        unknown_workspace,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::UnknownWorkspace
    ));

    let mut second = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token)
        .await
        .unwrap();
    let error = second
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::ControllerBusy
    ));

    drop(first);
    let mut acquired_after_disconnect = false;
    for _ in 0..50 {
        match second
            .request(NodeRequest::AcquireController { lease_ms: 5_000 })
            .await
        {
            Ok(NodeResponse::Controller { controller: Some(controller) }) => {
                assert_eq!(controller.connection_id, second.hello().connection_id);
                acquired_after_disconnect = true;
                break;
            }
            Err(NodeClientError::Node(ref failure))
                if failure.code == NodeFailureCode::ControllerBusy =>
            {
                sleep(Duration::from_millis(20)).await;
            }
            other => panic!("unexpected controller retry result: {other:?}"),
        }
    }
    assert!(acquired_after_disconnect, "controller was not released on disconnect");

    let NodeResponse::Snapshot { snapshot, event_sequence, .. } = second
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert!(all_sessions(&snapshot).is_empty());
    assert!(event_sequence >= 3);

    second
        .request(NodeRequest::ReleaseController)
        .await
        .unwrap();
    drop(second);

    let mut reconnected = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token)
        .await
        .unwrap();
    assert!(reconnected.hello().controller.is_none());
    let NodeResponse::Resync { events, snapshot, .. } = reconnected
        .request(NodeRequest::Resync { after_sequence: 0 })
        .await
        .unwrap()
    else {
        panic!("resync request returned another response");
    };
    assert!(all_sessions(&snapshot).is_empty());
    assert!(events.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));
    assert!(events.len() >= 4);

    reconnected
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();
    let NodeResponse::SpawnAccepted { session } = reconnected
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("fixture spawn returned another response");
    };
    let mut physical_process_id = None;
    for _ in 0..250 {
        let NodeResponse::Snapshot { snapshot, .. } = reconnected
            .request(NodeRequest::Snapshot)
            .await
            .unwrap()
        else {
            panic!("snapshot request returned another response");
        };
        let fixture = addressed_session(&snapshot, &session);
        if let Some(fixture) = fixture {
            if fixture.status == SessionStatus::Running {
                physical_process_id = fixture.process_id;
                break;
            }
            if matches!(fixture.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. }) {
                panic!("fixture child stopped before shutdown: {:?}", fixture.status);
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    let physical_process_id = physical_process_id.expect("fixture PTY child did not reach running");
    assert!(reconnected
        .hello()
        .snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id.as_str() == "secondary")
        .unwrap()
        .sessions
        .is_empty());

    let mut burst_ids = Vec::new();
    for character in ["a", "b", "c"] {
        burst_ids.push(
            reconnected
                .send(NodeRequest::Input {
                    session: session.clone(),
                    text: character.to_owned(),
                })
                .await
                .unwrap(),
        );
    }
    let mut replies = Vec::new();
    let mut command_rejected = false;
    while replies.len() < burst_ids.len() {
        match reconnected.recv().await.unwrap() {
            ServerFrame::Reply(reply) => {
                assert!(burst_ids.contains(&reply.request_id));
                assert_eq!(reply.result.unwrap(), NodeResponse::Accepted);
                replies.push(reply.request_id);
            }
            ServerFrame::Event(event) => {
                if let NodeEvent::Control { address, event } = event.event {
                    if address == session
                        && matches!(event.event, ControlEventKind::CommandRejected { .. })
                    {
                        command_rejected = true;
                    }
                }
            }
            other => panic!("unexpected frame during input burst: {other:?}"),
        }
    }
    assert!(!command_rejected, "rapid abc input produced CommandRejected");
    assert_eq!(
        reconnected
            .request(NodeRequest::TerminalControl {
                session: session.clone(),
                control: TerminalControl::Enter,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    while let Some(event) = reconnected.take_event() {
        if let NodeEvent::Control { address, event } = event.event {
            assert!(
                address != session
                    || !matches!(event.event, ControlEventKind::CommandRejected { .. }),
                "rapid abc input produced a queued CommandRejected",
            );
        }
    }

    let resize_sizes = [
        TerminalSize { rows: 25, columns: 81 },
        TerminalSize { rows: 31, columns: 97 },
        TerminalSize { rows: 37, columns: 113 },
    ];
    let mut resize_ids = Vec::new();
    for size in resize_sizes {
        resize_ids.push(
            reconnected
                .send(NodeRequest::Resize {
                    session: session.clone(),
                    size,
                })
                .await
                .unwrap(),
        );
    }
    let mut resize_replies = Vec::new();
    let mut resize_command_rejected = false;
    while resize_replies.len() < resize_ids.len() {
        match reconnected.recv().await.unwrap() {
            ServerFrame::Reply(reply) => {
                assert!(resize_ids.contains(&reply.request_id));
                assert_eq!(reply.result.unwrap(), NodeResponse::Accepted);
                resize_replies.push(reply.request_id);
            }
            ServerFrame::Event(event) => {
                if let NodeEvent::Control { address, event } = event.event {
                    if address == session
                        && matches!(event.event, ControlEventKind::CommandRejected { .. })
                    {
                        resize_command_rejected = true;
                    }
                }
            }
            other => panic!("unexpected frame during resize burst: {other:?}"),
        }
    }
    assert!(
        !resize_command_rejected,
        "rapid resize burst produced CommandRejected",
    );
    let NodeResponse::Snapshot { snapshot, .. } = reconnected
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert_eq!(
        addressed_session(&snapshot, &session).unwrap().terminal_size,
        Some(*resize_sizes.last().unwrap()),
    );
    while let Some(event) = reconnected.take_event() {
        if let NodeEvent::Control { address, event } = event.event {
            assert!(
                address != session
                    || !matches!(event.event, ControlEventKind::CommandRejected { .. }),
                "rapid resize burst produced a queued CommandRejected",
            );
        }
    }

    let mismatch = SessionAddress {
        workspace_id: WorkspaceId::new("secondary").unwrap(),
        session: session.session,
    };
    let mismatch_error = reconnected
        .request(NodeRequest::Input {
            session: mismatch,
            text: "x".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        mismatch_error,
        NodeClientError::Node(ref failure)
            if failure.code == NodeFailureCode::SessionWorkspaceMismatch
    ));

    let stale = SessionAddress {
        workspace_id: session.workspace_id.clone(),
        session: gate4agent_node::protocol::SessionKey {
            instance_id: session.session.instance_id,
            generation: SessionGeneration(session.session.generation.0 + 1),
        },
    };
    let stale_error = reconnected
        .request(NodeRequest::Input {
            session: stale,
            text: "x".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        stale_error,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::StaleGeneration
    ));
    let active_remove = reconnected
        .request(NodeRequest::Remove { session: session.clone() })
        .await
        .unwrap_err();
    assert!(matches!(
        active_remove,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::BackendBusy
    ));
    let NodeResponse::Snapshot { snapshot, .. } = reconnected
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert!(addressed_session(&snapshot, &session).is_some());
    shutdown.request_shutdown().await.unwrap();
    let post_shutdown = reconnected
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        })
        .await
        .unwrap_err();
    match post_shutdown {
        NodeClientError::Node(failure) => {
            assert_eq!(failure.code, NodeFailureCode::ShuttingDown);
        }
        NodeClientError::Frame(FrameError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
            ) => {}
        other => panic!("unexpected post-shutdown result: {other:?}"),
    }
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server did not shut down")
        .expect("node server task panicked")
        .expect("node server failed");
    let tasklist = std::process::Command::new("tasklist.exe")
        .args(["/FI", &format!("PID eq {physical_process_id}"), "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist must be available for physical reap proof");
    let listing = String::from_utf8_lossy(&tasklist.stdout);
    assert!(
        !listing.contains(&physical_process_id.to_string()),
        "fixture child {physical_process_id} survived node shutdown: {listing}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_cmd_provider_uses_the_exact_registry_workspace_cwd() {
    let endpoint = endpoint();
    let token = "fixture-cmd-cwd-token";
    let workspace_root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("node-cmd-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&workspace_root).unwrap();
    let command = workspace_root.join("cwd-fixture.cmd");
    std::fs::write(
        &command,
        "@echo off\r\ncd\r\nfor /L %%i in (1,1,32) do @echo scroll-%%i\r\ncd\r\nping -n 60 127.0.0.1 >nul\r\n",
    )
    .unwrap();
    let workspace = WorkspaceConfig::new(
        WorkspaceId::new("primary").unwrap(),
        &workspace_root,
    )
    .unwrap();
    let expected_root = workspace.canonical_root().to_owned();
    let config = NodeServerConfig::new(
        &endpoint,
        token,
        expected_node_id(),
        [workspace],
    )
    .unwrap();
    let server = NodeServer::new_cmd_cwd_fixture(
        config,
        command.to_string_lossy().into_owned(),
    )
    .unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut client = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    client
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();
    let NodeResponse::SpawnAccepted { session } = client
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 8, columns: 120 },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("cmd cwd fixture spawn returned another response");
    };
    let mut observed_contents = String::new();
    let mut observed_scrollback = Vec::new();
    for _ in 0..250 {
        let NodeResponse::Snapshot { snapshot, .. } = client
            .request(NodeRequest::Snapshot)
            .await
            .unwrap()
        else {
            panic!("snapshot request returned another response");
        };
        if let Some(frame) = addressed_session(&snapshot, &session)
            .and_then(|current| current.terminal_frame.as_ref())
        {
            observed_contents = frame.contents.clone();
            observed_scrollback.clone_from(&frame.scrollback_formatted);
            if observed_contents.contains(&expected_root) && observed_scrollback.len() >= 20 {
                break;
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(
        observed_contents.contains(&expected_root),
        ".cmd provider cwd was not the canonical registry root; expected '{expected_root}', terminal was {observed_contents:?}",
    );
    assert!(!observed_contents.contains(r"C:\Windows"));
    assert!(observed_scrollback.len() >= 20);
    assert!(observed_scrollback.len() <= 256);
    assert!(observed_scrollback
        .iter()
        .any(|row| String::from_utf8_lossy(row).contains("scroll-")));
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("cmd cwd node did not shut down")
        .expect("cmd cwd node task panicked")
        .expect("cmd cwd node failed");
    std::fs::remove_file(&command).unwrap();
    std::fs::remove_dir(&workspace_root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_authenticated_connection_limit_releases_capacity() {
    let endpoint = endpoint();
    let token = "fixture-connection-limit-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let server_task = tokio::spawn(server.run());

    let mut clients = Vec::new();
    for _ in 0..16 {
        clients.push(
            NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Observer, token)
                .await
                .unwrap(),
        );
    }
    let rejected = timeout(
        Duration::from_secs(2),
        NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Observer, token),
    )
    .await
    .expect("connection over the authenticated cap did not finish");
    assert!(rejected.is_err(), "connection over the authenticated cap was accepted");

    drop(clients.pop());
    let mut replacement = None;
    for _ in 0..50 {
        match NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token).await {
            Ok(client) => {
                replacement = Some(client);
                break;
            }
            Err(_) => sleep(Duration::from_millis(20)).await,
        }
    }
    let mut replacement = replacement.expect("authenticated capacity was not released on disconnect");
    replacement
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();
    assert_eq!(
        replacement.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server did not shut down")
        .expect("node server task panicked")
        .expect("node server failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_shutdown_drains_idle_clients_and_allows_endpoint_restart() {
    let endpoint = endpoint();
    let token = "fixture-idle-restart-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut idle = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Observer, token)
        .await
        .unwrap();

    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server retained an idle connection task")
        .expect("node server task panicked")
        .expect("node server failed");
    let stale_connection = timeout(Duration::from_secs(2), idle.request(NodeRequest::Snapshot))
        .await
        .expect("idle client pipe remained open after node shutdown");
    assert!(stale_connection.is_err());

    let replacement =
        NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let replacement_task = tokio::spawn(replacement.run());
    let mut operator = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token)
        .await
        .unwrap();
    operator
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();
    assert_eq!(
        operator.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown
    );
    timeout(Duration::from_secs(5), replacement_task)
        .await
        .expect("replacement node did not shut down")
        .expect("replacement node task panicked")
        .expect("replacement node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_accept_error_stops_runtime_and_returns_the_original_error() {
    let endpoint = endpoint();
    let token = "fixture-accept-error-token";
    let blocker = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint)
        .unwrap();
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let result = timeout(Duration::from_secs(5), server.run())
        .await
        .expect("accept error left the runtime loop alive");
    assert!(matches!(result, Err(NodeServerError::Io(_))));
    drop(blocker);

    let replacement =
        NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let shutdown = replacement.shutdown_handle();
    let replacement_task = tokio::spawn(replacement.run());
    let _client = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Observer, token)
        .await
        .unwrap();
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), replacement_task)
        .await
        .expect("replacement node did not shut down")
        .expect("replacement node task panicked")
        .expect("replacement node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_malformed_authenticated_connection_releases_controller() {
    let endpoint = endpoint();
    let token = "fixture-malformed-control-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let server_task = tokio::spawn(server.run());

    let mut raw = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token)
        .await
        .unwrap();
    let NodeResponse::Controller { controller: Some(controller) } = raw
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap()
    else {
        panic!("raw operator did not acquire controller");
    };
    assert_eq!(controller.connection_id, raw.hello().connection_id);
    raw.send_malformed_json_frame_for_fixture().await.unwrap();
    drop(raw);

    let mut replacement = NamedPipeNodeClient::connect(&endpoint, &expected_node_id(), ClientRole::Operator, token)
        .await
        .unwrap();
    let mut acquired = false;
    for _ in 0..50 {
        match replacement
            .request(NodeRequest::AcquireController { lease_ms: 5_000 })
            .await
        {
            Ok(NodeResponse::Controller { controller: Some(_) }) => {
                acquired = true;
                break;
            }
            Err(NodeClientError::Node(ref failure))
                if failure.code == NodeFailureCode::ControllerBusy =>
            {
                sleep(Duration::from_millis(20)).await;
            }
            other => panic!("unexpected replacement controller result: {other:?}"),
        }
    }
    assert!(acquired, "malformed authenticated connection retained controller lease");
    assert_eq!(
        replacement.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server did not shut down")
        .expect("node server task panicked")
        .expect("node server failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_rejects_unix_bytes_workspace_path_before_filesystem_access() {
    let endpoint = endpoint();
    let token = "fixture-unix-bytes-path-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let server_task = tokio::spawn(server.run());
    let mut operator = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    operator
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();
    let before = operator.hello().snapshot.workspaces.clone();

    let error = operator
        .request(NodeRequest::RegisterWorkspace {
            workspace_id: WorkspaceId::new("foreign").unwrap(),
            root: OpaqueHostPath::unix_bytes(b"/srv/repo".to_vec()).unwrap(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::InvalidRequest
    ));
    let NodeResponse::Snapshot { snapshot, .. } = operator
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert_eq!(snapshot.workspaces, before);
    assert_eq!(
        operator.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    drop(operator);
    server_task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_dynamic_workspaces_are_authoritative_and_evented() {
    let endpoint = endpoint();
    let token = "fixture-dynamic-workspace-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token)).unwrap();
    let server_task = tokio::spawn(server.run());
    let mut operator = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    operator
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();

    let workspace_id = WorkspaceId::new("dynamic").unwrap();
    let root = std::env::temp_dir().to_string_lossy().into_owned();
    let NodeResponse::WorkspaceRegistered { workspace } = operator
        .request(NodeRequest::RegisterWorkspace {
            workspace_id: workspace_id.clone(),
            root: host_path(root.clone()),
        })
        .await
        .unwrap()
    else {
        panic!("register did not return the authoritative workspace");
    };
    assert_eq!(workspace.workspace_id, workspace_id);
    assert!(workspace.sessions.is_empty());
    let expected_workspace = WorkspaceConfig::new(workspace_id.clone(), &root).unwrap();
    assert_eq!(workspace.canonical_root.as_utf8(), Some(expected_workspace.canonical_root()));

    let NodeResponse::Snapshot { snapshot, .. } = operator
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert!(snapshot.workspaces.iter().any(|item| item == &workspace));
    let added = std::iter::from_fn(|| operator.take_event())
        .find(|event| matches!(
            event.event,
            NodeEvent::WorkspaceAdded { workspace: ref added_workspace }
                if added_workspace == &workspace
        ))
        .expect("workspace-added event was not delivered");

    let duplicate_id = operator
        .request(NodeRequest::RegisterWorkspace {
            workspace_id: workspace_id.clone(),
            root: host_path(std::env::current_dir().unwrap().to_string_lossy().into_owned()),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate_id,
        NodeClientError::Node(ref failure)
            if failure.code == NodeFailureCode::DuplicateWorkspaceId
    ));
    let duplicate_root = operator
        .request(NodeRequest::RegisterWorkspace {
            workspace_id: WorkspaceId::new("dynamic-copy").unwrap(),
            root: host_path(root),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate_root,
        NodeClientError::Node(ref failure)
            if failure.code == NodeFailureCode::DuplicateWorkspaceRoot
    ));

    assert_eq!(
        operator
            .request(NodeRequest::UnregisterWorkspace {
                workspace_id: workspace_id.clone(),
            })
            .await
            .unwrap(),
        NodeResponse::WorkspaceUnregistered {
            workspace_id: workspace_id.clone(),
        },
    );
    let NodeResponse::Snapshot { snapshot, .. } = operator
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert!(!snapshot.workspaces.iter().any(|item| item.workspace_id == workspace_id));
    let removed = std::iter::from_fn(|| operator.take_event())
        .find(|event| matches!(
            event.event,
            NodeEvent::WorkspaceRemoved { workspace_id: ref removed_id }
                if removed_id == &workspace_id
        ))
        .expect("workspace-removed event was not delivered");
    assert!(removed.sequence > added.sequence);

    let last = operator
        .request(NodeRequest::UnregisterWorkspace {
            workspace_id: WorkspaceId::new("primary").unwrap(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        last,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::LastWorkspace
    ));
    assert_eq!(
        operator.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server did not shut down")
        .expect("node server task panicked")
        .expect("node server failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_git_worktree_lifecycle_is_clean_registered_and_evented() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let test_root = std::env::temp_dir().join(format!(
        "gate4agent-node-worktree-e2e-{}-{unique}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&test_root).unwrap();
    let _cleanup = RemoveTestDirectoryOnDrop(test_root.clone());
    let repository = test_root.join("repository");
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&repository)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr),
    );
    std::fs::write(repository.join("README.md"), b"fixture\n").unwrap();
    assert_git_success(&repository, &["add", "--", "README.md"]);
    assert_git_success(
        &repository,
        &[
            "-c",
            "user.name=Gate4Agent Fixture",
            "-c",
            "user.email=fixture@gate4agent.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=NUL",
            "commit",
            "-m",
            "initial",
        ],
    );

    let endpoint = endpoint();
    let token = "fixture-git-worktree-token";
    let server = NodeServer::new_fixture(git_workspace_server_config(
        &endpoint,
        token,
        &repository,
    ))
    .unwrap();
    let server_task = tokio::spawn(server.run());
    let mut operator = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    let target = test_root.join("topic-one");
    let target_root = target.to_string_lossy().into_owned();
    let unauthorized = operator
        .request(NodeRequest::CreateWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("topic-one").unwrap(),
            target_root: host_path(target_root.clone()),
            branch: "codex/topic-one".to_owned(),
            base: Some("HEAD".to_owned()),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        unauthorized,
        NodeClientError::Node(ref failure)
            if failure.code == NodeFailureCode::ControllerRequired
    ));
    operator
        .request(NodeRequest::AcquireController { lease_ms: 30_000 })
        .await
        .unwrap();

    let NodeResponse::WorktreeCreated { worktree, workspace } = operator
        .request(NodeRequest::CreateWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("topic-one").unwrap(),
            target_root: host_path(target_root.clone()),
            branch: "codex/topic-one".to_owned(),
            base: Some("HEAD".to_owned()),
        })
        .await
        .unwrap()
    else {
        panic!("worktree create returned another response");
    };
    assert_eq!(
        std::fs::canonicalize(worktree.path.as_utf8().unwrap()).unwrap(),
        std::fs::canonicalize(workspace.canonical_root.as_utf8().unwrap()).unwrap(),
    );
    assert_eq!(worktree.branch.as_deref(), Some("codex/topic-one"));
    assert_eq!(worktree.workspace_id.as_ref(), Some(&workspace.workspace_id));
    assert!(target.join("README.md").is_file());

    let NodeResponse::WorkspaceInspected { inspection } = operator
        .request(NodeRequest::InspectWorkspace {
            workspace_id: WorkspaceId::new("primary").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!("worktree inspection returned another response");
    };
    assert_eq!(inspection.git.worktrees.len(), 2);
    assert!(inspection.git.worktrees.iter().any(|item| {
        item.workspace_id.as_ref() == Some(&WorkspaceId::new("topic-one").unwrap())
            && item.branch.as_deref() == Some("codex/topic-one")
    }));

    let NodeResponse::SpawnAccepted { session } = operator
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("topic-one").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("fixture spawn returned another response");
    };
    let busy = operator
        .request(NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            target_root: host_path(target_root.clone()),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        busy,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::WorkspaceBusy
    ));
    assert_eq!(
        operator
            .request(NodeRequest::Stop {
                session: session.clone(),
                force: true,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let mut stopped = false;
    for _ in 0..250 {
        let NodeResponse::Snapshot { snapshot, .. } = operator
            .request(NodeRequest::Snapshot)
            .await
            .unwrap()
        else {
            panic!("snapshot request returned another response");
        };
        if addressed_session(&snapshot, &session).is_some_and(|item| {
            matches!(item.status, SessionStatus::Exited { .. } | SessionStatus::Failed { .. })
        }) {
            stopped = true;
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(stopped, "fixture session did not stop before worktree cleanup");
    assert_eq!(
        operator
            .request(NodeRequest::Remove {
                session: session.clone(),
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );

    std::fs::write(target.join("dirty.txt"), b"dirty\n").unwrap();
    let dirty = operator
        .request(NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            target_root: host_path(target_root.clone()),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        &dirty,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::WorktreeDirty
    ), "unexpected dirty-worktree refusal: {dirty:?}");
    assert!(target.is_dir());
    std::fs::remove_file(target.join("dirty.txt")).unwrap();

    let removal_token = target_root.to_ascii_uppercase();
    assert_ne!(removal_token, target_root);
    let NodeResponse::WorktreeRemoved {
        target_root: removed_root,
        workspace_id,
    } = operator
        .request(NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            target_root: host_path(removal_token.clone()),
        })
        .await
        .unwrap()
    else {
        panic!("worktree remove returned another response");
    };
    assert_eq!(removed_root, host_path(removal_token));
    assert_ne!(removed_root, worktree.path);
    assert_eq!(workspace_id.as_ref(), Some(&workspace.workspace_id));
    assert!(!target.exists());
    assert_git_success(
        &repository,
        &["show-ref", "--verify", "--quiet", "refs/heads/codex/topic-one"],
    );
    let list = git_command(&repository, &["worktree", "list", "--porcelain"]);
    assert!(list.status.success());
    assert_eq!(
        String::from_utf8_lossy(&list.stdout)
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
    );
    let NodeResponse::Snapshot { snapshot, .. } = operator
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    assert!(!snapshot
        .workspaces
        .iter()
        .any(|item| item.workspace_id == workspace.workspace_id));
    let events = std::iter::from_fn(|| operator.take_event()).collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        NodeEvent::WorkspaceAdded { workspace: added }
            if added.workspace_id == workspace.workspace_id
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        NodeEvent::WorkspaceRemoved { workspace_id }
            if workspace_id == &workspace.workspace_id
    )));

    assert_eq!(
        operator.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server did not shut down")
        .expect("node server task panicked")
        .expect("node server failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_named_pipe_inspection_preserves_git_rename_path_identity() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let test_root = std::env::temp_dir().join(format!(
        "gate4agent-node-worktree-e2e-status-{}-{unique}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&test_root).unwrap();
    let _cleanup = RemoveTestDirectoryOnDrop(test_root.clone());
    let repository = test_root.join("repository");
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&repository)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr),
    );
    std::fs::write(repository.join("previous name.rs"), b"fn original() {}\n").unwrap();
    assert_git_success(&repository, &["add", "--", "previous name.rs"]);
    assert_git_success(
        &repository,
        &[
            "-c",
            "user.name=Gate4Agent Fixture",
            "-c",
            "user.email=fixture@gate4agent.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=NUL",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    assert_git_success(&repository, &["config", "status.renames", "false"]);
    std::fs::rename(
        repository.join("previous name.rs"),
        repository.join("current name.rs"),
    )
    .unwrap();
    assert_git_success(&repository, &["add", "-A", "--", "."]);

    let endpoint = endpoint();
    let token = "fixture-git-status-rename-token";
    let server = NodeServer::new_fixture(git_workspace_server_config(
        &endpoint,
        token,
        &repository,
    ))
    .unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut observer = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();

    let NodeResponse::WorkspaceInspected { inspection } = observer
        .request(NodeRequest::InspectWorkspace {
            workspace_id: WorkspaceId::new("primary").unwrap(),
        })
        .await
        .unwrap()
    else {
        panic!("named-pipe workspace inspection returned another response");
    };
    let rename = inspection
        .git
        .status
        .iter()
        .find(|entry| entry.index_status == "R")
        .expect("named-pipe workspace inspection did not return a rename");
    assert_eq!(rename.path.as_utf8(), Some("current name.rs"));
    assert_eq!(
        rename
            .previous_path
            .as_ref()
            .and_then(gate4agent_node::protocol::RepositoryPath::as_utf8),
        Some("previous name.rs"),
    );
    assert!(!inspection.git.truncated, "{:?}", inspection.git.diagnostic);

    drop(observer);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("git status node did not shut down")
        .expect("git status node task panicked")
        .expect("git status node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_promptless_resume_advances_generation_through_node() {
    let endpoint = endpoint();
    let token = "fixture-promptless-resume-token";
    let server = NodeServer::new_resume_fixture(server_config(&endpoint, token)).unwrap();
    let server_task = tokio::spawn(server.run());
    let mut operator = NamedPipeNodeClient::connect(
        &endpoint,
        &expected_node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    operator
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap();
    let NodeResponse::SpawnAccepted { session } = operator
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("fixture spawn returned another response");
    };
    let mut fresh_provider_session = None;
    for _ in 0..250 {
        let NodeResponse::Snapshot { snapshot, .. } = operator
            .request(NodeRequest::Snapshot)
            .await
            .unwrap()
        else {
            panic!("snapshot request returned another response");
        };
        if let Some(fresh) = addressed_session(&snapshot, &session) {
            if fresh.status == SessionStatus::Running {
                if let Some(identity) = fresh.provider.session.as_ref() {
                    fresh_provider_session = Some(identity.id.clone());
                    break;
                }
            }
            if matches!(fresh.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. }) {
                panic!("resumable fixture stopped during fresh launch: {:?}", fresh.status);
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    let fresh_provider_session = fresh_provider_session
        .expect("fresh fixture did not publish its provider session");
    assert_eq!(
        operator
            .request(NodeRequest::Stop {
                session: session.clone(),
                force: true,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let mut stopped = false;
    for _ in 0..250 {
        let NodeResponse::Snapshot { snapshot, .. } = operator
            .request(NodeRequest::Snapshot)
            .await
            .unwrap()
        else {
            panic!("snapshot request returned another response");
        };
        if addressed_session(&snapshot, &session)
            .is_some_and(|item| matches!(item.status, SessionStatus::Exited { .. }))
        {
            stopped = true;
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(stopped, "fresh fixture did not stop before resume");
    while operator.take_event().is_some() {}
    assert_eq!(
        operator
            .request(NodeRequest::Resume {
                session: session.clone(),
                terminal_size: TerminalSize { rows: 31, columns: 97 },
                initial_prompt: None,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let expected_generation = SessionGeneration(session.session.generation.0 + 1);
    let mut resumed_running = false;
    let mut last_session = None;
    let mut resume_trace = Vec::new();
    for _ in 0..250 {
        let NodeResponse::Snapshot { snapshot, .. } = operator
            .request(NodeRequest::Snapshot)
            .await
            .unwrap()
        else {
            panic!("snapshot request returned another response");
        };
        let resumed = all_sessions(&snapshot)
            .into_iter()
            .find(|item| item.instance_id == session.session.instance_id);
        if let Some(resumed) = resumed {
            last_session = Some(format!("{resumed:?}"));
            if resumed.generation == expected_generation
                && resumed.status == SessionStatus::Running
            {
                assert_eq!(
                    resumed.terminal_size,
                    Some(TerminalSize { rows: 31, columns: 97 }),
                );
                resumed_running = true;
                break;
            }
            if matches!(resumed.status, SessionStatus::Failed { .. }) {
                panic!("promptless resume failed: {:?}", resumed.status);
            }
        }
        while let Some(event) = operator.take_event() {
            if let NodeEvent::Control { address, event } = event.event {
                if address.session.instance_id == session.session.instance_id {
                    resume_trace.push(format!("{:?}", event.event));
                    match event.event {
                        ControlEventKind::ResumeDenied { reason } => {
                            panic!("promptless resume was denied: {reason}");
                        }
                        ControlEventKind::ResumeFailed { message } => {
                            panic!("promptless resume failed before spawn: {message}");
                        }
                        ControlEventKind::CommandRejected { message } => {
                            panic!("promptless resume command was rejected: {message}");
                        }
                        _ => {}
                    }
                }
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(
        resumed_running,
        "promptless resume did not advance to a running generation; last_session={last_session:?}; events={resume_trace:?}",
    );
    println!(
        "promptless-resume-observed instance={} generation={}=>{} provider_session={} terminal=31x97",
        session.session.instance_id.0,
        session.session.generation.0,
        expected_generation.0,
        fresh_provider_session,
    );
    assert_eq!(
        operator.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("node server did not shut down")
        .expect("node server task panicked")
        .expect("node server failed");
}
