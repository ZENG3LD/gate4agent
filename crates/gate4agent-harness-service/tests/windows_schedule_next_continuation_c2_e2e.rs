#![cfg(windows)]

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client};
use gate4agent_c2_protocol::{C2NodeResponse, NodeRoute};
use gate4agent_harness_api::HarnessOperatorCredential;
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_engine::HarnessMutationV1;
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessContextPermissionsV1, HarnessCreateTaskRequestV1,
    HarnessExecutionModeV1, HarnessGrantTargetV1, HarnessIdempotencyRef,
    HarnessMonitoringVisibilityV1, HarnessOperationId, HarnessOperationKindV1,
    HarnessOperationStateV1, HarnessOperationTimeoutsV1, HarnessOperationV1,
    HarnessReadPermissionsV1, HarnessRequestDigest, HarnessRevision, HarnessRunId,
    HarnessRunLifecycleV1, HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1,
    HarnessSelectorV1, HarnessSessionBindingV1, HarnessSessionIdentityV1,
    HarnessTaskId, HarnessTaskPermissionsV1, HarnessTaskStateV1, HarnessTaskV1,
    HarnessWorktreeIntentV1, SessionGrantId, SessionGrantStateV1, SessionGrantV1,
};
use gate4agent_harness_service::{
    c2::{HarnessC2Adapter, HarnessC2EventReceiver},
    dispatch::{
        HarnessContinuationPolicyV1, HarnessGrantPolicyV1, HarnessLaunchCatalog,
        HarnessLaunchPlanV1, HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    mutation_request_digest,
    runtime::{start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs},
    HarnessService,
};
use gate4agent_node::protocol::{
    ManagedSessionState, NodeId, NodeRequest, SessionAddress, SessionKey, SessionMode,
    SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{
    HistorySourceLayout, NativeHistoryConfig, NativeHistoryRoot, NodeSecretReference,
    NodeSecretResolveError, NodeSecretResolver, NodeSecretValue, NodeServer,
    NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig,
};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{
    AdapterId, AgentId, AgentInstanceId, SessionGeneration, TerminalSize,
};
use ring::digest::{digest, SHA256};
use serde_json::{json, Value};
use tokio::time::{sleep, timeout};

const CONTEXT_USER: &str = "continue the bounded parent run through native C2";
const CONTEXT_ASSISTANT: &str = "the exact parent context is ready for the child run";
const QWEN_SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
const CONTEXT_SCHEMA: &str = "g4a-context-pack-v1";

struct FixturePaths {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    node_state: PathBuf,
    workspace: PathBuf,
    materializations: PathBuf,
    proof: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-schedule-next-continuation-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        let workspace = root.join("workspace");
        let materializations = root.join("private-materializations");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            node_state: root.join("node-state.json"),
            workspace,
            materializations,
            proof: root.join("kimi-context.proof"),
            root,
        }
    }
}

