#![cfg(windows)]

use gate4agent_node::protocol::{
    AgentProvider, ClientRole, ManagedSessionRecord, ManagedSessionState, NodeFailureCode, NodeId,
    NodeEvent, NodeRequest, NodeResponse, OpaqueHostPath, SessionMode, SessionRecordId, WorkspaceId,
    NODE_STATE_SCHEMA_V1, NODE_STATE_SCHEMA_V2,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::{NamedPipeNodeClient, NodeClientError};
use gate4agent_types::{ControlEventKind, SessionStatus, TerminalControl, TerminalSize};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout, Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CHILD_MODE_ENV: &str = "GATE4AGENT_DURABLE_FIXTURE_CHILD";
const CHILD_ENDPOINT_ENV: &str = "GATE4AGENT_DURABLE_FIXTURE_ENDPOINT";
const CHILD_TOKEN_ENV: &str = "GATE4AGENT_DURABLE_FIXTURE_TOKEN";
const CHILD_WORKSPACE_ENV: &str = "GATE4AGENT_DURABLE_FIXTURE_WORKSPACE";
const CHILD_STATE_ENV: &str = "GATE4AGENT_DURABLE_FIXTURE_STATE";
const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_CANARY";

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("fixture child was already consumed")
    }

    async fn wait_success(mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child_mut().try_wait().unwrap() {
                if status.success() {
                    self.0.take();
                    return;
                }
                let diagnostics = bounded_child_diagnostics(self.0.take().unwrap());
                panic!("fixture node child failed with {status}: {diagnostics}");
            }
            if Instant::now() >= deadline {
                panic!("fixture node child did not exit after shutdown");
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_failure(mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child_mut().try_wait().unwrap() {
                assert!(!status.success(), "fixture node unexpectedly started successfully");
                return bounded_child_diagnostics(self.0.take().unwrap());
            }
            if Instant::now() >= deadline {
                panic!("fixture node did not reject incompatible durable state");
            }
            sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        if self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-durable-node-e2e-"))
        {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn unique_root() -> TempRoot {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "gate4agent-durable-node-e2e-{}-{nonce}-{sequence}",
        std::process::id(),
    ));
    std::fs::create_dir_all(root.join("primary")).unwrap();
    std::fs::create_dir_all(root.join("persisted-secondary")).unwrap();
    TempRoot(root)
}

fn endpoint() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-durable-e2e-{}-{nonce}",
        std::process::id(),
    )
}

fn spawn_fixture_node(endpoint: &str, token: &str, workspace: &Path, state: &Path) -> ChildGuard {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "durable_fixture_node_child_process",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_MODE_ENV, "1")
        .env(CHILD_ENDPOINT_ENV, endpoint)
        .env(CHILD_TOKEN_ENV, token)
        .env(CHILD_WORKSPACE_ENV, workspace)
        .env(CHILD_STATE_ENV, state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
    ChildGuard(Some(child))
}

async fn connect(
    endpoint: &str,
    token: &str,
) -> NamedPipeNodeClient {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match NamedPipeNodeClient::connect(
            endpoint,
            &NodeId::new("durable-fixture-node").unwrap(),
            ClientRole::Operator,
            token,
        )
        .await
        {
            Ok(client) => return client,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("fixture node did not accept a bounded connection: {error}"),
        }
    }
}

async fn snapshot(client: &mut NamedPipeNodeClient) -> gate4agent_node::protocol::NodeSnapshot {
    let NodeResponse::Snapshot { snapshot, .. } = client
        .request(NodeRequest::Snapshot)
        .await
        .unwrap()
    else {
        panic!("snapshot request returned another response");
    };
    snapshot
}

