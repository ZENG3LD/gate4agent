//! A2 durable ContextPack E2E — design §8 of
//! `docs/gate4agent/plans/gate4agent-a2-durable-context-pack-design-2026-08-17.md`.
//!
//! C2 allows exactly one authenticated operator connection at a time
//! (`HarnessC2Adapter::connect` and a running harness host both compete for
//! that sole seat), so this test alternates between "harness host up,
//! talking through `HarnessOperatorClient`" and "bare adapter, reading the
//! Node's raw `C2NodeSnapshot` directly" phases — never both at once. That
//! shape is also what makes the section map below read as separate rounds
//! rather than one continuous session; it mirrors the flagship's own
//! stop-host-then-reconnect-adapter idiom, just applied at every point this
//! test needs raw Node truth instead of only at the very end.
//!
//! Section map (design step -> code section, search for the literal marker
//! comment in the test body):
//!   step 1  -> "== STEP 1 =="  (schedule_next dispatches run A; source is an
//!               ordinary Operator-grant dispatch, mirroring the flagship's
//!               own source-run shape rather than a reviewed start_task_v2)
//!   step 2  -> "== STEP 2 =="  (bare-adapter Node snapshot poll)
//!   step 3  -> "== STEP 3 =="  (harness host reconnect = one resync, then
//!               store reopen #1)
//!   step 4  -> "== STEP 4 =="  (store reopen #2)
//!   step 5  -> "== STEP 5 =="  (Node restart; ContextPackStore reseed is
//!               proved together with steps 7-9, see the comment there)
//!   step 6  -> "== STEP 6 =="  (task B launch options, Durable availability)
//!   step 7  -> "== STEP 7 =="  (spec select + start_task_v2 B)
//!   step 8  -> "== STEP 8 =="  (bare-adapter proof the durable path served it)
//!   step 9  -> "== STEP 9 =="  (materialized context file + lineage chain)

#![cfg(windows)]

use std::{
    fs,
    io::Write as _,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    HarnessOperatorCredential, HarnessOperatorMutationOutcomeV1,
    HarnessReplaceTaskExecutionSpecRequestV2, HarnessReviewedTaskLaunchSelectionV1,
    HarnessReviewedWorktreeSelectionV1, HarnessStartTaskRequestV2,
};
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_protocol::{
    HarnessContextSourceAvailabilityV1, HarnessContinuationStateV1, HarnessCreateTaskRequestV1,
    HarnessExecutionModeV1, HarnessExpectedExecutionSpecRevisionV1, HarnessIdempotencyRef,
    HarnessOperationId, HarnessOperatorAuthorityV1, HarnessRevision, HarnessRunLifecycleV1,
    HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1, HarnessSelectorV1, HarnessTaskId,
    HarnessTaskReviewPolicyV1, HarnessTaskStateV1, HarnessWorktreeIntentV1,
};
use gate4agent_harness_service::{
    c2::HarnessC2Adapter,
    dispatch::{
        HarnessContinuationPolicyV1, HarnessGrantPolicyV1, HarnessLaunchCatalog,
        HarnessLaunchPlanV1, HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    runtime::{start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs},
    HarnessService,
};
use gate4agent_node::protocol::{
    ManagedSessionState, NodeId, ResolvedContextPackReceipt, SessionMode, SpawnContextId,
    SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{
    HistorySourceLayout, NativeHistoryConfig, NativeHistoryRoot, NodeSecretReference,
    NodeSecretResolveError, NodeSecretResolver, NodeSecretValue, NodeServer, NodeServerConfig,
    SpawnProfileRegistry, WorkspaceConfig,
};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AdapterId, AgentId, TerminalSize};
use serde_json::json;
use tokio::time::{sleep, timeout};

/// The claude-code Hook `session_id` the source fixture posts at `SessionStart`, and the
/// `sessionId` the paired native-history fixture file carries. Bounded to
/// ASCII-alphanumeric-or-hyphen because `identified_clean_exit_agent_spec` places it into a
/// PowerShell argv element unescaped.
const SOURCE_SESSION_ID: &str = "g4a-a2-durable-source-session";
const SOURCE_USER_MESSAGE: &str = "a2-durable-private-source-user-payload";
const SOURCE_ASSISTANT_MESSAGE: &str = "a2-durable-private-source-assistant-payload";

struct FixturePaths {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    node_state: PathBuf,
    workspace: PathBuf,
    materializations: PathBuf,
    history_root: PathBuf,
    started: PathBuf,
    release: PathBuf,
    started_round2: PathBuf,
    release_round2: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-durable-context-pack-e2e-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        let workspace = root.join("workspace");
        // Left uncreated on purpose: `SessionEnvironmentMaterializer::new`
        // (via `ensure_materialization_root`) requires this directory to
        // either not exist yet (so it creates it itself with a locked-down,
        // owner-only-protected ACL via `secure_create_directory`) or already
        // carry that exact ACL. A plain `fs::create_dir_all` inherits the
        // parent's normal ACL and fails `validate_owner_only_dacl`.
        let materializations = root.join("materializations");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            node_state: root.join("node-state.json"),
            history_root: root.join("history"),
            started: root.join("started.marker"),
            release: root.join("release.signal"),
            started_round2: root.join("started-round2.marker"),
            release_round2: root.join("release-round2.signal"),
            workspace,
            materializations,
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
        r"\\.\pipe\gate4agent-durable-context-pack-e2e-{label}-{}-{}",
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

fn task_a_id() -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap()
}

fn task_b_id() -> HarnessTaskId {
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

/// Both plans dispatch into the same `Existing` workspace on the same Node -- durable
/// continuation only needs matching node/workspace (§10 "cross-node continuation stays
/// unsupported"), not a managed worktree, so this test skips the flagship's managed-worktree
/// machinery entirely.
fn ordinary_plan(
    plan_id: &str,
    provider_profile: &str,
    provider: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
) -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector(plan_id),
        revision: revision(1),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector(provider_profile),
        provider: AgentId::new(provider).unwrap(),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: HarnessMcpPolicyV1::Disabled,
        deadline_ms: 30_000,
    }
}