impl Drop for FixturePaths {
    fn drop(&mut self) {
        if self.root.is_dir()
            && self.root.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                name.starts_with("gate4agent-schedule-next-continuation-")
            })
        {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

struct UnusedSecretResolver;

impl NodeSecretResolver for UnusedSecretResolver {
    fn resolve(&self, _: &NodeSecretReference) -> Result<NodeSecretValue, NodeSecretResolveError> {
        Err(NodeSecretResolveError::Unavailable)
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

fn pipe(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        r"\\.\pipe\gate4agent-schedule-next-continuation-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn revision(value: u64) -> HarnessRevision {
    HarnessRevision::new(value).unwrap()
}

fn operation_id(marker: char) -> HarnessOperationId {
    HarnessOperationId::new(format!("hop_{}", marker.to_string().repeat(24))).unwrap()
}

fn idempotency_ref(marker: char) -> HarnessIdempotencyRef {
    HarnessIdempotencyRef::new(format!("hidem_{}", marker.to_string().repeat(24))).unwrap()
}

fn source_task_id() -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap()
}

fn target_task_id() -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", "2".repeat(24))).unwrap()
}

fn grant_id() -> SessionGrantId {
    SessionGrantId::new(format!("hgrant_{}", "3".repeat(24))).unwrap()
}

fn authority(marker: char) -> gate4agent_harness_protocol::HarnessOperatorAuthorityV1 {
    gate4agent_harness_protocol::HarnessOperatorAuthorityV1 {
        operation_id: operation_id(marker),
        idempotency_ref: idempotency_ref(marker),
        actor_id: selector("fixture-operator"),
        now_unix_ms: unix_time_ms(),
    }
}

fn operation(
    marker: char,
    actor: HarnessActorV1,
    kind: HarnessOperationKindV1,
    task_id: Option<HarnessTaskId>,
    grant_id: Option<SessionGrantId>,
    now: u64,
) -> HarnessOperationV1 {
    HarnessOperationV1 {
        operation_id: operation_id(marker),
        revision: revision(1),
        actor,
        kind,
        state: HarnessOperationStateV1::Succeeded,
        task_id,
        run_id: None,
        grant_id,
        reconciles_operation_id: None,
        expected_revision: None,
        request_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
        idempotency_ref: idempotency_ref(marker),
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(now),
    }
}

fn apply(service: &mut HarnessService, mut mutation: HarnessMutationV1) {
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    service.apply(mutation).unwrap();
}

fn source_plan(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector("source-qwen"),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("source-qwen"),
        provider: AgentId::new("qwen-code").unwrap(),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 30, columns: 120 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 20_000,
    }
}

fn target_plan(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector("target-kimi-continuation"),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("target-kimi"),
        provider: AgentId::new("kimi").unwrap(),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 30, columns: 120 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::ParentRun,
        grant: HarnessGrantPolicyV1::Exact {
            grant_id: grant_id(),
            revision: revision(1),
        },
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 20_000,
    }
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

fn qwen_history(root: &Path, private_cwd: &str) -> NativeHistoryConfig {
    let projects = root.join("qwen-projects");
    write_json_lines(
        &projects
            .join("c--fixture")
            .join("chats")
            .join(format!("{QWEN_SESSION_ID}.jsonl")),
        &[
            json!({
                "uuid": "u1",
                "parentUuid": null,
                "sessionId": QWEN_SESSION_ID,
                "timestamp": "2026-08-15T00:00:00Z",
                "type": "user",
                "provenance": "real_user",
                "cwd": private_cwd,
                "message": { "role": "user", "parts": [{ "text": CONTEXT_USER }] },
            }),
            json!({
                "uuid": "a1",
                "parentUuid": "u1",
                "sessionId": QWEN_SESSION_ID,
                "timestamp": "2026-08-15T00:00:01Z",
                "type": "assistant",
                "provenance": "assistant_output",
                "cwd": private_cwd,
                "model": "qwen-fixture",
                "message": {
                    "role": "model",
                    "parts": [
                        { "text": "private qwen thought", "thought": true },
                        { "text": CONTEXT_ASSISTANT }
                    ]
                },
            }),
        ],
    );
    NativeHistoryConfig::new(vec![
        NativeHistoryRoot::new(
            AdapterId::new("qwen-code").unwrap(),
            HistorySourceLayout::SingleNdjson,
            projects,
        )
        .unwrap(),
    ])
    .unwrap()
}

