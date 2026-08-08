#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, C2RelayFailureCode, NodeId, NodeRoute, NodeTransportState,
    PathStyle, C2_COMPATIBILITY_METADATA_CAPABILITY, C2_CONTROL_PROTOCOL_VERSION,
    C2_OPAQUE_UNIX_PATH_CAPABILITY, C2_REPOSITORY_PATH_CAPABILITY, RepositoryPath,
};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlError};
use gate4agent_node::protocol::{
    AgentProvider, ClientRole, ManagedSessionState, NodeFailureCode, NodeRequest,
    NodeResponse, OpaqueHostPath, SessionMode, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::NamedPipeNodeClient;
use gate4agent_types::TerminalSize;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

struct KillChildOnDrop(Option<tokio::process::Child>);

struct RemoveStateDirectoryOnDrop(PathBuf);

impl Drop for RemoveStateDirectoryOnDrop {
    fn drop(&mut self) {
        let safe = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-c2-durable-e2e-"));
        if safe {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

impl KillChildOnDrop {
    fn id(&self) -> u32 { self.0.as_ref().and_then(tokio::process::Child::id).unwrap() }

    async fn terminate(&mut self) {
        let child = self.0.as_mut().expect("C2 child is present");
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        self.0.take();
    }
}

impl Drop for KillChildOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() { let _ = child.start_kill(); }
    }
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!(r"\\.\pipe\gate4agent-c2-relay-{label}-{}-{nonce}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

fn node_config(node_id: &str, endpoint: &str, token: &str) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        NodeId::new(node_id).unwrap(),
        [WorkspaceConfig::new(WorkspaceId::new("primary").unwrap(), std::env::current_dir().unwrap()).unwrap()],
    ).unwrap()
}

fn durable_state_directory() -> PathBuf {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!(
        "gate4agent-c2-durable-e2e-{}-{nonce}",
        std::process::id(),
    ))
}

fn durable_node_config(
    node_id: &str,
    endpoint: &str,
    token: &str,
    state_path: &std::path::Path,
) -> NodeServerConfig {
    node_config(node_id, endpoint, token)
        .with_state_path(state_path)
        .unwrap()
}

async fn wait_status(
    client: &C2Client,
    predicate: impl Fn(&gate4agent_c2::protocol::StatusResponse) -> bool,
) -> gate4agent_c2::protocol::StatusResponse {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(status) = client.status().await {
                if predicate(&status) { return status; }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 status condition was not reached")
}

async fn wait_http(address: std::net::SocketAddr, token: &str) -> C2Client {
    timeout(Duration::from_secs(5), async {
        loop {
            let client = C2Client::new(address, token).unwrap().with_deadline(Duration::from_millis(500));
            if client.health().await.is_ok() { return client; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 child did not expose health")
}

fn route(status: &gate4agent_c2::protocol::StatusResponse, node_id: &NodeId) -> NodeRoute {
    NodeRoute {
        node_id: node_id.clone(),
        expected_incarnation_id: status.nodes[node_id].cursor.expect("node cursor").incarnation_id,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_real_two_node_c2_control_relay_routes_commands_events_and_preserves_nodes_on_exit() {
    let endpoint_a = endpoint("a");
    let endpoint_b = endpoint("b");
    let control_endpoint = endpoint("control");
    let id_a = NodeId::new("node-a").unwrap();
    let id_b = NodeId::new("node-b").unwrap();
    let token_a = "node-a-token";
    let token_b = "node-b-token";
    let c2_token = "c2-control-token";
    let state_directory = durable_state_directory();
    let _state_cleanup = RemoveStateDirectoryOnDrop(state_directory.clone());
    let state_path_a = state_directory.join("node-a-state.json");

    let server_a = NodeServer::new_resume_fixture(durable_node_config(
        id_a.as_str(),
        &endpoint_a,
        token_a,
        &state_path_a,
    )).unwrap();
    let mut shutdown_a = server_a.shutdown_handle();
    let mut task_a = tokio::spawn(server_a.run());
    let server_b = NodeServer::new_fixture(node_config(id_b.as_str(), &endpoint_b, token_b)).unwrap();
    let shutdown_b = server_b.shutdown_handle();
    let task_b = tokio::spawn(server_b.run());

    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let c2_addr = reservation.local_addr().unwrap();
    drop(reservation);
    let mut command = Command::new(env!("CARGO_BIN_EXE_gate4agent-c2"));
    command.args([
        "--api-listen", &c2_addr.to_string(),
        "--control-endpoint", &control_endpoint,
        "--node", &format!("{}={endpoint_a}", id_a.as_str()),
        "--node", &format!("{}={endpoint_b}", id_b.as_str()),
    ]);
    command.env("GATE4AGENT_C2_TOKEN", c2_token)
        .env("GATE4AGENT_NODE_TOKEN_NODE_A", token_a)
        .env("GATE4AGENT_NODE_TOKEN_NODE_B", token_b)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.as_std_mut().creation_flags(0x08000000);
    let mut c2_child = KillChildOnDrop(Some(command.spawn().unwrap()));
    let c2_pid = c2_child.id();
    let http = wait_http(c2_addr, c2_token).await;
    assert_eq!(http.health().await.unwrap().pid, c2_pid);
    let initial = wait_status(&http, |status| {
        status.ready && status.nodes.values().all(|node| node.transport == NodeTransportState::Online)
    }).await;

    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    assert_eq!(control.hello().status.nodes.len(), 2);
    let compatibility = control.hello().compatibility.as_ref()
        .expect("negotiated C2 compatibility metadata");
    assert_eq!(compatibility.protocol_version, C2_CONTROL_PROTOCOL_VERSION);
    assert_eq!(compatibility.capabilities.len(), 3);
    assert_eq!(
        compatibility.capabilities[0].as_str(),
        C2_COMPATIBILITY_METADATA_CAPABILITY,
    );
    assert_eq!(
        compatibility.capabilities[1].as_str(),
        C2_OPAQUE_UNIX_PATH_CAPABILITY,
    );
    assert_eq!(
        compatibility.capabilities[2].as_str(),
        C2_REPOSITORY_PATH_CAPABILITY,
    );
    assert_eq!(compatibility.host.operating_system.as_str(), "windows");
    assert_eq!(compatibility.host.architecture.as_str(), std::env::consts::ARCH);
    assert_eq!(compatibility.path_semantics.style, PathStyle::Windows);
    let hello_a_cursor = control.hello().status.nodes[&id_a].cursor.expect("node A hello cursor");
    let event_node_a = id_a.clone();
    let event_drain = tokio::spawn(async move {
        let mut count = 0_usize;
        let mut last_a_cursor = Some(hello_a_cursor);
        let mut saw_added = false;
        let mut record_upserts = Vec::new();
        let mut record_removals = Vec::new();
        while let Some(event) = events.recv().await {
            count += 1;
            if event.node_id == event_node_a {
                if let Some(previous) = last_a_cursor {
                    if event.cursor.incarnation_id == previous.incarnation_id {
                        assert!(event.cursor.sequence > previous.sequence, "node A events were not monotonic");
                    }
                }
                last_a_cursor = Some(event.cursor);
                if matches!(&event.event, C2NodeEvent::WorkspaceAdded { workspace }
                    if workspace.workspace_id.as_str() == "relay-added") {
                    saw_added = true;
                }
                if let C2NodeEvent::SessionRecordUpserted { record } = &event.event {
                    record_upserts.push((
                        record.record_id.as_str().to_owned(),
                        record.display_name.clone(),
                    ));
                }
                if let C2NodeEvent::SessionRecordRemoved { record_id } = &event.event {
                    record_removals.push(record_id.as_str().to_owned());
                }
            }
        }
        (count, saw_added, record_upserts, record_removals)
    });
    let mut route_a = route(&initial, &id_a);
    let route_b = route(&initial, &id_b);
    let second = match connect_local(&control_endpoint, c2_token).await {
        Err(error) => error,
        Ok(_) => panic!("second C2 operator unexpectedly connected"),
    };
    assert!(matches!(second, C2ControlError::Relay(ref failure) if failure.code == C2RelayFailureCode::OperatorAlreadyConnected));

    let (snapshot_a, snapshot_b) = tokio::join!(
        control.request(route_a.clone(), NodeRequest::Snapshot),
        control.request(route_b.clone(), NodeRequest::Snapshot),
    );
    assert!(matches!(snapshot_a.unwrap().response, Ok(C2NodeResponse::Snapshot { .. })));
    assert!(matches!(snapshot_b.unwrap().response, Ok(C2NodeResponse::Snapshot { .. })));
    let forbidden = control.request(
        route_a.clone(),
        NodeRequest::AcquireController { lease_ms: 1_000 },
    ).await.unwrap_err();
    assert!(matches!(forbidden, C2ControlError::Relay(ref failure) if failure.code == C2RelayFailureCode::RequestForbidden));
    let inspected = control.request(
        route_a.clone(),
        NodeRequest::InspectWorkspace { workspace_id: WorkspaceId::new("primary").unwrap() },
    ).await.unwrap();
    let inspection = match inspected.response {
        Ok(C2NodeResponse::WorkspaceInspected { inspection }) => inspection,
        response => panic!("unexpected inspect response: {response:?}"),
    };
    let expected_repository_path = RepositoryPath::utf8("Cargo.toml".to_owned()).unwrap();
    let relayed_repository_path = inspection.entries.iter()
        .find(|entry| entry.relative_path == expected_repository_path)
        .expect("real C2 relay inspection omitted Cargo.toml");
    assert_eq!(relayed_repository_path.relative_path.as_utf8(), Some("Cargo.toml"));
    assert_eq!(relayed_repository_path.relative_path.as_unix_bytes(), None);

    let added_id = WorkspaceId::new("relay-added").unwrap();
    let added = control.request(route_a.clone(), NodeRequest::RegisterWorkspace {
        workspace_id: added_id.clone(),
        root: OpaqueHostPath::utf8(std::env::temp_dir().to_string_lossy().into_owned()).unwrap(),
    }).await.unwrap();
    assert!(matches!(added.response, Ok(C2NodeResponse::WorkspaceRegistered { .. })));
    let changed = wait_status(&http, |status| {
        status.nodes[&id_a].inventory.as_ref().is_some_and(|inventory| inventory.workspaces.contains_key(&added_id))
    }).await;
    assert_eq!(changed.nodes[&id_b].cursor, initial.nodes[&id_b].cursor);

    let spawned = control.request(route_a.clone(), NodeRequest::Spawn {
        workspace_id: WorkspaceId::new("primary").unwrap(),
        provider: AgentProvider::Claude,
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        initial_prompt: None,
    }).await.unwrap();
    let session = match spawned.response {
        Ok(C2NodeResponse::SpawnAccepted { session }) => session,
        response => panic!("unexpected spawn response: {response:?}"),
    };
    timeout(Duration::from_secs(5), async {
        loop {
            let response = control.request(route_a.clone(), NodeRequest::Input {
                session: session.clone(), text: "relay input".to_owned(),
            }).await.unwrap();
            if matches!(response.response, Ok(C2NodeResponse::Accepted)) { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("fixture session did not become ready for routed input");
    assert!(matches!(control.request(route_a.clone(), NodeRequest::Resize {
        session: session.clone(), size: TerminalSize { rows: 30, columns: 100 },
    }).await.unwrap().response, Ok(C2NodeResponse::Accepted)));
    assert!(matches!(control.request(route_a.clone(), NodeRequest::Interrupt {
        session: session.clone(),
    }).await.unwrap().response, Ok(C2NodeResponse::Accepted)));

    let identified = wait_status(&http, |status| {
        status.nodes[&id_a]
            .inventory
            .as_ref()
            .is_some_and(|inventory| inventory.managed_sessions.iter().any(|record| {
                record.active_session.as_ref() == Some(&session)
                    && record.provider_identity_present
                    && record.state == ManagedSessionState::Live
            }))
    }).await;
    let record_id = identified.nodes[&id_a]
        .inventory
        .as_ref()
        .unwrap()
        .managed_sessions
        .iter()
        .find(|record| record.active_session.as_ref() == Some(&session))
        .unwrap()
        .record_id
        .clone();
    let renamed = control.request(route_a.clone(), NodeRequest::RenameSessionRecord {
        record_id: record_id.clone(),
        display_name: "release shepherd".to_owned(),
    }).await.unwrap();
    assert!(matches!(renamed.response, Ok(C2NodeResponse::SessionRecordUpdated { ref record })
        if record.record_id == record_id
            && record.display_name == "release shepherd"
            && record.provider_identity_present));

    assert!(matches!(control.request(route_a.clone(), NodeRequest::Stop {
        session: session.clone(),
        force: true,
    }).await.unwrap().response, Ok(C2NodeResponse::Accepted)));
    wait_status(&http, |status| {
        status.nodes[&id_a]
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.managed_sessions.iter().find(|record| record.record_id == record_id))
            .is_some_and(|record| {
                record.display_name == "release shepherd"
                    && record.state == ManagedSessionState::Dormant
                    && record.active_session.is_none()
            })
    }).await;

    shutdown_a.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), task_a).await.unwrap().unwrap().unwrap();
    wait_status(&http, |status| status.nodes[&id_a].transport != NodeTransportState::Online).await;
    let restarted_a = NodeServer::new_resume_fixture(durable_node_config(
        id_a.as_str(),
        &endpoint_a,
        token_a,
        &state_path_a,
    )).unwrap();
    shutdown_a = restarted_a.shutdown_handle();
    task_a = tokio::spawn(restarted_a.run());
    let recovered_a = wait_status(&http, |status| {
        status.nodes[&id_a].transport == NodeTransportState::Online
            && status.nodes[&id_a].cursor.is_some_and(|cursor| {
                cursor.incarnation_id != route_a.expected_incarnation_id
            })
            && status.nodes[&id_a]
                .inventory
                .as_ref()
                .and_then(|inventory| inventory.managed_sessions.iter().find(|record| record.record_id == record_id))
                .is_some_and(|record| {
                    record.display_name == "release shepherd"
                        && record.state == ManagedSessionState::Dormant
                        && record.active_session.is_none()
                        && record.provider_identity_present
                })
    }).await;
    route_a = route(&recovered_a, &id_a);

    let recovered_snapshot = control.request(route_a.clone(), NodeRequest::Snapshot).await.unwrap();
    assert!(matches!(recovered_snapshot.response, Ok(C2NodeResponse::Snapshot { ref snapshot, .. })
        if snapshot.session_records.iter().any(|record| {
            record.record_id == record_id
                && record.display_name == "release shepherd"
                && record.provider_identity_present
        })));

    let resumed = timeout(Duration::from_secs(35), control.request(route_a.clone(), NodeRequest::ResumeSessionRecord {
        record_id: record_id.clone(),
        terminal_size: TerminalSize { rows: 32, columns: 96 },
        initial_prompt: None,
    })).await.expect("C2 managed resume exceeded its end-to-end 35-second budget").unwrap();
    let resumed_session = match resumed.response {
        Ok(C2NodeResponse::SessionRecordResumed { record, session }) => {
            assert_eq!(record.record_id, record_id);
            assert_eq!(record.display_name, "release shepherd");
            assert_eq!(record.workspace_id, session.workspace_id);
            assert_eq!(record.active_session.as_ref(), Some(&session));
            assert!(record.provider_identity_present);
            session
        }
        response => panic!("unexpected managed resume response: {response:?}"),
    };
    let duplicate = control.request(route_a.clone(), NodeRequest::ResumeSessionRecord {
        record_id: record_id.clone(),
        terminal_size: TerminalSize { rows: 32, columns: 96 },
        initial_prompt: None,
    }).await.unwrap();
    assert!(matches!(duplicate.response, Err(ref failure)
        if failure.code == NodeFailureCode::SessionRecordBusy));
    wait_status(&http, |status| {
        status.nodes[&id_a]
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.managed_sessions.iter().find(|record| record.record_id == record_id))
            .is_some_and(|record| {
                record.state == ManagedSessionState::Live
                    && record.active_session.as_ref() == Some(&resumed_session)
                    && record.provider_identity_present
            })
    }).await;
    assert!(matches!(control.request(route_a.clone(), NodeRequest::Stop {
        session: resumed_session,
        force: true,
    }).await.unwrap().response, Ok(C2NodeResponse::Accepted)));
    wait_status(&http, |status| {
        status.nodes[&id_a]
            .inventory
            .as_ref()
            .and_then(|inventory| inventory.managed_sessions.iter().find(|record| record.record_id == record_id))
            .is_some_and(|record| {
                record.state == ManagedSessionState::Dormant && record.active_session.is_none()
            })
    }).await;
    let forgotten = control.request(route_a.clone(), NodeRequest::ForgetSessionRecord {
        record_id: record_id.clone(),
    }).await.unwrap();
    assert!(matches!(forgotten.response, Ok(C2NodeResponse::SessionRecordForgotten { record_id: ref forgotten_id })
        if forgotten_id == &record_id));
    wait_status(&http, |status| {
        status.nodes[&id_a]
            .inventory
            .as_ref()
            .is_some_and(|inventory| {
                !inventory.managed_sessions.iter().any(|record| record.record_id == record_id)
            })
    }).await;

    let survivor = control.request(route_a.clone(), NodeRequest::Spawn {
        workspace_id: WorkspaceId::new("primary").unwrap(),
        provider: AgentProvider::Claude,
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        initial_prompt: None,
    }).await.unwrap();
    let survivor = match survivor.response {
        Ok(C2NodeResponse::SpawnAccepted { session }) => session,
        response => panic!("unexpected survivor spawn response: {response:?}"),
    };

    shutdown_b.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), task_b).await.unwrap().unwrap().unwrap();
    wait_status(&http, |status| status.nodes[&id_b].transport != NodeTransportState::Online).await;
    let restarted_b = NodeServer::new_fixture(node_config(id_b.as_str(), &endpoint_b, token_b)).unwrap();
    let restarted_shutdown_b = restarted_b.shutdown_handle();
    let restarted_task_b = tokio::spawn(restarted_b.run());
    let recovered = wait_status(&http, |status| {
        status.nodes[&id_b].transport == NodeTransportState::Online
            && status.nodes[&id_b].cursor.is_some_and(|cursor| cursor.incarnation_id != route_b.expected_incarnation_id)
    }).await;
    let stale = control.request(route_b, NodeRequest::Snapshot).await.unwrap_err();
    assert!(matches!(stale, C2ControlError::Relay(ref failure)
        if failure.code == C2RelayFailureCode::StaleNodeIncarnation
            && failure.current_incarnation_id == recovered.nodes[&id_b].cursor.map(|cursor| cursor.incarnation_id)));

    c2_child.terminate().await;
    assert!(!matches!(timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(c2_addr)).await, Ok(Ok(_))));
    drop(control);
    let (event_count, saw_added, record_upserts, record_removals) =
        timeout(Duration::from_secs(2), event_drain).await.unwrap().unwrap();
    assert!(event_count > 0);
    assert!(saw_added, "routed WorkspaceAdded event was not observed");
    assert!(record_upserts.iter().any(|(id, name)| {
        id == record_id.as_str() && name == "release shepherd"
    }), "routed rename SessionRecordUpserted event was not observed");
    assert!(record_removals.iter().any(|id| id == record_id.as_str()),
        "routed SessionRecordRemoved event was not observed");

    let mut direct_a = NamedPipeNodeClient::connect(&endpoint_a, &id_a, ClientRole::Operator, token_a).await.unwrap();
    let post_exit = direct_a.request(NodeRequest::Snapshot).await.unwrap();
    assert!(matches!(post_exit, NodeResponse::Snapshot { ref snapshot, .. }
        if snapshot.workspaces.iter().flat_map(|workspace| &workspace.sessions).any(|item| item.instance_id == survivor.session.instance_id)));
    let acquired = timeout(Duration::from_secs(6), async {
        loop {
            match direct_a.request(NodeRequest::AcquireController { lease_ms: 5_000 }).await {
                Ok(NodeResponse::Controller { controller: Some(_) }) => break,
                _ => sleep(Duration::from_millis(20)).await,
            }
        }
    }).await;
    assert!(acquired.is_ok(), "node controller was not released after C2 exit");
    assert_eq!(direct_a.request(NodeRequest::Stop { session: survivor, force: true }).await.unwrap(), NodeResponse::Accepted);
    drop(direct_a);

    shutdown_a.request_shutdown().await.unwrap();
    restarted_shutdown_b.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), task_a).await.unwrap().unwrap().unwrap();
    timeout(Duration::from_secs(5), restarted_task_b).await.unwrap().unwrap().unwrap();
}
