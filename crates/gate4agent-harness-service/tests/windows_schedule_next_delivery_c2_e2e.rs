#![cfg(windows)]

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicU64, Ordering}, Arc},
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::HarnessOperatorCredential;
use gate4agent_harness_client::{HarnessOperatorClient, HarnessOperatorClientError};
use gate4agent_harness_delivery::{
    compile_reviewed_delivery_bundle_v2, DeliveryCatalogV2, ReviewedDeliverySourceV2,
};
use gate4agent_harness_engine::HarnessMutationV1;
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessContextPermissionsV1, HarnessExecutionModeV1,
    HarnessGrantTargetV1, HarnessIdempotencyRef, HarnessMonitoringVisibilityV1,
    HarnessOperationId, HarnessOperationKindV1, HarnessOperationStateV1,
    HarnessOperationTimeoutsV1, HarnessOperationV1, HarnessReadPermissionsV1,
    HarnessRequestDigest, HarnessRevision, HarnessRunId, HarnessRunIntentV1,
    HarnessRunLifecycleV1, HarnessRunV1, HarnessScheduleNextRequestV1,
    HarnessScheduleOutcomeV1, HarnessSelectorV1, HarnessTaskId,
    HarnessTaskPermissionsV1, HarnessTaskStateV1, HarnessTaskV1,
    HarnessWorktreeIntentV1, SessionGrantId, SessionGrantStateV1, SessionGrantV1,
};
use gate4agent_harness_service::{
    c2::{HarnessC2Adapter, HarnessC2EventReceiver},
    dispatch::{
        HarnessContinuationPolicyV1, HarnessDeliveryPolicyV1, HarnessGrantPolicyV1,
        HarnessLaunchCatalog, HarnessLaunchPlanV1, HarnessMcpPolicyV1,
        HarnessPromptSourceV1,
    },
    mutation_request_digest,
    runtime::{start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs},
    HarnessService,
};
use gate4agent_node::{
    protect_bundle_source_tree_fixture, NodeSecretReference, NodeSecretResolveError,
    NodeSecretResolver, NodeSecretValue, NodeServer, NodeServerConfig,
    SpawnProfileRegistry, WorkspaceConfig,
};
use gate4agent_node::protocol::{
    DeliveryComponentKindV2, DeliveryRelativePathV2, DeliveryScopeV2, NodeId,
    SessionMode, SpawnBundleId, SpawnBundleRevision, SpawnProfileDefaults,
    SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

struct FixtureRoot(PathBuf);

impl FixtureRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-schedule-next-delivery-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path { &self.0 }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

struct UnusedSecretResolver;

impl NodeSecretResolver for UnusedSecretResolver {
    fn resolve(&self, _: &NodeSecretReference) -> Result<NodeSecretValue, NodeSecretResolveError> {
        Err(NodeSecretResolveError::Unavailable)
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap()
        .as_millis().try_into().unwrap()
}

fn pipe(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        r"\\.\pipe\gate4agent-schedule-next-delivery-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn revision(value: u64) -> HarnessRevision { HarnessRevision::new(value).unwrap() }
fn parent_task_id() -> HarnessTaskId { HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap() }
fn child_task_id() -> HarnessTaskId { HarnessTaskId::new(format!("htask_{}", "3".repeat(24))).unwrap() }
fn parent_run_id() -> HarnessRunId { HarnessRunId::new(format!("hrun_{}", "1".repeat(24))).unwrap() }
fn exact_grant_id() -> SessionGrantId { SessionGrantId::new(format!("hgrant_{}", "2".repeat(24))).unwrap() }
fn operation_id(marker: char) -> HarnessOperationId {
    HarnessOperationId::new(format!("hop_{}", marker.to_string().repeat(24))).unwrap()
}

fn authority(marker: char) -> gate4agent_harness_protocol::HarnessOperatorAuthorityV1 {
    gate4agent_harness_protocol::HarnessOperatorAuthorityV1 {
        operation_id: operation_id(marker),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}", marker.to_string().repeat(24),
        )).unwrap(),
        actor_id: selector("fixture-operator"),
        now_unix_ms: unix_time_ms(),
    }
}