fn seed_target_task_and_grant(service: &mut HarnessService, source_run_id: &HarnessRunId) {
    let now = unix_time_ms();
    let actor = HarnessActorV1::ParentRun { run_id: source_run_id.clone() };
    apply(
        service,
        HarnessMutationV1::CreateTask {
            operation: operation(
                'c',
                actor.clone(),
                HarnessOperationKindV1::CreateTask,
                Some(target_task_id()),
                None,
                now,
            ),
            task: HarnessTaskV1 {
                task_id: target_task_id(),
                revision: revision(1),
                title: "Continue exact parent context".to_owned(),
                body: String::new(),
                creator: actor,
                parent_task_id: Some(source_task_id()),
                dependencies: Vec::new(),
                state: HarnessTaskStateV1::Ready,
                run_ids: Vec::new(),
                result_refs: Vec::new(),
                artifact_refs: Vec::new(),
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
            },
        },
    );

    apply(
        service,
        HarnessMutationV1::CreateGrant {
            operation: operation(
                'd',
                HarnessActorV1::User { actor_id: selector("fixture-operator") },
                HarnessOperationKindV1::CreateGrant,
                None,
                Some(grant_id()),
                now + 1,
            ),
            grant: SessionGrantV1 {
                grant_id: grant_id(),
                revision: revision(1),
                actor_run_id: source_run_id.clone(),
                allowed_targets: vec![HarnessGrantTargetV1 {
                    node_id: selector("schedule-next-continuation-node"),
                    workspace_id: selector("primary"),
                    provider_profile: selector("target-kimi"),
                    mode: HarnessExecutionModeV1::Pty,
                }],
                allowed_delivery_bundles: Vec::new(),
                maximum_child_count: 1,
                maximum_child_depth: 1,
                operation_timeouts: HarnessOperationTimeoutsV1 {
                    dispatch_ms: 20_000,
                    wait_ms: 20_000,
                    reconciliation_ms: 20_000,
                },
                task_permissions: HarnessTaskPermissionsV1 {
                    read: true,
                    create: true,
                    mutate: true,
                    request_run: true,
                },
                read_permissions: HarnessReadPermissionsV1::default(),
                monitoring_visibility: HarnessMonitoringVisibilityV1::None,
                context_permissions: HarnessContextPermissionsV1 {
                    export: true,
                    restore: true,
                },
                state: SessionGrantStateV1::Active,
                created_at_unix_ms: now + 1,
                updated_at_unix_ms: now + 1,
            },
        },
    );
}

fn source_route_and_address(
    source_binding: &HarnessSessionBindingV1,
) -> (NodeRoute, SessionAddress) {
    assert_eq!(source_binding.node_id.as_str(), "schedule-next-continuation-node");
    assert_eq!(source_binding.workspace_id.as_str(), "primary");
    let HarnessSessionIdentityV1::Managed {
        active_session: Some(active),
        ..
    } = &source_binding.session
    else {
        panic!("source run has no exact live managed session binding");
    };
    (
        NodeRoute {
            node_id: NodeId::new(source_binding.node_id.as_str()).unwrap(),
            expected_incarnation_id: source_binding.node_incarnation.as_str().parse().unwrap(),
        },
        SessionAddress {
            workspace_id: WorkspaceId::new(source_binding.workspace_id.as_str()).unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(active.instance_id),
                generation: SessionGeneration(active.generation),
            },
        },
    )
}

