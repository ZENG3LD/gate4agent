#![cfg(unix)]

use gate4agent_c2::protocol::{
    C2NodeResponse, C2RelayFailureCode, NodeId, NodeRoute, NodeTransportState,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlError};
use gate4agent_node::protocol::{
    AgentId, ClientRole, NodeRequest, NodeResponse, OpaqueHostPath, SessionMode,
    WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::LocalNodeClient;
use gate4agent_types::TerminalSize;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::{sleep, timeout};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

struct PrivateSocketDir(PathBuf);

impl PrivateSocketDir {
    fn new() -> Self {
        let root = std::env::temp_dir();
        for _ in 0..100 {
            let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("g4a-c2-node-{}-{serial}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    assert_eq!(
                        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                        0o700,
                    );
                    return Self(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private socket directory: {error}"),
            }
        }
        panic!("could not allocate private socket directory")
    }

    fn endpoint(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }

    fn directory(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir(&path).unwrap();
        path
    }
}

impl Drop for PrivateSocketDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn node_config(node_id: &NodeId, endpoint: &str, token: &str) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
}

fn route(
    status: &gate4agent_c2::protocol::StatusResponse,
    node_id: &NodeId,
) -> NodeRoute {
    NodeRoute {
        node_id: node_id.clone(),
        expected_incarnation_id: status.nodes[node_id]
            .cursor
            .expect("online node has a cursor")
            .incarnation_id,
    }
}

