#![cfg(windows)]

use std::{
    ffi::OsString,
    io::{BufRead as _, BufReader as StdBufReader, BufWriter as StdBufWriter, Read as _, Write as _},
    path::PathBuf,
    process::{Child as StdChild, ChildStdin as StdChildStdin, ChildStdout as StdChildStdout, Command as StdCommand, Stdio},
    sync::{atomic::{AtomicU64, Ordering}, Mutex},
    thread,
    time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_engine::HarnessMutationV1;
use gate4agent_harness_client::{
    HarnessOperatorClient, HarnessOperatorCredential, HarnessReadClient,
    HarnessReadCredential,
};
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessContextPermissionsV1, HarnessEntityReadScopeV1,
    HarnessExecutionModeV1, HarnessGrantTargetV1, HarnessIdempotencyRef,
    HarnessMonitoringVisibilityV1, HarnessOperationId, HarnessOperationKindV1,
    HarnessOperationStateV1, HarnessOperationTimeoutsV1, HarnessOperationV1,
    HarnessOperatorAuthorityV1,
    HarnessReadPermissionsV1, HarnessRevision, HarnessRunId, HarnessRunIntentV1,
    HarnessRunLifecycleV1, HarnessRunV1, HarnessSelectorV1,
    HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1,
    HarnessSessionIdentityV1, HarnessTaskId,
    HarnessTaskPermissionsV1, HarnessTaskStateV1, HarnessTaskV1,
    HarnessWorktreeIntentV1, SessionGrantId, SessionGrantStateV1, SessionGrantV1,
};
use gate4agent_harness_service::{
    c2::{
        HarnessC2Adapter, HarnessC2EventReceiver,
    },
    credential::CredentialBindingV1,
    mutation_request_digest,
    dispatch::{
        deterministic_dispatch_ids, HarnessContinuationPolicyV1,
        HarnessGrantPolicyV1, HarnessLaunchCatalog, HarnessLaunchPlanV1,
        HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    runtime::{
        start_harness_host, start_harness_host_with_operator_and_catalogs,
        HarnessRuntimeCatalogs,
    },
    HarnessMcpReservationStateV1, HarnessService,
};
use gate4agent_node::{
    protocol::{
        NodeId, ProviderRuntimeMode, SessionMode, SpawnProfileDefaults,
        SpawnProfileId, SpawnProfileRevision, WorkspaceId,
    },
    NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig,
};
use gate4agent_observation_service::ObservationService;
use gate4agent_testkit::{
    require_windows_headless_supervisor_for_test, MONITORING_PROMPT_CANARY,
    MONITORING_PROVIDER_SESSION_CANARY, MONITORING_TOOL_INPUT_CANARY,
    MONITORING_TOOL_OUTPUT_CANARY,
};
use gate4agent_types::{AgentId, TerminalSize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{sleep, timeout},
};

static H3B_E2E_LOCK: Mutex<()> = Mutex::new(());

struct Databases {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
}

impl Databases {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-h2-e2e-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        std::fs::create_dir(&root).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            root,
        }
    }

    fn assert_private_bytes_absent(&self, canaries: &[&str]) {
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() { continue; }
            let bytes = std::fs::read(entry.path()).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            for canary in canaries {
                assert!(!text.contains(canary), "private canary persisted in {}", entry.path().display());
            }
        }
    }
}