/// `provider_profile` here MUST be the literal source AgentId string ("claude"): once run A
/// carries its own `context_pack`, `HarnessRunV1::validate()` requires
/// `pack.lineage.source_provider == self.intent.provider_profile`, and the exported pack's
/// lineage carries the managed session's real provider AgentId, not an arbitrary plan label.
fn source_plan(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchPlanV1 {
    ordinary_plan("durable-source", "claude", "claude", node_id, workspace_id)
}

fn target_plan(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchPlanV1 {
    ordinary_plan(
        "durable-target",
        "durable-target-codex",
        "codex",
        node_id,
        workspace_id,
    )
}

fn write_json_lines(path: &Path, values: &[serde_json::Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = Vec::new();
    for value in values {
        serde_json::to_writer(&mut bytes, value).unwrap();
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
}

/// Non-trivial (message_count == 2) native history for `SOURCE_SESSION_ID`, readable through
/// the Node's `claude-code` History adapter so the exported pack is non-empty.
fn source_history(root: &Path, cwd: &str) -> NativeHistoryConfig {
    let projects = root.join("claude-projects");
    write_json_lines(
        &projects.join(format!("{SOURCE_SESSION_ID}.jsonl")),
        &[
            json!({
                "sessionId": SOURCE_SESSION_ID,
                "timestamp": "2026-08-17T00:00:00Z",
                "type": "user",
                "cwd": cwd,
                "message": { "role": "user", "content": SOURCE_USER_MESSAGE },
            }),
            json!({
                "sessionId": SOURCE_SESSION_ID,
                "timestamp": "2026-08-17T00:00:01Z",
                "type": "assistant",
                "cwd": cwd,
                "message": {
                    "role": "assistant",
                    "model": "fixture-model",
                    "content": [{ "type": "text", "text": SOURCE_ASSISTANT_MESSAGE }],
                },
            }),
        ],
    );
    NativeHistoryConfig::new(vec![NativeHistoryRoot::new(
        AdapterId::new("claude-code").unwrap(),
        HistorySourceLayout::SingleNdjson,
        projects,
    )
    .unwrap()])
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn build_node_config(
    node_endpoint: &str,
    node_token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    workspace: &Path,
    node_state: &Path,
    materializations: &Path,
    history_root: &Path,
) -> NodeServerConfig {
    let spawn_profiles = SpawnProfileRegistry::new([
        SpawnProfileDefaults {
            profile_id: SpawnProfileId::new("claude").unwrap(),
            revision: SpawnProfileRevision::new("durable-source-r1").unwrap(),
            provider: AgentId::new("claude").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: None,
        },
        SpawnProfileDefaults {
            profile_id: SpawnProfileId::new("durable-target-codex").unwrap(),
            revision: SpawnProfileRevision::new("durable-target-r1").unwrap(),
            provider: AgentId::new("codex").unwrap(),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: None,
        },
    ])
    .unwrap();
    NodeServerConfig::new(
        node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), workspace.to_path_buf()).unwrap()],
    )
    .unwrap()
    .with_state_path(node_state.to_path_buf())
    .unwrap()
    .with_spawn_profiles(spawn_profiles)
    .with_history(source_history(
        history_root,
        workspace.to_string_lossy().as_ref(),
    ))
    .with_session_environment_materialization(
        materializations.to_path_buf(),
        Arc::new(UnusedSecretResolver),
    )
    .unwrap()
}

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
    expected_node_id: &NodeId,
) -> (HarnessC2Adapter, gate4agent_harness_service::c2::HarnessC2EventReceiver) {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok((adapter, events)) = HarnessC2Adapter::connect(endpoint, token).await {
                if let Ok(route) = adapter.exact_route(expected_node_id) {
                    if adapter.snapshot(&route).await.is_ok() {
                        break (adapter, events);
                    }
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("C2 did not expose the expected current Node topology")
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(15), async {
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

async fn stop_host(
    host: gate4agent_harness_service::runtime::HarnessHostHandle,
    task: tokio::task::JoinHandle<
        Result<(), gate4agent_harness_service::runtime::HarnessRuntimeError>,
    >,
) {
    let shutdown = host.shutdown().await;
    let host_result = timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    match (shutdown, host_result) {
        (Ok(()), Ok(())) => {}
        (Err(shutdown_error), Err(host_error)) => panic!(
            "Harness host stopped before shutdown completed: shutdown={shutdown_error:?} host={host_error:?}",
        ),
        (Err(error), Ok(())) => panic!("Harness shutdown failed after clean host exit: {error:?}"),
        (Ok(()), Err(error)) => panic!("Harness host failed during shutdown: {error:?}"),
    }
}

fn find_files_named(root: &Path, file_name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) == Some(file_name) {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_context_pack_survives_two_restarts_and_serves_finished_run_continuation() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("durable-context-pack-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "node-secret-durable-context-pack";
    let c2_token = "c2-secret-durable-context-pack";
    let operator_secret = format!("g4aho_{}", "d".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret).unwrap();

    let catalogs = HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::new([
            source_plan(&node_id, &workspace_id),
            target_plan(&node_id, &workspace_id),
        ])
        .unwrap(),
        DeliveryCatalogV2::default(),
    )
    .unwrap();

    let node_config = build_node_config(
        &node_endpoint,
        node_token,
        &node_id,
        &workspace_id,
        &fixture.workspace,
        &fixture.node_state,
        &fixture.materializations,
        &fixture.history_root,
    );
    let node1 = NodeServer::new_durable_context_pack_clean_exit_fixture(
        node_config,
        fixture.root.clone(),
        fixture.started.clone(),
        fixture.release.clone(),
        SOURCE_SESSION_ID,
    )
    .unwrap();
    let node1_shutdown = node1.shutdown_handle();
    let node1_task = tokio::spawn(node1.run());

    let c2 = C2Running::start(
        C2Config::new(
            "127.0.0.1:0".parse().unwrap(),
            c2_token,
            vec![C2NodeConfig::new(node_id.clone(), node_endpoint.clone(), node_token).unwrap()],
        )
        .unwrap()
        .with_control_endpoint(control_endpoint.clone())
        .unwrap()
        .with_timings(C2Timings {
            poll_interval: Duration::from_millis(20),
            fresh_for: Duration::from_secs(2),
            attempt_deadline: Duration::from_secs(2),
            transient_backoffs: [Duration::from_millis(20); 5],
            parked_backoff: Duration::from_millis(100),
            http_io_deadline: Duration::from_secs(1),
        }),
    )
    .await
    .unwrap();
    let c2_client = C2Client::new(c2.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    // == STEP 1 ==
    // run A spawns against the fixture provider (bounded work, clean exit
    // 0). The fixture establishes a verified provider identity via an
    // early SessionStart Hook post so the Node's auto-export-at-exit path
    // is eligible once it exits.
    let (adapter_a, events_a) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (host_a, host_a_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter_a,
        events_a,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs.clone(),
    )
    .await
    .unwrap();
    let client_a = HarnessOperatorClient::new(host_a.endpoint().socket_addr(), operator_credential.clone())
        .unwrap();

    assert_eq!(
        client_a
            .create_task(HarnessCreateTaskRequestV1 {
                authority: authority('a'),
                task_id: task_a_id(),
                title: "A2 durable context pack source".to_owned(),
                body: String::new(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Ready,
            })
            .unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let run_a_id = match client_a
        .schedule_next(HarnessScheduleNextRequestV1 {
            authority: authority('b'),
            plan_id: Some(selector("durable-source")),
        })
        .unwrap()
    {
        HarnessScheduleOutcomeV1::Dispatch(dispatch) => dispatch.run_id,
        HarnessScheduleOutcomeV1::Idle => panic!("source ScheduleNext returned idle"),
    };

    timeout(Duration::from_secs(30), async {
        loop {
            let running = client_a.run_get(run_a_id.clone()).unwrap().lifecycle
                == HarnessRunLifecycleV1::Running;
            let identified = client_a
                .runtime_inventory_list(None, 32)
                .unwrap()
                .nodes
                .iter()
                .flat_map(|node| &node.inventory.managed_sessions)
                .any(|session| session.provider == "claude" && session.provider_identity_present);
            if running && identified {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("source run did not become Running with an admitted provider identity");

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fixture.release)
        .unwrap()
        .write_all(b"release\n")
        .unwrap();

    // Run A's own lifecycle transition (Running -> Completed) is driven by
    // the already-proven live ControlEvent path, independent of the new
    // durable machinery -- wait for it so run A is a genuinely finished
    // run before anything else happens.
    timeout(Duration::from_secs(15), async {
        loop {
            if client_a.run_get(run_a_id.clone()).unwrap().lifecycle
                == HarnessRunLifecycleV1::Completed
            {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("source run did not become Completed after its clean exit");

    // A live-client E2E proof of the steady-state receipt path (host_a is
    // still up, never restarted, right here) was evaluated and is not
    // feasible as a black-box client assertion: `client_a.run_get` returns
    // the operator-facing `RedactedRunV1`, which does not project
    // `HarnessRunV1.context_pack` at all (no field for it, redacted or
    // otherwise) -- unlike the flagship's `run.lifecycle`/`revision`, there
    // is currently no operator-facing signal that would let a client
    // distinguish "landed via the steady-state live path" from "not landed
    // yet" without either reaching into the durable store directly (which
    // would need `HarnessService::open` on the same path host_a already has
    // open, i.e. a restart in every way that matters here) or widening a
    // public read API as a side effect of this change. Coverage for
    // `apply_live_context_pack_receipt` itself lives in
    // `runtime.rs`'s `apply_live_context_pack_receipt_lands_within_one_live_event`
    // unit test instead.
    stop_host(host_a, host_a_task).await;
    drop(client_a);

    // == STEP 2 ==
    // Node auto-export-at-exit. C2 allows one operator connection at a
    // time, so this reconnects a bare adapter (no harness host attached)
    // and polls NodeRequest::Snapshot directly until
    // `session_records[…].exported_context.is_some()`. The adapter
    // reconnects on any transport error rather than failing outright —
    // this bare connection is freshly established right as the Node just
    // finished processing the source's Exited control event, and a
    // transient reconnect here is a client-robustness concern, not part of
    // the durable ContextPack mechanism under test.
    let (mut adapter_n1, mut events_n1) =
        connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let route_n1 = adapter_n1.exact_route(&node_id).unwrap();
    let node_receipt: ResolvedContextPackReceipt = timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = match adapter_n1.snapshot(&route_n1).await {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    let reconnected =
                        connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
                    adapter_n1 = reconnected.0;
                    events_n1 = reconnected.1;
                    continue;
                }
            };
            if let Some(record) = snapshot
                .session_records
                .iter()
                .find(|record| record.provider.as_str() == "claude")
            {
                if let Some(receipt) = record.exported_context.clone() {
                    break receipt;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Node did not auto-export a durable ContextPack for the exited source session");
    assert_eq!(node_receipt.source_message_count, 2);
    assert_eq!(node_receipt.lineage.source_provider.as_str(), "claude");

    // The source session record is torn down (no longer Live, no active
    // session) the moment it cleanly exits -- first half of the "the
    // durable path was used" proof (design step 8), re-confirmed after the
    // Node restart below where it becomes unconditional.
    let snapshot_n1 = adapter_n1.snapshot(&route_n1).await.unwrap();
    let source_record_n1 = snapshot_n1
        .session_records
        .iter()
        .find(|record| record.provider.as_str() == "claude")
        .unwrap();
    assert_ne!(source_record_n1.state, ManagedSessionState::Live);
    assert!(source_record_n1.active_session.is_none());
    drop(adapter_n1);
    drop(events_n1);

    // == STEP 3 ==
    // Drive one harness resync. `apply_snapshot_context_pack_receipts` is
    // wired into `apply_resync_lifecycle`, which only fires on observation
    // *recovery* (route (re)connect), not on a steady-state timer -- so a
    // fresh harness host connection is exactly "one resync".
    let (adapter_b, events_b) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (host_b, host_b_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter_b,
        events_b,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs.clone(),
    )
    .await
    .unwrap();
    let client_b = HarnessOperatorClient::new(host_b.endpoint().socket_addr(), operator_credential.clone())
        .unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let options = client_b.task_launch_options_get(task_a_id()).unwrap();
            if options
                .context_sources
                .iter()
                .any(|source| source.source_run_id == run_a_id)
            {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("harness resync did not record the durable ContextPack receipt on run A");
    stop_host(host_b, host_b_task).await;
    drop(client_b);

    let store1 = HarnessService::open(&fixture.harness).unwrap();
    let harness_receipt = store1
        .engine()
        .run(&run_a_id)
        .unwrap()
        .context_pack
        .clone()
        .expect("run A must carry its own exported context_pack after resync");
    assert_eq!(harness_receipt.id.as_str(), node_receipt.id.as_str());
    assert_eq!(harness_receipt.digest, node_receipt.digest.as_str());
    assert_eq!(harness_receipt.source_message_count, node_receipt.source_message_count);
    assert_eq!(harness_receipt.retained_message_count, node_receipt.retained_message_count);
    assert_eq!(harness_receipt.byte_len, node_receipt.byte_len);
    assert_eq!(harness_receipt.truncated, node_receipt.truncated);
    drop(store1);

    // == STEP 4 ==
    // Harness store reopen (drop + reopen the same sqlite) -- the receipt
    // must survive unchanged.
    let store2 = HarnessService::open(&fixture.harness).unwrap();
    assert_eq!(
        store2.engine().run(&run_a_id).unwrap().context_pack.as_ref(),
        Some(&harness_receipt),
    );
    drop(store2);

    // == STEP 5 ==
    // Node restart (drop NodeServer, reopen the same state_path).
    // `ContextPackStore::open`'s reseed has no public single-shot probe
    // reachable from an external test (`HarnessC2Adapter` deliberately
    // exposes no generic NodeRequest handle -- see its doc comment in
    // crates/gate4agent-harness-service/src/c2.rs), so the reseed is
    // proved together with steps 7-9: run B's spawn can only resolve the
    // pack via a durable Node whose `context_catalog` was reseeded from
    // `ContextPackStore::open`, since run A's original session/process no
    // longer exists anywhere in node2's process tree.
    node1_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node1_task).await.unwrap().unwrap().unwrap();
    drop(node1_shutdown);

    let node_config2 = build_node_config(
        &node_endpoint,
        node_token,
        &node_id,
        &workspace_id,
        &fixture.workspace,
        &fixture.node_state,
        &fixture.materializations,
        &fixture.history_root,
    );
    let node2 = NodeServer::new_durable_context_pack_clean_exit_fixture(
        node_config2,
        fixture.root.clone(),
        fixture.started_round2.clone(),
        fixture.release_round2.clone(),
        SOURCE_SESSION_ID,
    )
    .unwrap();
    let node2_shutdown = node2.shutdown_handle();
    let node2_task = tokio::spawn(node2.run());
    wait_online(&c2_client, &node_id).await;

    let (adapter_n2, events_n2) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let route_n2 = adapter_n2.exact_route(&node_id).unwrap();
    let snapshot_n2 = adapter_n2.snapshot(&route_n2).await.unwrap();
    let source_record_n2 = snapshot_n2
        .session_records
        .iter()
        .find(|record| record.provider.as_str() == "claude")
        .expect("session_records must persist across a Node restart");
    // The Node's own `state.json` persistence (pre-A2, unrelated to
    // ContextPackStore) already carries `exported_context` across restart --
    // check it here as a first, weaker checkpoint before the durable-store
    // proof in steps 7-9.
    assert_eq!(source_record_n2.exported_context.as_ref(), Some(&node_receipt));
    // The source session cannot possibly be alive in node2's process tree --
    // it belongs to a Node process instance that no longer exists. Second,
    // now-unconditional half of design step 8's "durable path was used"
    // proof.
    assert_ne!(source_record_n2.state, ManagedSessionState::Live);
    assert!(source_record_n2.active_session.is_none());
    drop(adapter_n2);
    drop(events_n2);

    // == STEP 6 ==
    // Task B's launch options show run A as `availability: Durable`
    // without ever calling ObserveRunContextSource for run A (that RPC is
    // never invoked anywhere in this test).
    let (adapter_c, events_c) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (host_c, mut host_c_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter_c,
        events_c,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs,
    )
    .await
    .unwrap();
    let client_c = HarnessOperatorClient::new(host_c.endpoint().socket_addr(), operator_credential)
        .unwrap();
    assert_eq!(
        client_c
            .create_task(HarnessCreateTaskRequestV1 {
                authority: authority('c'),
                task_id: task_b_id(),
                title: "A2 durable context pack target".to_owned(),
                body: String::new(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Ready,
            })
            .unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let options = timeout(Duration::from_secs(15), async {
        loop {
            let options = client_c.task_launch_options_get(task_b_id()).unwrap();
            if options
                .context_sources
                .iter()
                .any(|source| source.source_run_id == run_a_id)
            {
                break options;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("task B launch options did not surface run A as a durable context source");
    let durable_source = options
        .context_sources
        .iter()
        .find(|source| source.source_run_id == run_a_id)
        .unwrap();
    assert_eq!(durable_source.availability, HarnessContextSourceAvailabilityV1::Durable);
    assert!(durable_source.active_session.is_none());
    let context_pack_on_source = durable_source
        .context_pack
        .as_ref()
        .expect("a Durable source must carry its context_pack inline");
    assert_eq!(context_pack_on_source, &harness_receipt);

    // == STEP 7 ==
    // Select that context source, ReplaceTaskExecutionSpecV2, then
    // StartTaskV2 -> run B dispatches.
    let plan_option = options
        .plans
        .iter()
        .find(|plan| plan.plan.plan_id.as_str() == "durable-target")
        .unwrap()
        .clone();
    let selection = HarnessReviewedTaskLaunchSelectionV1 {
        plan: plan_option,
        worktree: HarnessReviewedWorktreeSelectionV1::Existing,
        context_source: Some(durable_source.clone()),
        delivery: None,
        review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
    };
    assert_eq!(
        client_c
            .replace_task_execution_spec_v2(HarnessReplaceTaskExecutionSpecRequestV2 {
                authority: authority('d'),
                task_id: task_b_id(),
                expected_task_revision: options.task_revision,
                expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                selection,
            })
            .unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let issued_options = client_c.task_launch_options_get(task_b_id()).unwrap();
    let issued_summary = issued_options
        .current_issued_spec
        .expect("ReplaceTaskExecutionSpecV2 must issue a current spec");

    let started = client_c
        .start_task_v2(HarnessStartTaskRequestV2 {
            authority: authority('e'),
            task_id: task_b_id(),
            expected_task_revision: options.task_revision,
            expected_execution_spec_revision: issued_summary.revision,
            expected_launch_issuance: issued_summary.launch_issuance,
        })
        .unwrap();
    assert!(!started.replayed);
    let run_b_id = started.dispatch.run_id;

    if timeout(Duration::from_secs(30), async {
        loop {
            match client_c.run_get(run_b_id.clone()) {
                Ok(run) if run.lifecycle == HarnessRunLifecycleV1::Running => break,
                Ok(_) => {}
                Err(error) => {
                    if host_c_task.is_finished() {
                        panic!(
                            "target run poll failed and host_c_task already finished: poll_error={error:?} host_task_result={:?}",
                            (&mut host_c_task).await,
                        );
                    }
                    panic!("target run poll failed while host_c_task is still running: {error:?}");
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_err()
    {
        let run = client_c.run_get(run_b_id.clone()).unwrap();
        let transfer = client_c.run_transfer_get(run_b_id.clone()).ok();
        panic!(
            "target run did not become Running: lifecycle={:?} failure_category={:?} result_disposition={:?} transfer={:?}",
            run.lifecycle, run.failure_category, run.result_disposition, transfer,
        );
    }

    // Harness-side half of the design step 9 lineage chain: run B's own
    // continuation receipt references the same digest as the pack exported
    // in step 2. `run_transfer_get` reads harness-internal state, so it can
    // be read while host_c still owns the C2 operator seat.
    let transfer = client_c.run_transfer_get(run_b_id.clone()).unwrap();
    let continuation = transfer
        .continuation
        .as_ref()
        .expect("run B must carry a continuation transfer");
    assert_eq!(continuation.state, HarnessContinuationStateV1::Bound);
    let transferred_context = continuation
        .context
        .as_ref()
        .expect("a Bound continuation must carry its resolved context");
    assert_eq!(transferred_context.context_ref.as_str(), harness_receipt.id.as_str());
    assert_eq!(transferred_context.digest, harness_receipt.digest);
    stop_host(host_c, host_c_task).await;
    drop(client_c);

    // == STEP 8 ==
    // The Node served ResolveDurableContextPack, not
    // ExportContextPackForSessionRecord: run A's original managed session
    // was already torn down (proved unconditionally in step 5, since node2
    // is a different Node process instance) at the moment run B's spawn
    // resolved, which is only possible if the durable path served it. This
    // reconnects a bare adapter (host_c released the operator seat above)
    // to read the exact `context_id`/`context` the resolve produced on run
    // B's own managed session record.
    let (adapter_d, events_d) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let route_d = adapter_d.exact_route(&node_id).unwrap();
    let final_snapshot = adapter_d.snapshot(&route_d).await.unwrap();
    let target_record = final_snapshot
        .session_records
        .iter()
        .find(|record| record.provider.as_str() == "codex")
        .expect("target managed session record must be present");
    let expected_context_id = SpawnContextId::new(harness_receipt.id.as_str()).unwrap();
    assert_eq!(target_record.context_id.as_ref(), Some(&expected_context_id));
    let target_context = target_record
        .context
        .as_ref()
        .expect("target record must carry the resolved durable context receipt");
    assert_eq!(target_context.id.as_str(), harness_receipt.id.as_str());
    assert_eq!(target_context.digest.as_str(), harness_receipt.digest);
    drop(adapter_d);
    drop(events_d);

    // == STEP 9 ==
    // Node-side half: run B's materialized session actually received the
    // injected context (materialized file on disk). Only run B ever binds
    // a context, so exactly one `context-pack.json` exists anywhere under
    // the materialization root.
    let context_pack_files = find_files_named(&fixture.materializations, "context-pack.json");
    assert_eq!(context_pack_files.len(), 1, "exactly one session materializes a context pack");
    let raw_context = fs::read_to_string(&context_pack_files[0]).unwrap();
    // The pack's whole job is carrying the retained conversation forward, so
    // the message content is SUPPOSED to appear here (asserted positively
    // below) — only the internal session-identity canary (never part of the
    // conversation itself) must not leak, mirroring the flagship test's own
    // exact distinction between "identity" and "content".
    assert!(
        !raw_context.contains(SOURCE_SESSION_ID),
        "private source session identity leaked into the materialized context",
    );
    let document: serde_json::Value = serde_json::from_str(&raw_context).unwrap();
    assert_eq!(document["schema"], "g4a-context-pack-v1");
    assert_eq!(document["source_provider"], "claude");
    let retained_messages = document["retained_messages"].as_array().unwrap();
    assert_eq!(retained_messages.len(), 2);
    assert_eq!(retained_messages[0]["role"], "user");
    assert_eq!(retained_messages[0]["text"], SOURCE_USER_MESSAGE);
    assert_eq!(retained_messages[1]["role"], "assistant");
    assert_eq!(retained_messages[1]["text"], SOURCE_ASSISTANT_MESSAGE);

    node2_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node2_task).await.unwrap().unwrap().unwrap();
    drop(node2_shutdown);
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    drop(c2_shutdown);
}