fn operation(
    marker: char,
    actor: HarnessActorV1,
    kind: HarnessOperationKindV1,
    task_id: Option<HarnessTaskId>,
    run_id: Option<HarnessRunId>,
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
        run_id,
        grant_id,
        reconciles_operation_id: None,
        expected_revision: None,
        request_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}", marker.to_string().repeat(24),
        )).unwrap(),
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

fn seed_parent_grant_and_child(
    database: &Path,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    revoke: bool,
) -> HarnessRevision {
    let now = unix_time_ms();
    let user = HarnessActorV1::User { actor_id: selector("fixture-operator") };
    let parent_actor = HarnessActorV1::ParentRun { run_id: parent_run_id() };
    let mut service = HarnessService::open(database).unwrap();
    let parent_task = HarnessTaskV1 {
        task_id: parent_task_id(),
        revision: revision(1),
        title: "Delivery authority parent".to_owned(),
        body: String::new(),
        creator: user.clone(),
        parent_task_id: None,
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Backlog,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    apply(&mut service, HarnessMutationV1::CreateTask {
        operation: operation('a', user.clone(), HarnessOperationKindV1::CreateTask,
            Some(parent_task_id()), None, None, now),
        task: parent_task,
    });
    let mut linked_parent = service.engine().task(&parent_task_id()).unwrap().clone();
    linked_parent.revision = revision(2);
    linked_parent.run_ids = vec![parent_run_id()];
    linked_parent.updated_at_unix_ms = now + 1;
    let mut parent_operation = operation(
        'b', user.clone(), HarnessOperationKindV1::CreateRun,
        Some(parent_task_id()), Some(parent_run_id()), None, now + 1,
    );
    parent_operation.state = HarnessOperationStateV1::Prepared;
    parent_operation.finished_at_unix_ms = None;
    parent_operation.expected_revision = Some(revision(1));
    let parent_run = HarnessRunV1 {
        run_id: parent_run_id(),
        revision: revision(1),
        parent_run_id: None,
        task_id: parent_task_id(),
        operation_id: operation_id('b'),
        intent: HarnessRunIntentV1 {
            node_id: selector(node_id.as_str()),
            workspace_id: selector(workspace_id.as_str()),
            worktree: HarnessWorktreeIntentV1::Existing,
            provider_profile: selector(profile_id.as_str()),
            mode: HarnessExecutionModeV1::Pty,
            delivery_bundle: None,
            continuation: None,
        },
        delivery_receipt: None,
        continuation_receipt: None,
        context_pack: None,
        binding: None,
        lifecycle: HarnessRunLifecycleV1::Requested,
        result_disposition: None,
        failure: None,
        created_at_unix_ms: now + 1,
        updated_at_unix_ms: now + 1,
    };
    apply(&mut service, HarnessMutationV1::CreateRun {
        operation: parent_operation,
        expected_task_revision: revision(1),
        task: linked_parent,
        run: parent_run,
    });
    let grant = SessionGrantV1 {
        grant_id: exact_grant_id(),
        revision: revision(1),
        actor_run_id: parent_run_id(),
        allowed_targets: vec![HarnessGrantTargetV1 {
            node_id: selector(node_id.as_str()),
            workspace_id: selector(workspace_id.as_str()),
            provider_profile: selector(profile_id.as_str()),
            mode: HarnessExecutionModeV1::Pty,
        }],
        allowed_delivery_bundles: vec![selector("review-kit")],
        maximum_child_count: 1,
        maximum_child_depth: 1,
        operation_timeouts: HarnessOperationTimeoutsV1 {
            dispatch_ms: 20_000,
            wait_ms: 20_000,
            reconciliation_ms: 20_000,
        },
        task_permissions: HarnessTaskPermissionsV1 {
            read: true,
            create: false,
            mutate: false,
            request_run: true,
        },
        read_permissions: HarnessReadPermissionsV1::default(),
        monitoring_visibility: HarnessMonitoringVisibilityV1::None,
        context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
        state: SessionGrantStateV1::Active,
        created_at_unix_ms: now + 2,
        updated_at_unix_ms: now + 2,
    };
    apply(&mut service, HarnessMutationV1::CreateGrant {
        operation: operation('c', user, HarnessOperationKindV1::CreateGrant,
            None, None, Some(exact_grant_id()), now + 2),
        grant: grant.clone(),
    });
    let grant_revision = if revoke {
        let mut revoked = grant;
        revoked.revision = revision(2);
        revoked.state = SessionGrantStateV1::Revoked;
        revoked.updated_at_unix_ms = now + 3;
        let mut revoke_operation = operation('e', parent_actor.clone(),
            HarnessOperationKindV1::RevokeGrant, None, None, Some(exact_grant_id()), now + 3);
        revoke_operation.expected_revision = Some(revision(1));
        apply(&mut service, HarnessMutationV1::ReplaceGrant {
            operation: revoke_operation,
            expected_revision: revision(1),
            grant: revoked,
        });
        revision(2)
    } else {
        revision(1)
    };
    let child = HarnessTaskV1 {
        task_id: child_task_id(),
        revision: revision(1),
        title: "ScheduleNext reviewed delivery".to_owned(),
        body: String::new(),
        creator: parent_actor.clone(),
        parent_task_id: Some(parent_task_id()),
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Ready,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: now + 4,
        updated_at_unix_ms: now + 4,
    };
    apply(&mut service, HarnessMutationV1::CreateTask {
        operation: operation('d', parent_actor, HarnessOperationKindV1::CreateTask,
            Some(child_task_id()), None, None, now + 4),
        task: child,
    });
    service.close().unwrap();
    grant_revision
}

fn launch_catalog(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    bundle_id: &SpawnBundleId,
    grant_revision: HarnessRevision,
) -> HarnessLaunchCatalog {
    HarnessLaunchCatalog::new([HarnessLaunchPlanV1 {
        plan_id: selector("reviewed-delivery"),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector(profile_id.as_str()),
        provider: AgentId::new("kimi").unwrap(),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 30, columns: 120 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: Some(HarnessDeliveryPolicyV1 {
            selector: selector("review-kit"),
            bundle_id: bundle_id.clone(),
        }),
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Exact {
            grant_id: exact_grant_id(),
            revision: grant_revision,
        },
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 20_000,
    }]).unwrap()
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.ok().and_then(|status| status.nodes.get(node_id)
                .map(|node| node.transport == gate4agent_c2_protocol::NodeTransportState::Online))
                == Some(true) { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Node did not become online through C2");
}

async fn connect_adapter(endpoint: &str, token: &str) -> (HarnessC2Adapter, HarnessC2EventReceiver) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) = HarnessC2Adapter::connect(endpoint, token).await {
                break connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 did not release its sole Harness operator")
}

fn inventory(root: &Path) -> Vec<(PathBuf, u64)> {
    if !root.is_dir() { return Vec::new(); }
    let mut pending = vec![root.to_path_buf()];
    let mut values = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_type().unwrap().is_file() {
                values.push((entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    entry.metadata().unwrap().len()));
            }
        }
    }
    values.sort();
    values
}