async fn load_source_history_through_c2(
    control_endpoint: &str,
    c2_token: &str,
    route: NodeRoute,
    source: SessionAddress,
) {
    let (control, mut events) = connect_local(control_endpoint, c2_token).await.unwrap();
    let event_drain = tokio::spawn(async move {
        while events.recv().await.is_some() {}
    });
    let discovered = timeout(Duration::from_secs(10), async {
        loop {
            let routed = control
                .request(
                    route.clone(),
                    NodeRequest::DiscoverHistory {
                        session: source.clone(),
                        limit: 4,
                    },
                )
                .await
                .unwrap();
            if matches!(routed.response, Ok(C2NodeResponse::HistoryDiscovered { .. })) {
                break routed;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("source session did not become ready for typed history discovery");
    assert_eq!(discovered.node_id, route.node_id);
    assert_eq!(discovered.incarnation_id, route.expected_incarnation_id);
    let C2NodeResponse::HistoryDiscovered {
        session,
        candidates,
    } = discovered.response.unwrap()
    else {
        panic!("typed C2 history discovery returned another response");
    };
    assert_eq!(session, source);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].session_id_hint, QWEN_SESSION_ID);

    let loaded = control
        .request(
            route.clone(),
            NodeRequest::LoadHistory {
                session: source.clone(),
                candidate_id: candidates[0].id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(loaded.node_id, route.node_id);
    assert_eq!(loaded.incarnation_id, route.expected_incarnation_id);
    let C2NodeResponse::HistoryLoaded {
        session,
        session_id,
        message_count,
        ..
    } = loaded.response.unwrap()
    else {
        panic!("typed C2 history load returned another response");
    };
    assert_eq!(session, source);
    assert_eq!(session_id, QWEN_SESSION_ID);
    assert_eq!(message_count, 2);
    drop(control);
    event_drain.abort();
    let _ = event_drain.await;
}

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
) -> (HarnessC2Adapter, HarnessC2EventReceiver) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) = HarnessC2Adapter::connect(endpoint, token).await {
                break connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("C2 did not release its sole Harness operator")
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client
                .status()
                .await
                .ok()
                .and_then(|status| {
                    status.nodes.get(node_id).map(|node| {
                        node.transport == gate4agent_c2_protocol::NodeTransportState::Online
                            && node.cursor.is_some()
                    })
                })
                == Some(true)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Node did not become online through C2");
}

fn files_named(root: &Path, name: &str) -> Vec<PathBuf> {
    fn visit(root: &Path, name: &str, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(root) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                visit(&path, name, found);
            } else if kind.is_file() && path.file_name().and_then(|value| value.to_str()) == Some(name) {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    visit(root, name, &mut found);
    found.sort();
    found
}

fn assert_private_bytes_absent(root: &Path, canaries: &[&str]) {
    for suffix in [
        "harness.sqlite3",
        "harness.sqlite3-wal",
        "harness.sqlite3-shm",
        "observation.sqlite3",
        "observation.sqlite3-wal",
        "observation.sqlite3-shm",
        "node-state.json",
    ] {
        let path = root.join(suffix);
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for canary in canaries {
            assert!(
                !text.contains(canary),
                "private continuation canary persisted in {}",
                path.display(),
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_next_continuation_exports_restores_and_restart_does_not_repeat() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("schedule-next-continuation-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "node-secret-continuation-canary";
    let c2_token = "c2-secret-continuation-canary";
    let operator_secret = format!("g4aho_{}", "c".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();
    let private_cwd = fixture.root.join("private-source-cwd");
    let history = qwen_history(&fixture.root.join("history"), &private_cwd.to_string_lossy());

    let profiles = SpawnProfileRegistry::new([
        SpawnProfileDefaults {
            profile_id: SpawnProfileId::new("source-qwen").unwrap(),
            revision: SpawnProfileRevision::new("source-qwen-r1").unwrap(),
            provider: AgentId::new("qwen-code").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 30, columns: 120 },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: None,
        },
        SpawnProfileDefaults {
            profile_id: SpawnProfileId::new("target-kimi").unwrap(),
            revision: SpawnProfileRevision::new("target-kimi-r1").unwrap(),
            provider: AgentId::new("kimi").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 30, columns: 120 },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: None,
        },
    ])
    .unwrap();
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), fixture.workspace.clone()).unwrap()],
    )
    .unwrap()
    .with_state_path(fixture.node_state.clone())
    .unwrap()
    .with_spawn_profiles(profiles)
    .with_history(history)
    .with_session_environment_materialization(
        fixture.materializations.clone(),
        Arc::new(UnusedSecretResolver),
    )
    .unwrap();
    // Supplied by the Node fixture feature: unlike the older Codex bundle fixture,
    // this verifies only the exact context root and never touches a provider home.
    let node = NodeServer::new_context_only_proof_fixture(
        node_config,
        AgentId::new("kimi").unwrap(),
        fixture.proof.clone(),
    )
    .unwrap();
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
    let c2 = C2Running::start(
        C2Config::new(
            "127.0.0.1:0".parse().unwrap(),
            c2_token,
            vec![C2NodeConfig::new(node_id.clone(), node_endpoint, node_token).unwrap()],
        )
        .unwrap()
        .with_control_endpoint(control_endpoint.clone())
        .unwrap()
        .with_timings(timings),
    )
    .await
    .unwrap();
    let c2_client = C2Client::new(c2.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    let source_catalogs = HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::new([source_plan(&node_id, &workspace_id)]).unwrap(),
        DeliveryCatalogV2::default(),
    )
    .unwrap();
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let (source_host, source_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        source_catalogs,
    )
    .await
    .unwrap();
    let source_client = HarnessOperatorClient::new(
        source_host.endpoint().socket_addr(),
        operator_credential.clone(),
    )
    .unwrap();
    assert_eq!(
        source_client
            .create_task(HarnessCreateTaskRequestV1 {
                authority: authority('a'),
                task_id: source_task_id(),
                title: "Managed source for continuation".to_owned(),
                body: String::new(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Ready,
            })
            .unwrap(),
        gate4agent_harness_api::HarnessOperatorMutationOutcomeV1::Applied,
    );
    let source_dispatch = match source_client
        .schedule_next(HarnessScheduleNextRequestV1 {
            authority: authority('b'),
            plan_id: Some(selector("source-qwen")),
        })
        .unwrap()
    {
        HarnessScheduleOutcomeV1::Dispatch(dispatch) => dispatch,
        HarnessScheduleOutcomeV1::Idle => panic!("source ScheduleNext returned idle"),
    };
    timeout(Duration::from_secs(15), async {
        loop {
            let run = source_client.run_get(source_dispatch.run_id.clone()).unwrap();
            if run.lifecycle == HarnessRunLifecycleV1::Running {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("source run did not become Running");
    source_host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), source_host_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let mut service = HarnessService::open(&fixture.harness).unwrap();
    let source_run = service
        .engine()
        .run(&source_dispatch.run_id)
        .unwrap()
        .clone();
    assert_eq!(source_run.lifecycle, HarnessRunLifecycleV1::Running);
    let source_binding = source_run.binding.as_ref().unwrap().clone();
    let source_context = service.dispatch_context(&source_run.operation_id).unwrap();
    assert_eq!(source_context.node_id, source_binding.node_id);
    assert_eq!(source_context.node_incarnation_id, source_binding.node_incarnation);
    assert_eq!(source_context.workspace_id, source_binding.workspace_id);
    assert_eq!(source_context.expected_provider.as_str(), "qwen-code");
    let (source_route, source_address) = source_route_and_address(&source_binding);
    seed_target_task_and_grant(&mut service, &source_dispatch.run_id);
    service.close().unwrap();

    // Fixture setup only: establish the source run's settled history through C2.
    // ContextPack export and every target-side action remain owned by ScheduleNext.
    load_source_history_through_c2(
        &control_endpoint,
        c2_token,
        source_route,
        source_address,
    )
    .await;

    let target_catalogs = HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::new([
            source_plan(&node_id, &workspace_id),
            target_plan(&node_id, &workspace_id),
        ])
        .unwrap(),
        DeliveryCatalogV2::default(),
    )
    .unwrap();
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let (target_host, mut target_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        target_catalogs.clone(),
    )
    .await
    .unwrap();
    let target_client = HarnessOperatorClient::new(
        target_host.endpoint().socket_addr(),
        operator_credential.clone(),
    )
    .unwrap();
    let target_schedule = HarnessScheduleNextRequestV1 {
        authority: authority('e'),
        plan_id: Some(selector("target-kimi-continuation")),
    };
    let target_dispatch = match target_client.schedule_next(target_schedule.clone()).unwrap() {
        HarnessScheduleOutcomeV1::Dispatch(dispatch) => dispatch,
        HarnessScheduleOutcomeV1::Idle => panic!("continuation ScheduleNext returned idle"),
    };
    assert_eq!(target_client.schedule_next(target_schedule).unwrap(),
        HarnessScheduleOutcomeV1::Dispatch(target_dispatch.clone()));
    assert_eq!(target_dispatch.parent_run_id.as_ref(), Some(&source_dispatch.run_id));
    assert_eq!(
        target_dispatch.intent.continuation_source_run_id().unwrap().as_ref(),
        Some(&source_dispatch.run_id),
    );
    assert!(target_dispatch.intent.delivery_bundle.is_none());

    let proof_ready = match timeout(Duration::from_secs(20), async {
        loop {
            if fixture.proof.is_file()
                && target_client
                    .run_get(target_dispatch.run_id.clone())
                    .unwrap()
                    .lifecycle
                    == HarnessRunLifecycleV1::Running
            {
                break true;
            }
            if target_host_task.is_finished() {
                break false;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await {
        Ok(ready) => ready,
        Err(_) => {
            let host_finished_before_shutdown = target_host_task.is_finished();
            let host_shutdown = target_host.shutdown().await;
            let host_stop = match timeout(Duration::from_secs(5), &mut target_host_task).await {
                Ok(result) => format!("{result:?}"),
                Err(_) => {
                    target_host_task.abort();
                    format!("aborted-after-timeout:{:?}", target_host_task.await)
                }
            };
            let service = HarnessService::open(&fixture.harness).unwrap();
            let task = service.engine().task(&target_task_id())
                .map(|task| format!("revision={} state={:?}", task.revision.get(), task.state))
                .unwrap_or_else(|| "missing".to_owned());
            let run = service.engine().run(&target_dispatch.run_id)
                .map(|run| format!(
                    "revision={} lifecycle={:?} binding={} continuation_receipt={} failure={:?}",
                    run.revision.get(),
                    run.lifecycle,
                    run.binding.is_some(),
                    run.continuation_receipt.is_some(),
                    run.failure,
                ))
                .unwrap_or_else(|| "missing".to_owned());
            let operation = service.engine().operation(&target_dispatch.operation_id)
                .map(|operation| format!(
                    "revision={} state={:?} failure={:?}",
                    operation.revision.get(),
                    operation.state,
                    operation.failure,
                ))
                .unwrap_or_else(|| "missing".to_owned());
            let continuation = service.engine().continuation_for_run(&target_dispatch.run_id)
                .map(|continuation| format!(
                    "revision={} state={:?} context={} target_binding={}",
                    continuation.revision.get(),
                    continuation.state,
                    continuation.context.is_some(),
                    continuation.target_binding.is_some(),
                ))
                .unwrap_or_else(|| "missing".to_owned());
            drop(service);

            let (node_sessions, provider_runtime_statuses) = match timeout(
                Duration::from_secs(5),
                async {
                    loop {
                        if let Ok(connected) = HarnessC2Adapter::connect(
                            &control_endpoint,
                            c2_token,
                        ).await {
                            break connected;
                        }
                        sleep(Duration::from_millis(20)).await;
                    }
                }
            ).await {
                Ok((adapter, events)) => {
                    let route = adapter.exact_route(&node_id).unwrap();
                    let summary = match timeout(Duration::from_secs(3), adapter.snapshot(&route)).await {
                        Ok(Ok(snapshot)) => (
                            snapshot.session_records.iter()
                                .map(|record| format!(
                                    "provider={} state={:?} context={} bundle={}",
                                    record.provider.as_str(),
                                    record.state,
                                    record.context.is_some(),
                                    record.bundle.is_some(),
                                ))
                                .collect::<Vec<_>>(),
                            snapshot.provider_runtime_statuses.iter()
                                .map(|status| format!(
                                    "provider={} mode={:?}",
                                    status.provider().as_str(),
                                    status.mode(),
                                ))
                                .collect::<Vec<_>>(),
                        ),
                        Ok(Err(error)) => (
                            vec![format!("snapshot-error:{error}")],
                            Vec::new(),
                        ),
                        Err(_) => (vec!["snapshot-timeout".to_owned()], Vec::new()),
                    };
                    drop(adapter);
                    drop(events);
                    summary
                }
                Err(_) => (vec!["adapter-connect-timeout".to_owned()], Vec::new()),
            };
            panic!(
                "continuation target proof timeout: host_finished_before_shutdown={host_finished_before_shutdown}; host_shutdown={host_shutdown:?}; host_stop={host_stop}; task={task}; run={run}; operation={operation}; continuation={continuation}; node_sessions={node_sessions:?}; provider_runtime_statuses={provider_runtime_statuses:?}; proof_exists={}; proof_len={:?}",
                fixture.proof.is_file(),
                fs::metadata(&fixture.proof).ok().map(|metadata| metadata.len()),
            );
        }
    };
    if !proof_ready {
        let stopped = target_host_task.await;
        panic!("Harness host stopped during continuation dispatch: {stopped:?}");
    }
    assert_eq!(
        target_client.run_get(target_dispatch.run_id.clone()).unwrap().lifecycle,
        HarnessRunLifecycleV1::Running,
    );
    let transfer_before = target_client
        .run_transfer_get(target_dispatch.run_id.clone())
        .unwrap();
    assert!(transfer_before.delivery.is_none());
    let continuation_transfer = transfer_before.continuation.as_ref()
        .expect("bound continuation transfer");
    assert_eq!(
        continuation_transfer.state,
        gate4agent_harness_protocol::HarnessContinuationStateV1::Bound,
    );
    assert_eq!(&continuation_transfer.source_run_id, &source_dispatch.run_id);
    let context_transfer = continuation_transfer.context.as_ref()
        .expect("exported context transfer");
    assert_eq!(context_transfer.source_message_count, 2);
    assert_eq!(context_transfer.retained_message_count, 2);
    assert!(!context_transfer.truncated);
    let transfer_wire = serde_json::to_string(&transfer_before).unwrap();
    let workspace_display = fixture.workspace.to_string_lossy();
    for private in [
        workspace_display.as_ref(),
        CONTEXT_USER,
        CONTEXT_ASSISTANT,
        "private qwen thought",
    ] {
        assert!(!transfer_wire.contains(private));
    }
    target_host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), target_host_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let service = HarnessService::open(&fixture.harness).unwrap();
    let target_run = service.engine().run(&target_dispatch.run_id).unwrap().clone();
    assert_eq!(target_run.lifecycle, HarnessRunLifecycleV1::Running);
    assert!(target_run.delivery_receipt.is_none());
    assert!(target_run.continuation_receipt.is_some());
    let continuation_before = service
        .engine()
        .continuation_for_run(&target_dispatch.run_id)
        .unwrap()
        .clone();
    assert_eq!(
        continuation_before.state,
        gate4agent_harness_protocol::HarnessContinuationStateV1::Bound,
    );
    assert_eq!(continuation_before.source_run_id, source_dispatch.run_id);
    assert_eq!(continuation_before.target_run_id, target_dispatch.run_id);
    assert_eq!(continuation_before.source_binding, source_binding);
    assert_eq!(continuation_before.target_binding.as_ref(), target_run.binding.as_ref());
    let context_receipt = continuation_before.context.as_ref().unwrap();
    assert_eq!(context_receipt.lineage.source_provider.as_str(), "qwen-code");
    assert_eq!(context_receipt.source_message_count, 2);
    assert_eq!(context_receipt.retained_message_count, 2);
    assert!(!context_receipt.truncated);
    drop(service);

    let context_files = files_named(&fixture.materializations, "context-pack.json");
    assert_eq!(context_files.len(), 1);
    let context_bytes = fs::read(&context_files[0]).unwrap();
    let document: Value = serde_json::from_slice(&context_bytes).unwrap();
    assert_eq!(document["schema"], CONTEXT_SCHEMA);
    assert_eq!(document["source_provider"], "qwen-code");
    assert!(document.get("cwd").is_none());
    assert_eq!(document["retained_messages"][0]["text"], CONTEXT_USER);
    assert_eq!(document["retained_messages"][1]["text"], CONTEXT_ASSISTANT);
    assert!(!String::from_utf8_lossy(&context_bytes).contains("private qwen thought"));
    let proof_before = fs::read(&fixture.proof).unwrap();
    let proof_text = String::from_utf8(proof_before.clone()).unwrap();
    let proof = proof_text.lines().collect::<Vec<_>>();
    assert_eq!(proof.len(), 7, "Kimi child emitted a non-exact context-only proof");
    assert_eq!(proof[0], "context-only");
    assert_eq!(
        fs::canonicalize(Path::new(proof[1])).unwrap(),
        fs::canonicalize(context_files[0].parent().unwrap()).unwrap(),
    );
    assert_eq!(
        fs::canonicalize(Path::new(proof[2])).unwrap(),
        fs::canonicalize(&fixture.workspace).unwrap(),
    );
    let materialized_context_hash = digest(&SHA256, &context_bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(proof[3], materialized_context_hash);
    assert_eq!(proof[4], CONTEXT_SCHEMA);
    assert_eq!(proof[5], "qwen-code");
    assert_eq!(proof[6], "2");

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let route = adapter.exact_route(&node_id).unwrap();
    let before_restart = adapter.snapshot(&route).await.unwrap();
    assert_eq!(before_restart.session_records.len(), 2);
    let target_binding = target_run.binding.as_ref().unwrap();
    let (target_record_id, target_runtime) = match &target_binding.session {
        HarnessSessionIdentityV1::Managed {
            record_id,
            active_session: Some(active_session),
        } => (record_id, active_session),
        _ => panic!("continuation target did not retain an exact managed active session"),
    };
    let target_record = before_restart
        .session_records
        .iter()
        .find(|record| record.record_id.as_str() == target_record_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(target_record.provider.as_str(), "kimi");
    assert_eq!(target_record.state, ManagedSessionState::Live);
    assert!(target_record.bundle.is_none());
    assert!(target_record.context_id.is_none());
    assert!(target_record.context.is_none());
    let target_active_session = target_record.active_session.as_ref().unwrap();
    assert_eq!(target_active_session.workspace_id.as_str(), target_binding.workspace_id.as_str());
    assert_eq!(
        target_active_session.session.instance_id.0,
        target_runtime.instance_id,
    );
    assert_eq!(
        target_active_session.session.generation.0,
        target_runtime.generation,
    );
    let record_ids_before = before_restart
        .session_records
        .iter()
        .map(|record| record.record_id.clone())
        .collect::<Vec<_>>();

    let (restarted_host, restarted_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        target_catalogs,
    )
    .await
    .unwrap();
    sleep(Duration::from_millis(500)).await;
    let restarted_client = HarnessOperatorClient::new(
        restarted_host.endpoint().socket_addr(),
        operator_credential.clone(),
    )
    .unwrap();
    let transfer_after = restarted_client
        .run_transfer_get(target_dispatch.run_id.clone())
        .unwrap();
    assert_eq!(transfer_after, transfer_before);
    restarted_host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), restarted_host_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let service = HarnessService::open(&fixture.harness).unwrap();
    let continuation_after = service
        .engine()
        .continuation_for_run(&target_dispatch.run_id)
        .unwrap();
    assert_eq!(continuation_after, &continuation_before);
    assert_eq!(
        service.engine().run(&target_dispatch.run_id).unwrap(),
        &target_run,
    );
    drop(service);
    assert_eq!(files_named(&fixture.materializations, "context-pack.json"), context_files);
    assert_eq!(fs::read(&fixture.proof).unwrap(), proof_before);

    let (final_adapter, final_events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let final_route = final_adapter.exact_route(&node_id).unwrap();
    let after_restart = final_adapter.snapshot(&final_route).await.unwrap();
    assert_eq!(after_restart.session_records.len(), 2);
    assert_eq!(
        after_restart
            .session_records
            .iter()
            .map(|record| record.record_id.clone())
            .collect::<Vec<_>>(),
        record_ids_before,
    );
    let final_target_record = after_restart
        .session_records
        .iter()
        .find(|record| record.record_id == target_record.record_id)
        .unwrap();
    assert_eq!(final_target_record, &target_record);
    drop(final_adapter);
    drop(final_events);

    assert_private_bytes_absent(
        &fixture.root,
        &[
            CONTEXT_USER,
            CONTEXT_ASSISTANT,
            "private qwen thought",
            private_cwd.to_string_lossy().as_ref(),
            c2_token,
            node_token,
            operator_secret.as_str(),
        ],
    );
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
