#![cfg(windows)]

use gate4agent_node::protocol::{
    AgentId, ClientRole, NodeFailureCode, NodeId, NodeRequest, NodeResponse, NodeSnapshot,
    ProviderRuntimeMode, SessionAddress, SessionMode, SessionRecordId, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::{LocalNodeClient, NodeClientError};
use gate4agent_types::{SessionStatus, TerminalControl, TerminalSize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout, Duration};

fn endpoint() -> String {
    gate4agent_testkit::suppress_windows_fault_dialogs_for_test();
    gate4agent_testkit::require_windows_headless_supervisor_for_test();
    static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-runtime-policy-e2e-{}-{nonce}-{}",
        std::process::id(),
        NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed),
    )
}

fn node_id() -> NodeId {
    NodeId::new("runtime-policy-fixture-node").unwrap()
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
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

fn assert_raw_address_invariant(
    snapshot: &NodeSnapshot,
    address: &SessionAddress,
    expected_record_count: usize,
) {
    let matching = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.sessions.iter())
        .filter(|session| session.instance_id == address.session.instance_id)
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "raw session address changed or duplicated");
    assert_eq!(matching[0].generation, address.session.generation);
    assert_eq!(snapshot.session_records.len(), expected_record_count);
    assert!(addressed_session(snapshot, address).is_some());
}

fn assert_typed_failure(error: NodeClientError, expected: NodeFailureCode) {
    match error {
        NodeClientError::Node(failure) => assert_eq!(failure.code, expected),
        other => panic!("request failed outside the typed Node protocol: {other:?}"),
    }
}

async fn snapshot(client: &mut LocalNodeClient) -> NodeSnapshot {
    let NodeResponse::Snapshot { snapshot, .. } = client
        .request(NodeRequest::Snapshot)
        .await
        .expect("authenticated Snapshot failed")
    else {
        panic!("Snapshot returned another response");
    };
    snapshot
}