async fn wait_status(
    client: &C2Client,
    predicate: impl Fn(&gate4agent_c2::protocol::StatusResponse) -> bool,
) -> gate4agent_c2::protocol::StatusResponse {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(status) = client.status().await {
                if predicate(&status) {
                    return status;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("C2 status condition was not reached")
}

async fn stop_node(
    shutdown: gate4agent_node::NodeShutdownHandle,
    task: tokio::task::JoinHandle<Result<(), gate4agent_node::NodeServerError>>,
) {
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), task)
        .await
        .expect("node shutdown timed out")
        .expect("node task panicked")
        .expect("node shutdown failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unix_real_two_node_c2_routes_control_survives_restart_and_releases_nodes() {
    let sockets = PrivateSocketDir::new();
    let endpoint_a = sockets.endpoint("node-a.sock");
    let endpoint_b = sockets.endpoint("node-b.sock");
    let control_endpoint = sockets.endpoint("c2.sock");
    let added_workspace = sockets.directory("relay-added");
    let id_a = NodeId::new("node-a").unwrap();
    let id_b = NodeId::new("node-b").unwrap();
    let token_a = "node-a-token";
    let token_b = "node-b-token";
    let c2_token = "c2-control-token";

    let server_a = NodeServer::new_fixture(node_config(&id_a, &endpoint_a, token_a)).unwrap();
    let mut shutdown_a = server_a.shutdown_handle();
    let mut task_a = tokio::spawn(server_a.run());
    let server_b = NodeServer::new_fixture(node_config(&id_b, &endpoint_b, token_b)).unwrap();
    let shutdown_b = server_b.shutdown_handle();
    let task_b = tokio::spawn(server_b.run());

    let timings = C2Timings {
        poll_interval: Duration::from_millis(20),
        fresh_for: Duration::from_secs(1),
        attempt_deadline: Duration::from_millis(500),
        transient_backoffs: [Duration::from_millis(20); 5],
        parked_backoff: Duration::from_millis(100),
        http_io_deadline: Duration::from_secs(1),
    };
    let config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![
            C2NodeConfig::new(id_a.clone(), endpoint_a.clone(), token_a).unwrap(),
            C2NodeConfig::new(id_b.clone(), endpoint_b.clone(), token_b).unwrap(),
        ],
    )
    .unwrap()
    .with_control_endpoint(control_endpoint.clone())
    .unwrap()
    .with_timings(timings);
    let running = C2Running::start(config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let initial = wait_status(&http, |status| {
        status.ready
            && status.nodes.len() == 2
            && status
                .nodes
                .values()
                .all(|node| node.transport == NodeTransportState::Online)
    })
    .await;
    assert_eq!(
        fs::metadata(&endpoint_a).unwrap().permissions().mode() & 0o7777,
        0o600,
    );
    assert_eq!(
        fs::metadata(&endpoint_b).unwrap().permissions().mode() & 0o7777,
        0o600,
    );

    let (control, mut events) = timeout(
        Duration::from_secs(5),
        connect_local(&control_endpoint, c2_token),
    )
    .await
    .expect("C2 control UDS became available")
    .unwrap();
    assert!(Path::new(&control_endpoint).exists());
    assert_eq!(
        fs::metadata(&control_endpoint)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600,
    );
    let event_drain = tokio::spawn(async move {
        let mut count = 0_usize;
        while events.recv().await.is_some() {
            count += 1;
        }
        count
    });

    let stale_route_a = route(&initial, &id_a);
    let route_b = route(&initial, &id_b);
    let (snapshot_a, snapshot_b) = tokio::join!(
        control.request(stale_route_a.clone(), NodeRequest::Snapshot),
        control.request(route_b.clone(), NodeRequest::Snapshot),
    );
    assert!(matches!(
        snapshot_a.unwrap().response,
        Ok(C2NodeResponse::Snapshot { .. })
    ));
    assert!(matches!(
        snapshot_b.unwrap().response,
        Ok(C2NodeResponse::Snapshot { .. })
    ));

    let added_id = WorkspaceId::new("relay-added").unwrap();
    let registered = control
        .request(
            stale_route_a.clone(),
            NodeRequest::RegisterWorkspace {
                workspace_id: added_id.clone(),
                root: OpaqueHostPath::utf8(added_workspace.to_string_lossy().into_owned())
                    .unwrap(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        registered.response,
        Ok(C2NodeResponse::WorkspaceRegistered { .. })
    ));
    wait_status(&http, |status| {
        status.nodes[&id_a]
            .inventory
            .as_ref()
            .is_some_and(|inventory| inventory.workspaces.contains_key(&added_id))
    })
    .await;

    let spawned = control
        .request(
            stale_route_a.clone(),
            NodeRequest::Spawn {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                provider: AgentId::new("claude").unwrap(),
                mode: SessionMode::Pty,
                terminal_size: TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                initial_prompt: None,
            },
        )
        .await
        .unwrap();
    let session = match spawned.response {
        Ok(C2NodeResponse::SpawnAccepted { session }) => session,
        response => panic!("unexpected spawn response: {response:?}"),
    };
    timeout(Duration::from_secs(5), async {
        loop {
            let response = control
                .request(
                    stale_route_a.clone(),
                    NodeRequest::Input {
                        session: session.clone(),
                        text: "unix relay input".to_owned(),
                    },
                )
                .await
                .unwrap();
            if matches!(response.response, Ok(C2NodeResponse::Accepted)) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture session did not become ready for routed input");
    assert!(matches!(
        control
            .request(
                stale_route_a.clone(),
                NodeRequest::Stop {
                    session,
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));

    shutdown_a.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), &mut task_a)
        .await
        .expect("node A shutdown timed out")
        .expect("node A task panicked")
        .expect("node A shutdown failed");
    assert!(!Path::new(&endpoint_a).exists());
    wait_status(&http, |status| {
        status.nodes[&id_a].transport != NodeTransportState::Online
            && status.nodes[&id_b].transport == NodeTransportState::Online
    })
    .await;
    assert!(matches!(
        control
            .request(route_b.clone(), NodeRequest::Snapshot)
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Snapshot { .. })
    ));

    let restarted_a =
        NodeServer::new_fixture(node_config(&id_a, &endpoint_a, token_a)).unwrap();
    shutdown_a = restarted_a.shutdown_handle();
    task_a = tokio::spawn(restarted_a.run());
    let recovered = wait_status(&http, |status| {
        status.nodes[&id_a].transport == NodeTransportState::Online
            && status.nodes[&id_b].transport == NodeTransportState::Online
            && status.nodes[&id_a].cursor.is_some_and(|cursor| {
                cursor.incarnation_id != stale_route_a.expected_incarnation_id
            })
    })
    .await;
    let stale = control
        .request(stale_route_a, NodeRequest::Snapshot)
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        C2ControlError::Relay(ref failure)
            if failure.code == C2RelayFailureCode::StaleNodeIncarnation
    ));
    let recovered_route_a = route(&recovered, &id_a);
    assert!(matches!(
        control
            .request(recovered_route_a, NodeRequest::Snapshot)
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Snapshot { .. })
    ));

    let c2_shutdown = running.shutdown_handle();
    let c2_api_addr = running.api_addr();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    assert!(!Path::new(&control_endpoint).exists());
    assert!(!matches!(
        timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect(c2_api_addr),
        )
        .await,
        Ok(Ok(_))
    ));
    drop(control);
    let event_count = timeout(Duration::from_secs(2), event_drain)
        .await
        .expect("C2 event stream did not close")
        .expect("C2 event task panicked");
    assert!(event_count > 0);

    let mut direct_a = LocalNodeClient::connect(
        &endpoint_a,
        &id_a,
        ClientRole::Operator,
        token_a,
    )
    .await
    .unwrap();
    assert!(matches!(
        direct_a.request(NodeRequest::Snapshot).await.unwrap(),
        NodeResponse::Snapshot { .. }
    ));
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                direct_a
                    .request(NodeRequest::AcquireController { lease_ms: 5_000 })
                    .await,
                Ok(NodeResponse::Controller {
                    controller: Some(_)
                })
            ) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("C2 did not release node A controller ownership");
    drop(direct_a);

    let mut direct_b = LocalNodeClient::connect(
        &endpoint_b,
        &id_b,
        ClientRole::Observer,
        token_b,
    )
    .await
    .unwrap();
    assert!(matches!(
        direct_b.request(NodeRequest::Snapshot).await.unwrap(),
        NodeResponse::Snapshot { .. }
    ));
    drop(direct_b);

    stop_node(shutdown_a, task_a).await;
    stop_node(shutdown_b, task_b).await;
    assert!(!Path::new(&endpoint_a).exists());
    assert!(!Path::new(&endpoint_b).exists());
}