impl Drop for Databases {
    fn drop(&mut self) {
        if self.root.is_dir() { let _ = std::fs::remove_dir_all(&self.root); }
    }
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!(r"\\.\pipe\gate4agent-h2-{label}-{}-{nonce}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().try_into().unwrap()
}

fn is_harness_read_unavailable(reply: &Value) -> bool {
    reply["error"]["code"] == -32603
        && reply["error"]["message"] == "harness read unavailable"
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 { HarnessSelectorV1::new(value).unwrap() }
fn revision(value: u64) -> HarnessRevision { HarnessRevision::new(value).unwrap() }
fn agent(value: &str) -> AgentId { AgentId::new(value).unwrap() }
fn task_id(value: char) -> HarnessTaskId { HarnessTaskId::new(format!("htask_{}", value.to_string().repeat(24))).unwrap() }
fn hidden_run_id() -> HarnessRunId { HarnessRunId::new("hrun_999999999999999999999999").unwrap() }
fn grant_id() -> SessionGrantId { SessionGrantId::new("hgrant_333333333333333333333333").unwrap() }
fn operation_id(value: char) -> HarnessOperationId { HarnessOperationId::new(format!("hop_{}", value.to_string().repeat(24))).unwrap() }
fn actor() -> HarnessActorV1 { HarnessActorV1::User { actor_id: selector("h2-fixture-operator") } }

fn operation(
    id: char,
    kind: HarnessOperationKindV1,
    state: HarnessOperationStateV1,
    task_id: Option<HarnessTaskId>,
    run_id: Option<HarnessRunId>,
    grant_id: Option<SessionGrantId>,
    expected_revision: Option<HarnessRevision>,
    now: u64,
) -> HarnessOperationV1 {
    HarnessOperationV1 {
        operation_id: operation_id(id),
        revision: revision(1),
        actor: actor(),
        kind,
        state,
        task_id,
        run_id,
        grant_id,
        reconciles_operation_id: None,
        expected_revision,
        request_digest: gate4agent_harness_protocol::HarnessRequestDigest::new("0".repeat(64)).unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{}", id.to_string().repeat(24))).unwrap(),
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        dispatched_at_unix_ms: matches!(state, HarnessOperationStateV1::Dispatching).then_some(now),
        finished_at_unix_ms: matches!(state, HarnessOperationStateV1::Succeeded | HarnessOperationStateV1::Failed | HarnessOperationStateV1::Reconciled).then_some(now),
    }
}

fn apply(service: &mut HarnessService, mut mutation: HarnessMutationV1) {
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    service.apply(mutation).unwrap();
}

fn create_task(service: &mut HarnessService, id: HarnessTaskId, title: &str, body: &str, now: u64) {
    let operation = operation(
        if id == task_id('1') { 'a' } else { '9' },
        HarnessOperationKindV1::CreateTask,
        HarnessOperationStateV1::Succeeded,
        Some(id.clone()),
        None,
        None,
        None,
        now,
    );
    let task = HarnessTaskV1 {
        task_id: id,
        revision: revision(1),
        title: title.to_owned(),
        body: body.to_owned(),
        creator: actor(),
        parent_task_id: None,
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Ready,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    apply(service, HarnessMutationV1::CreateTask { operation, task });
}

fn create_actor_child_task(
    service: &mut HarnessService,
    parent_run_id: &HarnessRunId,
    now: u64,
) {
    let id = task_id('4');
    let mut operation = operation(
        '4',
        HarnessOperationKindV1::CreateTask,
        HarnessOperationStateV1::Succeeded,
        Some(id.clone()),
        None,
        None,
        None,
        now,
    );
    operation.actor = HarnessActorV1::ParentRun { run_id: parent_run_id.clone() };
    let task = HarnessTaskV1 {
        task_id: id,
        revision: revision(1),
        title: "Actor child task".to_owned(),
        body: "Child task created by the actor run".to_owned(),
        creator: HarnessActorV1::ParentRun { run_id: parent_run_id.clone() },
        parent_task_id: Some(task_id('1')),
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Ready,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    apply(service, HarnessMutationV1::CreateTask { operation, task });
}

fn create_hidden_requested_run(
    service: &mut HarnessService,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    now: u64,
) {
    let mut task = service.engine().task(&task_id('9')).unwrap().clone();
    task.revision = revision(2);
    task.run_ids = vec![hidden_run_id()];
    task.updated_at_unix_ms = now;
    let operation = operation(
        '8',
        HarnessOperationKindV1::CreateRun,
        HarnessOperationStateV1::Prepared,
        Some(task_id('9')),
        Some(hidden_run_id()),
        None,
        Some(revision(1)),
        now,
    );
    let run = HarnessRunV1 {
        run_id: hidden_run_id(),
        revision: revision(1),
        parent_run_id: None,
        task_id: task_id('9'),
        operation_id: operation.operation_id.clone(),
        intent: HarnessRunIntentV1 {
            node_id: selector(node_id.as_str()),
            workspace_id: selector(workspace_id.as_str()),
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
        operation,
        expected_task_revision: revision(1),
        task,
        run,
    };
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    service.apply(mutation).unwrap();
}

fn create_grant(
    service: &mut HarnessService,
    actor_run_id: &HarnessRunId,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    now: u64,
) -> SessionGrantV1 {
    let grant = SessionGrantV1 {
        grant_id: grant_id(),
        revision: revision(1),
        actor_run_id: actor_run_id.clone(),
        allowed_targets: vec![HarnessGrantTargetV1 {
            node_id: selector(node_id.as_str()),
            workspace_id: selector(workspace_id.as_str()),
            provider_profile: selector("claude"),
            mode: HarnessExecutionModeV1::Pty,
        }],
        allowed_delivery_bundles: Vec::new(),
        maximum_child_count: 4,
        maximum_child_depth: 2,
        operation_timeouts: HarnessOperationTimeoutsV1 {
            dispatch_ms: 5_000,
            wait_ms: 5_000,
            reconciliation_ms: 5_000,
        },
        task_permissions: HarnessTaskPermissionsV1 {
            read: true,
            create: false,
            mutate: false,
            request_run: false,
        },
        read_permissions: HarnessReadPermissionsV1 {
            tasks: HarnessEntityReadScopeV1::Descendants,
            runs: HarnessEntityReadScopeV1::Descendants,
            operations: HarnessEntityReadScopeV1::Descendants,
        },
        monitoring_visibility: HarnessMonitoringVisibilityV1::Timeline,
        context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
        state: SessionGrantStateV1::Active,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    let operation = operation(
        'c',
        HarnessOperationKindV1::CreateGrant,
        HarnessOperationStateV1::Succeeded,
        None,
        None,
        Some(grant.grant_id.clone()),
        None,
        now,
    );
    apply(service, HarnessMutationV1::CreateGrant { operation, grant: grant.clone() });
    grant
}

fn create_h3b_grant(
    service: &mut HarnessService,
    actor_run_id: &HarnessRunId,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    now: u64,
) -> SessionGrantV1 {
    let grant = SessionGrantV1 {
        grant_id: grant_id(),
        revision: revision(1),
        actor_run_id: actor_run_id.clone(),
        allowed_targets: vec![HarnessGrantTargetV1 {
            node_id: selector(node_id.as_str()),
            workspace_id: selector(workspace_id.as_str()),
            provider_profile: selector("codex"),
            mode: HarnessExecutionModeV1::Pty,
        }],
        allowed_delivery_bundles: Vec::new(),
        maximum_child_count: 16,
        maximum_child_depth: 3,
        operation_timeouts: HarnessOperationTimeoutsV1 {
            dispatch_ms: 20_000,
            wait_ms: 20_000,
            reconciliation_ms: 20_000,
        },
        task_permissions: HarnessTaskPermissionsV1 {
            read: true,
            create: true,
            mutate: false,
            request_run: true,
        },
        read_permissions: HarnessReadPermissionsV1 {
            tasks: HarnessEntityReadScopeV1::Descendants,
            runs: HarnessEntityReadScopeV1::Descendants,
            operations: HarnessEntityReadScopeV1::Descendants,
        },
        monitoring_visibility: HarnessMonitoringVisibilityV1::Timeline,
        context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
        state: SessionGrantStateV1::Active,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    let operation = operation(
        'c',
        HarnessOperationKindV1::CreateGrant,
        HarnessOperationStateV1::Succeeded,
        None,
        None,
        Some(grant.grant_id.clone()),
        None,
        now,
    );
    apply(service, HarnessMutationV1::CreateGrant { operation, grant: grant.clone() });
    grant
}

fn h3b_schedule_plan(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    grant: &SessionGrantV1,
) -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector("h3b-schedule-next"),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("codex"),
        provider: agent("codex"),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Exact {
            grant_id: grant.grant_id.clone(),
            revision: grant.revision,
        },
        harness_mcp: HarnessMcpPolicyV1::GrantBound,
        deadline_ms: 90_000,
    }
}

fn ordinary_schedule_plan(
    plan_id: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    provider_profile: &str,
    provider: &str,
) -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector(plan_id),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector(provider_profile),
        provider: agent(provider),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 20_000,
    }
}

fn schedule_authority(marker: char) -> HarnessOperatorAuthorityV1 {
    HarnessOperatorAuthorityV1 {
        operation_id: operation_id(marker),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        actor_id: selector("h3b-schedule-operator"),
        now_unix_ms: unix_time_ms(),
    }
}

async fn schedule_ordinary_parent(
    harness: HarnessService,
    observation: ObservationService,
    adapter: HarnessC2Adapter,
    events: HarnessC2EventReceiver,
    operator_credential: HarnessOperatorCredential,
    plan: HarnessLaunchPlanV1,
    authority: HarnessOperatorAuthorityV1,
    expected_task_id: HarnessTaskId,
) -> HarnessRunId {
    assert!(plan.is_ordinary_dispatch());
    let catalogs = HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::new([plan.clone()]).unwrap(),
        Default::default(),
    ).unwrap();
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        harness,
        observation,
        adapter,
        events,
        "127.0.0.1:0".parse().unwrap(),
        Some(operator_credential.clone()),
        catalogs,
    ).await.unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(),
        operator_credential,
    ).unwrap();
    let request = HarnessScheduleNextRequestV1 {
        authority,
        plan_id: Some(plan.plan_id.clone()),
    };
    let outcome = client.schedule_next(request.clone()).unwrap();
    assert_eq!(client.schedule_next(request).unwrap(), outcome);
    let HarnessScheduleOutcomeV1::Dispatch(dispatch) = outcome else {
        panic!("ordinary parent ScheduleNext returned Idle");
    };
    assert_eq!(dispatch.task_id, expected_task_id);
    assert_eq!(dispatch.parent_run_id, None);
    timeout(Duration::from_secs(20), async {
        loop {
            if client.run_get(dispatch.run_id.clone())
                .is_ok_and(|run| run.lifecycle == HarnessRunLifecycleV1::Running)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("ordinary parent ScheduleNext did not reach Running");
    host.shutdown().await.unwrap();
    host_task.await.unwrap().unwrap();
    dispatch.run_id
}

fn create_h3b_child_task(
    service: &mut HarnessService,
    parent_run_id: &HarnessRunId,
    marker: char,
    body: String,
    now: u64,
) {
    let id = task_id(marker);
    let mut operation = operation(
        marker,
        HarnessOperationKindV1::CreateTask,
        HarnessOperationStateV1::Succeeded,
        Some(id.clone()),
        None,
        None,
        None,
        now,
    );
    operation.actor = HarnessActorV1::ParentRun { run_id: parent_run_id.clone() };
    let task = HarnessTaskV1 {
        task_id: id,
        revision: revision(1),
        title: format!("Visible H3B child {marker}"),
        body,
        creator: HarnessActorV1::ParentRun { run_id: parent_run_id.clone() },
        parent_task_id: Some(task_id('1')),
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Ready,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    apply(service, HarnessMutationV1::CreateTask { operation, task });
}

async fn drain_available_events(events: &mut HarnessC2EventReceiver) -> usize {
    let mut count = 0;
    loop {
        match timeout(Duration::from_millis(30), events.recv()).await {
            Ok(Some(_)) => count += 1,
            Ok(None) => panic!("C2 event stream closed during H3B setup"),
            Err(_) => return count,
        }
    }
}

async fn wait_fixture_marker(name: &str) {
    timeout(Duration::from_secs(60), async {
        while !fixture_marker(name).is_file() {
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap_or_else(|_| panic!("fixture marker timed out: {name}"));
}

fn replace_h3b_run_generation(
    current: &HarnessRunV1,
    generation: u64,
    operation_marker: char,
    now: u64,
) -> HarnessMutationV1 {
    let mut run = current.clone();
    run.revision = revision(current.revision.get() + 1);
    match run.binding.as_mut().unwrap().session {
        HarnessSessionIdentityV1::Managed { ref mut active_session, .. } => {
            active_session.as_mut().unwrap().generation = generation;
        }
        _ => panic!("H3B run binding is not managed"),
    }
    run.updated_at_unix_ms = now;
    let operation = operation(
        operation_marker,
        HarnessOperationKindV1::MutateRun,
        HarnessOperationStateV1::Succeeded,
        None,
        Some(run.run_id.clone()),
        None,
        Some(current.revision),
        now,
    );
    let mut mutation = HarnessMutationV1::ReplaceRun {
        operation,
        expected_revision: current.revision,
        run,
    };
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    mutation
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes[node_id].transport == gate4agent_c2::protocol::NodeTransportState::Online
            }) { return; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("fixture node did not become online");
}

async fn wait_runtime(adapter: &HarnessC2Adapter, route: &gate4agent_c2::protocol::NodeRoute) {
    timeout(Duration::from_secs(10), async {
        loop {
            if adapter.snapshot(route).await.unwrap().provider_runtime_statuses.iter().any(|status| {
                status.provider() == &agent("claude") && status.mode() != ProviderRuntimeMode::Unavailable
            }) { return; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("fixture runtime unavailable");
}

async fn connect_harness(
    endpoint: &str,
    token: &str,
) -> (HarnessC2Adapter, HarnessC2EventReceiver) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) = HarnessC2Adapter::connect(endpoint, token).await {
                return connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 operator lease was not released")
}

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
    transcript: String,
}

impl McpProcess {
    async fn start(endpoint: std::net::SocketAddr, credential: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gate4agent-harness-mcp"));
        command.env_clear();
        for name in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) { command.env(name, value); }
        }
        let mut child = command
            .env("GATE4AGENT_HARNESS_READ_ENDPOINT", endpoint.to_string())
            .env("GATE4AGENT_HARNESS_READ_CREDENTIAL", credential)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self { child, stdin, stdout, transcript: String::new() }
    }

    async fn send(&mut self, value: Value) -> Option<Value> {
        let mut encoded = serde_json::to_vec(&value).unwrap();
        encoded.push(b'\n');
        self.stdin.write_all(&encoded).await.unwrap();
        self.stdin.flush().await.unwrap();
        if value.get("id").is_none() { return None; }
        let line = timeout(Duration::from_secs(5), self.stdout.next_line())
            .await.expect("MCP response timeout").unwrap().expect("MCP stdout closed");
        self.transcript.push_str(&line);
        self.transcript.push('\n');
        Some(serde_json::from_str(&line).expect("MCP stdout was not JSON"))
    }

    async fn initialize(&mut self) {
        let initialized = self.send(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"h2-e2e","version":"1"}}
        })).await.unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}})).await;
    }

    async fn stop(mut self) -> (String, String) {
        self.stdin.shutdown().await.unwrap();
        drop(self.stdin);
        let status = timeout(Duration::from_secs(5), self.child.wait()).await.expect("MCP child did not exit").unwrap();
        assert!(status.success());
        let mut stdout = self.transcript;
        while let Ok(Ok(Some(line))) = timeout(Duration::from_millis(50), self.stdout.next_line()).await {
            stdout.push_str(&line);
            stdout.push('\n');
        }
        let mut stderr = String::new();
        if let Some(mut stream) = self.child.stderr.take() { stream.read_to_string(&mut stderr).await.unwrap(); }
        (stdout, stderr)
    }
}

