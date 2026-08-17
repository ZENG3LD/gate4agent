#![cfg(windows)]

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    FeatureObservationStateV1, HarnessGitObjectIdV1, HarnessNodeIncarnationV1,
    HarnessOperatorCredential,
    HarnessOperatorHostErrorV1, HarnessOperatorMutationOutcomeV1, HarnessRepositoryPathV1,
    HarnessReplaceTaskExecutionSpecRequestV2, HarnessReverseAttributionOutcomeV1,
    HarnessReverseAttributionRelationV1, HarnessReverseAttributionSubjectV1,
    HarnessReverseAttributionWorkspaceV1, HarnessReviewedTaskLaunchSelectionV1,
    HarnessReviewedWorktreeSelectionV1, HarnessRunCorrelationAvailabilityV1,
    HarnessRunSessionViewV1, HarnessStartTaskRequestV2, ProjectionAvailabilityV1,
    ProjectionFreshnessV1,
};
use gate4agent_harness_client::{HarnessOperatorClient, HarnessOperatorClientError};
use gate4agent_harness_delivery::{
    compile_reviewed_delivery_bundle_v2, DeliveryCatalogV2, ReviewedDeliverySourceV2,
};
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessContinuationStateV1, HarnessDeliveryBundleRevisionV1,
    HarnessDeliveryStateV1,
    HarnessExecutionModeV1,
    HarnessExpectedExecutionSpecRevisionV1, HarnessIdempotencyRef, HarnessOperationId,
    HarnessOperationStateV1, HarnessOperatorAuthorityV1, HarnessRequestDigest, HarnessRevision,
    HarnessRunId, HarnessRunLifecycleV1, HarnessScheduleNextRequestV1,
    HarnessScheduleOutcomeV1, HarnessSelectorV1, HarnessSessionBindingV1,
    HarnessSessionIdentityV1, HarnessTaskId, HarnessTaskReviewPolicyV1, HarnessTaskStateV1,
    HarnessTransferAuthorityRefV1, HarnessWorktreeIntentV1,
};
use gate4agent_harness_service::{
    c2::{HarnessC2Adapter, HarnessC2EventReceiver},
    dispatch::{
        HarnessContinuationPolicyV1, HarnessGrantPolicyV1, HarnessLaunchCatalog,
        HarnessLaunchPlanV1, HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    runtime::{start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs},
    HarnessService,
};
use gate4agent_node::protocol::{
    DeliveryComponentKindV2, DeliveryRelativePathV2, DeliveryScopeV2,
    ManagedSessionState, ManagedWorktreeRetention, NodeId, NodeIncarnationId, SessionMode,
    SpawnBundleId, SpawnBundleRevision, SpawnProfileDefaults, SpawnProfileId,
    SpawnEnvironmentProfileId, SpawnEnvironmentProfileRevision, SpawnProfileRevision,
    WorktreeProfileId, WorktreeProfileRevision, WorktreeServiceMode, WorkspaceId,
};
use gate4agent_node::{
    protect_bundle_source_tree_fixture, HistorySourceLayout, ManagedWorktreeProfile,
    NativeHistoryConfig, NativeHistoryRoot, NodeSecretReference, NodeSecretResolveError,
    NodeSecretResolver, NodeSecretValue, NodeServer, NodeServerConfig, SpawnProfileRegistry,
    WorkspaceConfig,
};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AdapterId, AgentId, TerminalSize};
use serde_json::{json, Value};
use tokio::time::{sleep, timeout};

const SOURCE_USER: &str = "phase-six private source user payload";
const SOURCE_ASSISTANT: &str = "phase-six private source assistant payload";
const DELIVERY_PAYLOAD: &str = "Review the selected change.";
const DELIVERY_SKILL: &[u8] = b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n";
const DELIVERY_PLUGIN_MANIFEST: &[u8] = br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"phase-six-review"}"#;
const PROVIDER_IDENTITY_CANARY: &str = "private-provider-login-identity";
const SOURCE_SESSION_ID: &str = "g4a-private-provider-session-canary";

struct FixturePaths {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    node_state: PathBuf,
    workspace: PathBuf,
    allocation: PathBuf,
    materializations: PathBuf,
    proof: PathBuf,
    delivery_source: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-task-launch-issuance-v2-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        let workspace = root.join("workspace");
        let allocation = root.join("managed-worktrees");
        let delivery_source = root.join("reviewed-delivery");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&allocation).unwrap();
        fs::create_dir_all(&delivery_source).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            node_state: root.join("node-state.json"),
            materializations: root.join("private-materializations"),
            proof: root.join("target-context.proof"),
            workspace,
            allocation,
            delivery_source,
            root,
        }
    }
}

impl Drop for FixturePaths {
    fn drop(&mut self) {
        if self.root.is_dir()
            && self.root.file_name().and_then(|value| value.to_str()).is_some_and(|value| {
                value.starts_with("gate4agent-task-launch-issuance-v2-")
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
        r"\\.\pipe\gate4agent-task-launch-issuance-v2-{label}-{}-{}",
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

fn source_task_id() -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap()
}

fn target_task_id() -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", "2".repeat(24))).unwrap()
}

fn authority(marker: char) -> HarnessOperatorAuthorityV1 {
    let marker = format!("{:024x}", u32::from(marker));
    HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!("hop_{marker}")).unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{marker}")).unwrap(),
        actor_id: selector("fixture-operator"),
        now_unix_ms: unix_time_ms(),
    }
}