fn assert_private_bytes_absent(paths: &[PathBuf], canaries: &[&str]) {
    for path in paths {
        if !path.is_file() { continue; }
        let bytes = fs::read(path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for canary in canaries {
            assert!(!text.contains(canary), "private canary persisted in {}", path.display());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schedule_next_delivery_stages_materializes_and_does_not_resend() {
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows integration tests must run through windows-headless-supervisor",
    );
    let fixture = FixtureRoot::new();
    let source = fixture.path().join("reviewed-source");
    fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    fs::create_dir_all(source.join("skills/review-code")).unwrap();
    let root_manifest = br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"schedule-next-review"}"#;
    let claude_manifest = br#"{"name":"schedule-next-review","description":"controlled fixture","version":"1.0.0"}"#;
    fs::write(
        source.join("plugin.json"),
        root_manifest,
    ).unwrap();
    fs::write(
        source.join(".claude-plugin/plugin.json"),
        claude_manifest,
    ).unwrap();
    let mut skill = b"---\nname: review-code\ndescription: controlled fixture\n---\n".to_vec();
    skill.resize(300 * 1024 + 17, b'x');
    fs::write(source.join("skills/review-code/SKILL.md"), &skill).unwrap();
    protect_bundle_source_tree_fixture(&source).unwrap();

    let bundle_id = SpawnBundleId::new("bundle.schedule-next-review").unwrap();
    let compiled = compile_reviewed_delivery_bundle_v2(
        bundle_id.clone(),
        SpawnBundleRevision::new("revision-1").unwrap(),
        &[
            ReviewedDeliverySourceV2::new(
                source.join("plugin.json"),
                None,
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
            ).unwrap(),
            ReviewedDeliverySourceV2::new(
                source.join(".claude-plugin/plugin.json"),
                Some(DeliveryRelativePathV2::new(".claude-plugin").unwrap()),
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
            ).unwrap(),
            ReviewedDeliverySourceV2::new(
                source.join("skills/review-code/SKILL.md"),
                Some(DeliveryRelativePathV2::new("skills/review-code").unwrap()),
                DeliveryComponentKindV2::Skill,
                DeliveryScopeV2::Session,
            ).unwrap(),
        ],
    ).unwrap();
    assert!(compiled.blobs().iter().any(|blob| blob.bytes().len() > 256 * 1024));

    let workspace = fixture.path().join("workspace");
    let materialization = fixture.path().join("materialization");
    fs::create_dir(&workspace).unwrap();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("schedule-next-delivery-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let profile_id = SpawnProfileId::new("kimi-delivery-fixture").unwrap();
    let node_token = "node-secret-schedule-next-delivery";
    let c2_token = "c2-secret-schedule-next-delivery";
    let operator_secret = format!("g4aho_{}", "c".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();
    let node_state = fixture.path().join("node-state.json");
    let delivery_store = fixture.path().join("node-state.json.delivery-store");
    let proof_path = fixture.path().join("provider-bundle-argv.txt");
    let release_signal = fixture.path().join("provider-release.signal");
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: profile_id.clone(),
        revision: SpawnProfileRevision::new("kimi-delivery-r1").unwrap(),
        provider: AgentId::new("kimi").unwrap(),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 30, columns: 120 },
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
        .with_state_path(node_state.clone()).unwrap()
        .with_spawn_profiles(profiles)
        .with_session_environment_materialization(
            &materialization,
            Arc::new(UnusedSecretResolver),
        ).unwrap();
    let node = NodeServer::new_provider_bundle_argv_hold_fixture(
        node_config,
        AgentId::new("kimi").unwrap(),
        proof_path.clone(),
        release_signal.clone(),
    ).unwrap();
    let node_shutdown = node.shutdown_handle();
    let node_task = tokio::spawn(node.run());
    let c2 = C2Running::start(C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(node_id.clone(), node_endpoint, node_token).unwrap()],
    ).unwrap().with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(C2Timings {
            poll_interval: Duration::from_millis(20),
            fresh_for: Duration::from_secs(2),
            attempt_deadline: Duration::from_secs(2),
            transient_backoffs: [Duration::from_millis(20); 5],
            parked_backoff: Duration::from_millis(100),
            http_io_deadline: Duration::from_secs(1),
        })).await.unwrap();
    let http = C2Client::new(c2.api_addr(), c2_token).unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&http, &node_id).await;

    let negative_harness = fixture.path().join("negative-harness.sqlite3");
    let negative_observation = fixture.path().join("negative-observation.sqlite3");
    let negative_revision = seed_parent_grant_and_child(
        &negative_harness, &node_id, &workspace_id, &profile_id, true,
    );
    let negative_catalogs = HarnessRuntimeCatalogs::new(
        launch_catalog(&node_id, &workspace_id, &profile_id, &bundle_id, negative_revision),
        DeliveryCatalogV2::new([compiled.clone()]).unwrap(),
    ).unwrap();
    let (adapter, events) = connect_adapter(&control_endpoint, c2_token).await;
    let route = adapter.exact_route(&node_id).unwrap();
    let before_revoke = adapter.snapshot(&route).await.unwrap();
    let before_revoke_store = inventory(&delivery_store);
    let (negative_host, negative_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&negative_harness).unwrap(),
        ObservationService::open(&negative_observation).unwrap(),
        adapter, events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()), negative_catalogs,
    ).await.unwrap();
    let negative_client = HarnessOperatorClient::new(
        negative_host.endpoint().socket_addr(), operator_credential.clone(),
    ).unwrap();
    assert!(matches!(negative_client.schedule_next(HarnessScheduleNextRequestV1 {
        authority: authority('f'),
        plan_id: Some(selector("reviewed-delivery")),
    }).unwrap(), HarnessScheduleOutcomeV1::Dispatch(_)));
    sleep(Duration::from_millis(300)).await;
    negative_host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), negative_host_task).await.unwrap().unwrap().unwrap();
    let (adapter, negative_events) = connect_adapter(&control_endpoint, c2_token).await;
    assert_eq!(adapter.snapshot(&route).await.unwrap().session_records, before_revoke.session_records);
    assert_eq!(inventory(&delivery_store), before_revoke_store);
    drop(adapter);
    drop(negative_events);

    let harness_path = fixture.path().join("harness.sqlite3");
    let observation_path = fixture.path().join("observation.sqlite3");
    let grant_revision = seed_parent_grant_and_child(
        &harness_path, &node_id, &workspace_id, &profile_id, false,
    );
    let catalogs = HarnessRuntimeCatalogs::new(
        launch_catalog(&node_id, &workspace_id, &profile_id, &bundle_id, grant_revision),
        DeliveryCatalogV2::new([compiled]).unwrap(),
    ).unwrap();
    let (adapter, events) = connect_adapter(&control_endpoint, c2_token).await;
    let (host, mut host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&harness_path).unwrap(),
        ObservationService::open(&observation_path).unwrap(),
        adapter, events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()), catalogs.clone(),
    ).await.unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(), operator_credential.clone(),
    ).unwrap();
    let request = HarnessScheduleNextRequestV1 {
        authority: authority('9'),
        plan_id: Some(selector("reviewed-delivery")),
    };
    let dispatch = client.schedule_next(request.clone()).unwrap();
    assert!(matches!(dispatch, HarnessScheduleOutcomeV1::Dispatch(_)));
    assert_eq!(client.schedule_next(request).unwrap(), dispatch);
    let mut last_operator_error = None;
    let run_id = timeout(Duration::from_secs(20), async {
        loop {
            let task = match client.task_get(child_task_id()) {
                Ok(task) => task,
                Err(error @ (HarnessOperatorClientError::Unavailable
                    | HarnessOperatorClientError::Transport
                    | HarnessOperatorClientError::Deadline
                    | HarnessOperatorClientError::ConnectionClosed
                    | HarnessOperatorClientError::IncompleteResponse)) => {
                    last_operator_error = Some(format!("task_get {error:?}"));
                    if host_task.is_finished() {
                        let outcome = (&mut host_task).await;
                        panic!("Harness host exited during delivery poll after {last_operator_error:?}: {outcome:?}");
                    }
                    sleep(Duration::from_millis(25)).await;
                    continue;
                }
                Err(error) => panic!("task_get failed during delivery poll: {error:?}"),
            };
            if task.state == HarnessTaskStateV1::Running {
                if let Some(run_id) = task.run_ids.first().cloned() {
                    match client.run_get(run_id.clone()) {
                        Ok(run) if run.lifecycle == HarnessRunLifecycleV1::Running
                            && proof_path.is_file() => break run_id,
                        Ok(_) => {}
                        Err(error @ (HarnessOperatorClientError::Unavailable
                            | HarnessOperatorClientError::Transport
                            | HarnessOperatorClientError::Deadline
                            | HarnessOperatorClientError::ConnectionClosed
                            | HarnessOperatorClientError::IncompleteResponse)) => {
                            last_operator_error = Some(format!("run_get {error:?}"));
                            if host_task.is_finished() {
                                let outcome = (&mut host_task).await;
                                panic!("Harness host exited during delivery poll after {last_operator_error:?}: {outcome:?}");
                            }
                        }
                        Err(error) => panic!("run_get failed during delivery poll: {error:?}"),
                    }
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
    }).await;
    let run_id = match run_id {
        Ok(run_id) => run_id,
        Err(_) => {
            let shutdown_outcome = if host_task.is_finished() {
                None
            } else {
                Some(host.shutdown().await)
            };
            let host_outcome = timeout(Duration::from_secs(5), &mut host_task).await;
            let diagnostic = HarnessService::open(&harness_path).unwrap();
            let task = diagnostic.engine().task(&child_task_id())
                .map(|task| (task.state, task.revision));
            let diagnostic_run_id = diagnostic.engine().task(&child_task_id())
                .and_then(|task| task.run_ids.first());
            let run = diagnostic_run_id.and_then(|run_id| diagnostic.engine().run(run_id))
                .map(|run| (run.lifecycle, run.revision, run.failure.clone()));
            let operation = diagnostic_run_id.and_then(|run_id| diagnostic.engine().run(run_id))
                .and_then(|run| diagnostic.engine().operation(&run.operation_id))
                .map(|operation| (
                    operation.state,
                    operation.revision,
                    operation.failure.clone(),
                    operation.outcome_unknown_reason.clone(),
                ));
            let delivery = diagnostic_run_id
                .and_then(|run_id| diagnostic.engine().delivery_for_run(run_id))
                .map(|delivery| (
                    delivery.state,
                    delivery.revision,
                    delivery.authority.clone(),
                    delivery.stage_receipt.as_ref().map(|receipt| (
                        receipt.node_id.clone(),
                        receipt.node_incarnation.clone(),
                        receipt.workspace_id.clone(),
                        receipt.bundle == delivery.bundle,
                    )),
                ));
            let grant = diagnostic.engine().grant(&exact_grant_id())
                .map(|grant| (grant.state, grant.revision));
            diagnostic.close().unwrap();
            let (diagnostic_adapter, diagnostic_events) =
                connect_adapter(&control_endpoint, c2_token).await;
            let diagnostic_route = diagnostic_adapter.exact_route(&node_id).unwrap();
            let node_snapshot = diagnostic_adapter.snapshot(&diagnostic_route).await.unwrap();
            let node_sessions = node_snapshot.session_records.len();
            let profile = node_snapshot.launch_inventory.as_ref()
                .and_then(|inventory| inventory.spawn_profiles.as_ref())
                .and_then(|profiles| profiles.iter().find(|profile| profile.id == profile_id))
                .map(|profile| (profile.id.clone(), profile.revision.clone()));
            drop(diagnostic_adapter);
            drop(diagnostic_events);
            panic!(
                "delivery ScheduleNext did not reach Running; last operator error: \
                 {last_operator_error:?}; host shutdown: {shutdown_outcome:?}; \
                 host outcome: {host_outcome:?}; task: {task:?}; \
                 run: {run:?}; operation: {operation:?}; delivery: {delivery:?}; \
                 grant: {grant:?}; route: {diagnostic_route:?}; Node sessions: {node_sessions}; \
                 profile: {profile:?}; expected provider/mode: kimi/Pty; \
                 proof: {}; store: {:?}; \
                 materialization: {:?}",
                proof_path.is_file(), inventory(&delivery_store), inventory(&materialization),
            );
        }
    };
    let materializations = fs::read_dir(&materialization).unwrap()
        .map(|entry| entry.unwrap().path()).filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(materializations.len(), 1);
    let materialized_bundle = materializations[0].join("bundle");
    assert_eq!(fs::read(materialized_bundle.join("plugin.json")).unwrap(), root_manifest);
    assert_eq!(fs::read(materialized_bundle.join(".claude-plugin/plugin.json")).unwrap(), claude_manifest);
    assert_eq!(fs::read(materialized_bundle.join("skills/review-code/SKILL.md")).unwrap(), skill);
    let argv = fs::read_to_string(&proof_path).unwrap();
    let argv = argv.lines().collect::<Vec<_>>();
    assert_eq!(argv.len(), 2);
    assert_eq!(argv[0], "--skills-dir");
    assert_eq!(fs::canonicalize(argv[1]).unwrap(),
        fs::canonicalize(materialized_bundle.join("skills")).unwrap());
    let before_restart_store = inventory(&delivery_store);
    assert!(!before_restart_store.is_empty());

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    assert!(!node_task.is_finished());
    assert!(http.ready().await.unwrap().ready);
    let (adapter, events) = connect_adapter(&control_endpoint, c2_token).await;
    assert_eq!(adapter.snapshot(&route).await.unwrap().session_records.len(), 1);
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&harness_path).unwrap(),
        ObservationService::open(&observation_path).unwrap(),
        adapter, events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential), catalogs,
    ).await.unwrap();
    let client = HarnessOperatorClient::new(host.endpoint().socket_addr(),
        HarnessOperatorCredential::parse(operator_secret.clone()).unwrap()).unwrap();
    sleep(Duration::from_millis(300)).await;
    assert_eq!(client.run_get(run_id.clone()).unwrap().lifecycle, HarnessRunLifecycleV1::Running);
    assert_eq!(client.task_get(child_task_id()).unwrap().state, HarnessTaskStateV1::Running);
    let transfer = client.run_transfer_get(run_id.clone()).unwrap();
    let delivery_transfer = transfer.delivery.as_ref().expect("committed delivery transfer");
    assert_eq!(
        delivery_transfer.state,
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed,
    );
    assert!(delivery_transfer.receipt_ref.is_some());
    assert!(delivery_transfer.staged_at_unix_ms.is_some());
    assert!(delivery_transfer.committed_at_unix_ms.is_some());
    assert!(transfer.continuation.is_none());
    let transfer_wire = serde_json::to_string(&transfer).unwrap();
    let workspace_display = workspace.to_string_lossy();
    let source_display = source.to_string_lossy();
    let skill_display = String::from_utf8_lossy(&skill);
    for private in [
        workspace_display.as_ref(),
        source_display.as_ref(),
        skill_display.as_ref(),
    ] {
        assert!(!transfer_wire.contains(private));
    }
    assert_eq!(inventory(&delivery_store), before_restart_store);
    assert_eq!(fs::read_dir(&materialization).unwrap()
        .filter_map(Result::ok).filter(|entry| entry.path().is_dir()).count(), 1);
    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();

    let service = HarnessService::open(&harness_path).unwrap();
    let run = service.engine().run(&run_id).unwrap();
    let delivery = service.engine().delivery_for_run(&run_id).unwrap();
    assert_eq!(run.lifecycle, HarnessRunLifecycleV1::Running);
    assert!(run.delivery_receipt.is_some());
    assert_eq!(delivery.state, gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed);
    assert!(delivery.receipt.is_some());
    assert_eq!(service.engine().task(&child_task_id()).unwrap().state, HarnessTaskStateV1::Running);
    assert_eq!(service.engine().deliveries().count(), 1);
    service.close().unwrap();

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&release_signal)
        .unwrap();

    assert_private_bytes_absent(&[
        harness_path.clone(), PathBuf::from(format!("{}-wal", harness_path.display())),
        PathBuf::from(format!("{}-shm", harness_path.display())),
        observation_path.clone(), PathBuf::from(format!("{}-wal", observation_path.display())),
        PathBuf::from(format!("{}-shm", observation_path.display())),
        negative_harness.clone(), PathBuf::from(format!("{}-wal", negative_harness.display())),
        PathBuf::from(format!("{}-shm", negative_harness.display())),
        negative_observation.clone(),
        PathBuf::from(format!("{}-wal", negative_observation.display())),
        PathBuf::from(format!("{}-shm", negative_observation.display())),
        node_state.clone(),
    ], &[
        node_token, c2_token, operator_secret.as_str(),
        workspace.to_string_lossy().as_ref(), source.to_string_lossy().as_ref(),
    ]);
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
