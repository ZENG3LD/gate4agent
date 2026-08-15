#![cfg(windows)]

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::protocol::{NodeId, NodeTransportState};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::HarnessOperatorCredential;
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_engine::HarnessMutationV1;
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessCreateTaskRequestV1, HarnessExecutionModeV1,
    HarnessIdempotencyRef, HarnessOperationId, HarnessOperationKindV1,
    HarnessOperationStateV1, HarnessOperationV1, HarnessOperatorAuthorityV1,
    HarnessOutcomeUnknownReasonV1, HarnessRevision, HarnessRunId,
    HarnessRunIntentV1, HarnessRunLifecycleV1, HarnessRunV1,
    HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1, HarnessSelectorV1,
    HarnessTaskId, HarnessTaskStateV1, HarnessTaskV1, HarnessWorktreeIntentV1,
};
use gate4agent_harness_service::{
    c2::{spawn_spec_fingerprint, HarnessC2Adapter},
    dispatch::{
        HarnessContinuationPolicyV1, HarnessGrantPolicyV1, HarnessLaunchCatalog,
        HarnessLaunchPlanV1, HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    mutation_request_digest,
    runtime::{
        start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs,
    },
    HarnessDispatchContextV1, HarnessService,
};
use gate4agent_node::protocol::{
    CapabilityId, ProviderRuntimeMode, SessionMode,
    SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides,
    SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceId,
    SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_api::{ManagedSessionKey, ObservationTarget};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

struct HarnessDatabase {
    root: PathBuf,
    authority_path: PathBuf,
    operator_path: PathBuf,
    observation_path: PathBuf,
}

impl HarnessDatabase {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-harness-c2-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        std::fs::create_dir(&root).unwrap();
        Self {
            authority_path: root.join("authority.sqlite3"),
            operator_path: root.join("operator.sqlite3"),
            observation_path: root.join("observations.sqlite3"),
            root,
        }
    }

    fn authority_path(&self) -> &Path {
        &self.authority_path
    }

    fn operator_path(&self) -> &Path {
        &self.operator_path
    }

    fn observation_path(&self) -> &Path {
        &self.observation_path
    }

    fn assert_private_bytes_absent(&self, canaries: &[&str]) {
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() {
                continue;
            }
            let bytes = std::fs::read(entry.path()).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            for canary in canaries {
                assert!(
                    !text.contains(canary),
                    "private canary persisted in {}",
                    entry.path().display(),
                );
            }
        }
    }
}

impl Drop for HarnessDatabase {
    fn drop(&mut self) {
        if self.root.is_dir() {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }
}

fn require_headless_windows_fixture() {
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows PTY tests must run through windows-headless-supervisor",
    );
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-harness-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn revision(value: u64) -> HarnessRevision {
    HarnessRevision::new(value).unwrap()
}

fn authority(marker: char, now: u64) -> HarnessOperatorAuthorityV1 {
    HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!(
            "hop_{}",
            marker.to_string().repeat(24),
        ))
        .unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            marker.to_string().repeat(24),
        ))
        .unwrap(),
        actor_id: selector("operator"),
        now_unix_ms: now,
    }
}

fn frozen_task_id() -> HarnessTaskId {
    HarnessTaskId::new("htask_111111111111111111111111").unwrap()
}

fn frozen_run_id() -> HarnessRunId {
    HarnessRunId::new("hrun_222222222222222222222222").unwrap()
}

fn frozen_operation_id(value: char) -> HarnessOperationId {
    HarnessOperationId::new(format!("hop_{}", value.to_string().repeat(24))).unwrap()
}

fn frozen_idempotency_ref() -> HarnessIdempotencyRef {
    HarnessIdempotencyRef::new("hidem_444444444444444444444444").unwrap()
}

fn frozen_actor() -> HarnessActorV1 {
    HarnessActorV1::User {
        actor_id: selector("fixture-operator"),
    }
}