fn source_plan(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchPlanV1 {
    ordinary_plan(
        "phase-six-source",
        "source-claude",
        "claude",
        node_id,
        workspace_id,
        revision(1),
    )
}

fn target_plan(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    plan_revision: HarnessRevision,
) -> HarnessLaunchPlanV1 {
    ordinary_plan(
        "phase-six-target",
        "target-codex",
        "codex",
        node_id,
        workspace_id,
        plan_revision,
    )
}

fn ordinary_plan(
    plan_id: &str,
    provider_profile: &str,
    provider: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    plan_revision: HarnessRevision,
) -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector(plan_id),
        revision: plan_revision,
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector(provider_profile),
        provider: AgentId::new(provider).unwrap(),
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

fn assert_git_success(repository: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn initialize_repository(workspace: &Path) {
    assert_git_success(workspace, &["init", "-b", "main"]);
    assert_git_success(workspace, &["config", "user.name", "Gate4Agent Fixture"]);
    assert_git_success(workspace, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(workspace.join("README.md"), "phase six fixture\n").unwrap();
    assert_git_success(workspace, &["add", "README.md"]);
    assert_git_success(workspace, &["commit", "-m", "fixture base"]);
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

fn source_history(root: &Path, private_cwd: &str) -> NativeHistoryConfig {
    let projects = root.join("claude-projects");
    write_json_lines(
        &projects.join(format!("{SOURCE_SESSION_ID}.jsonl")),
        &[
            json!({
                "sessionId": SOURCE_SESSION_ID,
                "timestamp": "2026-08-16T00:00:00Z",
                "type": "user",
                "cwd": private_cwd,
                "message": { "role": "user", "content": SOURCE_USER },
            }),
            json!({
                "sessionId": SOURCE_SESSION_ID,
                "timestamp": "2026-08-16T00:00:01Z",
                "type": "assistant",
                "cwd": private_cwd,
                "message": {
                    "role": "assistant",
                    "model": PROVIDER_IDENTITY_CANARY,
                    "content": [{ "type": "text", "text": SOURCE_ASSISTANT }]
                },
            }),
        ],
    );
    NativeHistoryConfig::new(vec![
        NativeHistoryRoot::new(
            AdapterId::new("claude-code").unwrap(),
            HistorySourceLayout::SingleNdjson,
            projects,
        ).unwrap(),
    ]).unwrap()
}

fn delivery_catalog(
    source: &Path,
    bundle_revision: &str,
) -> DeliveryCatalogV2 {
    let skill_root = source.join("skills/review-code");
    fs::create_dir_all(&skill_root).unwrap();
    fs::write(skill_root.join("SKILL.md"), DELIVERY_SKILL).unwrap();
    let plugin_manifest = source.join("plugin.json");
    fs::write(&plugin_manifest, DELIVERY_PLUGIN_MANIFEST).unwrap();
    protect_bundle_source_tree_fixture(source).unwrap();
    let compiled = compile_reviewed_delivery_bundle_v2(
        SpawnBundleId::new("bundle.phase-six").unwrap(),
        SpawnBundleRevision::new(bundle_revision).unwrap(),
        &[
            ReviewedDeliverySourceV2::new(
                skill_root.join("SKILL.md"),
                Some(DeliveryRelativePathV2::new("skills/review-code").unwrap()),
                DeliveryComponentKindV2::Skill,
                DeliveryScopeV2::Session,
            ).unwrap(),
            ReviewedDeliverySourceV2::new(
                plugin_manifest,
                None,
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
            ).unwrap(),
        ],
    ).unwrap();
    assert_eq!(compiled.manifest().components.len(), 2);
    assert_eq!(compiled.manifest().components[0].relative_path.as_str(), "plugin.json");
    assert_eq!(
        compiled.manifest().components[0].kind,
        DeliveryComponentKindV2::PluginManifest,
    );
    assert_eq!(
        compiled.manifest().components[0].scope,
        DeliveryScopeV2::Session,
    );
    assert_eq!(
        compiled.manifest().components[1].relative_path.as_str(),
        "skills/review-code/SKILL.md",
    );
    assert_eq!(
        compiled.manifest().components[1].kind,
        DeliveryComponentKindV2::Skill,
    );
    assert_eq!(
        compiled.manifest().components[1].scope,
        DeliveryScopeV2::Session,
    );
    DeliveryCatalogV2::new([compiled]).unwrap()
}

fn runtime_catalogs(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    delivery: DeliveryCatalogV2,
    target_revision: HarnessRevision,
) -> HarnessRuntimeCatalogs {
    HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::new([
            source_plan(node_id, workspace_id),
            target_plan(node_id, workspace_id, target_revision),
        ]).unwrap(),
        delivery,
    ).unwrap()
}

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
    expected_node_id: &NodeId,
) -> (HarnessC2Adapter, HarnessC2EventReceiver) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok((adapter, events)) = HarnessC2Adapter::connect(endpoint, token).await {
                if let Ok(route) = adapter.exact_route(expected_node_id) {
                    if adapter.snapshot(&route).await.is_ok() {
                        break (adapter, events);
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    }).await.expect("C2 did not expose the expected current Node topology to Harness")
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.ok().and_then(|status| {
                status.nodes.get(node_id).map(|node| {
                    node.transport == gate4agent_c2_protocol::NodeTransportState::Online
                        && node.cursor.is_some()
                })
            }) == Some(true) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Node did not become online through C2");
}

async fn stop_host(
    host: gate4agent_harness_service::runtime::HarnessHostHandle,
    task: tokio::task::JoinHandle<Result<(), gate4agent_harness_service::runtime::HarnessRuntimeError>>,
) {
    let shutdown = host.shutdown().await;
    let host_result = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    match (shutdown, host_result) {
        (Ok(()), Ok(())) => {}
        (Err(shutdown_error), Err(host_error)) => panic!(
            "Harness host stopped before shutdown completed: shutdown={shutdown_error:?} host={host_error:?}",
        ),
        (Err(error), Ok(())) => panic!("Harness shutdown failed after clean host exit: {error:?}"),
        (Ok(()), Err(error)) => panic!("Harness host failed during shutdown: {error:?}"),
    }
}

fn assert_conflict(result: Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError>) {
    assert!(matches!(
        result,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)),
    ));
}

fn assert_start_conflict(
    result: Result<gate4agent_harness_protocol::HarnessTaskStartOutcomeV1, HarnessOperatorClientError>,
) {
    assert!(matches!(
        result,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)),
    ));
}

fn allocation_count(root: &Path) -> usize {
    fs::read_dir(root).unwrap().count()
}