struct ProviderFixtureMcp {
    child: StdChild,
    stdin: StdBufWriter<StdChildStdin>,
    stdout: StdBufReader<StdChildStdout>,
}

impl ProviderFixtureMcp {
    fn start(program: &std::path::Path, endpoint: &std::ffi::OsStr, token: &str) -> Self {
        let mut command = StdCommand::new(program);
        command.env_clear();
        for name in ["SystemRoot", "WINDIR", "ComSpec"] {
            if let Some(value) = std::env::var_os(name) { command.env(name, value); }
        }
        let mut child = command
            .arg("--session-proxy")
            .env("GATE4AGENT_HARNESS_SESSION_ENDPOINT", endpoint)
            .env("GATE4AGENT_HARNESS_SESSION_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = StdBufWriter::new(child.stdin.take().unwrap());
        let stdout = StdBufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    fn write(&mut self, value: Value) {
        serde_json::to_writer(&mut self.stdin, &value).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    fn read(&mut self) -> (Value, usize) {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "session-proxy MCP stdout closed");
        let byte_len = line.len();
        (serde_json::from_str(&line).unwrap(), byte_len)
    }

    fn send(&mut self, value: Value) -> (Value, usize) {
        self.write(value);
        self.read()
    }

    fn initialize(&mut self, id: u64) {
        let (reply, _) = self.send(json!({
            "jsonrpc":"2.0","id":id,"method":"initialize",
            "params":{
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"h3b-provider-fixture","version":"1"}
            }
        }));
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        self.write(json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }));
    }

    fn stop(self) {
        let Self { mut child, mut stdin, stdout: _ } = self;
        stdin.flush().unwrap();
        drop(stdin);
        let status = child.wait().unwrap();
        assert!(status.success(), "session-proxy MCP child failed: {status}");
        let mut stderr = String::new();
        if let Some(mut stream) = child.stderr.take() {
            stream.read_to_string(&mut stderr).unwrap();
        }
        assert!(stderr.is_empty(), "session-proxy MCP stderr was not empty");
    }
}

fn fixture_marker(name: &str) -> PathBuf {
    std::env::current_dir().unwrap().join(name)
}

fn write_fixture_marker(name: &str, value: &str) {
    std::fs::write(fixture_marker(name), value.as_bytes()).unwrap();
}