async fn wait_record(
    client: &mut NamedPipeNodeClient,
    record_id: Option<&SessionRecordId>,
    state: ManagedSessionState,
) -> ManagedSessionRecord {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = snapshot(client).await;
        if let Some(record) = snapshot.session_records.into_iter().find(|record| {
            record_id.is_none_or(|record_id| &record.record_id == record_id)
                && record.state == state
        }) {
            return record;
        }
        if Instant::now() >= deadline {
            panic!("managed session did not reach {state:?}");
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn stop_record(client: &mut NamedPipeNodeClient, record: &ManagedSessionRecord) {
    let active = record
        .active_session
        .clone()
        .expect("live managed record has no runtime address");
    assert_eq!(
        client
            .request(NodeRequest::Stop {
                session: active,
                force: true,
            })
            .await
            .unwrap(),
        NodeResponse::Accepted,
    );
    let _ = wait_record(client, Some(&record.record_id), ManagedSessionState::Dormant).await;
}

#[test]
#[ignore = "spawned only by the durable restart E2E parent"]
fn durable_fixture_node_child_process() {
    if std::env::var(CHILD_MODE_ENV).ok().as_deref() != Some("1") {
        return;
    }
    let endpoint = std::env::var(CHILD_ENDPOINT_ENV).unwrap();
    let token = std::env::var(CHILD_TOKEN_ENV).unwrap();
    let workspace = PathBuf::from(std::env::var_os(CHILD_WORKSPACE_ENV).unwrap());
    let state = PathBuf::from(std::env::var_os(CHILD_STATE_ENV).unwrap());
    let config = NodeServerConfig::new(
        endpoint,
        token,
        NodeId::new("durable-fixture-node").unwrap(),
        [WorkspaceConfig::new(WorkspaceId::new("primary").unwrap(), workspace).unwrap()],
    )
    .unwrap()
    .with_state_path(state)
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(NodeServer::new_resume_fixture(config).unwrap().run())
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_durable_record_survives_process_restart_and_cold_resumes() {
    let root = unique_root();
    let primary = root.0.join("primary");
    let secondary = root.0.join("persisted-secondary");
    let state = root.0.join("state").join("state-v1.json");
    let endpoint = endpoint();
    let token = "durable-fixture-token";

    let first_child = spawn_fixture_node(&endpoint, token, &primary, &state);
    let mut first = connect(&endpoint, token).await;
    let first_incarnation = first.hello().incarnation_id;
    first
        .request(NodeRequest::AcquireController { lease_ms: 15_000 })
        .await
        .unwrap();
    let secondary_id = WorkspaceId::new("persisted-secondary").unwrap();
    first
        .request(NodeRequest::RegisterWorkspace {
            workspace_id: secondary_id.clone(),
            root: OpaqueHostPath::utf8(secondary.to_string_lossy().into_owned()).unwrap(),
        })
        .await
        .unwrap();
    let NodeResponse::SpawnAccepted { session: fresh_address } = first
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider: AgentProvider::Claude,
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
    let live = wait_record(&mut first, None, ManagedSessionState::Live).await;
    assert_eq!(live.active_session.as_ref(), Some(&fresh_address));
    let identity = live
        .provider_session
        .clone()
        .expect("fixture did not publish a strict provider session identity");
    let record_id = live.record_id.clone();
    let NodeResponse::SessionRecordUpdated { record: renamed } = first
        .request(NodeRequest::RenameSessionRecord {
            record_id: record_id.clone(),
            display_name: "release shepherd".to_owned(),
        })
        .await
        .unwrap()
    else {
        panic!("rename returned another response");
    };
    assert_eq!(renamed.display_name, "release shepherd");
    stop_record(&mut first, &renamed).await;
    assert_eq!(
        first.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    drop(first);
    first_child.wait_success().await;

    let second_child = spawn_fixture_node(&endpoint, token, &primary, &state);
    let mut second = connect(&endpoint, token).await;
    assert_ne!(second.hello().incarnation_id, first_incarnation);
    second
        .request(NodeRequest::AcquireController { lease_ms: 15_000 })
        .await
        .unwrap();
    let restored_snapshot = snapshot(&mut second).await;
    assert!(restored_snapshot
        .workspaces
        .iter()
        .any(|workspace| workspace.workspace_id == secondary_id));
    let restored = restored_snapshot
        .session_records
        .iter()
        .find(|record| record.record_id == record_id)
        .cloned()
        .expect("durable record was not restored");
    assert_eq!(restored.display_name, "release shepherd");
    assert_eq!(restored.state, ManagedSessionState::Dormant);
    assert_eq!(restored.provider_session.as_ref(), Some(&identity));
    assert!(restored.active_session.is_none());

    let NodeResponse::SessionRecordResumed {
        record: resumed,
        session: resumed_address,
    } = second
        .request(NodeRequest::ResumeSessionRecord {
            record_id: record_id.clone(),
            terminal_size: TerminalSize {
                rows: 31,
                columns: 97,
            },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("cold resume returned another response");
    };
    assert_eq!(resumed.record_id, record_id);
    assert_eq!(resumed.provider_session.as_ref(), Some(&identity));
    assert_eq!(resumed.active_session.as_ref(), Some(&resumed_address));
    assert!(resumed_address.session.generation.0 > 0);
    assert_eq!(resumed.state, ManagedSessionState::Live);

    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = snapshot(&mut second).await;
        let ready = current
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.sessions.iter())
            .find(|session| {
                session.instance_id == resumed_address.session.instance_id
                    && session.generation == resumed_address.session.generation
            })
            .and_then(|session| session.terminal_frame.as_ref())
            .is_some_and(|frame| frame.contents.contains("fixture-ready>"));
        if ready {
            break;
        }
        if Instant::now() >= ready_deadline {
            panic!("cold-resumed PTY did not publish its readiness frame");
        }
        sleep(Duration::from_millis(20)).await;
    }

    let duplicate = second
        .request(NodeRequest::ResumeSessionRecord {
            record_id: record_id.clone(),
            terminal_size: TerminalSize {
                rows: 31,
                columns: 97,
            },
            initial_prompt: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::SessionRecordBusy
    ));
    second
        .request(NodeRequest::Prompt {
            session: resumed_address.clone(),
            text: "durable-resume-ok".to_owned(),
        })
        .await
        .unwrap();
    let NodeResponse::Resync { events, .. } = second
        .request(NodeRequest::Resync { after_sequence: 0 })
        .await
        .unwrap()
    else {
        panic!("post-prompt resync returned another response");
    };
    assert!(events.iter().any(|envelope| matches!(
        &envelope.event,
        NodeEvent::Control { address, event }
            if address == &resumed_address
                && matches!(event.event, ControlEventKind::InputCompleted { .. })
    )));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = snapshot(&mut second).await;
        let session = current
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.sessions.iter())
            .find(|session| {
                session.instance_id == resumed_address.session.instance_id
                    && session.generation == resumed_address.session.generation
            });
        if session.is_some_and(|session| {
            session.status == SessionStatus::Running
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains("fixture-echo:durable-resume-ok"))
        }) {
            break;
        }
        if Instant::now() >= deadline {
            panic!("cold-resumed PTY did not accept and echo the safe prompt");
        }
        sleep(Duration::from_millis(20)).await;
    }

    stop_record(&mut second, &resumed).await;
    let busy_forget = second
        .request(NodeRequest::ResumeSessionRecord {
            record_id: record_id.clone(),
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            initial_prompt: None,
        })
        .await
        .unwrap();
    let NodeResponse::SessionRecordResumed {
        record: live_again, ..
    } = busy_forget
    else {
        panic!("second cold resume returned another response");
    };
    let forget_live = second
        .request(NodeRequest::ForgetSessionRecord {
            record_id: record_id.clone(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        forget_live,
        NodeClientError::Node(ref failure) if failure.code == NodeFailureCode::SessionRecordBusy
    ));
    stop_record(&mut second, &live_again).await;
    let NodeResponse::SessionRecordForgotten { record_id: forgotten } = second
        .request(NodeRequest::ForgetSessionRecord {
            record_id: record_id.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("forget returned another response");
    };
    assert_eq!(forgotten, record_id);
    assert!(snapshot(&mut second)
        .await
        .session_records
        .iter()
        .all(|record| record.record_id != forgotten));
    assert_eq!(
        second.request(NodeRequest::Shutdown).await.unwrap(),
        NodeResponse::ShuttingDown,
    );
    drop(second);
    second_child.wait_success().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_node_refuses_unknown_newer_state_schema_without_rewrite() {
    let root = unique_root();
    let primary = root.0.join("primary");
    let state = root.0.join("state").join("state-v1.json");
    let backup = state.with_file_name(".state-v1.json.bak");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let future_state = serde_json::to_vec_pretty(&serde_json::json!({
        "version": NODE_STATE_SCHEMA_V2 + 1,
        "node_id": "durable-fixture-node",
        "workspaces": [],
        "session_records": [],
    }))
    .unwrap();
    let valid_backup = serde_json::to_vec_pretty(&serde_json::json!({
        "version": NODE_STATE_SCHEMA_V1,
        "node_id": "durable-fixture-node",
        "workspaces": [],
        "session_records": [],
    }))
    .unwrap();
    std::fs::write(&state, &future_state).unwrap();
    std::fs::write(&backup, &valid_backup).unwrap();

    let child = spawn_fixture_node(
        &endpoint(),
        "durable-schema-fixture-token",
        &primary,
        &state,
    );
    let diagnostics = child.wait_failure().await;
    assert!(diagnostics.contains("durable-state-schema-unsupported"));
    assert!(!diagnostics.contains(&state.to_string_lossy().into_owned()));
    assert_eq!(std::fs::read(&state).unwrap(), future_state);
    assert_eq!(std::fs::read(&backup).unwrap(), valid_backup);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_node_refuses_v2_unix_bytes_state_without_backup_fallback() {
    let root = unique_root();
    let primary = root.0.join("primary");
    let state = root.0.join("state").join("state-v1.json");
    let backup = state.with_file_name(".state-v1.json.bak");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let foreign_state = serde_json::to_vec_pretty(&serde_json::json!({
        "version": NODE_STATE_SCHEMA_V2,
        "node_id": "durable-fixture-node",
        "workspaces": [{
            "workspace_id": "foreign",
            "canonical_root": {
                "kind": "unix-bytes",
                "bytes": [47, 115, 114, 118, 47, 114, 101, 112, 111],
            },
        }],
        "session_records": [],
    }))
    .unwrap();
    let valid_backup = serde_json::to_vec_pretty(&serde_json::json!({
        "version": NODE_STATE_SCHEMA_V1,
        "node_id": "durable-fixture-node",
        "workspaces": [],
        "session_records": [],
    }))
    .unwrap();
    std::fs::write(&state, &foreign_state).unwrap();
    std::fs::write(&backup, &valid_backup).unwrap();

    let child = spawn_fixture_node(
        &endpoint(),
        "durable-path-semantics-fixture-token",
        &primary,
        &state,
    );
    let diagnostics = child.wait_failure().await;
    assert!(diagnostics.contains("durable-state-path-semantics-unsupported"));
    assert!(!diagnostics.contains(&state.to_string_lossy().into_owned()));
    assert_eq!(std::fs::read(&state).unwrap(), foreign_state);
    assert_eq!(std::fs::read(&backup).unwrap(), valid_backup);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_fixture_node_refuses_foreign_node_state_without_rewrite() {
    let root = unique_root();
    let primary = root.0.join("primary");
    let state = root.0.join("state").join("state-v1.json");
    let backup = state.with_file_name(".state-v1.json.bak");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let foreign_state = serde_json::to_vec_pretty(&serde_json::json!({
        "version": NODE_STATE_SCHEMA_V1,
        "node_id": "another-node",
        "workspaces": [],
        "session_records": [],
    }))
    .unwrap();
    let valid_backup = serde_json::to_vec_pretty(&serde_json::json!({
        "version": NODE_STATE_SCHEMA_V1,
        "node_id": "durable-fixture-node",
        "workspaces": [],
        "session_records": [],
    }))
    .unwrap();
    std::fs::write(&state, &foreign_state).unwrap();
    std::fs::write(&backup, &valid_backup).unwrap();

    let child = spawn_fixture_node(
        &endpoint(),
        "durable-identity-fixture-token",
        &primary,
        &state,
    );
    let diagnostics = child.wait_failure().await;
    assert!(diagnostics.contains("durable-state-conflict"));
    assert!(!diagnostics.contains(&state.to_string_lossy().into_owned()));
    assert_eq!(std::fs::read(&state).unwrap(), foreign_state);
    assert_eq!(std::fs::read(&backup).unwrap(), valid_backup);
}

fn bounded_child_diagnostics(mut child: Child) -> String {
    let mut bytes = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(8 * 1024).read_to_end(&mut bytes);
    }
    if bytes.is_empty() {
        if let Some(stdout) = child.stdout.take() {
            let _ = stdout.take(8 * 1024).read_to_end(&mut bytes);
        }
    }
    String::from_utf8_lossy(&bytes)
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn live_challenge(provider: AgentProvider, phase: &str) -> (String, String) {
    let provider = provider.agent_id().to_ascii_uppercase();
    let phase = phase.to_ascii_uppercase();
    let marker = format!("G4A{provider}NODE{phase}OK");
    let prompt = format!(
        "Reply with one string only: concatenate G4A, {provider}, NODE, {phase}, and OK with no separators.",
    );
    assert!(!prompt.contains(&marker));
    (marker, prompt)
}

async fn wait_live_terminal_stable(
    client: &mut NamedPipeNodeClient,
    address: &gate4agent_node::protocol::SessionAddress,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_sequence = None;
    let mut stable_samples = 0_u8;
    loop {
        let current = snapshot(client).await;
        let session = current
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.sessions.iter())
            .find(|session| {
                session.instance_id == address.session.instance_id
                    && session.generation == address.session.generation
            });
        if let Some(session) = session {
            if matches!(session.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. }) {
                panic!(
                    "live resumed provider stopped before composer stability; status={:?} frame_present={} terminal_stale={}",
                    session.status,
                    session.terminal_frame.is_some(),
                    session.terminal_stale.is_some(),
                );
            }
            if let Some(sequence) = session.terminal_frame.as_ref().map(|frame| frame.sequence) {
                if last_sequence == Some(sequence) {
                    stable_samples = stable_samples.saturating_add(1);
                } else {
                    last_sequence = Some(sequence);
                    stable_samples = 0;
                }
                if stable_samples >= 3 {
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            panic!("live resumed provider terminal did not stabilize before input");
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn connect_live(
    endpoint: &str,
    node_id: &NodeId,
    token: &str,
) -> NamedPipeNodeClient {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match NamedPipeNodeClient::connect(endpoint, node_id, ClientRole::Operator, token).await {
            Ok(client) => return client,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                sleep(Duration::from_millis(25)).await;
            }
            Err(error) => panic!("live node did not accept a bounded connection: {error}"),
        }
    }
}

async fn wait_live_marker(
    client: &mut NamedPipeNodeClient,
    address: &gate4agent_node::protocol::SessionAddress,
    marker: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let current = snapshot(client).await;
        let session = current
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.sessions.iter())
            .find(|session| {
                session.instance_id == address.session.instance_id
                    && session.generation == address.session.generation
            });
        if session.is_some_and(|session| {
            session
                .terminal_frame
                .as_ref()
                .is_some_and(|frame| frame.contents.contains(marker))
        }) {
            return;
        }
        if let Some(session) = session {
            if matches!(session.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. }) {
                panic!(
                    "live provider stopped before marker; status={:?} frame_present={} terminal_stale={} frame_flags={}",
                    session.status,
                    session.terminal_frame.is_some(),
                    session.terminal_stale.is_some(),
                    session
                        .terminal_frame
                        .as_ref()
                        .map_or_else(|| "none".to_owned(), |frame| terminal_flags(&frame.contents)),
                );
            }
        }
        if Instant::now() >= deadline {
            let record = current.session_records.first();
            panic!(
                "live provider marker timeout; record_state={:?} identity_present={} active_present={}",
                record.map(|record| record.state),
                record.and_then(|record| record.provider_session.as_ref()).is_some(),
                record.and_then(|record| record.active_session.as_ref()).is_some(),
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn terminal_flags(contents: &str) -> String {
    let normalized = contents.to_ascii_lowercase();
    let flags = [
        ("error", "error"),
        ("update", "update"),
        ("login", "login"),
        ("sign-in", "sign in"),
        ("trust", "trust"),
        ("usage-limit", "usage limit"),
        ("rate-limit", "rate limit"),
        ("api-error", "api error"),
        ("authentication", "authentication"),
        ("auth", "auth"),
        ("network", "network"),
        ("offline", "offline"),
        ("unavailable", "unavailable"),
        ("overloaded", "overloaded"),
        ("retry", "try again"),
        ("nested", "nested"),
        ("already-running", "already running"),
        ("failed", "failed"),
        ("invalid-session", "invalid session"),
        ("not-found", "not found"),
        ("in-use", "in use"),
        ("permission", "permission"),
    ]
    .into_iter()
    .filter_map(|(name, needle)| normalized.contains(needle).then_some(name))
    .collect::<Vec<_>>();
    if flags.is_empty() {
        "none".to_owned()
    } else {
        flags.join(",")
    }
}

async fn wait_live_record(client: &mut NamedPipeNodeClient) -> ManagedSessionRecord {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let current = snapshot(client).await;
        if let Some(record) = current
            .session_records
            .iter()
            .find(|record| record.state == ManagedSessionState::Live)
        {
            return record.clone();
        }
        if let Some(session) = current
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.sessions.iter())
            .next()
        {
            if matches!(session.status, SessionStatus::Failed { .. } | SessionStatus::Exited { .. }) {
                panic!(
                    "live provider stopped before identity; status={:?} identity_present={} frame_present={} terminal_stale={}",
                    session.status,
                    session.provider.session.is_some(),
                    session.terminal_frame.is_some(),
                    session.terminal_stale.is_some(),
                );
            }
        }
        if Instant::now() >= deadline {
            let record = current.session_records.first();
            panic!(
                "live provider identity timeout; record_state={:?} identity_present={} active_present={}",
                record.map(|record| record.state),
                record.and_then(|record| record.provider_session.as_ref()).is_some(),
                record.and_then(|record| record.active_session.as_ref()).is_some(),
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn live_vendor_durable_restart_canary(provider: AgentProvider) {
    if std::env::var(LIVE_CANARY_ENV).ok().as_deref() != Some("1") {
        eprintln!("skipped: set {LIVE_CANARY_ENV}=1 to run authenticated vendor node canaries");
        return;
    }
    let root = unique_root();
    // Vendor CLIs may require an operator-approved workspace. Reuse the current
    // already-approved checkout without mutating vendor trust configuration;
    // pipe, node state, and all durable artifacts remain isolated below `root`.
    let workspace = std::env::current_dir().unwrap();
    let state = root.0.join("state").join("state-v1.json");
    let endpoint = endpoint();
    let token = format!("live-{}-durable-token", provider.agent_id());
    let node_id = NodeId::new(format!("live-{}-node", provider.agent_id())).unwrap();
    let config = || {
        NodeServerConfig::new(
            endpoint.clone(),
            token.clone(),
            node_id.clone(),
            [WorkspaceConfig::new(WorkspaceId::new("primary").unwrap(), &workspace).unwrap()],
        )
        .unwrap()
        .with_state_path(&state)
        .unwrap()
    };

    let first_server = NodeServer::new(config()).unwrap();
    let first_task = tokio::spawn(first_server.run());
    let mut first = connect_live(&endpoint, &node_id, &token).await;
    let first_incarnation = first.hello().incarnation_id;
    first
        .request(NodeRequest::AcquireController { lease_ms: 60_000 })
        .await
        .unwrap();
    let (fresh_marker, fresh_prompt) = live_challenge(provider, "fresh");
    let NodeResponse::SpawnAccepted { session: fresh_address } = first
        .request(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            provider,
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 32,
                columns: 132,
            },
            initial_prompt: Some(fresh_prompt),
        })
        .await
        .unwrap()
    else {
        panic!("live spawn returned another response");
    };
    let live = wait_live_record(&mut first).await;
    let identity = live
        .provider_session
        .clone()
        .expect("live provider did not publish a strict session identity");
    wait_live_marker(&mut first, &fresh_address, &fresh_marker).await;
    let record_id = live.record_id.clone();
    stop_record(&mut first, &live).await;
    first.request(NodeRequest::Shutdown).await.unwrap();
    drop(first);
    timeout(Duration::from_secs(30), first_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let second_server = NodeServer::new(config()).unwrap();
    let second_task = tokio::spawn(second_server.run());
    let mut second = connect_live(&endpoint, &node_id, &token).await;
    assert_ne!(second.hello().incarnation_id, first_incarnation);
    second
        .request(NodeRequest::AcquireController { lease_ms: 60_000 })
        .await
        .unwrap();
    let (resume_marker, resume_prompt) = live_challenge(provider, "resume");
    let NodeResponse::SessionRecordResumed {
        record: resumed,
        session: resumed_address,
    } = second
        .request(NodeRequest::ResumeSessionRecord {
            record_id: record_id.clone(),
            terminal_size: TerminalSize {
                rows: 32,
                columns: 132,
            },
            initial_prompt: None,
        })
        .await
        .unwrap()
    else {
        panic!("live cold resume returned another response");
    };
    assert_eq!(resumed.provider_session.as_ref(), Some(&identity));
    wait_live_terminal_stable(&mut second, &resumed_address).await;
    second
        .request(NodeRequest::Input {
            session: resumed_address.clone(),
            text: resume_prompt,
        })
        .await
        .unwrap();
    second
        .request(NodeRequest::TerminalControl {
            session: resumed_address.clone(),
            control: TerminalControl::Enter,
        })
        .await
        .unwrap();
    wait_live_marker(&mut second, &resumed_address, &resume_marker).await;
    stop_record(&mut second, &resumed).await;
    second
        .request(NodeRequest::ForgetSessionRecord { record_id })
        .await
        .unwrap();
    second.request(NodeRequest::Shutdown).await.unwrap();
    drop(second);
    timeout(Duration::from_secs(30), second_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    println!(
        "vendor_node_durable_canary provider={} fresh_identity=true node_runtime_restart=true cold_resume_identity=true resumed_prompt=true cleanup=true",
        provider.agent_id(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authenticated Claude CLI and GATE4AGENT_VENDOR_CANARY=1"]
async fn windows_live_claude_durable_session_restart_canary() {
    live_vendor_durable_restart_canary(AgentProvider::Claude).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authenticated Codex CLI and GATE4AGENT_VENDOR_CANARY=1"]
async fn windows_live_codex_durable_session_restart_canary() {
    live_vendor_durable_restart_canary(AgentProvider::Codex).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authenticated Kimi CLI and GATE4AGENT_VENDOR_CANARY=1"]
async fn windows_live_kimi_durable_session_restart_canary() {
    live_vendor_durable_restart_canary(AgentProvider::Kimi).await;
}
