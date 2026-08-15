#![cfg(windows)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::protocol::C2RelayFailureCode;
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlError};
use gate4agent_harness_api::{
    HarnessNativeSessionCatalogScopeV1, HarnessNativeSessionPreviewRoleV1,
    HarnessNativeSessionRouteV1, HarnessNativeSessionSelectionV1,
    HarnessOperatorActionV1, HarnessOperatorCredential, HarnessOperatorIntentV1,
    HarnessOperatorMutationOutcomeV1, HarnessOperatorRequestRefV1,
    HarnessOperatorResponseV1, HarnessRuntimeManagedStateV1,
    HarnessRuntimeNodeInventoryV1,
};
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_protocol::{
    HarnessExecutionModeV1, HarnessRevision, HarnessRunLifecycleV1,
    HarnessScheduleOutcomeV1, HarnessSelectorV1, HarnessTaskStateV1,
    HarnessWorktreeIntentV1,
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
    NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
    WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AdapterId, AgentId, TerminalSize};
use serde_json::{json, Value};
use tokio::time::{sleep, timeout};

const QWEN_HISTORY_SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
const QWEN_HISTORY_TITLE: &str = "Harness-owned Qwen history";
const QWEN_HISTORY_USER: &str = "show this bounded history through Harness";
const QWEN_HISTORY_ASSISTANT: &str = "Harness returned the visible native session preview";
const QWEN_PRIVATE_THOUGHT: &str = "fixture-private-qwen-thought";

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
            "gate4agent-harness-mode-hierarchy-{}-{}",
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
        r"\\.\pipe\gate4agent-harness-mode-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn write_json_lines(path: &Path, values: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value).unwrap();
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
}

