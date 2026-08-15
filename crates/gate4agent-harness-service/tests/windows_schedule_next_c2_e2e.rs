#![cfg(windows)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::HarnessOperatorCredential;
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_protocol::{
    HarnessCreateTaskRequestV1, HarnessExecutionModeV1, HarnessIdempotencyRef,
    HarnessOperationId, HarnessOperatorAuthorityV1, HarnessRevision,
    HarnessRunLifecycleV1, HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1,
    HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1, HarnessWorktreeIntentV1,
};
use gate4agent_harness_service::{
    c2::HarnessC2Adapter,
    dispatch::{
        HarnessContinuationPolicyV1, HarnessGrantPolicyV1, HarnessLaunchCatalog,
        HarnessLaunchPlanV1, HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    runtime::{
        start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs,
    },
    HarnessService,
};
use gate4agent_node::protocol::{
    ManagedSessionState, NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId,
    SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

struct FixturePaths {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    started: PathBuf,
    release: PathBuf,
    node_state: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-schedule-next-c2-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        fs::create_dir(&root).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            started: root.join("started.marker"),
            release: root.join("release.signal"),
            node_state: root.join("node-state.json"),
            root,
        }
    }
}

impl Drop for FixturePaths {
    fn drop(&mut self) {
        if self.root.is_dir() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn require_headless_supervisor() {
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows PTY tests must run through windows-headless-supervisor",
    );
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
        .as_millis().try_into().unwrap()
}

fn pipe(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        r"\\.\pipe\gate4agent-schedule-next-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn authority(marker: char, now: u64) -> HarnessOperatorAuthorityV1 {
    HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!(
            "hop_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        actor_id: selector("operator"),
        now_unix_ms: now,
    }
}

fn launch_catalog(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchCatalog {
    HarnessLaunchCatalog::new([HarnessLaunchPlanV1 {
        plan_id: selector("default"),
        revision: HarnessRevision::new(1).unwrap(),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("clean-exit"),
        provider: AgentId::new("claude").unwrap(),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 20_000,
    }]).unwrap()
}

fn session_count(snapshot: &gate4agent_c2_protocol::C2NodeSnapshot) -> usize {
    snapshot.workspaces.iter().map(|workspace| workspace.sessions.len()).sum()
}

fn assert_private_bytes_absent(root: &Path, canaries: &[&str]) {
    for suffix in [
        "harness.sqlite3", "harness.sqlite3-wal", "harness.sqlite3-shm",
        "observation.sqlite3", "observation.sqlite3-wal", "observation.sqlite3-shm",
        "node-state.json",
    ] {
        let path = root.join(suffix);
        if !path.is_file() { continue; }
        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for canary in canaries {
            assert!(!text.contains(canary), "private canary persisted in {}", path.display());
        }
    }
}

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
) -> (HarnessC2Adapter, gate4agent_harness_service::c2::HarnessC2EventReceiver) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) = HarnessC2Adapter::connect(endpoint, token).await {
                break connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 did not release its sole Harness operator")
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.ok().and_then(|status| status.nodes.get(node_id)
                .map(|node| node.transport == gate4agent_c2_protocol::NodeTransportState::Online))
                == Some(true)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Node did not become online through C2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_next_replay_and_harness_restart_do_not_resend() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("schedule-next-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "node-secret-schedule-next-canary";
    let c2_token = "c2-secret-schedule-next-canary";
    let operator_secret = format!("g4aho_{}", "c".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();
    let workspace = std::env::current_dir().unwrap();

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("clean-exit").unwrap(),
        revision: SpawnProfileRevision::new("schedule-next-r1").unwrap(),
        provider: AgentId::new("claude").unwrap(),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }]).unwrap();
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), workspace.clone()).unwrap()],
    ).unwrap()
        .with_state_path(fixture.node_state.clone()).unwrap()
        .with_spawn_profiles(profiles);
    let node = NodeServer::new_clean_exit_fixture(
        node_config,
        fixture.root.clone(),
        fixture.started.clone(),
        fixture.release.clone(),
    ).unwrap();
    let node_shutdown = node.shutdown_handle();
    let node_task = tokio::spawn(node.run());

    let timings = C2Timings {
        poll_interval: Duration::from_millis(20),
        fresh_for: Duration::from_secs(2),
        attempt_deadline: Duration::from_secs(2),
        transient_backoffs: [Duration::from_millis(20); 5],
        parked_backoff: Duration::from_millis(100),
        http_io_deadline: Duration::from_secs(1),
    };
    let c2 = C2Running::start(C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(node_id.clone(), node_endpoint, node_token).unwrap()],
    ).unwrap().with_control_endpoint(control_endpoint.clone()).unwrap().with_timings(timings)).await.unwrap();
    let c2_client = C2Client::new(c2.api_addr(), c2_token).unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    let task_id = HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap();
    let body = "durable schedule-next fixture body";
    let catalogs = HarnessRuntimeCatalogs::new(
        launch_catalog(&node_id, &workspace_id),
        DeliveryCatalogV2::default(),
    ).unwrap();
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs.clone(),
    ).await.unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(), operator_credential.clone(),
    ).unwrap();
    assert_eq!(client.create_task(HarnessCreateTaskRequestV1 {
        authority: authority('a', unix_time_ms()),
        task_id: task_id.clone(),
        title: "Schedule next restart fixture".to_owned(),
        body: body.to_owned(),
        parent_task_id: None,
        dependencies: Vec::new(),
        initial_state: HarnessTaskStateV1::Ready,
    }).unwrap(), gate4agent_harness_api::HarnessOperatorMutationOutcomeV1::Applied);
    let schedule = HarnessScheduleNextRequestV1 {
        authority: authority('b', unix_time_ms()),
        plan_id: Some(selector("default")),
    };
    let dispatch = client.schedule_next(schedule.clone()).unwrap();
    assert!(matches!(dispatch, HarnessScheduleOutcomeV1::Dispatch(_)));
    assert_eq!(client.schedule_next(schedule).unwrap(), dispatch);

    timeout(Duration::from_secs(15), async {
        loop {
            if fs::read(&fixture.started).ok().as_deref() == Some(b"started\n") { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("provider did not create exact started marker");
    let run_id = timeout(Duration::from_secs(10), async {
        loop {
            let tasks = client.tasks_list(None, None, 10).unwrap();
            let task = tasks.tasks.iter().find(|task| task.task_id == task_id).unwrap();
            if task.state == HarnessTaskStateV1::Running {
                let run_id = task.run_ids.first().cloned().unwrap();
                if client.run_get(run_id.clone()).unwrap().lifecycle == HarnessRunLifecycleV1::Running {
                    break run_id;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("run/task did not become Running");

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    assert!(!node_task.is_finished());
    assert!(c2_client.ready().await.unwrap().ready);
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let route = adapter.exact_route(&node_id).unwrap();
    let live_snapshot = adapter.snapshot(&route).await.unwrap();
    assert_eq!(session_count(&live_snapshot), 1);
    assert_eq!(live_snapshot.session_records.len(), 1);
    let record_id = live_snapshot.session_records[0].record_id.clone();
    assert_eq!(live_snapshot.session_records[0].state, ManagedSessionState::Live);
    assert!(live_snapshot.session_records[0].active_session.is_some());
    drop(adapter);
    drop(events);

    OpenOptions::new().write(true).create_new(true).open(&fixture.release).unwrap()
        .write_all(b"release\n").unwrap();
    sleep(Duration::from_millis(500)).await;

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let route = adapter.exact_route(&node_id).unwrap();
    let terminal_snapshot = adapter.snapshot(&route).await.unwrap();
    assert_eq!(terminal_snapshot.session_records.len(), 1);
    assert_eq!(terminal_snapshot.session_records[0].record_id, record_id);
    assert_eq!(terminal_snapshot.session_records[0].state, ManagedSessionState::Unavailable);
    assert!(terminal_snapshot.session_records[0].active_session.is_none());
    assert_eq!(session_count(&terminal_snapshot), 1);
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs,
    ).await.unwrap();
    let client = HarnessOperatorClient::new(host.endpoint().socket_addr(), operator_credential).unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let run = client.run_get(run_id.clone()).unwrap();
            let task = client.task_get(task_id.clone()).unwrap();
            if run.lifecycle == HarnessRunLifecycleV1::Completed
                && task.state == HarnessTaskStateV1::Review { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("run/task did not become Completed/Review");
    let runs = client.runs_list(Some(task_id.clone()), None, None, 10).unwrap();
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(client.task_get(task_id).unwrap().body, body);

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let (final_adapter, final_events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let final_route = final_adapter.exact_route(&node_id).unwrap();
    assert_eq!(final_adapter.snapshot(&final_route).await.unwrap().session_records.len(), 1);
    drop(final_adapter);
    drop(final_events);
    assert_private_bytes_absent(&fixture.root, &[
        c2_token, node_token, operator_secret.as_str(), workspace.to_string_lossy().as_ref(),
    ]);
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