fn wait_fixture_marker_blocking(name: &str) {
    let deadline = StdInstant::now() + Duration::from_secs(60);
    while !fixture_marker(name).is_file() {
        assert!(StdInstant::now() < deadline, "fixture marker timed out: {name}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn provider_fixture_slot() -> char {
    for slot in ['a', 'b'] {
        let path = fixture_marker(&format!("proxy-{slot}.claim"));
        if std::fs::OpenOptions::new().write(true).create_new(true).open(path).is_ok() {
            return slot;
        }
    }
    panic!("no bounded H3B provider fixture slot remained");
}

fn write_provider_expected_ids(
    slot: char,
    task_id: &HarnessTaskId,
    run_id: &HarnessRunId,
    operation_id: &HarnessOperationId,
) {
    write_fixture_marker(
        &format!("proxy-{slot}.expected"),
        &format!("{}\n{}\n{}\n", task_id.as_str(), run_id.as_str(), operation_id.as_str()),
    );
}

fn provider_expected_ids(
    slot: char,
) -> (HarnessTaskId, HarnessRunId, HarnessOperationId) {
    let prefix = format!("proxy-{slot}");
    if !fixture_marker(&format!("{prefix}.schedule-next")).is_file() {
        let task_marker = if slot == 'a' { '4' } else { '5' };
        return (
            task_id(task_marker),
            h3b_child_run_id(slot),
            operation_id(task_marker),
        );
    }
    wait_fixture_marker_blocking(&format!("{prefix}.expected"));
    let encoded = std::fs::read_to_string(fixture_marker(&format!(
        "{prefix}.expected",
    ))).unwrap();
    let mut lines = encoded.lines();
    let task_id = HarnessTaskId::new(lines.next().expect("expected task id")).unwrap();
    let run_id = HarnessRunId::new(lines.next().expect("expected run id")).unwrap();
    let operation_id = HarnessOperationId::new(
        lines.next().expect("expected operation id"),
    ).unwrap();
    assert!(lines.next().is_none(), "unexpected provider expected-id fields");
    (task_id, run_id, operation_id)
}

fn h3b_child_run_id(slot: char) -> HarnessRunId {
    let digit = match slot { 'a' => '5', 'b' => '6', _ => panic!("invalid H3B slot") };
    HarnessRunId::new(format!("hrun_{}", digit.to_string().repeat(24))).unwrap()
}

fn assert_fixture_tool_success(reply: &Value, name: &str) {
    assert_eq!(reply["result"]["isError"], false, "fixture tool failed: {name}");
}

fn assert_fixture_read_unavailable(reply: &Value) {
    assert_eq!(reply["error"]["code"], -32603);
    assert_eq!(reply["error"]["message"], "harness read unavailable");
}

#[test]
#[ignore]
fn h3b_provider_fixture_child() {
    let endpoint = std::env::var_os("GATE4AGENT_HARNESS_SESSION_ENDPOINT");
    let token = std::env::var("GATE4AGENT_HARNESS_SESSION_TOKEN").ok();
    let program = std::env::var_os("GATE4AGENT_HARNESS_MCP_PROGRAM");
    if endpoint.is_none() && token.is_none() && program.is_none() {
        write_fixture_marker("parent.ready", "ready");
        wait_fixture_marker_blocking("parent.stop");
        return;
    }
    let endpoint = endpoint.expect("H3B endpoint missing");
    let token = token.expect("H3B token missing");
    let program = PathBuf::from(program.expect("H3B helper missing"));
    assert!(std::env::var_os("GATE4AGENT_HARNESS_READ_ENDPOINT").is_none());
    assert!(std::env::var_os("GATE4AGENT_HARNESS_READ_CREDENTIAL").is_none());
    let slot = provider_fixture_slot();
    let prefix = format!("proxy-{slot}");
    let (expected_task_id, run, expected_operation_id) = provider_expected_ids(slot);

    let wrong_token = if token.ends_with(&"e".repeat(64)) {
        format!("g4ah3_{}", "f".repeat(64))
    } else {
        format!("g4ah3_{}", "e".repeat(64))
    };
    let mut wrong = ProviderFixtureMcp::start(&program, &endpoint, &wrong_token);
    wrong.initialize(1);
    let (wrong_reply, _) = wrong.send(json!({
        "jsonrpc":"2.0","id":2,"method":"tools/list","params":{}
    }));
    assert_fixture_read_unavailable(&wrong_reply);
    wrong.stop();
    write_fixture_marker(&format!("{prefix}.wrong-token"), "denied");

    let mut mcp = ProviderFixtureMcp::start(&program, &endpoint, &token);
    mcp.initialize(10);
    mcp.write(json!({"jsonrpc":"2.0","id":11,"method":"tools/list","params":{}}));
    write_fixture_marker(&format!("{prefix}.preactivation"), "request-sent");
    let (listed, _) = mcp.read();
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    write_fixture_marker(&format!("{prefix}.activated"), "eight-tools");

    let calls = [
        (12, "g4a_context_get", json!({})),
        (13, "g4a_monitor_get", json!({})),
        (14, "g4a_timeline_read", json!({"limit":32})),
        (15, "g4a_tasks_get", json!({"task_id":expected_task_id.as_str()})),
        (16, "g4a_runs_list", json!({"limit":32})),
        (17, "g4a_runs_get", json!({"run_id":run.as_str()})),
        (18, "g4a_operation_get", json!({"operation_id":expected_operation_id.as_str()})),
    ];
    for (id, name, arguments) in calls {
        let (reply, _) = mcp.send(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }));
        assert_fixture_tool_success(&reply, name);
    }
    let (tasks, tasks_wire_bytes) = mcp.send(json!({
        "jsonrpc":"2.0","id":19,"method":"tools/call",
        "params":{"name":"g4a_tasks_list","arguments":{"limit":64}}
    }));
    assert_fixture_tool_success(&tasks, "g4a_tasks_list");
    assert!(tasks_wire_bytes > 48 * 1024, "tasks reply was not chunked");

    let (hidden, _) = mcp.send(json!({
        "jsonrpc":"2.0","id":20,"method":"tools/call",
        "params":{"name":"g4a_tasks_get","arguments":{"task_id":task_id('9').as_str()}}
    }));
    let (missing, _) = mcp.send(json!({
        "jsonrpc":"2.0","id":21,"method":"tools/call",
        "params":{"name":"g4a_tasks_get","arguments":{"task_id":task_id('0').as_str()}}
    }));
    assert_eq!(hidden["result"], missing["result"]);
    assert_eq!(hidden["result"]["isError"], true);
    write_fixture_marker(
        &format!("{prefix}.phase1"),
        &format!("tools=8\nchunked-bytes={tasks_wire_bytes}\nhidden=collapsed\nlegacy-env=absent\n"),
    );

    if slot == 'a' {
        wait_fixture_marker_blocking("proxy-a.restart");
        let (reply, _) = mcp.send(json!({
            "jsonrpc":"2.0","id":22,"method":"tools/call",
            "params":{"name":"g4a_context_get","arguments":{}}
        }));
        assert_fixture_tool_success(&reply, "g4a_context_get after restart");
        write_fixture_marker("proxy-a.phase2", "restart-read=ok");
        wait_fixture_marker_blocking("proxy-a.generation");
        let (reply, _) = mcp.send(json!({
            "jsonrpc":"2.0","id":23,"method":"tools/list","params":{}
        }));
        assert_fixture_read_unavailable(&reply);
        write_fixture_marker("proxy-a.generation-denied", "denied");
    } else {
        wait_fixture_marker_blocking("proxy-b.revoke");
        let (reply, _) = mcp.send(json!({
            "jsonrpc":"2.0","id":22,"method":"tools/list","params":{}
        }));
        assert_fixture_read_unavailable(&reply);
        write_fixture_marker("proxy-b.revoke-denied", "denied");
    }
    wait_fixture_marker_blocking(&format!("{prefix}.stop"));
    mcp.stop();
    write_fixture_marker(&format!("{prefix}.stopped"), "stopped");
}

fn replace_run_generation(
    current: &HarnessRunV1,
    generation: u64,
    operation_marker: char,
    now: u64,
) -> (HarnessRunV1, HarnessMutationV1) {
    let mut run = current.clone();
    run.revision = revision(current.revision.get() + 1);
    match run.binding.as_mut().unwrap().session {
        HarnessSessionIdentityV1::Managed { ref mut active_session, .. } => {
            active_session.as_mut().unwrap().generation = generation;
        }
        _ => panic!("fixture run is not managed"),
    }
    run.updated_at_unix_ms = now;
    let operation = operation(
        operation_marker,
        HarnessOperationKindV1::MutateRun,
        HarnessOperationStateV1::Succeeded,
        None,
        Some(run.run_id.clone()),
        None,
        Some(current.revision),
        now,
    );
    let mut mutation = HarnessMutationV1::ReplaceRun {
        operation,
        expected_revision: current.revision,
        run: run.clone(),
    };
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    (run, mutation)
}

