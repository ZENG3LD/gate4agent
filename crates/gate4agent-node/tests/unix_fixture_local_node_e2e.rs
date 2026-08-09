#![cfg(unix)]

use gate4agent_node::protocol::{
    AgentId, ClientRole, LocalTransportKind, NodeFailureCode, NodeId, NodeRequest,
    NodeResponse, NodeSnapshot, PathEncoding, PathStyle, SessionAddress, SessionMode, WorkspaceId,
    NODE_PROTOCOL_VERSION,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::{LocalNodeClient, NodeClientError};
use gate4agent_types::{SessionStatus, TerminalControl, TerminalSize};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{sleep, timeout, Duration, Instant};

static NEXT_TREE: AtomicU64 = AtomicU64::new(1);

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

struct TestTree {
    base: PathBuf,
    workspace: PathBuf,
    secondary: PathBuf,
    endpoint: PathBuf,
}

impl TestTree {
    fn create() -> Self {
        let serial = NEXT_TREE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "g4a-unix-node-e2e-{}-{serial}",
            std::process::id(),
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&base).unwrap();
        let workspace = base.join("workspace");
        let secondary = workspace.join("secondary");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&secondary).unwrap();
        Self {
            endpoint: base.join("node.sock"),
            base,
            workspace,
            secondary,
        }
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        let safe_name = self
            .base
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("g4a-unix-node-e2e-"));
        if safe_name {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

fn node_id() -> NodeId {
    NodeId::new("unix-fixture-node").unwrap()
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::new("primary").unwrap()
}

fn config(tree: &TestTree, token: &str) -> NodeServerConfig {
    NodeServerConfig::new(
        tree.endpoint.to_str().unwrap(),
        token,
        node_id(),
        [
            WorkspaceConfig::new(workspace_id(), &tree.workspace).unwrap(),
            WorkspaceConfig::new(WorkspaceId::new("secondary").unwrap(), &tree.secondary).unwrap(),
        ],
    )
    .unwrap()
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

async fn snapshot(client: &mut LocalNodeClient) -> NodeSnapshot {
    let NodeResponse::Snapshot { snapshot, .. } = client
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_production_socket_node_preserves_auth_controller_pty_reconnect_and_cleanup() {
    let tree = TestTree::create();
    assert_eq!(fs::metadata(&tree.base).unwrap().permissions().mode() & 0o7777, 0o700);
    let token = "unix-production-node-token";
    let server = NodeServer::new_fixture(config(&tree, token)).unwrap();
    let server_task = tokio::spawn(server.run());

    assert!(LocalNodeClient::connect(
        &tree.endpoint,
        &node_id(),
        ClientRole::Operator,
        "wrong-token",
    )
    .await
    .is_err());
    let identity_error = match LocalNodeClient::connect(
        &tree.endpoint,
        &NodeId::new("another-node").unwrap(),
        ClientRole::Observer,
        token,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("checked connect accepted another node identity"),
    };
    assert!(matches!(
        identity_error,
        NodeClientError::Protocol(ref message) if message.contains("node identity mismatch")
    ));

    let mut first = LocalNodeClient::connect(
        &tree.endpoint,
        &node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    assert_eq!(first.hello().protocol_version, NODE_PROTOCOL_VERSION);
    assert_eq!(first.hello().snapshot.workspaces.len(), 2);
    let compatibility = first.hello().compatibility.as_ref().unwrap();
    assert_eq!(compatibility.local_transport, LocalTransportKind::UnixDomainSocket);
    assert_eq!(compatibility.path_semantics.style, PathStyle::Posix);
    assert_eq!(compatibility.path_semantics.encoding, PathEncoding::Utf8);
    assert_eq!(compatibility.host.operating_system.as_str(), std::env::consts::OS);
    assert_eq!(compatibility.host.architecture.as_str(), std::env::consts::ARCH);
    let NodeResponse::Controller {
        controller: Some(first_controller),
    } = first
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap()
    else {
        panic!("first operator did not acquire controller");
    };
    assert_eq!(first_controller.connection_id, first.hello().connection_id);

    let mut second = LocalNodeClient::connect(
        &tree.endpoint,
        &node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    let busy = second
        .request(NodeRequest::AcquireController { lease_ms: 5_000 })
        .await
        .unwrap_err();
    assert!(matches!(
        busy,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::ControllerBusy
    ));
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match second
            .request(NodeRequest::AcquireController { lease_ms: 5_000 })
            .await
        {
            Ok(NodeResponse::Controller {
                controller: Some(_),
            }) => break,
            Err(NodeClientError::Node(ref failure))
                if failure.code == NodeFailureCode::ControllerBusy
                    && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(20)).await;
            }
            result => panic!("controller was not released after disconnect: {result:?}"),
        }
    }
    second.request(NodeRequest::ReleaseController).await.unwrap();
    drop(second);

    let mut operator = LocalNodeClient::connect(
        &tree.endpoint,
        &node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    let NodeResponse::Resync {
        events,
        snapshot: reconnected_snapshot,
        ..
    } = operator
        .request(NodeRequest::Resync { after_sequence: 0 })
        .await
        .unwrap()
    else {
        panic!("resync request returned another response");
    };
    assert!(events.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));
    assert!(reconnected_snapshot
        .workspaces
        .iter()
        .all(|workspace| workspace.sessions.is_empty()));
    operator
        .request(NodeRequest::AcquireController { lease_ms: 10_000 })
        .await
        .unwrap();

    let NodeResponse::SpawnAccepted { session } = operator
        .request(NodeRequest::Spawn {
            workspace_id: workspace_id(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("fixture spawn returned another response");
    };
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = snapshot(&mut operator).await;
        let ready = addressed_session(&current, &session).is_some_and(|item| {
            item.status == SessionStatus::Running
                && item
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains("fixture-ready>"))
        });
        if ready {
            break;
        }
        if Instant::now() >= ready_deadline {
            panic!("native Unix fixture PTY did not become ready");
        }
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        operator
            .request(NodeRequest::Input {
                session: session.clone(),
                text: "native-unix-ok".to_owned(),
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    assert_eq!(
        operator
            .request(NodeRequest::TerminalControl {
                session: session.clone(),
                control: TerminalControl::Enter,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let echo_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = snapshot(&mut operator).await;
        let echoed = addressed_session(&current, &session).is_some_and(|item| {
            item.terminal_frame.as_ref().is_some_and(|frame| {
                frame.contents.contains("fixture-echo:native-unix-ok")
            })
        });
        if echoed {
            break;
        }
        if Instant::now() >= echo_deadline {
            panic!("native Unix fixture PTY did not echo typed input");
        }
        sleep(Duration::from_millis(20)).await;
    }

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
    let stop_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = snapshot(&mut operator).await;
        let stopped = addressed_session(&current, &session).is_some_and(|item| {
            matches!(
                item.status,
                SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
            )
        });
        if stopped {
            break;
        }
        if Instant::now() >= stop_deadline {
            panic!("native Unix fixture PTY did not stop");
        }
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        operator.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    drop(operator);
    timeout(Duration::from_secs(10), server_task)
        .await
        .expect("native Unix node did not shut down")
        .expect("native Unix node task panicked")
        .expect("native Unix node failed");
    assert!(!tree.endpoint.exists(), "Unix socket survived node shutdown");
    assert!(tree.base.exists());
}