fn qwen_history(root: &Path, workspace: &Path) -> gate4agent_node::NativeHistoryConfig {
    let projects = root.join("projects");
    write_json_lines(
        &projects
            .join("c--harness-history-fixture")
            .join("chats")
            .join(format!("{QWEN_HISTORY_SESSION_ID}.jsonl")),
        &[
            json!({
                "uuid": "u1",
                "parentUuid": null,
                "sessionId": QWEN_HISTORY_SESSION_ID,
                "timestamp": "2026-08-15T00:00:00Z",
                "type": "user",
                "provenance": "real_user",
                "cwd": workspace.to_string_lossy(),
                "message": { "role": "user", "parts": [{ "text": QWEN_HISTORY_USER }] },
            }),
            json!({
                "uuid": "a1",
                "parentUuid": "u1",
                "sessionId": QWEN_HISTORY_SESSION_ID,
                "timestamp": "2026-08-15T00:00:01Z",
                "type": "assistant",
                "provenance": "assistant_output",
                "cwd": workspace.to_string_lossy(),
                "model": "qwen-harness-fixture",
                "message": {
                    "role": "model",
                    "parts": [
                        { "text": QWEN_PRIVATE_THOUGHT, "thought": true },
                        { "text": QWEN_HISTORY_ASSISTANT },
                    ],
                },
            }),
            json!({
                "uuid": "title",
                "parentUuid": "a1",
                "sessionId": QWEN_HISTORY_SESSION_ID,
                "type": "system",
                "subtype": "custom_title",
                "cwd": workspace.to_string_lossy(),
                "systemPayload": {
                    "customTitle": QWEN_HISTORY_TITLE,
                    "titleSource": "manual",
                },
            }),
        ],
    );
    gate4agent_node::NativeHistoryConfig::new(vec![
        gate4agent_node::NativeHistoryRoot::new(
            AdapterId::new("qwen-code").unwrap(),
            gate4agent_node::HistorySourceLayout::SingleNdjson,
            projects,
        ).unwrap(),
    ]).unwrap()
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn intent(marker: char, action: HarnessOperatorActionV1) -> HarnessOperatorIntentV1 {
    HarnessOperatorIntentV1 {
        request_ref: HarnessOperatorRequestRefV1::new(format!(
            "hireq_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        submitted_at_unix_ms: unix_time_ms(),
        action,
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

fn tui_harness_client(
    harness_endpoint: SocketAddr,
    harness_credential: HarnessOperatorCredential,
) -> HarnessOperatorClient {
    HarnessOperatorClient::new(harness_endpoint, harness_credential).unwrap()
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
    }).await.expect("C2 did not expose its sole Harness operator slot")
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.ok().and_then(|status| status.nodes.get(node_id)
                .map(|node| node.transport == gate4agent_c2::protocol::NodeTransportState::Online))
                == Some(true)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Node did not become online through C2");
}

async fn wait_runtime_inventory(
    client: &HarnessOperatorClient,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
) -> HarnessRuntimeNodeInventoryV1 {
    timeout(Duration::from_secs(10), async {
        loop {
            let page = client.runtime_inventory_list(None, 16).unwrap();
            if let Some(node) = page.nodes.iter().find(|node| node.node_id == node_id.as_str()) {
                if node.event_sequence > 0 && node.inventory.managed_sessions.len() == 1 {
                    let record = &node.inventory.managed_sessions[0];
                    if let Some(active_binding) = record.active_binding.as_ref() {
                        let exact_session_visible = node.inventory.workspaces.get(workspace_id.as_str())
                            .is_some_and(|workspace| workspace.sessions.iter().any(|session| {
                                session.instance_id == active_binding.instance_id
                                    && session.generation == active_binding.generation
                            }));
                        if record.state == HarnessRuntimeManagedStateV1::Live
                            && record.workspace_id == workspace_id.as_str()
                            && active_binding.workspace_id == workspace_id.as_str()
                            && exact_session_visible
                        {
                            break node.clone();
                        }
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness runtime inventory did not expose the resynced Node session")
}

async fn wait_node_inventory(
    client: &HarnessOperatorClient,
    node_id: &NodeId,
) -> HarnessRuntimeNodeInventoryV1 {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if let Some(node) = page.nodes.iter().find(|node| node.node_id == node_id.as_str()) {
                    break node.clone();
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness runtime inventory did not expose the history fixture Node")
}

fn assert_qwen_history_visible_through_harness(
    client: &HarnessOperatorClient,
    route: HarnessNativeSessionRouteV1,
    fixture_path_canary: &str,
) {
    let catalog = client.catalog_native_sessions(route.clone(), 8).unwrap();
    assert_eq!(catalog.route, route);
    assert_eq!(catalog.entries.len(), 1);
    let entry = &catalog.entries[0];
    assert_eq!(entry.title, None);
    assert_eq!(entry.message_count, 0);
    assert_ne!(entry.selection_id, QWEN_HISTORY_SESSION_ID);
    assert!(!entry.selection_id.contains(['/', '\\', ':']));
    let summary = catalog.summary.as_ref()
        .expect("initial native history catalog omitted its bounded revision authority");
    let selection = HarnessNativeSessionSelectionV1 {
        route: route.clone(),
        catalog_revision: summary.catalog_revision,
        recent_cutoff_unix_ms: summary.recent_cutoff_unix_ms,
        selection_id: entry.selection_id.clone(),
    };
    let catalog_json = serde_json::to_string(&catalog).unwrap();
    assert!(!catalog_json.contains(fixture_path_canary));
    assert!(!catalog_json.contains(QWEN_PRIVATE_THOUGHT));

    let previewed = client.preview_native_session(selection.clone(), 8).unwrap();
    assert_eq!(previewed.selection, selection);
    assert_eq!(previewed.preview.title.as_deref(), Some(QWEN_HISTORY_TITLE));
    assert_eq!(previewed.preview.message_count, 2);
    assert_eq!(previewed.preview.messages.len(), 2);
    assert_eq!(
        previewed.preview.messages[0].role,
        HarnessNativeSessionPreviewRoleV1::User,
    );
    assert_eq!(previewed.preview.messages[0].text, QWEN_HISTORY_USER);
    assert_eq!(
        previewed.preview.messages[1].role,
        HarnessNativeSessionPreviewRoleV1::Assistant,
    );
    assert_eq!(previewed.preview.messages[1].text, QWEN_HISTORY_ASSISTANT);
    let preview_json = serde_json::to_string(&previewed).unwrap();
    assert!(!preview_json.contains(fixture_path_canary));
    assert!(!preview_json.contains(QWEN_PRIVATE_THOUGHT));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_skin_reconnect_preserves_harness_owned_c2_workflow() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("harness-mode-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "harness-mode-node-token";
    let c2_token = "harness-mode-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "c".repeat(64),
    )).unwrap();
    let workspace = std::env::current_dir().unwrap();

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("clean-exit").unwrap(),
        revision: SpawnProfileRevision::new("harness-mode-r1").unwrap(),
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
        [WorkspaceConfig::new(workspace_id.clone(), workspace).unwrap()],
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
    ).unwrap()
        .with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(timings)).await.unwrap();
    let c2_client = C2Client::new(c2.api_addr(), c2_token).unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let catalogs = HarnessRuntimeCatalogs::new(
        launch_catalog(&node_id, &workspace_id),
        DeliveryCatalogV2::default(),
    ).unwrap();
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs,
    ).await.unwrap();
    let harness_endpoint = host.endpoint().socket_addr();
    let tui = tui_harness_client(harness_endpoint, operator_credential.clone());

    let direct_c2 = match connect_local(&control_endpoint, c2_token).await {
        Err(error) => error,
        Ok(_) => panic!("direct C2 operator connected while Harness owned the operator slot"),
    };
    assert!(matches!(
        direct_c2,
        C2ControlError::Relay(ref failure)
            if failure.code == C2RelayFailureCode::OperatorAlreadyConnected
    ));

    let task_title = "Harness mode hierarchy";
    assert_eq!(
        tui.submit_intent(intent('a', HarnessOperatorActionV1::CreateTask {
            title: task_title.to_owned(),
            body: "TUI skin delegates workflow ownership to Harness".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Ready,
        })).unwrap(),
        HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
    );
    let task_id = tui.tasks_list(None, None, 10).unwrap().tasks.into_iter()
        .find(|task| task.title == task_title)
        .expect("Harness did not materialize the submitted V3 create intent")
        .task_id;
    assert!(matches!(
        tui.submit_intent(intent('b', HarnessOperatorActionV1::ScheduleNext {
            plan_id: Some(selector("default")),
        })).unwrap(),
        HarnessOperatorResponseV1::Schedule(HarnessScheduleOutcomeV1::Dispatch(_)),
    ));

    timeout(Duration::from_secs(15), async {
        loop {
            if fs::read(&fixture.started).ok().as_deref() == Some(b"started\n") {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness did not drive C2 and Node to the fixture start marker");
    let run_id = timeout(Duration::from_secs(10), async {
        loop {
            let task = tui.task_get(task_id.clone()).unwrap();
            if task.state == HarnessTaskStateV1::Running {
                let run_id = task.run_ids.first().cloned().unwrap();
                if tui.run_get(run_id.clone()).unwrap().lifecycle == HarnessRunLifecycleV1::Running {
                    break run_id;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness workflow did not become Running through C2 and Node");
    let runtime_inventory = wait_runtime_inventory(&tui, &node_id, &workspace_id).await;
    assert_eq!(runtime_inventory.node_id, node_id.as_str());
    assert_eq!(runtime_inventory.inventory.workspace_count, 1);
    assert_eq!(runtime_inventory.inventory.session_count, 1);
    assert_eq!(runtime_inventory.inventory.managed_session_count, 1);
    let runtime_record_id = runtime_inventory.inventory.managed_sessions[0].record_id.clone();

    drop(tui);
    assert!(!host_task.is_finished(), "dropping TUI client stopped Harness");
    assert!(!node_task.is_finished(), "dropping TUI client stopped Node");
    assert!(c2_client.ready().await.unwrap().ready, "dropping TUI client stopped C2");

    let reconnected_tui = tui_harness_client(harness_endpoint, operator_credential);
    let reconnected_task = reconnected_tui.task_get(task_id.clone()).unwrap();
    assert_eq!(reconnected_task.state, HarnessTaskStateV1::Running);
    assert_eq!(reconnected_task.run_ids, vec![run_id.clone()]);
    assert_eq!(
        reconnected_tui.run_get(run_id).unwrap().lifecycle,
        HarnessRunLifecycleV1::Running,
    );
    let reconnected_inventory = reconnected_tui.runtime_inventory_list(None, 16).unwrap();
    let reconnected_node = reconnected_inventory.nodes.iter()
        .find(|node| node.node_id == node_id.as_str())
        .expect("reconnected TUI skin lost the Harness-owned Node inventory");
    assert_eq!(reconnected_node.incarnation_id, runtime_inventory.incarnation_id);
    assert_eq!(reconnected_node.inventory.managed_sessions.len(), 1);
    assert_eq!(
        reconnected_node.inventory.managed_sessions[0].record_id,
        runtime_record_id,
    );

    OpenOptions::new().write(true).create_new(true).open(&fixture.release).unwrap()
        .write_all(b"release\n").unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let task = reconnected_tui.task_get(task_id.clone()).unwrap();
            if task.state == HarnessTaskStateV1::Review {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("released workflow did not reach Review through the reconnected TUI skin");

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_skin_reads_explicit_qwen_history_only_through_harness() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("history-node");
    let control_endpoint = pipe("history-control");
    let node_id = NodeId::new("harness-history-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "harness-history-node-token";
    let c2_token = "harness-history-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "d".repeat(64),
    )).unwrap();
    let workspace = fixture.root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let history = qwen_history(&fixture.root.join("qwen-history"), &workspace);
    let fixture_path_canary = fixture.root.file_name().unwrap().to_string_lossy().into_owned();

    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), workspace).unwrap()],
    ).unwrap()
        .with_state_path(fixture.node_state.clone()).unwrap()
        .with_history(history);
    let node = NodeServer::new(node_config).unwrap();
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
    ).unwrap()
        .with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(timings)).await.unwrap();
    let c2_client = C2Client::new(c2.api_addr(), c2_token).unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        HarnessRuntimeCatalogs::default(),
    ).await.unwrap();
    let harness_endpoint = host.endpoint().socket_addr();
    let tui = tui_harness_client(harness_endpoint, operator_credential.clone());

    let direct_c2 = match connect_local(&control_endpoint, c2_token).await {
        Err(error) => error,
        Ok(_) => panic!("direct C2 operator connected while Harness owned the operator slot"),
    };
    assert!(matches!(
        direct_c2,
        C2ControlError::Relay(ref failure)
            if failure.code == C2RelayFailureCode::OperatorAlreadyConnected
    ));

    let node_inventory = wait_node_inventory(&tui, &node_id).await;
    let history_route = HarnessNativeSessionRouteV1 {
        node_id: node_inventory.node_id.clone(),
        incarnation_id: node_inventory.incarnation_id.clone(),
        scope: HarnessNativeSessionCatalogScopeV1::Workspace,
        workspace_id: Some(workspace_id.as_str().to_owned()),
        provider: "qwen-code".to_owned(),
    };
    assert_qwen_history_visible_through_harness(
        &tui,
        history_route.clone(),
        &fixture_path_canary,
    );

    drop(tui);
    assert!(!host_task.is_finished(), "dropping TUI client stopped Harness");
    assert!(!node_task.is_finished(), "dropping TUI client stopped Node");
    assert!(c2_client.ready().await.unwrap().ready, "dropping TUI client stopped C2");

    let reconnected_tui = tui_harness_client(harness_endpoint, operator_credential);
    assert_qwen_history_visible_through_harness(
        &reconnected_tui,
        history_route,
        &fixture_path_canary,
    );

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