async fn wait_for_session<'a>(
    client: &'a mut LocalNodeClient,
    address: &SessionAddress,
    predicate: impl Fn(&gate4agent_types::SessionSnapshot) -> bool,
) -> NodeSnapshot {
    timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(client).await;
            if let Some(session) = addressed_session(&current, address) {
                if matches!(session.status, SessionStatus::Failed { .. }) {
                    panic!("fixture session failed: {:?}", session.status);
                }
                if predicate(session) {
                    return current;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture session did not reach the expected state")
}

struct RemoveFixtureDirectory(PathBuf);

impl Drop for RemoveFixtureDirectory {
    fn drop(&mut self) {
        let safe = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-runtime-policy-e2e-"));
        if safe {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn fixture_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "gate4agent-runtime-policy-e2e-{}-{nonce}",
        std::process::id(),
    ))
}

fn exact_cmd_launcher() -> String {
    let configured = std::env::var_os("ComSpec").expect("Windows ComSpec is unavailable");
    let launcher = std::fs::canonicalize(Path::new(&configured))
        .expect("Windows command processor is unavailable");
    launcher
        .into_os_string()
        .into_string()
        .expect("Windows command processor path is not Unicode")
}

fn assert_no_durable_managed_records(state_path: &Path) {
    if !state_path.exists() {
        return;
    }
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state_path).expect("durable Node state is unreadable"),
    )
    .expect("durable Node state is not valid JSON");
    let records = state
        .get("session_records")
        .and_then(serde_json::Value::as_array)
        .expect("durable Node state has no session_records array");
    assert!(records.is_empty(), "raw PTY created a durable managed record");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_raw_provider_runtime_policy_preserves_pty_and_rejects_semantics() {
    let endpoint = endpoint();
    let token = "runtime-policy-fixture-token";
    let fixture_root = fixture_root();
    std::fs::create_dir_all(&fixture_root).unwrap();
    let _fixture_cleanup = RemoveFixtureDirectory(fixture_root.clone());
    let state_path = fixture_root.join("node-state.json");
    let config = NodeServerConfig::new(
        &endpoint,
        token,
        node_id(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
    .with_state_path(&state_path)
    .unwrap();
    let server = NodeServer::new_exact_launcher_fixture(
        config,
        agent("claude"),
        exact_cmd_launcher(),
    )
    .unwrap();
    let server_task = tokio::spawn(server.run());

    let mut client = LocalNodeClient::connect(
        &endpoint,
        &node_id(),
        ClientRole::Operator,
        token,
    )
    .await
    .unwrap();
    let runtime_status = client
        .hello()
        .snapshot
        .provider_runtime_statuses
        .iter()
        .find(|status| status.provider().as_str() == "claude")
        .expect("fixture provider runtime status is missing");
    assert_eq!(runtime_status.mode(), ProviderRuntimeMode::RawPassthrough);
    client
        .request(NodeRequest::AcquireController { lease_ms: 30_000 })
        .await
        .unwrap();

    let initial_size = TerminalSize {
        rows: 24,
        columns: 80,
    };
    let NodeResponse::SpawnAccepted { session } = client
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: agent("claude"),
            mode: SessionMode::Pty,
            terminal_size: initial_size,
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("raw fixture spawn returned another response");
    };

    let running = wait_for_session(&mut client, &session, |current| {
        current.status == SessionStatus::Running
            && current.terminal_frame.as_ref().is_some_and(|frame| {
                !frame.contents.is_empty() && !frame.formatted.is_empty()
            })
    })
    .await;
    assert_raw_address_invariant(&running, &session, 0);
    assert!(running.session_records.iter().all(|record| {
        record.state != gate4agent_node::protocol::ManagedSessionState::IdentityPending
    }));
    assert_no_durable_managed_records(&state_path);

    let echo_marker = "fixture-echo:raw-runtime-policy";
    assert_eq!(
        client
            .request(NodeRequest::Input {
                session: session.clone(),
                text: format!("echo {echo_marker}"),
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    assert_eq!(
        client
            .request(NodeRequest::TerminalControl {
                session: session.clone(),
                control: TerminalControl::Enter,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let echoed = wait_for_session(&mut client, &session, |current| {
        current.status == SessionStatus::Running
            && current.terminal_frame.as_ref().is_some_and(|frame| {
                frame.contents.contains(echo_marker) && !frame.formatted.is_empty()
            })
    })
    .await;
    assert_raw_address_invariant(&echoed, &session, 0);

    for request in [
        NodeRequest::Prompt {
            session: session.clone(),
            text: "semantic prompt must be rejected".to_owned(),
        },
        NodeRequest::Paste {
            session: session.clone(),
            text: "semantic paste must be rejected".to_owned(),
        },
        NodeRequest::Resume {
            session: session.clone(),
            terminal_size: initial_size,
            initial_prompt: None,
        },
    ] {
        let error = client.request(request).await.unwrap_err();
        assert_typed_failure(error, NodeFailureCode::UnsupportedCapability);
    }
    let missing_record = SessionRecordId::new("raw-runtime-missing-record").unwrap();
    let error = client
        .request(NodeRequest::ResumeSessionRecord {
            record_id: missing_record,
            terminal_size: initial_size,
            initial_prompt: None,
        })
        .await
        .unwrap_err();
    assert_typed_failure(error, NodeFailureCode::UnknownSessionRecord);

    let after_rejections = snapshot(&mut client).await;
    assert_raw_address_invariant(&after_rejections, &session, 0);
    assert_eq!(
        addressed_session(&after_rejections, &session).unwrap().status,
        SessionStatus::Running,
    );
    assert_no_durable_managed_records(&state_path);

    let resized_size = TerminalSize {
        rows: 31,
        columns: 97,
    };
    assert_eq!(
        client
            .request(NodeRequest::Resize {
                session: session.clone(),
                size: resized_size,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let resized = wait_for_session(&mut client, &session, |current| {
        current.status == SessionStatus::Running
            && current.terminal_size == Some(resized_size)
    })
    .await;
    assert_raw_address_invariant(&resized, &session, 0);

    assert_eq!(
        client
            .request(NodeRequest::Interrupt {
                session: session.clone(),
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let after_interrupt = snapshot(&mut client).await;
    assert_raw_address_invariant(&after_interrupt, &session, 0);
    assert_eq!(
        addressed_session(&after_interrupt, &session).unwrap().status,
        SessionStatus::Running,
    );

    assert_eq!(
        client
            .request(NodeRequest::Stop {
                session: session.clone(),
                force: true,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let stopped = wait_for_session(&mut client, &session, |current| {
        matches!(current.status, SessionStatus::Exited { .. })
    })
    .await;
    assert_raw_address_invariant(&stopped, &session, 0);

    let final_snapshot = snapshot(&mut client).await;
    assert_raw_address_invariant(&final_snapshot, &session, 0);
    assert_no_durable_managed_records(&state_path);
    assert_eq!(
        client.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("fixture Node did not shut down")
        .expect("fixture Node task panicked")
        .expect("fixture Node failed");
    assert_no_durable_managed_records(&state_path);
}
