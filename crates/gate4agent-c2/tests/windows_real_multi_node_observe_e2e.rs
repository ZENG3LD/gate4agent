#![cfg(windows)]

use gate4agent_c2::protocol::{GapKind, NodeFreshness, NodeId, NodeTransportState};
use gate4agent_c2_client::{C2Client, C2ClientError};
use gate4agent_node::protocol::{ClientRole, NodeRequest, OpaqueHostPath, WorkspaceId};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::NamedPipeNodeClient;
use std::sync::atomic::{AtomicU64, Ordering};
use std::process::Stdio;
use std::os::windows::process::CommandExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

struct KillChildOnDrop(Option<tokio::process::Child>);

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
    format!(r"\\.\pipe\gate4agent-c2-{label}-{}-{nonce}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

fn node_config(node_id: &str, endpoint: &str, token: &str) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        NodeId::new(node_id).unwrap(),
        [WorkspaceConfig::new(WorkspaceId::new("primary").unwrap(), std::env::current_dir().unwrap()).unwrap()],
    ).unwrap()
}

async fn status_until(
    client: &C2Client,
    deadline: Duration,
    predicate: impl Fn(&gate4agent_c2::protocol::StatusResponse) -> bool,
) -> gate4agent_c2::protocol::StatusResponse {
    timeout(deadline, async {
        loop {
            if let Ok(status) = client.status().await {
                if predicate(&status) { return status; }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 status condition was not reached")
}

async fn client_when_healthy(address: std::net::SocketAddr, token: &str) -> C2Client {
    timeout(Duration::from_secs(5), async {
        loop {
            let client = C2Client::new(address, token).unwrap().with_deadline(Duration::from_millis(500));
            if client.health().await.is_ok() { return client; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 child did not expose health")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_real_multi_node_observe_only_c2_survives_partial_loss_restart_and_shutdown() {
    let endpoint_a = endpoint("a");
    let endpoint_b = endpoint("b");
    let control_endpoint = endpoint("control");
    let token_a = "node-a-token";
    let token_b = "node-b-token";
    let c2_token = "c2-api-token";
    let id_a = NodeId::new("node-a").unwrap();
    let id_b = NodeId::new("node-b").unwrap();

    let server_a = NodeServer::new_fixture(node_config(id_a.as_str(), &endpoint_a, token_a)).unwrap();
    let shutdown_a = server_a.shutdown_handle();
    let task_a = tokio::spawn(server_a.run());
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
    let client = client_when_healthy(c2_addr, c2_token).await;

    let health = client.health().await.unwrap();
    assert!(health.ok);
    assert_eq!(health.service, "gate4agent-c2");
    assert_eq!(health.pid, c2_pid);
    assert!(matches!(C2Client::new(c2_addr, "wrong-token").unwrap().status().await, Err(C2ClientError::Unauthorized)));

    let initial = status_until(&client, Duration::from_secs(8), |status| {
        status.ready && status.nodes.values().all(|node| node.transport == NodeTransportState::Online && node.freshness == NodeFreshness::Fresh)
    }).await;
    assert_eq!(initial.nodes.len(), 2);
    assert_eq!(client.ready().await.unwrap().online_nodes, 2);
    assert_eq!(initial.nodes[&id_a].inventory.as_ref().unwrap().workspace_count, 1);
    assert_eq!(initial.nodes[&id_b].inventory.as_ref().unwrap().workspace_count, 1);
    let initial_b_cursor = initial.nodes[&id_b].cursor;

    let mut operator_a = NamedPipeNodeClient::connect(&endpoint_a, &id_a, ClientRole::Operator, token_a).await.unwrap();
    operator_a.request(NodeRequest::AcquireController { lease_ms: 5_000 }).await.unwrap();
    operator_a.request(NodeRequest::RegisterWorkspace {
        workspace_id: WorkspaceId::new("added-by-operator").unwrap(),
        root: OpaqueHostPath::utf8(std::env::temp_dir().to_string_lossy().into_owned()).unwrap(),
    }).await.unwrap();
    drop(operator_a);
    let changed = status_until(&client, Duration::from_secs(5), |status| {
        status.nodes[&id_a].inventory.as_ref().is_some_and(|inventory| inventory.workspace_count == 2)
    }).await;
    assert_eq!(changed.nodes[&id_b].inventory.as_ref().unwrap().workspace_count, 1);
    assert_eq!(changed.nodes[&id_b].cursor, initial_b_cursor);

    shutdown_b.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), task_b).await.unwrap().unwrap().unwrap();
    let partial = status_until(&client, Duration::from_secs(8), |status| {
        status.nodes[&id_a].transport == NodeTransportState::Online
            && status.nodes[&id_b].transport != NodeTransportState::Online
    }).await;
    assert_eq!(partial.nodes[&id_a].transport, NodeTransportState::Online);
    assert!(client.ready().await.unwrap().ready);

    let restarted_b = NodeServer::new_fixture(node_config(id_b.as_str(), &endpoint_b, token_b)).unwrap();
    let restarted_shutdown_b = restarted_b.shutdown_handle();
    let restarted_task_b = tokio::spawn(restarted_b.run());
    let recovered = status_until(&client, Duration::from_secs(8), |status| {
        let node = &status.nodes[&id_b];
        node.transport == NodeTransportState::Online
            && node.freshness == NodeFreshness::Fresh
            && node.gaps.iter().any(|gap| gap.kind == GapKind::IncarnationChanged)
    }).await;
    assert!(recovered.nodes[&id_b].gaps.iter().any(|gap| gap.kind == GapKind::IncarnationChanged));

    c2_child.terminate().await;
    let post_exit_connect = timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(c2_addr)).await;
    assert!(!matches!(post_exit_connect, Ok(Ok(_))), "C2 port still accepted after exact child exit");
    let observer_a = NamedPipeNodeClient::connect(&endpoint_a, &id_a, ClientRole::Observer, token_a).await.unwrap();
    let observer_b = NamedPipeNodeClient::connect(&endpoint_b, &id_b, ClientRole::Observer, token_b).await.unwrap();
    assert_eq!(observer_a.hello().snapshot.node_id, id_a);
    assert_eq!(observer_b.hello().snapshot.node_id, id_b);
    drop(observer_a);
    drop(observer_b);

    shutdown_a.request_shutdown().await.unwrap();
    restarted_shutdown_b.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), task_a).await.unwrap().unwrap().unwrap();
    timeout(Duration::from_secs(5), restarted_task_b).await.unwrap().unwrap().unwrap();
}