fn create_frozen_task(service: &mut HarnessService, now: u64) {
    let task_id = frozen_task_id();
    let operation = HarnessOperationV1 {
        operation_id: frozen_operation_id('a'),
        revision: revision(1),
        actor: frozen_actor(),
        kind: HarnessOperationKindV1::CreateTask,
        state: HarnessOperationStateV1::Succeeded,
        task_id: Some(task_id.clone()),
        run_id: None,
        grant_id: None,
        reconciles_operation_id: None,
        expected_revision: None,
        request_digest: gate4agent_harness_protocol::HarnessRequestDigest::new(
            "a".repeat(64),
        )
        .unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(
            "hidem_aaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap(),
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(now),
    };
    let task = HarnessTaskV1 {
        task_id,
        revision: revision(1),
        title: "Frozen OutcomeUnknown inventory authority".to_owned(),
        body: String::new(),
        creator: frozen_actor(),
        parent_task_id: None,
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Ready,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    let mut mutation = HarnessMutationV1::CreateTask { operation, task };
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    service.apply(mutation).unwrap();
}

fn create_frozen_run(
    service: &mut HarnessService,
    now: u64,
) -> (HarnessRunV1, HarnessOperationV1) {
    let mut task = service.engine().task(&frozen_task_id()).unwrap().clone();
    task.revision = revision(2);
    task.run_ids = vec![frozen_run_id()];
    task.updated_at_unix_ms = now;
    let operation = HarnessOperationV1 {
        operation_id: frozen_operation_id('b'),
        revision: revision(1),
        actor: frozen_actor(),
        kind: HarnessOperationKindV1::CreateRun,
        state: HarnessOperationStateV1::Prepared,
        task_id: Some(frozen_task_id()),
        run_id: Some(frozen_run_id()),
        grant_id: None,
        reconciles_operation_id: None,
        expected_revision: Some(revision(1)),
        request_digest: gate4agent_harness_protocol::HarnessRequestDigest::new(
            "0".repeat(64),
        )
        .unwrap(),
        idempotency_ref: frozen_idempotency_ref(),
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: None,
    };
    let run = HarnessRunV1 {
        run_id: frozen_run_id(),
        revision: revision(1),
        parent_run_id: None,
        task_id: frozen_task_id(),
        operation_id: operation.operation_id.clone(),
        intent: HarnessRunIntentV1 {
            node_id: selector("harness-c2-fixture-node"),
            workspace_id: selector("primary"),
            worktree: HarnessWorktreeIntentV1::Existing,
            provider_profile: selector("claude"),
            mode: HarnessExecutionModeV1::Pty,
            delivery_bundle: None,
            continuation: None,
        },
        delivery_receipt: None,
        continuation_receipt: None,
        binding: None,
        lifecycle: HarnessRunLifecycleV1::Requested,
        result_disposition: None,
        failure: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    let mut mutation = HarnessMutationV1::CreateRun {
        operation: operation.clone(),
        expected_task_revision: revision(1),
        task,
        run: run.clone(),
    };
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    service.apply(mutation).unwrap();
    (
        service.engine().run(&frozen_run_id()).unwrap().clone(),
        service
            .engine()
            .operation(&frozen_operation_id('b'))
            .unwrap()
            .clone(),
    )
}

fn launch_catalog(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchCatalog {
    HarnessLaunchCatalog::new([HarnessLaunchPlanV1 {
        plan_id: selector("default"),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("claude"),
        provider: agent("claude"),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize {
            rows: 24,
            columns: 80,
        },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 20_000,
    }])
    .unwrap()
}

fn scheduled_task_id(marker: char) -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", marker.to_string().repeat(24))).unwrap()
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes[node_id].transport == NodeTransportState::Online
            }) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture node did not become online through C2");
}

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
) -> (
    HarnessC2Adapter,
    gate4agent_harness_service::c2::HarnessC2EventReceiver,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) = HarnessC2Adapter::connect(endpoint, token).await {
                return connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("C2 did not release its sole Harness operator")
}