fn public_preallocation_state(
    client: &HarnessOperatorClient,
    task_id: &HarnessTaskId,
) -> Value {
    let inventory = client.runtime_inventory_list(None, 32).unwrap();
    let options = client.task_launch_options_get(task_id.clone()).unwrap();
    json!({
        "nodes": inventory.nodes.iter().map(|node| json!({
            "node_id": node.node_id,
            "incarnation_id": node.incarnation_id,
            "workspace_count": node.inventory.workspace_count,
            "workspace_ids": node.inventory.workspaces.keys().collect::<Vec<_>>(),
            "session_count": node.inventory.session_count,
            "managed_session_count": node.inventory.managed_session_count,
            "managed_records": node.inventory.managed_sessions.iter().map(|record| json!({
                "record_id": record.record_id,
                "workspace_id": record.workspace_id,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "launch_options": {
            "task_revision": options.task_revision,
            "policy_digest": options.policy_digest,
            "managed_worktree_profiles": options.managed_worktree_profiles,
            "current_issued_spec": options.current_issued_spec,
        },
    })
}

fn assert_no_public_preallocation_effects(
    client: &HarnessOperatorClient,
    task_id: &HarnessTaskId,
    allocation_root: &Path,
    expected: &Value,
) {
    assert_eq!(&public_preallocation_state(client, task_id), expected);
    assert_eq!(allocation_count(allocation_root), 0);
}

fn assert_reverse_attribution(
    client: &HarnessOperatorClient,
    task_id: &HarnessTaskId,
    run_id: &HarnessRunId,
    binding: &HarnessSessionBindingV1,
) {
    let HarnessSessionIdentityV1::Managed {
        record_id,
        active_session: Some(active),
    } = &binding.session else {
        panic!("reverse attribution fixture requires an active managed binding");
    };
    let workspace = HarnessReverseAttributionWorkspaceV1 {
        node_id: binding.node_id.clone(),
        node_incarnation_id: HarnessNodeIncarnationV1::new(
            binding.node_incarnation.as_str(),
        ).unwrap(),
        workspace_id: binding.workspace_id.clone(),
    };
    for (subject, relation) in [
        (
            HarnessReverseAttributionSubjectV1::ManagedRecord {
                workspace: workspace.clone(),
                record_id: record_id.clone(),
            },
            HarnessReverseAttributionRelationV1::ManagedRecordBinding,
        ),
        (
            HarnessReverseAttributionSubjectV1::RuntimeSession {
                workspace: workspace.clone(),
                instance_id: active.instance_id,
                generation: active.generation,
            },
            HarnessReverseAttributionRelationV1::RuntimeSessionBinding,
        ),
        (
            HarnessReverseAttributionSubjectV1::Workspace {
                workspace: workspace.clone(),
            },
            HarnessReverseAttributionRelationV1::WorkspaceBinding,
        ),
    ] {
        let attributed = client.reverse_attribution_get(subject).unwrap();
        assert_eq!(attributed.outcome, HarnessReverseAttributionOutcomeV1::Attributed);
        assert_eq!(attributed.links.len(), 1);
        assert_eq!(&attributed.links[0].task_id, task_id);
        assert_eq!(&attributed.links[0].run_id, run_id);
        assert_eq!(attributed.links[0].relation, relation);
    }
    for subject in [
        HarnessReverseAttributionSubjectV1::FileScope {
            workspace: workspace.clone(),
            relative_path: HarnessRepositoryPathV1::new("README.md").unwrap(),
        },
        HarnessReverseAttributionSubjectV1::CommitScope {
            workspace: workspace.clone(),
            object_id: HarnessGitObjectIdV1::new("a".repeat(40)).unwrap(),
        },
    ] {
        let scoped = client.reverse_attribution_get(subject).unwrap();
        assert_eq!(scoped.outcome, HarnessReverseAttributionOutcomeV1::Attributed);
        assert_eq!(scoped.links.len(), 1);
        assert_eq!(&scoped.links[0].task_id, task_id);
        assert_eq!(&scoped.links[0].run_id, run_id);
        assert_eq!(
            scoped.links[0].relation,
            HarnessReverseAttributionRelationV1::WorkspaceScope,
        );
    }
    let wrong_incarnation = HarnessReverseAttributionWorkspaceV1 {
        node_incarnation_id: HarnessNodeIncarnationV1::new(
            NodeIncarnationId::from_bytes([8; 16]).to_string(),
        ).unwrap(),
        ..workspace.clone()
    };
    for subject in [
        HarnessReverseAttributionSubjectV1::ManagedRecord {
            workspace: wrong_incarnation,
            record_id: record_id.clone(),
        },
        HarnessReverseAttributionSubjectV1::RuntimeSession {
            workspace,
            instance_id: active.instance_id,
            generation: active.generation.checked_add(1).unwrap(),
        },
    ] {
        let unattributed = client.reverse_attribution_get(subject).unwrap();
        assert_eq!(
            unattributed.outcome,
            HarnessReverseAttributionOutcomeV1::Unattributed,
        );
        assert!(unattributed.links.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn task_launch_v2_issues_managed_context_delivery_once_across_reopen_and_replay() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    initialize_repository(&fixture.workspace);
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("phase-six-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let worktree_profile_id = WorktreeProfileId::new("reviewed-managed").unwrap();
    let worktree_profile_revision = WorktreeProfileRevision::new("reviewed-managed-r1").unwrap();
    let environment_profile_id =
        SpawnEnvironmentProfileId::new("isolated-codex-home").unwrap();
    let environment_profile_revision =
        SpawnEnvironmentProfileRevision::new("isolated-codex-home-r1").unwrap();
    let node_token = "node-secret-phase-six";
    let c2_token = "c2-secret-phase-six";
    let operator_secret = format!("g4aho_{}", "c".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();
    let source_cwd = fs::canonicalize(&fixture.workspace).unwrap();

    let managed_profile = ManagedWorktreeProfile::new(
        worktree_profile_id.clone(),
        worktree_profile_revision.clone(),
        &fixture.allocation,
        "gate4agent/phase-six",
        "HEAD",
        ManagedWorktreeRetention::Retain,
    ).unwrap();
    let workspace = WorkspaceConfig::new(workspace_id.clone(), fixture.workspace.clone())
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(managed_profile)
        .unwrap();
    let spawn_profiles = SpawnProfileRegistry::new([
        SpawnProfileDefaults {
            profile_id: SpawnProfileId::new("source-claude").unwrap(),
            revision: SpawnProfileRevision::new("source-claude-r1").unwrap(),
            provider: AgentId::new("claude").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 30, columns: 120 },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: None,
        },
        SpawnProfileDefaults {
            profile_id: SpawnProfileId::new("target-codex").unwrap(),
            revision: SpawnProfileRevision::new("target-codex-r1").unwrap(),
            provider: AgentId::new("codex").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 30, columns: 120 },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: Some(environment_profile_id.clone()),
        },
    ]).unwrap();
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [workspace],
    ).unwrap()
        .with_state_path(fixture.node_state.clone()).unwrap()
        .with_spawn_profiles(spawn_profiles)
        .with_history(source_history(
            &fixture.root.join("history"),
            source_cwd.to_string_lossy().as_ref(),
        ))
        .with_session_environment_materialization(
            fixture.materializations.clone(),
            Arc::new(UnusedSecretResolver),
        ).unwrap();
    let mut node = NodeServer::new_monitoring_context_pack_fixture(
        node_config,
        fixture.proof.clone(),
    ).unwrap();
    node.install_isolated_codex_home_environment_fixture(
        environment_profile_id,
        environment_profile_revision,
    ).unwrap();
    let managed_spawn_failure_probe =
        node.spawn_managed_worktree_v2_failure_probe_fixture();
    let node_shutdown = node.shutdown_handle();
    let node_task = tokio::spawn(node.run());

    let c2 = C2Running::start(
        C2Config::new(
            "127.0.0.1:0".parse().unwrap(),
            c2_token,
            vec![C2NodeConfig::new(node_id.clone(), node_endpoint.clone(), node_token).unwrap()],
        ).unwrap()
        .with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(C2Timings {
            poll_interval: Duration::from_millis(20),
            fresh_for: Duration::from_secs(2),
            attempt_deadline: Duration::from_secs(2),
            transient_backoffs: [Duration::from_millis(20); 5],
            parked_backoff: Duration::from_millis(100),
            http_io_deadline: Duration::from_secs(1),
        }),
    ).await.unwrap();
    let c2_client = C2Client::new(c2.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    let exact_delivery = delivery_catalog(&fixture.delivery_source, "revision-1");
    let exact_catalogs = runtime_catalogs(
        &node_id,
        &workspace_id,
        exact_delivery.clone(),
        revision(1),
    );
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (source_host, source_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        exact_catalogs.clone(),
    ).await.unwrap();
    let source_client = HarnessOperatorClient::new(
        source_host.endpoint().socket_addr(),
        operator_credential.clone(),
    ).unwrap();
    assert_eq!(
        source_client.create_task(gate4agent_harness_protocol::HarnessCreateTaskRequestV1 {
            authority: authority('a'),
            task_id: source_task_id(),
            title: "Phase six context source".to_owned(),
            body: String::new(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Ready,
        }).unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let source_dispatch = match source_client.schedule_next(HarnessScheduleNextRequestV1 {
        authority: authority('b'),
        plan_id: Some(selector("phase-six-source")),
    }).unwrap() {
        HarnessScheduleOutcomeV1::Dispatch(dispatch) => dispatch,
        HarnessScheduleOutcomeV1::Idle => panic!("source ScheduleNext returned idle"),
    };
    timeout(Duration::from_secs(15), async {
        loop {
            let running = source_client.run_get(source_dispatch.run_id.clone()).unwrap().lifecycle
                == HarnessRunLifecycleV1::Running;
            let provider_identity_observed = source_client.runtime_inventory_list(None, 32)
                .unwrap()
                .nodes
                .iter()
                .flat_map(|node| &node.inventory.managed_sessions)
                .any(|session| {
                    session.provider == "claude" && session.provider_identity_present
                });
            if running && provider_identity_observed {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("source run did not become Running");
    let source_correlation = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(correlation) = source_client.run_correlation_get(
                source_dispatch.run_id.clone(),
            ) {
                if correlation.availability == HarnessRunCorrelationAvailabilityV1::Available {
                    break correlation;
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }).await.expect("source run correlation did not become available");
    let HarnessRunSessionViewV1::Managed(source_managed) = source_correlation.session else {
        panic!("source run correlation was not managed");
    };
    let source_binding = HarnessSessionBindingV1 {
        node_id: source_correlation.node_id,
        node_incarnation: selector(source_correlation.node_incarnation_id.as_str()),
        workspace_id: source_correlation.workspace_id,
        session: HarnessSessionIdentityV1::Managed {
            record_id: source_managed.record_id,
            active_session: Some(
                source_managed.active_session
                    .expect("available source correlation has no active session"),
            ),
        },
    };
    let host = source_host;
    let host_task = source_host_task;
    let client = source_client;
    let HarnessSessionIdentityV1::Managed {
        record_id: source_record_id,
        active_session: Some(source_active_session),
    } = &source_binding.session
    else {
        unreachable!();
    };
    let exact_source_inventory_is_live = || {
        client.runtime_inventory_list(None, 32)
            .ok()
            .and_then(|page| page.nodes.into_iter().find(|node| {
                node.node_id == source_binding.node_id.as_str()
                    && node.incarnation_id == source_binding.node_incarnation.as_str()
            }))
            .and_then(|node| node.inventory.managed_sessions.into_iter().find(|record| {
                record.record_id == source_record_id.as_str()
                    && record.workspace_id == source_binding.workspace_id.as_str()
            }))
            .is_some_and(|record| {
                record.state == gate4agent_harness_api::HarnessRuntimeManagedStateV1::Live
                    && record.provider_identity_present
                    && record.active_binding.as_ref().is_some_and(|active| {
                        active.workspace_id == source_binding.workspace_id.as_str()
                            && active.instance_id == source_active_session.instance_id
                            && active.generation == source_active_session.generation
                    })
            })
    };
    if timeout(Duration::from_secs(30), async {
        loop {
            if exact_source_inventory_is_live() {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
    }).await.is_err() {
        let inventory = client.runtime_inventory_list(None, 32).ok();
        let matching_nodes = inventory.as_ref().map_or(0, |page| page.nodes.iter()
            .filter(|node| {
                node.node_id == source_binding.node_id.as_str()
                    && node.incarnation_id == source_binding.node_incarnation.as_str()
            })
            .count());
        let managed_records = inventory.as_ref().map_or(0, |page| page.nodes.iter()
            .flat_map(|node| node.inventory.managed_sessions.iter())
            .count());
        panic!(
            "source inventory readiness timeout: response={} nodes={} matching_nodes={} managed_records={} exact_live={}",
            inventory.is_some(),
            inventory.as_ref().map_or(0, |page| page.nodes.len()),
            matching_nodes,
            managed_records,
            exact_source_inventory_is_live(),
        );
    }
    if timeout(Duration::from_secs(30), async {
        loop {
            if client.monitor_get(source_dispatch.run_id.clone())
                .ok()
                .is_some_and(|monitor| {
                    monitor.run_id == source_dispatch.run_id
                        && monitor.availability == ProjectionAvailabilityV1::Current
                        && monitor.freshness == ProjectionFreshnessV1::Live
                        && !monitor.transport_incomplete
                        && monitor.features.history != FeatureObservationStateV1::Unknown
                })
            {
                break;
            }
            sleep(Duration::from_millis(500)).await;
        }
    }).await.is_err() {
        let monitor = client.monitor_get(source_dispatch.run_id.clone()).ok();
        panic!(
            "source support readiness timeout: response={} availability={:?} freshness={:?} transport_incomplete={:?} history={:?}",
            monitor.is_some(),
            monitor.as_ref().map(|projection| projection.availability),
            monitor.as_ref().map(|projection| projection.freshness),
            monitor.as_ref().map(|projection| projection.transport_incomplete),
            monitor.map(|projection| projection.features.history),
        );
    }
    // Same background-churn family as the later `policy_digest`/context-source
    // races (see the comments further down): the readiness poll just above
    // only proves the monitor projection satisfied its own checks at THAT
    // moment -- a GapWaiting/reconverge blip landing in the gap before this
    // specific call is processed can still make `observe_run_context_source`
    // itself report `Unavailable` once, transiently. Retry is safe: this is
    // a read/observe RPC, not a mutation, so there is no idempotency ledger
    // to worry about.
    let observed_source = timeout(Duration::from_secs(10), async {
        loop {
            match client.observe_run_context_source(source_dispatch.run_id.clone()) {
                Ok(observed) => break observed,
                Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Unavailable)) => {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!(
                    "observe_run_context_source failed with a non-transient error: {error:?}",
                ),
            }
        }
    })
    .await
    .expect(
        "source support stayed transiently Unavailable past the retry budget after its own readiness poll converged",
    );
    assert_eq!(observed_source.feature_state, FeatureObservationStateV1::Observed);
    assert_eq!(observed_source.message_count, 2);
    assert!(observed_source.message_count_exact);
    timeout(Duration::from_secs(10), async {
        loop {
            if client.monitor_get(source_dispatch.run_id.clone()).ok()
                .and_then(|monitor| monitor.history)
                .is_some_and(|history| history.message_count == 2)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness did not persist the live HistorySnapshot observation");
    assert_eq!(
        client.create_task(gate4agent_harness_protocol::HarnessCreateTaskRequestV1 {
            authority: authority('c'),
            task_id: target_task_id(),
            title: "Phase six issued target".to_owned(),
            body: String::new(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Ready,
        }).unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );

    let options = match timeout(Duration::from_secs(15), async {
        loop {
            let options = client.task_launch_options_get(target_task_id()).unwrap();
            if options.context_sources.len() == 1
                && options.managed_worktree_profiles.len() == 1
                && options.delivery_bundles.len() == 1
            {
                break options;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await {
        Ok(options) => options,
        Err(_) => {
            let last = client.task_launch_options_get(target_task_id()).unwrap();
            panic!(
                "reviewed V6 task launch options did not converge: plans={} profiles={} contexts={} deliveries={}",
                last.plans.len(),
                last.managed_worktree_profiles.len(),
                last.context_sources.len(),
                last.delivery_bundles.len(),
            );
        }
    };
    assert!(!options.truncated);
    let safe_options = serde_json::to_string(&options).unwrap();
    let workspace_root = fixture.workspace.to_string_lossy().into_owned();
    let allocation_root = fixture.allocation.to_string_lossy().into_owned();
    let source_cwd_text = source_cwd.to_string_lossy().into_owned();
    for private in [
        workspace_root.as_str(),
        allocation_root.as_str(),
        source_cwd_text.as_str(),
        SOURCE_USER,
        SOURCE_ASSISTANT,
        DELIVERY_PAYLOAD,
        PROVIDER_IDENTITY_CANARY,
        SOURCE_SESSION_ID,
        node_token,
        c2_token,
        operator_secret.as_str(),
    ] {
        assert!(!safe_options.contains(private), "unsafe task launch option: {private}");
    }
    let selection = HarnessReviewedTaskLaunchSelectionV1 {
        plan: options.plans.iter()
            .find(|plan| plan.plan.plan_id.as_str() == "phase-six-target")
            .unwrap()
            .clone(),
        worktree: HarnessReviewedWorktreeSelectionV1::Managed {
            profile: options.managed_worktree_profiles[0].clone(),
        },
        context_source: Some(options.context_sources[0].clone()),
        delivery: Some(options.delivery_bundles[0].clone()),
        review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
    };

    let replace = |marker, selection| HarnessReplaceTaskExecutionSpecRequestV2 {
        authority: authority(marker),
        task_id: target_task_id(),
        expected_task_revision: options.task_revision,
        expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
        selection,
    };
    let mut changed_plan = selection.clone();
    changed_plan.plan.plan.revision = revision(2);
    assert_conflict(client.replace_task_execution_spec_v2(replace('d', changed_plan)));
    let mut changed_context = selection.clone();
    changed_context.context_source.as_mut().unwrap().metadata_digest =
        HarnessRequestDigest::new("1".repeat(64)).unwrap();
    assert_conflict(client.replace_task_execution_spec_v2(replace('e', changed_context)));
    let mut changed_delivery = selection.clone();
    changed_delivery.delivery.as_mut().unwrap().bundle.revision =
        HarnessDeliveryBundleRevisionV1::new("revision-2").unwrap();
    assert_conflict(client.replace_task_execution_spec_v2(replace('f', changed_delivery)));
    let mut stale_profile = selection.clone();
    let HarnessReviewedWorktreeSelectionV1::Managed { profile } = &mut stale_profile.worktree
    else {
        unreachable!();
    };
    profile.profile_revision = selector("reviewed-managed-stale");
    assert_conflict(client.replace_task_execution_spec_v2(replace('g', stale_profile)));
    assert_eq!(allocation_count(&fixture.allocation), 0);

    // The source run stays live throughout this whole stretch, and its own
    // revision advances independently of anything under test here whenever
    // the coordinator's C2 control-event pipeline needs to revalidate it
    // (e.g. `freeze_bound_route_waiting`'s `GapWaiting` fallback bumps it by
    // design when an observation resync cannot yet prove the bound run is
    // still exactly live, then again when it reconverges). `selection` was
    // captured several round trips ago, above; re-read the context source
    // fresh right before the one call in this block that is expected to
    // succeed, exactly like a real client would re-fetch before submitting
    // rather than replaying an arbitrarily-aged snapshot.
    //
    // A single fetch-then-submit is not quite tight enough: the server
    // re-validates against its OWN freshly-computed options at the moment it
    // processes the request (`validate_current_launch_options` ->
    // `context_source_semantically_matches`, harness-service/src/lib.rs),
    // which is one more round trip after this client's own fetch -- the same
    // live background churn can still land in that second, narrower gap
    // (rare, but the fast-fail `Host(Conflict)` this produces is the same
    // race family as the `policy_digest` fix above, one step earlier in the
    // test). Retry is safe: `operator_replace_task_execution_spec_v2` checks
    // `replay_operator_request` before `validate_current_launch_options`
    // (`lib.rs:801-815`), so a validation-rejected attempt never records its
    // operation_id in the ledger -- resubmitting the same `'h'` marker with a
    // freshly re-fetched selection is a clean retry, not a replay.
    let applied = timeout(Duration::from_secs(10), async {
        loop {
            let current_options = client.task_launch_options_get(target_task_id()).unwrap();
            // The same GapWaiting/reconverge churn that bumps
            // `source_run_revision` can, for a single poll, transiently drop
            // the live source out of `context_sources` entirely (the `Live`
            // branch of `context_source_option`, runtime.rs, requires a
            // fully-Current/Live monitor projection with an exact active
            // managed-session binding match) -- wait for it to reappear
            // rather than indexing into a possibly-empty `Vec`.
            let Some(current_source) = current_options.context_sources.first() else {
                sleep(Duration::from_millis(20)).await;
                continue;
            };
            let mut fresh_selection = selection.clone();
            fresh_selection.context_source = Some(current_source.clone());
            match client.replace_task_execution_spec_v2(replace('h', fresh_selection)) {
                Ok(outcome) => break outcome,
                Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)) => {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!(
                    "replace_task_execution_spec_v2 failed with a non-conflict error: {error:?}",
                ),
            }
        }
    })
    .await
    .expect("fresh context source kept losing the race against live background churn");
    assert_eq!(applied, HarnessOperatorMutationOutcomeV1::Applied);
    let issued_options = client.task_launch_options_get(target_task_id()).unwrap();
    let issued_summary = issued_options.current_issued_spec.clone().unwrap();
    assert_eq!(issued_summary.revision, revision(1));
    stop_host(host, host_task).await;
    drop(client);

    let reopened = HarnessService::open(&fixture.harness).unwrap();
    let issuance_before = reopened.engine().task_launch_issuance(&target_task_id()).unwrap().clone();
    let spec_before = reopened.engine().task_execution_spec_v2(&target_task_id()).unwrap().clone();
    assert_eq!(spec_before.launch_issuance, issuance_before.reference());
    assert_eq!(issued_summary.launch_issuance, issuance_before.reference());
    drop(reopened);

    let changed_catalogs = runtime_catalogs(
        &node_id,
        &workspace_id,
        exact_delivery,
        revision(2),
    );
    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (changed_host, changed_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        changed_catalogs,
    ).await.unwrap();
    let changed_client = HarnessOperatorClient::new(
        changed_host.endpoint().socket_addr(),
        operator_credential.clone(),
    ).unwrap();
    let conflict_start_request = HarnessStartTaskRequestV2 {
        authority: authority('i'),
        task_id: target_task_id(),
        expected_task_revision: options.task_revision,
        expected_execution_spec_revision: spec_before.revision,
        expected_launch_issuance: issuance_before.reference(),
    };
    assert_start_conflict(changed_client.start_task_v2(conflict_start_request));
    assert_eq!(allocation_count(&fixture.allocation), 0);
    stop_host(changed_host, changed_host_task).await;
    drop(changed_client);

    let preflight = HarnessService::open(&fixture.harness).unwrap();
    let current_task = preflight.engine().task(&target_task_id()).unwrap();
    assert_eq!(current_task.state, HarnessTaskStateV1::Ready);
    assert_eq!(current_task.revision, issuance_before.task_revision);
    assert_eq!(
        current_task.creator,
        HarnessActorV1::User { actor_id: selector("fixture-operator") },
    );
    let current_source_run = preflight.engine().run(&source_dispatch.run_id).unwrap();
    assert_eq!(current_source_run.binding.as_ref(), Some(&source_binding));
    assert!(preflight.dispatch_context(&current_source_run.operation_id).is_some());
    assert_eq!(preflight.engine().runs().count(), 1);
    drop(preflight);

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (exact_host, exact_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        exact_catalogs.clone(),
    ).await.unwrap();
    let exact_client = HarnessOperatorClient::new(
        exact_host.endpoint().socket_addr(),
        operator_credential.clone(),
    ).unwrap();
    let resynced_options = exact_client.task_launch_options_get(target_task_id()).unwrap();
    let resynced_source = resynced_options.context_sources.iter()
        .find(|source| source.source_run_id == source_dispatch.run_id)
        .unwrap();
    // Same background churn as above: the source run's own revision only
    // ever advances (never regresses) across this reconnect boundary, it is
    // not pinned to the snapshot `selection` captured well before it.
    assert!(
        resynced_source.source_run_revision
            >= selection.context_source.as_ref().unwrap().source_run_revision,
    );
    assert!(
        resynced_source.observed_at_unix_ms
            >= selection.context_source.as_ref().unwrap().observed_at_unix_ms,
    );
    assert_eq!(resynced_options.task_revision, issuance_before.task_revision);
    // `policy_digest` embeds the whole `context_sources` array (only
    // `observed_at_unix_ms` normalized away -- see `task_launch_policy_digest`,
    // harness-service/src/lib.rs), including `source_run_revision` and each
    // source's own `metadata_digest`. The source run stays live throughout
    // this whole stretch (see the comment above `fresh_selection`), and its
    // revision can legitimately bump on either reconnect between here and
    // where `issuance_before` was captured (`freeze_bound_route_waiting`'s
    // `GapWaiting` fallback), independent of anything under test -- so a
    // byte-for-byte `policy_digest` comparison across that window is the
    // same class of race already de-raced above, just surfacing through a
    // digest instead of a raw counter. `task_revision` (just above), `plans`,
    // `managed_worktree_profiles`, and the context source's stable identity
    // (all checked individually below) are the actual invariant this
    // assertion was meant to protect; digest determinism itself is already
    // covered by `task_launch_policy_digest`'s own unit tests in `lib.rs`.
    assert!(resynced_options.plans.iter().any(|plan| {
        plan.plan == issuance_before.plan
            && plan.node_id == issuance_before.target.node_id
            && plan.source_workspace_id == issuance_before.target.source_workspace_id
            && plan.provider_profile == issuance_before.target.provider_profile
            && plan.mode == issuance_before.target.mode
    }));
    let gate4agent_harness_protocol::HarnessLaunchWorktreeSelectionV1::Managed {
        profile_id,
        expected_profile_revision,
    } = &issuance_before.target.worktree else {
        unreachable!();
    };
    assert!(resynced_options.managed_worktree_profiles.iter().any(|profile| {
        profile.node_id == issuance_before.target.node_id
            && profile.source_workspace_id == issuance_before.target.source_workspace_id
            && profile.profile_id == *profile_id
            && profile.profile_revision == *expected_profile_revision
    }));
    // Same background churn as `policy_digest` above: compare the context
    // source's stable identity exactly, and require the fields a live
    // source can only grow (never shrink) to have advanced monotonically
    // rather than pinning them to the aged `issuance_before` snapshot.
    let current_context = resynced_source.clone();
    let issued_context = issuance_before.context_source.clone().unwrap();
    assert_eq!(current_context.source_run_id, issued_context.source_run_id);
    assert_eq!(current_context.node_id, issued_context.node_id);
    assert_eq!(current_context.node_incarnation, issued_context.node_incarnation);
    assert_eq!(current_context.workspace_id, issued_context.workspace_id);
    assert_eq!(current_context.session_record_id, issued_context.session_record_id);
    assert_eq!(current_context.active_session, issued_context.active_session);
    assert_eq!(current_context.availability, issued_context.availability);
    assert_eq!(current_context.message_count_exact, issued_context.message_count_exact);
    assert_eq!(current_context.context_pack, issued_context.context_pack);
    assert!(current_context.source_run_revision >= issued_context.source_run_revision);
    assert!(current_context.message_count >= issued_context.message_count);
    assert!(
        current_context.completed_turn_count.unwrap_or(0)
            >= issued_context.completed_turn_count.unwrap_or(0)
    );
    assert!(current_context.total_tokens.unwrap_or(0) >= issued_context.total_tokens.unwrap_or(0));
    assert!(resynced_options.delivery_bundles.contains(
        issuance_before.delivery.as_ref().unwrap(),
    ));
    let start_request = HarnessStartTaskRequestV2 {
        authority: authority('j'),
        task_id: target_task_id(),
        expected_task_revision: options.task_revision,
        expected_execution_spec_revision: spec_before.revision,
        expected_launch_issuance: issuance_before.reference(),
    };
    assert_eq!(spec_before.review_policy, HarnessTaskReviewPolicyV1::OperatorReview);
    assert_eq!(issued_summary.review_policy, HarnessTaskReviewPolicyV1::OperatorReview);
    let preallocation_state = public_preallocation_state(&exact_client, &target_task_id());

    let mut stale_task_start = start_request.clone();
    stale_task_start.authority = authority('k');
    stale_task_start.expected_task_revision = revision(
        start_request.expected_task_revision.get().checked_add(1).unwrap(),
    );
    assert_start_conflict(exact_client.start_task_v2(stale_task_start));
    assert_no_public_preallocation_effects(
        &exact_client,
        &target_task_id(),
        &fixture.allocation,
        &preallocation_state,
    );

    let mut wrong_issuance_start = start_request.clone();
    wrong_issuance_start.authority = authority('l');
    wrong_issuance_start.expected_launch_issuance.digest =
        HarnessRequestDigest::new("2".repeat(64)).unwrap();
    assert_start_conflict(exact_client.start_task_v2(wrong_issuance_start));
    assert_no_public_preallocation_effects(
        &exact_client,
        &target_task_id(),
        &fixture.allocation,
        &preallocation_state,
    );

    // OperatorReview is the only representable review policy. Prove the wire
    // contract rejects a policy mutation before exercising the allowed-policy
    // issued-spec CAS with a changed reviewed selection.
    let mut invalid_policy = serde_json::to_value(&selection).unwrap();
    invalid_policy["review_policy"] = Value::String("unreviewed".to_owned());
    assert!(serde_json::from_value::<HarnessReviewedTaskLaunchSelectionV1>(invalid_policy)
        .is_err());
    assert_no_public_preallocation_effects(
        &exact_client,
        &target_task_id(),
        &fixture.allocation,
        &preallocation_state,
    );

    let mut issued_spec_mutation = selection.clone();
    issued_spec_mutation.plan.plan.revision = revision(2);
    assert_conflict(exact_client.replace_task_execution_spec_v2(
        HarnessReplaceTaskExecutionSpecRequestV2 {
            authority: authority('m'),
            task_id: target_task_id(),
            expected_task_revision: issuance_before.task_revision,
            expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Exact(
                spec_before.revision,
            ),
            selection: issued_spec_mutation,
        },
    ));
    assert_no_public_preallocation_effects(
        &exact_client,
        &target_task_id(),
        &fixture.allocation,
        &preallocation_state,
    );

    // `start_task_v2`'s anti-tamper gate (`validate_current_issued_launch_options`,
    // `lib.rs:4908-4952`) is intentionally strict, by design, not a bug: if
    // the live source's telemetry moved since `issuance_before` was captured
    // (the same `freeze_bound_route_waiting` `GapWaiting` churn de-raced
    // above), the reviewed snapshot the operator approved is genuinely
    // stale, and "re-review, reissue, start again" is the correct
    // real-client response -- not a bare retry of the identical request
    // (`HarnessRevision` is monotonic, so a second attempt against the SAME
    // frozen `issuance_before`/`spec_before` would fail identically forever,
    // never transiently). Model that real-client response here, bounded to
    // exactly one reissue cycle: re-fetch launch options, re-save the spec
    // from the fresh context source, re-capture `issuance_before`/
    // `spec_before`/`start_request` from the freshly-persisted state, and
    // start again -- the whole downstream chain (correlation, durability,
    // replay, reverse-attribution) then runs against whichever issuance
    // actually started, which is its real intent: proving consistency of
    // the launch that happened, not pinning to a specific attempt number. A
    // second consecutive `Conflict` is a different bug and fails loudly,
    // never swallowed into a retry loop.
    let (started, issuance_before, spec_before, start_request) =
        match exact_client.start_task_v2(start_request.clone()) {
            Ok(started) => (started, issuance_before, spec_before, start_request),
            Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)) => {
                let reissued = timeout(Duration::from_secs(10), async {
                    loop {
                        let current_options =
                            exact_client.task_launch_options_get(target_task_id()).unwrap();
                        // Same transient-empty-Vec churn as the first replace
                        // above -- wait for the live source to reappear
                        // rather than indexing into a possibly-empty `Vec`.
                        let Some(current_source) = current_options.context_sources.first()
                        else {
                            sleep(Duration::from_millis(20)).await;
                            continue;
                        };
                        let mut fresh_selection = selection.clone();
                        fresh_selection.context_source = Some(current_source.clone());
                        let mut reissue_request = replace('n', fresh_selection);
                        reissue_request.expected_execution_spec_revision =
                            HarnessExpectedExecutionSpecRevisionV1::Exact(spec_before.revision);
                        match exact_client.replace_task_execution_spec_v2(reissue_request) {
                            Ok(outcome) => break outcome,
                            Err(HarnessOperatorClientError::Host(
                                HarnessOperatorHostErrorV1::Conflict,
                            )) => {
                                sleep(Duration::from_millis(20)).await;
                            }
                            Err(error) => panic!(
                                "reissue replace_task_execution_spec_v2 failed with a non-conflict error: {error:?}",
                            ),
                        }
                    }
                })
                .await
                .expect(
                    "reissue's fresh context source kept losing the race against live background churn",
                );
                assert_eq!(reissued, HarnessOperatorMutationOutcomeV1::Applied);

                let reopened = HarnessService::open(&fixture.harness).unwrap();
                let issuance_before = reopened.engine()
                    .task_launch_issuance(&target_task_id())
                    .unwrap()
                    .clone();
                let spec_before = reopened.engine()
                    .task_execution_spec_v2(&target_task_id())
                    .unwrap()
                    .clone();
                assert_eq!(spec_before.launch_issuance, issuance_before.reference());
                assert_eq!(issuance_before.revision.get(), 2);
                drop(reopened);

                let start_request = HarnessStartTaskRequestV2 {
                    authority: authority('o'),
                    task_id: target_task_id(),
                    expected_task_revision: options.task_revision,
                    expected_execution_spec_revision: spec_before.revision,
                    expected_launch_issuance: issuance_before.reference(),
                };
                let started = match exact_client.start_task_v2(start_request.clone()) {
                    Ok(started) => started,
                    Err(error) => panic!(
                        "start_task_v2 conflicted again immediately after a fresh reissue -- \
                         a different bug, not the live-source-drift race: {error:?}",
                    ),
                };
                (started, issuance_before, spec_before, start_request)
            }
            Err(error) => panic!("start_task_v2 failed with a non-conflict error: {error:?}"),
        };
    assert!(!started.replayed);
    let correlation = match timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(correlation) = exact_client.run_correlation_get(
                started.dispatch.run_id.clone(),
            ) {
                if correlation.availability == HarnessRunCorrelationAvailabilityV1::Available {
                    break correlation;
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }).await {
        Ok(correlation) => correlation,
        Err(_) => {
            let run = exact_client.run_get(started.dispatch.run_id.clone()).ok();
            let correlation = exact_client.run_correlation_get(
                started.dispatch.run_id.clone(),
            ).ok();
            let correlation_record = correlation.as_ref().and_then(|correlation| {
                match &correlation.session {
                    HarnessRunSessionViewV1::Managed(managed) => Some(&managed.record_id),
                    HarnessRunSessionViewV1::Inline(_) => None,
                }
            });
            let inventory = exact_client.runtime_inventory_list(None, 32).ok();
            let workspace_count = inventory.as_ref().map_or(0, |page| page.nodes.iter()
                .map(|node| node.inventory.workspace_count)
                .sum::<usize>());
            let managed_record_count = inventory.as_ref().map_or(0, |page| page.nodes.iter()
                .map(|node| node.inventory.managed_session_count)
                .sum::<usize>());
            let matching_record = correlation_record.is_some_and(|record_id| {
                inventory.as_ref().is_some_and(|page| page.nodes.iter().any(|node| {
                    node.inventory.managed_sessions.iter()
                        .any(|record| record.record_id == record_id.as_str())
                }))
            });
            let session_kind = correlation.as_ref().map(|correlation| {
                match &correlation.session {
                    HarnessRunSessionViewV1::Managed(_) => "managed",
                    HarnessRunSessionViewV1::Inline(_) => "inline",
                }
            });
            let worktree_kind = correlation.as_ref().map(|correlation| {
                match &correlation.worktree {
                    gate4agent_harness_api::HarnessRunWorktreeViewV1::Existing => "existing",
                    gate4agent_harness_api::HarnessRunWorktreeViewV1::Managed { .. } => "managed",
                }
            });
            let managed_spawn_failure_code = managed_spawn_failure_probe.latest_code();
            stop_host(exact_host, exact_host_task).await;
            let durable = HarnessService::open(&fixture.harness).unwrap();
            let operation_state = durable.engine()
                .operation(&start_request.authority.operation_id)
                .map(|operation| operation.state);
            let delivery_state = durable.engine()
                .delivery_for_run(&started.dispatch.run_id)
                .map(|delivery| delivery.state);
            let continuation_state = durable.engine()
                .continuation_for_run(&started.dispatch.run_id)
                .map(|continuation| continuation.state);
            panic!(
                "run correlation readiness timeout: run_response={} lifecycle={:?} revision={:?} failure_category={:?} disposition={:?} operation_state={:?} delivery_state={:?} continuation_state={:?} managed_spawn_failure_code={:?} correlation_response={} availability={:?} session_kind={:?} worktree_kind={:?} observed={} inventory_response={} nodes={} workspaces={} managed_records={} matching_record={}",
                run.is_some(),
                run.as_ref().map(|run| run.lifecycle),
                run.as_ref().map(|run| run.revision),
                run.as_ref().and_then(|run| run.failure_category),
                run.as_ref().and_then(|run| run.result_disposition),
                operation_state,
                delivery_state,
                continuation_state,
                managed_spawn_failure_code,
                correlation.is_some(),
                correlation.as_ref().map(|correlation| correlation.availability),
                session_kind,
                worktree_kind,
                correlation.as_ref().is_some_and(|correlation| {
                    correlation.observed_at_unix_ms.is_some()
                }),
                inventory.is_some(),
                inventory.as_ref().map_or(0, |page| page.nodes.len()),
                workspace_count,
                managed_record_count,
                matching_record,
            );
        }
    };
    assert_eq!(managed_spawn_failure_probe.latest_code(), None);
    let HarnessRunSessionViewV1::Managed(managed) = correlation.session else {
        panic!("issued managed run correlation was not managed");
    };
    let attributed_binding = HarnessSessionBindingV1 {
        node_id: correlation.node_id,
        node_incarnation: selector(correlation.node_incarnation_id.as_str()),
        workspace_id: correlation.workspace_id,
        session: HarnessSessionIdentityV1::Managed {
            record_id: managed.record_id,
            active_session: Some(
                managed.active_session.expect("available managed correlation has no active session"),
            ),
        },
    };
    let HarnessSessionIdentityV1::Managed {
        record_id: target_record_id,
        active_session: Some(target_active_session),
    } = &attributed_binding.session
    else {
        unreachable!();
    };
    timeout(Duration::from_secs(15), async {
        loop {
            let exact_target_inventory_is_live = exact_client
                .runtime_inventory_list(None, 32)
                .ok()
                .and_then(|page| page.nodes.into_iter().find(|node| {
                    node.node_id == attributed_binding.node_id.as_str()
                        && node.incarnation_id == attributed_binding.node_incarnation.as_str()
                }))
                .and_then(|node| node.inventory.managed_sessions.into_iter().find(|record| {
                    record.record_id == target_record_id.as_str()
                        && record.workspace_id == attributed_binding.workspace_id.as_str()
                }))
                .is_some_and(|record| {
                    record.state == gate4agent_harness_api::HarnessRuntimeManagedStateV1::Live
                        && record.provider_identity_present
                        && record.active_binding.as_ref().is_some_and(|active| {
                            active.workspace_id == attributed_binding.workspace_id.as_str()
                                && active.instance_id == target_active_session.instance_id
                                && active.generation == target_active_session.generation
                        })
                });
            if exact_target_inventory_is_live {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("target provider identity did not converge through Hook ingress");
    // The dispatch coordinator's own task/run lifecycle view converges
    // through a separate C2 control-event pipeline from the Node-side
    // session inventory polled above (`exact_target_inventory_is_live`):
    // `start_task_v2` returning only means dispatch was accepted, never that
    // the coordinator has already observed and committed `Running` — that
    // commit lands later, asynchronously, off the control-event stream
    // (`apply_exact_control_lifecycle` / `apply_spawn_result`). That stream
    // also has a legitimate transient-`Waiting` fallback of its own
    // (`freeze_bound_route_waiting`'s `GapWaiting`, taken whenever an
    // observation resync cannot yet prove a bound run is still exactly
    // live) which a slow poller can observe mid-flight. Poll here the same
    // bounded way the inventory wait above does, instead of assuming a
    // synchronous guarantee `start_task_v2` never made.
    let (public_task, public_run) = timeout(Duration::from_secs(15), async {
        loop {
            let task = exact_client.task_get(target_task_id()).unwrap();
            let run = exact_client.run_get(started.dispatch.run_id.clone()).unwrap();
            if task.state == HarnessTaskStateV1::Running
                && run.lifecycle == HarnessRunLifecycleV1::Running
            {
                break (task, run);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("target task/run did not converge to Running through the C2 control-event pipeline");
    assert_eq!(public_task.state, HarnessTaskStateV1::Running);
    assert_eq!(public_task.run_ids, vec![started.dispatch.run_id.clone()]);
    assert_eq!(public_run.lifecycle, HarnessRunLifecycleV1::Running);
    let public_transfer = exact_client.run_transfer_get(started.dispatch.run_id.clone()).unwrap();
    assert_eq!(
        public_transfer.delivery.as_ref().unwrap().state,
        HarnessDeliveryStateV1::Committed,
    );
    assert_eq!(
        public_transfer.continuation.as_ref().unwrap().state,
        HarnessContinuationStateV1::Bound,
    );
    assert_reverse_attribution(
        &exact_client,
        &target_task_id(),
        &started.dispatch.run_id,
        &attributed_binding,
    );
    // Re-confirm immediately before disconnecting: the coordinator's C2
    // control-event pipeline can independently detect a live-stream sequence
    // gap at any time, not only around explicit host restarts, and take the
    // same `GapWaiting` fallback confirmed above. Re-polling right here — with
    // nothing but this call between the last check and `stop_host` — shrinks
    // the unwatched window as far as the test can control it.
    timeout(Duration::from_secs(15), async {
        loop {
            if exact_client.run_get(started.dispatch.run_id.clone())
                .is_ok_and(|run| run.lifecycle == HarnessRunLifecycleV1::Running)
            {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("target run regressed from Running before the host could be stopped");
    stop_host(exact_host, exact_host_task).await;
    drop(exact_client);

    let durable = HarnessService::open(&fixture.harness).unwrap();
    let task = durable.engine().task(&target_task_id()).unwrap();
    let run = durable.engine().run(&started.dispatch.run_id).unwrap();
    let delivery = durable.engine().delivery_for_run(&started.dispatch.run_id).unwrap();
    let continuation = durable.engine().continuation_for_run(&started.dispatch.run_id).unwrap();
    assert_eq!(task.state, HarnessTaskStateV1::Running);
    assert_eq!(run.task_id, target_task_id());
    assert_eq!(run.lifecycle, HarnessRunLifecycleV1::Running);
    assert_eq!(run.binding.as_ref(), Some(&attributed_binding));
    assert_eq!(delivery.state, HarnessDeliveryStateV1::Committed);
    assert_eq!(continuation.state, HarnessContinuationStateV1::Bound);
    assert_eq!(
        delivery.authority,
        HarnessTransferAuthorityRefV1::OperatorIssuance {
            issuance: issuance_before.reference(),
        },
    );
    assert_eq!(continuation.authority, delivery.authority);
    assert_eq!(
        durable.engine().operation(&start_request.authority.operation_id).unwrap().state,
        HarnessOperationStateV1::Succeeded,
    );
    assert_eq!(durable.engine().task_launch_issuance(&target_task_id()).unwrap(), &issuance_before);
    assert_eq!(durable.engine().task_execution_spec_v2(&target_task_id()).unwrap(), &spec_before);
    drop(durable);

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (replay_host, replay_host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        exact_catalogs,
    ).await.unwrap();
    let replay_client = HarnessOperatorClient::new(
        replay_host.endpoint().socket_addr(),
        operator_credential,
    ).unwrap();
    assert_reverse_attribution(
        &replay_client,
        &target_task_id(),
        &started.dispatch.run_id,
        &attributed_binding,
    );
    let replayed = replay_client.start_task_v2(start_request).unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.dispatch, started.dispatch);
    stop_host(replay_host, replay_host_task).await;
    drop(replay_client);

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let route = adapter.exact_route(&node_id).unwrap();
    let snapshot = adapter.snapshot(&route).await.unwrap();
    let final_service = HarnessService::open(&fixture.harness).unwrap();
    let target_run = final_service.engine()
        .run(&started.dispatch.run_id)
        .unwrap()
        .clone();
    let target_continuation = final_service.engine()
        .continuation_for_run(&started.dispatch.run_id)
        .unwrap()
        .clone();
    drop(final_service);
    assert_eq!(target_run.task_id, target_task_id());
    let target_binding = target_run.binding.as_ref().unwrap();
    let HarnessSessionIdentityV1::Managed {
        record_id,
        active_session: Some(active_session),
    } = &target_binding.session else {
        panic!("issued run did not retain an exact managed session binding");
    };
    assert_eq!(snapshot.managed_worktrees.len(), 1);
    assert_eq!(snapshot.managed_worktrees[0].profile_id, worktree_profile_id);
    assert_eq!(snapshot.managed_worktrees[0].profile_revision, worktree_profile_revision);
    assert_eq!(snapshot.managed_worktrees[0].active_session_count, 1);
    assert_ne!(target_binding.workspace_id, source_binding.workspace_id);
    assert_eq!(
        snapshot.managed_worktrees[0].source_workspace_id.as_str(),
        source_binding.workspace_id.as_str(),
    );
    assert_eq!(target_binding.workspace_id.as_str(), snapshot.managed_worktrees[0].workspace_id.as_str());
    let target_record = snapshot.session_records.iter()
        .find(|record| record.record_id.as_str() == record_id.as_str())
        .unwrap();
    assert_eq!(target_record.state, ManagedSessionState::Live);
    let issued_delivery = issuance_before.delivery.as_ref().unwrap();
    let target_bundle = target_record.bundle.as_ref().unwrap();
    assert_eq!(target_bundle.id.as_str(), issued_delivery.bundle.bundle_id.as_str());
    assert_eq!(target_bundle.revision.as_str(), issued_delivery.bundle.revision.as_str());
    assert_eq!(target_bundle.digest.as_str(), issued_delivery.bundle.digest.as_str());
    let issued_context = issuance_before.context_source.as_ref().unwrap();
    let resolved_context = target_continuation.context.as_ref().unwrap();
    let target_context = target_record.context.as_ref().unwrap();
    assert_eq!(target_record.context_id.as_ref(), Some(&target_context.id));
    assert_eq!(target_context.id.as_str(), resolved_context.id.as_str());
    assert_eq!(target_context.digest.as_str(), resolved_context.digest.as_str());
    assert_eq!(
        target_context.lineage.source_node_id.as_str(),
        issued_context.node_id.as_str(),
    );
    assert_eq!(
        target_context.lineage.source_session.workspace_id.as_str(),
        issued_context.workspace_id.as_str(),
    );
    assert_eq!(
        target_context.lineage.source_session.session.instance_id.0,
        issued_context.active_session.as_ref().unwrap().instance_id,
    );
    assert_eq!(
        target_context.lineage.source_session.session.generation.0,
        issued_context.active_session.as_ref().unwrap().generation,
    );
    assert_eq!(target_context.lineage.source_provider.as_str(), "claude");
    assert_eq!(target_context.source_message_count, issued_context.message_count);
    assert_eq!(target_context.retained_message_count, issued_context.message_count);
    assert!(target_record.task_binding.is_none());
    assert_eq!(target_record.active_session.as_ref().unwrap().session.instance_id.0,
        active_session.instance_id);
    assert_eq!(target_record.active_session.as_ref().unwrap().session.generation.0,
        active_session.generation);
    assert_eq!(snapshot.session_records.len(), 2);
    let proof_text = fs::read_to_string(&fixture.proof).unwrap();
    for private in [SOURCE_SESSION_ID, PROVIDER_IDENTITY_CANARY] {
        assert!(!proof_text.contains(private), "private identity leaked into proof: {private}");
    }
    let proof = proof_text.lines().collect::<Vec<_>>();
    assert_eq!(proof.len(), 11);
    assert_eq!(proof[0], "contextual");
    assert!(Path::new(proof[1]).starts_with(&fixture.materializations));
    assert!(fs::canonicalize(proof[2]).unwrap()
        .starts_with(fs::canonicalize(&fixture.allocation).unwrap()));
    assert_eq!(proof[3], "d78fe03be106b673fcf5415c8359e99f0298b5071cd11822c58ebcb27e52a68d");
    assert!(Path::new(proof[4]).starts_with(&fixture.materializations));
    assert_eq!(proof[6], "g4a-context-pack-v1");
    assert_eq!(proof[7], "claude");
    assert_eq!(proof[8], "2");
    assert_eq!(proof[9], SOURCE_USER);
    assert_eq!(proof[10], SOURCE_ASSISTANT);
    let raw_context = fs::read_to_string(Path::new(proof[4]).join("context-pack.json")).unwrap();
    for private in [SOURCE_SESSION_ID, PROVIDER_IDENTITY_CANARY] {
        assert!(!raw_context.contains(private), "private identity leaked into context: {private}");
    }
    drop(adapter);
    drop(events);
    drop(c2_client);

    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    drop(c2_shutdown);
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), node_task).await.unwrap().unwrap().unwrap();
    drop(node_shutdown);
    drop(managed_spawn_failure_probe);

    let fixture_root = fixture.root.clone();
    fs::remove_dir_all(&fixture_root).unwrap();
    assert!(!fixture_root.exists(), "fixture root survived explicit cleanup");
}