fn replace_grant(
    current: &SessionGrantV1,
    state: SessionGrantStateV1,
    operation_marker: char,
    now: u64,
) -> (SessionGrantV1, HarnessMutationV1) {
    let mut grant = current.clone();
    grant.revision = revision(current.revision.get() + 1);
    grant.state = state;
    grant.updated_at_unix_ms = now;
    let kind = if state == SessionGrantStateV1::Revoked {
        HarnessOperationKindV1::RevokeGrant
    } else {
        HarnessOperationKindV1::MutateGrant
    };
    let operation = operation(
        operation_marker,
        kind,
        HarnessOperationStateV1::Succeeded,
        None,
        None,
        Some(grant.grant_id.clone()),
        Some(current.revision),
        now,
    );
    let mut mutation = HarnessMutationV1::ReplaceGrant {
        operation,
        expected_revision: current.revision,
        grant: grant.clone(),
    };
    mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
    (grant, mutation)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_host_and_real_mcp_child_enforce_durable_grant_filtered_reads() {
    require_windows_headless_supervisor_for_test();
    let hidden_body = r"C:\private\h2-hidden-provider-session.json";
    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("h2-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "h2-node-token";
    let c2_token = "h2-c2-token";

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("claude").unwrap(),
        revision: SpawnProfileRevision::new("h2-r1").unwrap(),
        provider: agent("claude"),
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
        [WorkspaceConfig::new(workspace_id.clone(), std::env::current_dir().unwrap()).unwrap()],
    ).unwrap().with_spawn_profiles(profiles);
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
    let config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(node_id.clone(), node_endpoint, node_token).unwrap()],
    ).unwrap().with_control_endpoint(control_endpoint.clone()).unwrap().with_timings(timings);
    let running = C2Running::start(config).await.unwrap();
    let c2_shutdown = running.shutdown_handle();
    let http = C2Client::new(running.api_addr(), c2_token).unwrap().with_deadline(Duration::from_secs(1));
    wait_online(&http, &node_id).await;

    let (adapter, events) = connect_harness(&control_endpoint, c2_token).await;
    let route = adapter.exact_route(&node_id).unwrap();
    wait_runtime(&adapter, &route).await;
    let databases = Databases::new();
    let now = unix_time_ms().max(10);
    let mut harness = HarnessService::open(&databases.harness).unwrap();
    create_task(&mut harness, task_id('1'), "Visible task", "Visible task body", now);
    create_task(&mut harness, task_id('9'), "Hidden unrelated task", hidden_body, now + 1);
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "a".repeat(64),
    )).unwrap();
    let parent_plan = ordinary_schedule_plan(
        "h2-parent",
        &node_id,
        &workspace_id,
        "claude",
        "claude",
    );
    let parent_run_id = schedule_ordinary_parent(
        harness,
        ObservationService::open(&databases.observation).unwrap(),
        adapter,
        events,
        operator_credential,
        parent_plan,
        schedule_authority('b'),
        task_id('1'),
    ).await;
    assert!(!node_task.is_finished(), "parent Harness shutdown stopped Node");
    assert!(http.ready().await.unwrap().ready, "parent Harness shutdown stopped C2");
    let mut harness = HarnessService::open(&databases.harness).unwrap();
    let running_run = harness.engine().run(&parent_run_id).unwrap().clone();
    assert_eq!(running_run.lifecycle, HarnessRunLifecycleV1::Running);
    create_hidden_requested_run(&mut harness, &node_id, &workspace_id, now + 2);
    create_actor_child_task(&mut harness, &parent_run_id, now + 5);
    let grant = create_grant(
        &mut harness,
        &parent_run_id,
        &node_id,
        &workspace_id,
        now + 6,
    );

    let (adapter, events) = connect_harness(&control_endpoint, c2_token).await;
    assert_eq!(adapter.exact_route(&node_id).unwrap(), route);
    assert_eq!(adapter.snapshot(&route).await.unwrap().session_records.len(), 1);

    let observation = ObservationService::open(&databases.observation).unwrap();
    let (host, host_task) = start_harness_host(
        harness,
        observation,
        adapter,
        events,
        "127.0.0.1:0".parse().unwrap(),
    ).await.unwrap();
    let (record_id, active_session) = match &running_run.binding.as_ref().unwrap().session {
        HarnessSessionIdentityV1::Managed {
            record_id,
            active_session: Some(active_session),
        } => (record_id.clone(), active_session.clone()),
        _ => panic!("scheduled parent run has no managed active binding"),
    };
    let binding = CredentialBindingV1 {
        grant_id: grant.grant_id.clone(),
        grant_revision: grant.revision,
        actor_run_id: parent_run_id.clone(),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        node_incarnation: selector(route.expected_incarnation_id.to_string()),
        record_id,
        instance_id: active_session.instance_id,
        generation: active_session.generation,
    };
    let credential = host.mint_credential(binding.clone(), unix_time_ms(), unix_time_ms() + 120_000).await.unwrap();
    assert!(!format!("{credential:?}").contains(credential.expose()));
    let reparsed_credential = HarnessReadCredential::parse(credential.expose().to_owned()).unwrap();
    assert_eq!(reparsed_credential, credential);
    HarnessReadClient::new(host.endpoint().socket_addr(), reparsed_credential).unwrap()
        .context_get()
        .unwrap_or_else(|error| panic!("direct production read failed before MCP: {error:?}"));

    let mut mcp = McpProcess::start(host.endpoint().socket_addr(), credential.expose()).await;
    mcp.initialize().await;
    let listed = mcp.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})).await.unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap_or_else(|| {
        let direct_after = HarnessReadClient::new(
            host.endpoint().socket_addr(),
            credential.clone(),
        ).unwrap().context_get();
        panic!("tools/list failed after initialize: {listed}; direct-after={direct_after:?}")
    });
    assert_eq!(tools.len(), 8);
    assert!(tools.iter().all(|tool| tool["inputSchema"]["additionalProperties"] == false));
    for (id, name, arguments) in [
        (3, "g4a_context_get", json!({})),
        (6, "g4a_tasks_list", json!({"limit":32})),
        (7, "g4a_tasks_get", json!({"task_id":task_id('1').as_str()})),
        (8, "g4a_runs_list", json!({"limit":32})),
        (9, "g4a_runs_get", json!({"run_id":parent_run_id.as_str()})),
        (10, "g4a_operation_get", json!({"operation_id":operation_id('4').as_str()})),
    ] {
        let response = mcp.send(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}
        })).await.unwrap();
        assert_eq!(response["result"]["isError"], false, "tool failed: {name}: {response}");
    }
    let mut observed_hook_monitoring = false;
    let mut last_monitor = Value::Null;
    let mut last_timeline = Value::Null;
    for attempt in 0..150_u64 {
        let monitor_reply = mcp.send(json!({
            "jsonrpc":"2.0","id":100 + attempt * 2,"method":"tools/call",
            "params":{"name":"g4a_monitor_get","arguments":{}}
        })).await.unwrap();
        let timeline_reply = mcp.send(json!({
            "jsonrpc":"2.0","id":101 + attempt * 2,"method":"tools/call",
            "params":{"name":"g4a_timeline_read","arguments":{"limit":32}}
        })).await.unwrap();
        if is_harness_read_unavailable(&monitor_reply)
            || is_harness_read_unavailable(&timeline_reply)
        {
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        assert_eq!(
            monitor_reply["result"]["isError"],
            false,
            "monitor call failed: {monitor_reply}",
        );
        assert_eq!(
            timeline_reply["result"]["isError"],
            false,
            "timeline call failed: {timeline_reply}",
        );
        let monitor: Value = serde_json::from_str(
            monitor_reply["result"]["content"][0]["text"].as_str().unwrap(),
        ).unwrap();
        let timeline: Value = serde_json::from_str(
            timeline_reply["result"]["content"][0]["text"].as_str().unwrap(),
        ).unwrap();
        assert_eq!(monitor["kind"], "monitor");
        assert_eq!(timeline["kind"], "timeline");
        let monitor_value = &monitor["value"];
        let timeline_value = &timeline["value"];
        let tool_fact = monitor_value["detail"]["tool_facts"].as_array()
            .into_iter().flatten()
            .any(|fact| fact["class"] == "tool" && fact["evidence"] == "managed-hook");
        let managed_hook_timeline = timeline_value["entries"].as_array()
            .into_iter().flatten()
            .any(|entry| entry["category"] == "tool" && entry["evidence"] == "managed-hook");
        let node_lifecycle_timeline = timeline_value["entries"].as_array()
            .into_iter().flatten()
            .any(|entry| entry["category"] == "lifecycle" && entry["evidence"] == "node-lifecycle");
        if tool_fact && managed_hook_timeline && node_lifecycle_timeline {
            assert_eq!(monitor_value["availability"], "current");
            assert_eq!(monitor_value["freshness"], "live");
            assert_eq!(monitor_value["features"]["tools"], "observed");
            assert_eq!(timeline_value["availability"], "current");
            assert_eq!(timeline_value["freshness"], "live");
            observed_hook_monitoring = true;
            break;
        }
        last_monitor = monitor;
        last_timeline = timeline;
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        observed_hook_monitoring,
        "real managed-hook and Node lifecycle facts were not observed; monitor={last_monitor}; timeline={last_timeline}",
    );
    for (id, tool, argument, hidden, missing) in [
        (11, "g4a_tasks_get", "task_id", task_id('9').to_string(), task_id('8').to_string()),
        (13, "g4a_runs_get", "run_id", hidden_run_id().to_string(), HarnessRunId::new("hrun_888888888888888888888888").unwrap().to_string()),
        (15, "g4a_operation_get", "operation_id", operation_id('8').to_string(), operation_id('0').to_string()),
    ] {
        let hidden_reply = mcp.send(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":tool,"arguments":{argument:hidden}}
        })).await.unwrap();
        let missing_reply = mcp.send(json!({
            "jsonrpc":"2.0","id":id + 1,"method":"tools/call",
            "params":{"name":tool,"arguments":{argument:missing}}
        })).await.unwrap();
        assert_eq!(hidden_reply["result"], missing_reply["result"]);
        assert_eq!(hidden_reply["result"]["isError"], true);
        assert_eq!(hidden_reply["result"]["content"][0]["text"], "not found or denied");
    }
    let (first_stdout, first_stderr) = mcp.stop().await;
    let mut all_mcp_stdout = first_stdout.clone();
    assert!(first_stderr.is_empty(), "MCP stderr leaked details: {first_stderr}");
    for canary in [
        task_id('9').as_str(),
        hidden_run_id().as_str(),
        operation_id('8').as_str(),
        hidden_body,
        credential.expose(),
        MONITORING_PROMPT_CANARY,
        MONITORING_PROVIDER_SESSION_CANARY,
        MONITORING_TOOL_INPUT_CANARY,
        MONITORING_TOOL_OUTPUT_CANARY,
    ] {
        assert!(!first_stdout.contains(canary), "private value leaked to MCP stdout");
    }

    assert!(!node_task.is_finished(), "dropping MCP stopped Node");
    assert!(http.ready().await.unwrap().ready, "dropping MCP stopped C2");
    host.shutdown().await.unwrap();
    host_task.await.unwrap().unwrap();
    assert!(!node_task.is_finished(), "stopping harness host stopped Node");
    assert!(http.ready().await.unwrap().ready, "stopping harness host stopped C2");

    let (adapter, events) = connect_harness(&control_endpoint, c2_token).await;
    let restarted_route = adapter.exact_route(&node_id).unwrap();
    assert_eq!(restarted_route, route);
    assert_eq!(adapter.snapshot(&route).await.unwrap().session_records.len(), 1);
    let harness = HarnessService::open(&databases.harness).unwrap();
    let restarted_run = harness.engine().run(&parent_run_id).unwrap().clone();
    assert_eq!(restarted_run.lifecycle, HarnessRunLifecycleV1::Running);
    assert_eq!(restarted_run.binding, running_run.binding);
    let observation = ObservationService::open(&databases.observation).unwrap();
    let (host, host_task) = start_harness_host(
        harness,
        observation,
        adapter,
        events,
        "127.0.0.1:0".parse().unwrap(),
    ).await.unwrap();
    let mut old_credential_mcp = McpProcess::start(host.endpoint().socket_addr(), credential.expose()).await;
    old_credential_mcp.initialize().await;
    let old_credential_reply = old_credential_mcp.send(json!({
        "jsonrpc":"2.0","id":1200,"method":"tools/list","params":{}
    })).await.unwrap();
    assert_eq!(old_credential_reply["error"]["code"], -32603);
    let (old_credential_stdout, old_credential_stderr) = old_credential_mcp.stop().await;
    all_mcp_stdout.push_str(&old_credential_stdout);
    assert!(!old_credential_stdout.contains(credential.expose()));
    assert!(old_credential_stderr.is_empty());
    let restarted_credential = host.mint_credential(binding.clone(), unix_time_ms(), unix_time_ms() + 120_000).await.unwrap();
    let mut restarted_mcp = McpProcess::start(host.endpoint().socket_addr(), restarted_credential.expose()).await;
    restarted_mcp.initialize().await;
    let context = restarted_mcp.send(json!({
        "jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"g4a_context_get","arguments":{}}
    })).await.unwrap();
    assert_eq!(context["result"]["isError"], false);

    let (generation_two_run, generation_mutation) = replace_run_generation(
        &restarted_run,
        binding.generation + 1,
        'd',
        unix_time_ms(),
    );
    host.apply_harness_mutation(generation_mutation).await.unwrap();
    let invalid_generation = restarted_mcp.send(json!({"jsonrpc":"2.0","id":13,"method":"tools/list"})).await.unwrap();
    assert_eq!(invalid_generation["error"]["code"], -32603);
    let (generation_stdout, generation_stderr) = restarted_mcp.stop().await;
    all_mcp_stdout.push_str(&generation_stdout);
    assert!(generation_stderr.is_empty());

    let (restored_run, restore_mutation) = replace_run_generation(
        &generation_two_run,
        binding.generation,
        'e',
        unix_time_ms(),
    );
    host.apply_harness_mutation(restore_mutation).await.unwrap();
    assert_eq!(restored_run.binding.as_ref().unwrap(), restarted_run.binding.as_ref().unwrap());
    let (grant_v2, grant_mutation) = replace_grant(&grant, SessionGrantStateV1::Active, 'f', unix_time_ms());
    host.apply_harness_mutation(grant_mutation).await.unwrap();
    let stale_revision = host.mint_credential(binding.clone(), unix_time_ms(), unix_time_ms() + 120_000).await;
    assert!(stale_revision.is_err(), "stale grant revision minted a credential");
    let mut binding_v2 = binding.clone();
    binding_v2.grant_revision = grant_v2.revision;
    let credential_v2 = host.mint_credential(binding_v2.clone(), unix_time_ms(), unix_time_ms() + 120_000).await.unwrap();
    let mut revision_mcp = McpProcess::start(host.endpoint().socket_addr(), credential_v2.expose()).await;
    revision_mcp.initialize().await;
    let (revoked, revoke_mutation) = replace_grant(&grant_v2, SessionGrantStateV1::Revoked, '7', unix_time_ms());
    host.apply_harness_mutation(revoke_mutation).await.unwrap();
    assert_eq!(revoked.state, SessionGrantStateV1::Revoked);
    let revoked_reply = revision_mcp.send(json!({"jsonrpc":"2.0","id":14,"method":"tools/list"})).await.unwrap();
    assert_eq!(revoked_reply["error"]["code"], -32603);
    let (revision_stdout, revision_stderr) = revision_mcp.stop().await;
    all_mcp_stdout.push_str(&revision_stdout);
    assert!(revision_stderr.is_empty());

    host.shutdown().await.unwrap();
    host_task.await.unwrap().unwrap();
    let workspace_path = std::env::current_dir().unwrap().to_string_lossy().into_owned();
    for canary in [
        task_id('9').as_str(),
        hidden_run_id().as_str(),
        operation_id('8').as_str(),
        hidden_body,
        node_token,
        c2_token,
        credential.expose(),
        restarted_credential.expose(),
        credential_v2.expose(),
        MONITORING_PROMPT_CANARY,
        MONITORING_PROVIDER_SESSION_CANARY,
        MONITORING_TOOL_INPUT_CANARY,
        MONITORING_TOOL_OUTPUT_CANARY,
        workspace_path.as_str(),
    ] {
        assert!(!all_mcp_stdout.contains(canary), "private value leaked across MCP stdout");
    }
    databases.assert_private_bytes_absent(&[
        node_token,
        c2_token,
        credential.expose(),
        restarted_credential.expose(),
        credential_v2.expose(),
        MONITORING_PROMPT_CANARY,
        MONITORING_PROVIDER_SESSION_CANARY,
        MONITORING_TOOL_INPUT_CANARY,
        MONITORING_TOOL_OUTPUT_CANARY,
    ]);
    assert!(!node_task.is_finished());
    assert!(http.ready().await.unwrap().ready);

    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), node_task).await.expect("Node did not stop").unwrap().unwrap();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait()).await.expect("C2 did not stop").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_schedule_next_h3b_is_exact_replay_restart_generation_and_revoke_bound() {
    let _exclusive = H3B_E2E_LOCK.lock().unwrap();
    require_windows_headless_supervisor_for_test();
    let databases = Databases::new();
    let workspace = databases.root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let original_working_directory = std::env::current_dir().unwrap();
    std::env::set_current_dir(&workspace).unwrap();
    let node_endpoint = endpoint("h3b-schedule-node");
    let control_endpoint = endpoint("h3b-schedule-control");
    let node_id = NodeId::new("h3b-schedule-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "h3b-schedule-node-token";
    let c2_token = "h3b-schedule-c2-token";
    let operator_secret = format!("g4aho_{}", "c".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(
        operator_secret.clone(),
    ).unwrap();
    let legacy_endpoint_canary = "127.0.0.1:65534";
    let legacy_credential_canary = "hread_h3b-schedule-legacy-must-be-removed";
    std::env::set_var("GATE4AGENT_HARNESS_READ_ENDPOINT", legacy_endpoint_canary);
    std::env::set_var("GATE4AGENT_HARNESS_READ_CREDENTIAL", legacy_credential_canary);

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("codex").unwrap(),
        revision: SpawnProfileRevision::new("h3b-schedule-r1").unwrap(),
        provider: agent("codex"),
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
        .with_spawn_profiles(profiles)
        .with_state_path(databases.root.join("node-state.json"))
        .unwrap();
    let provider_test_program = std::env::current_exe().unwrap();
    let provider_fixture_script = workspace.join("h3b-provider-fixture.cmd");
    std::fs::write(
        &provider_fixture_script,
        format!(
            "@echo off\r\n\"{}\" --exact h3b_provider_fixture_child --ignored --nocapture --test-threads=1\r\n",
            provider_test_program.display(),
        ),
    ).unwrap();
    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .expect("Windows system root is unavailable");
    let provider_program = std::fs::canonicalize(
        PathBuf::from(system_root).join("System32").join("cmd.exe"),
    ).expect("Windows command launcher is unavailable");
    let provider_args = vec![
        OsString::from("/D"),
        OsString::from("/S"),
        OsString::from("/C"),
        provider_fixture_script.into_os_string(),
    ];
    let helper_program = PathBuf::from(env!("CARGO_BIN_EXE_gate4agent-harness-mcp"));
    let server = NodeServer::new_harness_mcp_proxy_fixture(
        node_config,
        provider_program,
        provider_args,
        helper_program,
    ).unwrap();
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
    let config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(node_id.clone(), node_endpoint, node_token).unwrap()],
    ).unwrap()
        .with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(timings);
    let running = C2Running::start(config).await.unwrap();
    let c2_shutdown = running.shutdown_handle();
    let http = C2Client::new(running.api_addr(), c2_token).unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&http, &node_id).await;

    let (adapter, events) = connect_harness(&control_endpoint, c2_token).await;
    let route = adapter.exact_route(&node_id).unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if adapter.snapshot(&route).await.unwrap().provider_runtime_statuses.iter()
                .any(|status| {
                    status.provider() == &agent("codex")
                        && status.mode() != ProviderRuntimeMode::Unavailable
                })
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("H3B ScheduleNext fixture runtime unavailable");

    let now = unix_time_ms().max(10);
    let mut harness = HarnessService::open(&databases.harness).unwrap();
    create_task(&mut harness, task_id('1'), "H3B root task", "Root task", now);
    create_task(
        &mut harness,
        task_id('9'),
        "Hidden unrelated task",
        r"C:\private\h3b-schedule-hidden-provider-session.json",
        now + 1,
    );
    let parent_plan = ordinary_schedule_plan(
        "h3b-parent",
        &node_id,
        &workspace_id,
        "codex",
        "codex",
    );
    let parent_run_id = schedule_ordinary_parent(
        harness,
        ObservationService::open(&databases.observation).unwrap(),
        adapter,
        events,
        operator_credential.clone(),
        parent_plan,
        schedule_authority('b'),
        task_id('1'),
    ).await;
    wait_fixture_marker("parent.ready").await;
    assert!(!node_task.is_finished(), "parent Harness shutdown stopped Node");
    assert!(http.ready().await.unwrap().ready, "parent Harness shutdown stopped C2");

    let mut harness = HarnessService::open(&databases.harness).unwrap();
    let grant = create_h3b_grant(
        &mut harness,
        &parent_run_id,
        &node_id,
        &workspace_id,
        unix_time_ms(),
    );
    for (index, marker) in ['4', '5', '6', '7', '8', 'd', 'e', 'f']
        .into_iter().enumerate()
    {
        let body = format!("visible-h3b-{marker}-{}-end", "x".repeat(7_800));
        create_h3b_child_task(
            &mut harness,
            &parent_run_id,
            marker,
            body,
            unix_time_ms() + index as u64,
        );
    }
    let plan = h3b_schedule_plan(&node_id, &workspace_id, &grant);
    let catalogs = HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::new([plan.clone()]).unwrap(),
        Default::default(),
    ).unwrap();
    write_fixture_marker("proxy-a.schedule-next", "enabled");
    write_fixture_marker("proxy-b.schedule-next", "enabled");
    let (adapter, events) = connect_harness(&control_endpoint, c2_token).await;
    assert_eq!(adapter.exact_route(&node_id).unwrap(), route);
    let observation = ObservationService::open(&databases.observation).unwrap();
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        harness,
        observation,
        adapter,
        events,
        "127.0.0.1:0".parse().unwrap(),
        Some(operator_credential.clone()),
        catalogs.clone(),
    ).await.unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(),
        operator_credential.clone(),
    ).unwrap();

    let first_request = HarnessScheduleNextRequestV1 {
        authority: schedule_authority('3'),
        plan_id: Some(plan.plan_id.clone()),
    };
    let first_outcome = client.schedule_next(first_request.clone()).unwrap();
    assert_eq!(client.schedule_next(first_request).unwrap(), first_outcome);
    let HarnessScheduleOutcomeV1::Dispatch(first_dispatch) = first_outcome else {
        panic!("first H3B ScheduleNext returned Idle");
    };
    assert_eq!(first_dispatch.task_id, task_id('4'));
    assert_eq!(first_dispatch.parent_run_id, Some(parent_run_id.clone()));
    let first_ids = deterministic_dispatch_ids(
        &first_dispatch.operation_id,
        &plan,
    ).unwrap();
    let first_reservation_id = first_ids.harness_mcp_reservation_id.unwrap();
    assert_eq!(first_dispatch.run_id, first_ids.run_id);
    write_provider_expected_ids(
        'a',
        &first_dispatch.task_id,
        &first_dispatch.run_id,
        &first_dispatch.operation_id,
    );
    wait_fixture_marker("proxy-a.wrong-token").await;
    wait_fixture_marker("proxy-a.preactivation").await;
    wait_fixture_marker("proxy-a.activated").await;
    wait_fixture_marker("proxy-a.phase1").await;
    assert_eq!(
        client.run_get(first_dispatch.run_id.clone()).unwrap().lifecycle,
        HarnessRunLifecycleV1::Running,
    );

    let second_request = HarnessScheduleNextRequestV1 {
        authority: schedule_authority('2'),
        plan_id: Some(plan.plan_id.clone()),
    };
    let second_outcome = client.schedule_next(second_request.clone()).unwrap();
    assert_eq!(client.schedule_next(second_request).unwrap(), second_outcome);
    let HarnessScheduleOutcomeV1::Dispatch(second_dispatch) = second_outcome else {
        panic!("second H3B ScheduleNext returned Idle");
    };
    assert_eq!(second_dispatch.task_id, task_id('5'));
    assert_eq!(second_dispatch.parent_run_id, Some(parent_run_id.clone()));
    let second_ids = deterministic_dispatch_ids(
        &second_dispatch.operation_id,
        &plan,
    ).unwrap();
    let second_reservation_id = second_ids.harness_mcp_reservation_id.unwrap();
    assert_eq!(second_dispatch.run_id, second_ids.run_id);
    write_provider_expected_ids(
        'b',
        &second_dispatch.task_id,
        &second_dispatch.run_id,
        &second_dispatch.operation_id,
    );
    wait_fixture_marker("proxy-b.wrong-token").await;
    wait_fixture_marker("proxy-b.preactivation").await;
    wait_fixture_marker("proxy-b.activated").await;
    wait_fixture_marker("proxy-b.phase1").await;
    assert_eq!(
        client.run_get(second_dispatch.run_id.clone()).unwrap().lifecycle,
        HarnessRunLifecycleV1::Running,
    );
    for slot in ['a', 'b'] {
        let report = std::fs::read_to_string(fixture_marker(&format!(
            "proxy-{slot}.phase1",
        ))).unwrap();
        assert!(report.contains("tools=8"));
        assert!(report.contains("hidden=collapsed"));
        assert!(report.contains("legacy-env=absent"));
        let chunked_bytes = report.lines()
            .find_map(|line| line.strip_prefix("chunked-bytes="))
            .unwrap().parse::<usize>().unwrap();
        assert!(chunked_bytes > 48 * 1024);
    }

    host.shutdown().await.unwrap();
    host_task.await.unwrap().unwrap();
    assert!(!node_task.is_finished(), "Harness shutdown stopped Node");
    assert!(http.ready().await.unwrap().ready, "Harness shutdown stopped C2");
    let (adapter, events) = connect_harness(&control_endpoint, c2_token).await;
    assert_eq!(adapter.exact_route(&node_id).unwrap(), route);
    let snapshot = adapter.snapshot(&route).await.unwrap();
    assert_eq!(snapshot.session_records.len(), 3);
    let mut session_record_ids = snapshot.session_records.iter()
        .map(|record| record.record_id.clone())
        .collect::<Vec<_>>();
    session_record_ids.sort();
    let harness = HarnessService::open(&databases.harness).unwrap();
    assert_eq!(
        harness.harness_mcp_reservation_state(&first_reservation_id),
        Some(HarnessMcpReservationStateV1::Active),
    );
    assert_eq!(
        harness.harness_mcp_reservation_state(&second_reservation_id),
        Some(HarnessMcpReservationStateV1::Active),
    );
    let first_running = harness.engine().run(&first_dispatch.run_id).unwrap().clone();
    let observation = ObservationService::open(&databases.observation).unwrap();
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        harness,
        observation,
        adapter,
        events,
        "127.0.0.1:0".parse().unwrap(),
        Some(operator_credential),
        catalogs,
    ).await.unwrap();
    write_fixture_marker("proxy-a.restart", "go");
    wait_fixture_marker("proxy-a.phase2").await;

    let generation = first_running.binding.as_ref().and_then(|binding| {
        match binding.session {
            HarnessSessionIdentityV1::Managed { ref active_session, .. } => {
                active_session.as_ref().map(|runtime| runtime.generation + 1)
            }
            _ => None,
        }
    }).unwrap();
    host.apply_harness_mutation(replace_h3b_run_generation(
        &first_running,
        generation,
        '1',
        unix_time_ms(),
    )).await.unwrap();
    write_fixture_marker("proxy-a.generation", "go");
    wait_fixture_marker("proxy-a.generation-denied").await;

    let (revoked_grant, revoke_mutation) = replace_grant(
        &grant,
        SessionGrantStateV1::Revoked,
        '0',
        unix_time_ms(),
    );
    host.apply_harness_mutation(revoke_mutation).await.unwrap();
    assert_eq!(revoked_grant.state, SessionGrantStateV1::Revoked);
    write_fixture_marker("proxy-b.revoke", "go");
    wait_fixture_marker("proxy-b.revoke-denied").await;

    host.shutdown().await.unwrap();
    host_task.await.unwrap().unwrap();
    let harness = HarnessService::open(&databases.harness).unwrap();
    assert_eq!(
        harness.harness_mcp_reservation_state(&first_reservation_id),
        Some(HarnessMcpReservationStateV1::Revoked),
    );
    assert_eq!(
        harness.harness_mcp_reservation_state(&second_reservation_id),
        Some(HarnessMcpReservationStateV1::Revoked),
    );
    harness.close().unwrap();
    let (adapter, mut events) = connect_harness(&control_endpoint, c2_token).await;
    let final_snapshot = adapter.snapshot(&route).await.unwrap();
    let mut final_record_ids = final_snapshot.session_records.iter()
        .map(|record| record.record_id.clone())
        .collect::<Vec<_>>();
    final_record_ids.sort();
    assert_eq!(final_record_ids, session_record_ids);
    let _ = drain_available_events(&mut events).await;
    write_fixture_marker("proxy-a.stop", "stop");
    write_fixture_marker("proxy-b.stop", "stop");
    write_fixture_marker("parent.stop", "stop");
    wait_fixture_marker("proxy-a.stopped").await;
    wait_fixture_marker("proxy-b.stopped").await;
    drop(adapter);
    drop(events);

    assert!(!node_task.is_finished());
    assert!(http.ready().await.unwrap().ready);
    std::env::remove_var("GATE4AGENT_HARNESS_READ_ENDPOINT");
    std::env::remove_var("GATE4AGENT_HARNESS_READ_CREDENTIAL");
    std::env::set_current_dir(original_working_directory).unwrap();

    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), node_task)
        .await.expect("H3B ScheduleNext Node did not stop").unwrap().unwrap();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await.expect("H3B ScheduleNext C2 did not stop").unwrap();
    drop(node_shutdown);
    drop(c2_shutdown);
    drop(http);

    let mut privacy_scan = vec![databases.root.clone()];
    while let Some(directory) = privacy_scan.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let file_type = entry.file_type().unwrap();
            assert!(!file_type.is_symlink(), "fixture privacy root contains a link");
            if file_type.is_dir() {
                privacy_scan.push(entry.path());
                continue;
            }
            if !file_type.is_file() { continue; }
            let bytes = std::fs::read(entry.path()).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            for canary in [
                node_token,
                c2_token,
                operator_secret.as_str(),
                legacy_endpoint_canary,
                legacy_credential_canary,
                "g4aho_",
                "g4ah3_",
                r"\\.\pipe\gate4agent-h3b-",
            ] {
                assert!(
                    !text.contains(canary),
                    "private H3B ScheduleNext value persisted in {}",
                    entry.path().display(),
                );
            }
        }
    }
}