async fn wait_fixture_runtime(
    harness: &HarnessC2Adapter,
    route: &gate4agent_c2::protocol::NodeRoute,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = harness.snapshot(route).await.unwrap();
            if snapshot.provider_runtime_statuses.iter().any(|status| {
                status.provider() == &agent("claude")
                    && status.mode() != ProviderRuntimeMode::Unavailable
            }) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("controlled fixture runtime did not become available");
}

async fn wait_durable_observation(path: &Path, node_id: &NodeId) -> ObservationTarget {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(service) = ObservationService::open(path) {
                let target = service
                    .committed_snapshot()
                    .projections
                    .iter()
                    .find_map(|projection| match &projection.target {
                        ObservationTarget::Managed { key }
                            if &key.node_id == node_id =>
                        {
                            Some(projection.target.clone())
                        }
                        _ => None,
                    });
                service.close().unwrap();
                if let Some(target) = target {
                    return target;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("production C2 observation was not committed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_next_observation_and_outcome_unknown_inventory_are_exact() {
    require_headless_windows_fixture();
    let database = HarnessDatabase::new();
    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("harness-c2-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "harness-c2-fixture-node-token";
    let c2_token = "harness-c2-fixture-control-token";
    let operator_secret = format!("g4aho_{}", "c".repeat(64));
    let operator_credential =
        HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();
    let workspace = std::env::current_dir().unwrap();
    let profile_revision = SpawnProfileRevision::new("harness-review-r1").unwrap();

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("claude").unwrap(),
        revision: profile_revision.clone(),
        provider: agent("claude"),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize {
            rows: 24,
            columns: 80,
        },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }])
    .unwrap();
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), workspace.clone()).unwrap()],
    )
    .unwrap()
    .with_spawn_profiles(profiles);
    let server = NodeServer::new_monitoring_hook_fixture(node_config).unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

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
            vec![C2NodeConfig::new(
                node_id.clone(),
                node_endpoint,
                node_token,
            )
            .unwrap()],
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

    let (baseline_adapter, baseline_events) =
        connect_harness_adapter(&control_endpoint, c2_token).await;
    let route = baseline_adapter.exact_route(&node_id).unwrap();
    wait_fixture_runtime(&baseline_adapter, &route).await;
    let baseline = baseline_adapter.spawn_baseline(&route).await.unwrap();
    assert!(baseline.is_empty());
    drop(baseline_adapter);
    drop(baseline_events);

    let spec = SpawnSpec {
        target: SpawnTarget {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            worktree_id: None,
        },
        profile_id: SpawnProfileId::new("claude").unwrap(),
        expected_profile_revision: profile_revision,
        overrides: SpawnOverrides {
            provider: SpawnOverride::Set {
                value: agent("claude"),
            },
            mode: SpawnOverride::Set {
                value: SessionMode::Pty,
            },
            terminal_size: SpawnOverride::Set {
                value: TerminalSize {
                    rows: 24,
                    columns: 80,
                },
            },
            prompt: SpawnOverride::Clear,
            bundle_id: SpawnOverride::Clear,
            context_id: SpawnOverride::Clear,
            environment_profile_id: SpawnOverride::Clear,
        },
        deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
        idempotency_key: SpawnIdempotencyKey::new("frozen-outcome-unknown").unwrap(),
        required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
            SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
        )
        .unwrap()])
        .unwrap(),
    };
    let dispatched_at_unix_ms = unix_time_ms();
    let mut authority_service = HarnessService::open(database.authority_path()).unwrap();
    create_frozen_task(
        &mut authority_service,
        dispatched_at_unix_ms.saturating_sub(2),
    );
    let (mut dispatching_run, mut dispatching_operation) = create_frozen_run(
        &mut authority_service,
        dispatched_at_unix_ms.saturating_sub(1),
    );
    dispatching_run.revision = revision(2);
    dispatching_run.lifecycle = HarnessRunLifecycleV1::Dispatching;
    dispatching_run.updated_at_unix_ms = dispatched_at_unix_ms;
    dispatching_operation.revision = revision(2);
    dispatching_operation.state = HarnessOperationStateV1::Dispatching;
    dispatching_operation.updated_at_unix_ms = dispatched_at_unix_ms;
    dispatching_operation.dispatched_at_unix_ms = Some(dispatched_at_unix_ms);
    let context = HarnessDispatchContextV1 {
        operation_id: dispatching_operation.operation_id.clone(),
        node_id: selector(node_id.as_str()),
        node_incarnation_id: selector(route.expected_incarnation_id.to_string()),
        workspace_id: selector(workspace_id.as_str()),
        provider_profile: selector("claude"),
        expected_provider: selector("claude"),
        mode: HarnessExecutionModeV1::Pty,
        baseline_record_ids: baseline
            .iter()
            .map(|record_id| selector(record_id.as_str()))
            .collect(),
        spawn_spec_fingerprint: spawn_spec_fingerprint(&spec).unwrap(),
        dispatched_at_unix_ms,
        idempotency_ref: dispatching_operation.idempotency_ref.clone(),
    };
    authority_service
        .begin_run_dispatch(
            revision(1),
            dispatching_run,
            revision(1),
            dispatching_operation,
            context.clone(),
            &spec,
        )
        .unwrap();
    let unknown_at = unix_time_ms().max(dispatched_at_unix_ms);
    let mut unknown_run = authority_service
        .engine()
        .run(&frozen_run_id())
        .unwrap()
        .clone();
    unknown_run.revision = revision(3);
    unknown_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
    unknown_run.updated_at_unix_ms = unknown_at;
    let mut unknown_operation = authority_service
        .engine()
        .operation(&frozen_operation_id('b'))
        .unwrap()
        .clone();
    unknown_operation.revision = revision(3);
    unknown_operation.state = HarnessOperationStateV1::OutcomeUnknown;
    unknown_operation.outcome_unknown_reason =
        Some(HarnessOutcomeUnknownReasonV1::ReplyLost);
    unknown_operation.updated_at_unix_ms = unknown_at;
    authority_service
        .transition_run_operation(
            revision(2),
            unknown_run,
            revision(2),
            unknown_operation,
        )
        .unwrap();
    let frozen_authority = authority_service.committed_snapshot();

    let catalogs = HarnessRuntimeCatalogs::new(
        launch_catalog(&node_id, &workspace_id),
        DeliveryCatalogV2::default(),
    )
    .unwrap();
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(database.operator_path()).unwrap(),
        ObservationService::open(database.observation_path()).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs.clone(),
    )
    .await
    .unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(),
        operator_credential.clone(),
    )
    .unwrap();
    let first_task_id = scheduled_task_id('3');
    assert_eq!(
        client
            .create_task(HarnessCreateTaskRequestV1 {
                authority: authority('c', unix_time_ms()),
                task_id: first_task_id.clone(),
                title: "First production inventory candidate".to_owned(),
                body: "first monitored ScheduleNext session".to_owned(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Ready,
            })
            .unwrap(),
        gate4agent_harness_api::HarnessOperatorMutationOutcomeV1::Applied,
    );
    assert!(matches!(
        client
            .schedule_next(HarnessScheduleNextRequestV1 {
                authority: authority('d', unix_time_ms()),
                plan_id: Some(selector("default")),
            })
            .unwrap(),
        HarnessScheduleOutcomeV1::Dispatch(_),
    ));
    let first_run_id = timeout(Duration::from_secs(15), async {
        loop {
            let task = client.task_get(first_task_id.clone()).unwrap();
            if task.state == HarnessTaskStateV1::Running {
                let run_id = task.run_ids.first().unwrap().clone();
                let run = client.run_get(run_id.clone()).unwrap();
                if run.lifecycle == HarnessRunLifecycleV1::Running {
                    break run_id;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first production ScheduleNext run did not become Running");
    let task_before_observation = client.task_get(first_task_id.clone()).unwrap();
    let exact_observation_target =
        wait_durable_observation(database.observation_path(), &node_id).await;
    assert_eq!(
        client.task_get(first_task_id.clone()).unwrap(),
        task_before_observation,
        "monitoring ingress mutated the Harness task checkpoint",
    );
    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let persisted_operator = HarnessService::open(database.operator_path()).unwrap();
    let persisted_task = persisted_operator.engine().task(&first_task_id).unwrap();
    assert_eq!(persisted_task.task_id, task_before_observation.task_id);
    assert_eq!(persisted_task.revision, task_before_observation.revision);
    assert_eq!(persisted_task.title, task_before_observation.title);
    assert_eq!(persisted_task.body, task_before_observation.body);
    assert_eq!(persisted_task.parent_task_id, task_before_observation.parent_task_id);
    assert_eq!(persisted_task.dependencies, task_before_observation.dependency_ids);
    assert_eq!(persisted_task.state, task_before_observation.state);
    assert_eq!(persisted_task.run_ids, task_before_observation.run_ids);
    assert_eq!(persisted_task.result_refs, task_before_observation.result_refs);
    assert_eq!(persisted_task.artifact_refs, task_before_observation.artifact_refs);
    assert_eq!(
        persisted_task.created_at_unix_ms,
        task_before_observation.created_at_unix_ms,
    );
    assert_eq!(
        persisted_task.updated_at_unix_ms,
        task_before_observation.updated_at_unix_ms,
        "monitoring persistence changed the production task authority",
    );
    assert_eq!(
        persisted_operator
            .engine()
            .run(&first_run_id)
            .unwrap()
            .lifecycle,
        HarnessRunLifecycleV1::Running,
    );
    persisted_operator.close().unwrap();
    let observation_service = ObservationService::open(database.observation_path()).unwrap();
    let committed_observation = observation_service.committed_snapshot();
    assert!(committed_observation
        .projections
        .iter()
        .any(|projection| projection.target == exact_observation_target));
    let (_, durable_cursor) = committed_observation
        .durable_resume_cursors
        .iter()
        .find(|(candidate, _)| candidate == &node_id)
        .expect("exact C2 observation route has no durable cursor");
    assert_eq!(durable_cursor.incarnation_id, route.expected_incarnation_id);
    assert_ne!(durable_cursor.sequence, 0);
    observation_service.close().unwrap();
    let reopened_observation = ObservationService::open(database.observation_path()).unwrap();
    assert_eq!(
        reopened_observation.committed_snapshot(),
        committed_observation,
        "C2 observation projection was not durable across reopen",
    );
    reopened_observation.close().unwrap();

    let (unique_adapter, unique_events) =
        connect_harness_adapter(&control_endpoint, c2_token).await;
    let unique_route = unique_adapter.exact_route(&node_id).unwrap();
    assert_eq!(unique_route, route);
    let before_unique_candidate = unique_adapter.snapshot(&unique_route).await.unwrap();
    assert_eq!(before_unique_candidate.session_records.len(), 1);
    let first_record_id = before_unique_candidate.session_records[0].record_id.clone();
    assert_eq!(
        exact_observation_target,
        ObservationTarget::Managed {
            key: ManagedSessionKey {
                node_id: node_id.clone(),
                incarnation_id: route.expected_incarnation_id,
                record_id: first_record_id.clone(),
            },
        },
        "durable observation target did not match the exact Node record",
    );
    let unique_candidate = unique_adapter
        .spawn_inventory_candidates(
            &unique_route,
            &baseline,
            &workspace_id,
            &agent("claude"),
            SessionMode::Pty,
        )
        .await
        .unwrap();
    assert_eq!(unique_candidate.candidates.len(), 1);
    assert_eq!(unique_candidate.candidates[0].record_id, first_record_id);
    assert_eq!(
        unique_adapter.snapshot(&unique_route).await.unwrap(),
        before_unique_candidate,
        "candidate inventory query replayed a SpawnSpec or mutated Node",
    );
    assert_eq!(authority_service.committed_snapshot(), frozen_authority);
    assert_eq!(
        authority_service
            .engine()
            .operation(&frozen_operation_id('b'))
            .unwrap()
            .state,
        HarnessOperationStateV1::OutcomeUnknown,
    );
    assert_eq!(
        authority_service
            .engine()
            .run(&frozen_run_id())
            .unwrap()
            .lifecycle,
        HarnessRunLifecycleV1::OutcomeUnknown,
    );
    assert!(authority_service
        .engine()
        .run(&frozen_run_id())
        .unwrap()
        .binding
        .is_none());
    drop(unique_adapter);
    drop(unique_events);

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(database.operator_path()).unwrap(),
        ObservationService::open(database.observation_path()).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs,
    )
    .await
    .unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(),
        operator_credential,
    )
    .unwrap();
    let second_task_id = scheduled_task_id('5');
    assert_eq!(
        client
            .create_task(HarnessCreateTaskRequestV1 {
                authority: authority('e', unix_time_ms()),
                task_id: second_task_id.clone(),
                title: "Second production inventory candidate".to_owned(),
                body: "second monitored ScheduleNext session".to_owned(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Ready,
            })
            .unwrap(),
        gate4agent_harness_api::HarnessOperatorMutationOutcomeV1::Applied,
    );
    assert!(matches!(
        client
            .schedule_next(HarnessScheduleNextRequestV1 {
                authority: authority('f', unix_time_ms()),
                plan_id: Some(selector("default")),
            })
            .unwrap(),
        HarnessScheduleOutcomeV1::Dispatch(_),
    ));
    timeout(Duration::from_secs(15), async {
        loop {
            let task = client.task_get(second_task_id.clone()).unwrap();
            if task.state == HarnessTaskStateV1::Running {
                let run = client
                    .run_get(task.run_ids.first().unwrap().clone())
                    .unwrap();
                if run.lifecycle == HarnessRunLifecycleV1::Running {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("second production ScheduleNext run did not become Running");
    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let (ambiguous_adapter, ambiguous_events) =
        connect_harness_adapter(&control_endpoint, c2_token).await;
    let ambiguous_route = ambiguous_adapter.exact_route(&node_id).unwrap();
    assert_eq!(ambiguous_route, route);
    let before_ambiguous = ambiguous_adapter.snapshot(&ambiguous_route).await.unwrap();
    assert_eq!(before_ambiguous.session_records.len(), 2);
    let second_record_id = before_ambiguous
        .session_records
        .iter()
        .find(|record| record.record_id != first_record_id)
        .expect("second ScheduleNext session has no distinct Node record")
        .record_id
        .clone();
    let candidates = ambiguous_adapter
        .spawn_inventory_candidates(
            &ambiguous_route,
            &baseline,
            &workspace_id,
            &agent("claude"),
            SessionMode::Pty,
        )
        .await
        .unwrap();
    assert_eq!(candidates.candidates.len(), 2);
    assert!(candidates
        .candidates
        .iter()
        .any(|candidate| candidate.record_id == first_record_id));
    assert!(candidates
        .candidates
        .iter()
        .any(|candidate| candidate.record_id == second_record_id));
    assert_eq!(
        ambiguous_adapter.snapshot(&ambiguous_route).await.unwrap(),
        before_ambiguous,
        "ambiguous inventory query mutated Node session records",
    );
    assert_eq!(authority_service.committed_snapshot(), frozen_authority);
    assert_eq!(
        authority_service
            .engine()
            .operation(&frozen_operation_id('b'))
            .unwrap()
            .state,
        HarnessOperationStateV1::OutcomeUnknown,
    );
    assert_eq!(
        authority_service
            .engine()
            .run(&frozen_run_id())
            .unwrap()
            .lifecycle,
        HarnessRunLifecycleV1::OutcomeUnknown,
    );
    assert!(authority_service
        .engine()
        .run(&frozen_run_id())
        .unwrap()
        .binding
        .is_none());
    drop(ambiguous_adapter);
    drop(ambiguous_events);
    authority_service.close().unwrap();

    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait())
        .await
        .unwrap()
        .unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    database.assert_private_bytes_absent(&[
        "g4a-private-provider-session-canary",
        "g4a-private-prompt-canary",
        "g4a-private-tool-input-canary",
        "g4a-private-tool-output-canary",
        node_token,
        c2_token,
        operator_secret.as_str(),
        workspace.to_string_lossy().as_ref(),
    ]);
}
