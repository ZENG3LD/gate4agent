#![cfg(windows)]

//! Drives the OPERATOR-STARTED derived `-harness-mcp` launch-plan sibling
//! (`mode: Acp`, `harness_mcp: GrantBound`, `grant: Operator`) end to end
//! over the real operator wire, the real c2 relay, and a real Node with
//! `--harness-mcp-helper` configured -- the exact shape named in
//! `docs/gate4agent/audits/gate4agent-mailbox-arc-proof-2026-09-03.md`:
//! `task create -> task move ready -> spec save --plan -> task start`
//! selecting `auto-codex-<workspace>-<profile>-full-auto-harness-mcp`,
//! never `ScheduleNext` against an `Exact` grant and never a `ParentRun`
//! actor. `windows_harness_mcp_e2e.rs` already covers the SIBLING path this
//! one does not: a `ScheduleNext`-dispatched child run under an `Exact`
//! grant, `mode: Pty`
//! (`operator_schedule_next_h3b_is_exact_replay_restart_generation_and_revoke_bound`).
//! Neither test subsumes the other.
//!
//! No parent run exists here on purpose: commit `2551088` (an
//! operator-started top-level run may dispatch with harness MCP) is
//! exactly the fix that lets a `User`-actor top-level task dispatch this
//! plan at all -- `validate_harness_mcp_grant` refused every such task
//! before it, naming the actor as not a run.
//!
//! `ApprovalLevel::FullAuto` is the level under test, not `Unmanaged`:
//! `derive_launch_plans_from_inventory` (`gate4agent-harness-service::
//! dispatch`) only offers the harness-MCP/ACP sibling at a level whose
//! catalog row carries a sourced `acp_mode_id: Some(_)`
//! (`approval_level_resolution`, `gate4agent-catalog::launch`) --
//! `Unmanaged`'s row is `acp_mode_id: None` for every provider (it applies
//! nothing over `session/set_mode` by definition), so no `-harness-mcp`
//! sibling exists for it at all. `codex` at `FullAuto` carries
//! `acp_mode_id: Some("agent-full-access")`, which is exactly why this
//! test's fixture agent must answer `session/set_mode`: the ACP transport
//! calls it right after the handshake to apply the level
//! (`apply_acp_approval_mode`, `gate4agent-shell-native/src/lib.rs`), and a
//! silent/unanswered call there would fail the spawn before `session/
//! prompt` is ever reached.
//!
//! The Node's dispatch target for this derived plan is `acp_fixture_agent`
//! (`gate4agent-harness-mcp`'s own `[[bin]]`, `src/bin/
//! acp_fixture_agent.rs`): a minimal real ACP v1 agent over stdio,
//! launched through `NodeServer::new_harness_mcp_acp_launcher_fixture`
//! (`gate4agent-node/src/server.rs`). That constructor is the fix for a
//! defect this file's own earlier revision measured live: `NodeServer::
//! new_harness_mcp_proxy_fixture` unconditionally sets `config.
//! fixture_raw_pty_runtime = true`, which tears out the live
//! `ProviderRuntimeMonitor` entirely and makes `NodeShared::
//! admit_provider_runtime`'s no-monitor branch hardcode `acp_transport =
//! false` for every provider -- refusing `ProviderRuntimeRequirement::Acp`
//! unconditionally (`node_failure_code=UnsupportedCapability`), before any
//! spawn is even attempted, regardless of what the catalog declares.
//! `new_harness_mcp_acp_launcher_fixture` leaves `fixture_raw_pty_runtime`
//! at its default `false` (a real, catalog-honest `ProviderRuntimeMonitor`)
//! and additionally installs `spec.launch` as the ACP transport's own
//! `launch_override`, so the real spawn (`AcpSession::spawn_with_launch` ->
//! a bare `Command::new`, `gate4agent`'s `src/acp/spawn.rs`) reaches
//! `acp_fixture_agent` directly instead of the catalog's vendor-npm
//! resolution (`acp_command`, same file), which would otherwise try `npx -y
//! @agentclientprotocol/codex-acp@...` on a box that has no such thing
//! installed.
//!
//! Assertion (5) is the reason `acp_fixture_agent` exists at all: the only
//! ACP-speaking fixture agent this workspace already shipped
//! (`gate4agent-testkit`'s `acp_agent_spec`/`grok_acp_agent_spec`, its
//! shared `acp_fixture_launch` script) exists to enforce an EMPTY
//! `mcpServers` array -- it errors the handshake (exit 41/42) the moment it
//! is anything but `[]`, so it cannot serve as the receiving end for a live
//! harness-MCP overlay; using it here would prove the opposite of what
//! assertion (5) asks. `acp_fixture_agent` instead captures the ENTIRE
//! `session/new` params object it receives to a file this test reads back.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_client::{
    HarnessOperatorClient, HarnessOperatorCredential, HarnessOperatorMutationOutcomeV1,
    HarnessReplaceTaskExecutionSpecRequestV2, HarnessReviewedTaskLaunchSelectionV1,
    HarnessReviewedWorktreeSelectionV1, HarnessRuntimeManagedModeV1, HarnessStartTaskRequestV2,
};
use gate4agent_harness_protocol::{
    HarnessCreateTaskRequestV1, HarnessEntityReadScopeV1, HarnessExecutionModeV1,
    HarnessExpectedExecutionSpecRevisionV1, HarnessIdempotencyRef, HarnessMoveTaskRequestV1,
    HarnessOperationId, HarnessOperatorAuthorityV1, HarnessRevision, HarnessRunLifecycleV1,
    HarnessSelectorV1, HarnessTaskId, HarnessTaskReviewPolicyV1, HarnessTaskStateV1,
    HarnessWorktreeIntentV1, SessionGrantStateV1,
};
use gate4agent_harness_service::{
    c2::HarnessC2Adapter,
    dispatch::{
        deterministic_default_grant_ids, deterministic_dispatch_ids, HarnessContinuationPolicyV1,
        HarnessGrantPolicyV1, HarnessLaunchCatalog, HarnessLaunchPlanV1, HarnessMcpPolicyV1,
        HarnessPromptSourceV1,
    },
    runtime::{start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs},
    HarnessMcpReservationStateV1, HarnessService,
};
use gate4agent_node::{
    protocol::{
        NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
        WorkspaceId,
    },
    NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig,
};
use gate4agent_observation_service::ObservationService;
use gate4agent_testkit::require_windows_headless_supervisor_for_test;
use gate4agent_types::{AgentId, ApprovalLevel, TerminalSize};
use serde_json::Value;
use tokio::time::{sleep, timeout};

struct FixturePaths {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "gate4agent-op-acp-e2e-{}-{}-{}",
            std::process::id(),
            unix_time_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            root,
        }
    }
}

impl Drop for FixturePaths {
    fn drop(&mut self) {
        if self.root.is_dir() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().try_into().unwrap()
}

fn pipe(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        r"\\.\pipe\gate4agent-op-acp-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn authority(marker: char, now: u64) -> HarnessOperatorAuthorityV1 {
    HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!("hop_{}", marker.to_string().repeat(24))).unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        actor_id: selector("op-acp-e2e-operator"),
        now_unix_ms: now,
    }
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes.get(node_id).is_some_and(|node| {
                    node.transport == gate4agent_c2::protocol::NodeTransportState::Online
                })
            }) { return; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("fixture Node did not become online through C2");
}

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
) -> (HarnessC2Adapter, gate4agent_harness_service::c2::HarnessC2EventReceiver) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) = HarnessC2Adapter::connect(endpoint, token).await {
                return connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 did not release its sole Harness operator lease")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_started_derived_acp_harness_mcp_plan_mints_self_grant_and_arms_reservation() {
    require_windows_headless_supervisor_for_test();
    // Surfaces the harness's own `tracing::warn!`/`tracing::info!` dispatch
    // bookkeeping (in particular `note_terminal_pre_dispatch`'s `stage` +
    // `error` fields) and the Node's server-side spawn logging on stderr --
    // both run in-process (`tokio::spawn(server.run())`, `start_harness_host_
    // with_operator_and_catalogs`), so one process-wide subscriber captures
    // both. Matches the install already used by
    // `windows_schedule_next_c2_e2e.rs` and its siblings.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_writer(std::io::stderr)
        .try_init();
    let fixture = FixturePaths::new();
    let workspace = fixture.root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let original_working_directory = std::env::current_dir().unwrap();
    std::env::set_current_dir(&workspace).unwrap();

    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("op-acp-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let profile_id = SpawnProfileId::new("codex").unwrap();
    let node_token = "op-acp-node-token";
    let c2_token = "op-acp-c2-token";
    let operator_secret = format!("g4aho_{}", "9".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();

    // The exact derived plan id `derive_launch_plans_from_inventory` mints
    // for a single node (`include_node_id == false`) at `ApprovalLevel::
    // FullAuto` (`approval_level_slug` -> `"full-auto"`; see the file-level
    // doc comment for why `FullAuto`, not `Unmanaged`):
    // `auto-<provider>-<workspace_id>-<profile_id>-full-auto-harness-mcp`.
    let expected_plan_id = selector(format!(
        "auto-codex-{}-{}-full-auto-harness-mcp", workspace_id.as_str(), profile_id.as_str(),
    ));

    // The base `codex` spawn profile this derives its plain-PTY sibling
    // from -- unrelated to the ACP sibling's own launcher, which
    // `NodeServer::new_harness_mcp_acp_launcher_fixture` (below) replaces
    // independently through the ACP transport's `launch_override`.
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: profile_id.clone(),
        revision: SpawnProfileRevision::new("op-acp-r1").unwrap(),
        provider: AgentId::new("codex").unwrap(),
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
        .with_state_path(fixture.root.join("node-state.json"))
        .unwrap();

    // Captured by `acp_fixture_agent` at `session/new` time -- the whole
    // params object, read back for assertion (5). Set as an env var (not
    // argv) because `NodeServer::new_harness_mcp_acp_launcher_fixture`
    // replaces `spec.launch.fixed_args` wholesale; `AcpProcess::
    // spawn_with_launch` never calls `.env_clear()`, so a plain
    // `std::env::set_var` here reaches the spawned child exactly the way
    // `std::env::set_current_dir`, above, already reaches it as its cwd.
    let capture_path = fixture.root.join("acp-session-new-params.json");
    std::env::set_var("G4A_ACP_FIXTURE_CAPTURE", &capture_path);

    let helper_program = PathBuf::from(env!("CARGO_BIN_EXE_gate4agent-harness-mcp"));
    let provider_program = PathBuf::from(env!("CARGO_BIN_EXE_acp_fixture_agent"));
    let provider_id = AgentId::new("codex").unwrap();
    let server = NodeServer::new_harness_mcp_acp_launcher_fixture(
        node_config,
        helper_program,
        provider_id,
        provider_program,
        Vec::new(),
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

    let (adapter, events) = connect_harness_adapter(&control_endpoint, c2_token).await;
    let catalogs = HarnessRuntimeCatalogs::new(
        HarnessLaunchCatalog::default(),
        Default::default(),
    ).unwrap();
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        "127.0.0.1:0".parse().unwrap(),
        Some(operator_credential.clone()),
        catalogs,
    ).await.unwrap();
    let client = HarnessOperatorClient::new(
        host.endpoint().socket_addr(), operator_credential.clone(),
    ).unwrap();

    let task_id = HarnessTaskId::new(format!("htask_{}", "7".repeat(24))).unwrap();
    let body = "operator-started derived ACP harness-MCP plan fixture body";
    assert_eq!(
        client.create_task(HarnessCreateTaskRequestV1 {
            authority: authority('a', unix_time_ms()),
            task_id: task_id.clone(),
            title: "Operator-started ACP harness-MCP fixture".to_owned(),
            body: body.to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Backlog,
        }).unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    assert_eq!(
        client.move_task(HarnessMoveTaskRequestV1 {
            authority: authority('b', unix_time_ms()),
            task_id: task_id.clone(),
            expected_revision: HarnessRevision::new(1).unwrap(),
            state: HarnessTaskStateV1::Ready,
        }).unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );

    // Assertion (1) -- pins commit `78b1271` (an ACP session with harness
    // MCP is an ordinary operator plan): "every node x workspace x
    // provider x profile derives a -harness-mcp sibling (mode acp) that
    // launch-options lists". Before that commit no derived plan carried
    // `harness_mcp` at all.
    let options = timeout(Duration::from_secs(10), async {
        loop {
            let options = client.task_launch_options_get(task_id.clone()).unwrap();
            if options.plans.iter().any(|plan| plan.plan.plan_id == expected_plan_id) {
                break options;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("derived -harness-mcp launch plan did not surface in task launch options");
    let plan = options.plans.iter()
        .find(|plan| plan.plan.plan_id == expected_plan_id)
        .unwrap()
        .clone();
    assert_eq!(plan.node_id.as_str(), node_id.as_str());
    assert_eq!(plan.source_workspace_id.as_str(), workspace_id.as_str());
    assert_eq!(plan.provider_profile.as_str(), profile_id.as_str());
    assert_eq!(plan.provider_id.as_str(), "codex");
    assert_eq!(plan.mode, HarnessExecutionModeV1::Acp);

    let selection = HarnessReviewedTaskLaunchSelectionV1 {
        plan,
        worktree: HarnessReviewedWorktreeSelectionV1::Existing,
        context_source: None,
        delivery: None,
        review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
    };
    assert_eq!(
        client.replace_task_execution_spec_v2(HarnessReplaceTaskExecutionSpecRequestV2 {
            authority: authority('c', unix_time_ms()),
            task_id: task_id.clone(),
            expected_task_revision: options.task_revision,
            expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            selection,
        }).unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );

    let issued_options = client.task_launch_options_get(task_id.clone()).unwrap();
    let issued_spec = issued_options.current_issued_spec.clone()
        .expect("spec save issued an execution spec");

    // `start_task_v2` itself pins commit `2551088`: before it, this exact
    // call refused with "H3B operation actor is not a run" for every
    // operator-started top-level task -- there was no parent run to name.
    let dispatch = client.start_task_v2(HarnessStartTaskRequestV2 {
        authority: authority('d', unix_time_ms()),
        task_id: task_id.clone(),
        expected_task_revision: issued_options.task_revision,
        expected_execution_spec_revision: issued_spec.revision,
        expected_launch_issuance: issued_spec.launch_issuance,
    }).unwrap();
    assert!(!dispatch.replayed);

    // Assertion (2)/(3), part A -- live, operator-wire-observable: the run
    // reaches `Dispatching` and is never seen `Failed`/`OutcomeUnknown` in
    // the tight window right after `start_task_v2` returns, before the
    // Node's own spawn round trip has had time to resolve either way. Pins
    // `3a886b5` (the H3B grant-to-Arm preflight) and `7654075` (a lost
    // reply must not silently commit `OutcomeUnknown`) and `1983d5a` (an
    // Operator-policy dispatch must not be refused post-Arm for a grant it
    // never named).
    let mut observed_dispatching = false;
    let mut terminal_failure = None;
    let deadline = StdInstant::now() + Duration::from_secs(3);
    while StdInstant::now() < deadline {
        let run = client.run_get(dispatch.dispatch.run_id.clone()).unwrap();
        if matches!(run.lifecycle, HarnessRunLifecycleV1::Failed | HarnessRunLifecycleV1::OutcomeUnknown) {
            terminal_failure = Some((run.lifecycle, run.failure_category));
            break;
        }
        if run.lifecycle == HarnessRunLifecycleV1::Dispatching {
            observed_dispatching = true;
        }
        sleep(Duration::from_millis(10)).await;
    }
    if let Some((lifecycle, failure_category)) = terminal_failure {
        // Release the host's own handle on `fixture.harness` before this
        // diagnostic path re-opens it directly -- `HarnessService` is a
        // single-writer core (one live handle per store); reading it while
        // the host still holds it open is not a supported concurrent-read
        // path anywhere else in this suite.
        let _ = host.shutdown().await;
        let _ = host_task.await;
        let _ = node_shutdown.request_shutdown().await;
        let _ = timeout(Duration::from_secs(5), node_task).await;
        c2_shutdown.shutdown();
        let _ = timeout(Duration::from_secs(5), running.wait()).await;
        let (operation_detail, run_detail) = match HarnessService::open(&fixture.harness) {
            Ok(harness) => (
                harness.engine().operation(&dispatch.dispatch.operation_id).cloned(),
                harness.engine().run(&dispatch.dispatch.run_id).cloned(),
            ),
            Err(error) => {
                eprintln!("[diagnostic] re-opening the harness store for post-mortem detail failed: {error:?}");
                (None, None)
            }
        };
        panic!(
            "run reached a terminal failure state before the Node's spawn round trip: {lifecycle:?}\n\
             run.failure_category (redacted, from run_get) = {failure_category:?}\n\
             operation (direct engine read) = {operation_detail:?}\n\
             run (direct engine read) = {run_detail:?}\n\
             -- see the stderr tracing output above for `note_terminal_pre_dispatch`'s \
             `stage`/`error` fields (the pre-dispatch classifier that decided this was terminal).",
        );
    }
    assert!(observed_dispatching, "run never reached Dispatching after start_task_v2");

    // Assertion (4) -- pins commit `e60e6eb` (the harness MCP launch
    // overlay admits ACP with the exact provider binding): before it, the
    // Node accepted the ACP spawn and the runtime refused it ~400ms later
    // (requires PTY transport and the exact provider binding). A managed
    // session record with `mode: Acp` for this workspace, reported back
    // through the Node's own runtime inventory over the operator wire, is
    // exactly "the Node accepted a spawn for the ACP transport" -- the
    // overlay computation (`NativeHarnessMcpLaunchOverlay`) that feeds
    // `session/new.mcpServers` runs at spawn-preparation time, before any
    // ACP wire byte is exchanged with the (silent) child process above.
    let managed_acp_session = timeout(Duration::from_secs(15), async {
        loop {
            let page = client.runtime_inventory_list(None, 10).unwrap();
            let found = page.nodes.iter()
                .find(|node| node.node_id == node_id.as_str())
                .and_then(|node| node.inventory.managed_sessions.iter().find(|session| {
                    session.mode == HarnessRuntimeManagedModeV1::Acp
                        && session.workspace_id == workspace_id.as_str()
                }).cloned());
            if let Some(session) = found { break session; }
            sleep(Duration::from_millis(50)).await;
        }
    }).await.expect(
        "Node runtime inventory never reported a managed ACP session for this workspace",
    );
    assert_eq!(managed_acp_session.mode, HarnessRuntimeManagedModeV1::Acp);

    // Assertion (5) -- pins commit `e60e6eb` too: the exact harness-MCP
    // server entry a live ACP handshake actually receives, not just proof
    // the Node accepted the spawn. `acp_fixture_agent` (this crate's own
    // `[[bin]]`) writes the ENTIRE `session/new.params` object it received
    // to `capture_path` before it acks the handshake -- read back here and
    // checked against the exact provider binding `harness_mcp_acp_server`
    // (`gate4agent-shell-native/src/lib.rs`) installs: name `"gate4agent"`,
    // `args: ["--session-proxy"]`, and an `env` array of `{name, value}`
    // pairs carrying `GATE4AGENT_HARNESS_SESSION_ENDPOINT`/
    // `GATE4AGENT_HARNESS_SESSION_TOKEN` -- never the token's own value.
    let captured_params = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(bytes) = std::fs::read(&capture_path) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    break value;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("acp_fixture_agent never captured a session/new handshake");
    let mcp_servers = captured_params.get("mcpServers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        mcp_servers.len(), 1,
        "expected exactly one harness-MCP server entry: {mcp_servers:?}",
    );
    let mcp_server = &mcp_servers[0];
    assert_eq!(mcp_server.get("name").and_then(Value::as_str), Some("gate4agent"));
    let args = mcp_server.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
    assert_eq!(args, vec![Value::String("--session-proxy".to_owned())]);
    let env_names: Vec<&str> = mcp_server.get("env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        env_names.contains(&"GATE4AGENT_HARNESS_SESSION_ENDPOINT"),
        "harness-MCP env is missing the session endpoint key: {env_names:?}",
    );
    assert!(
        env_names.contains(&"GATE4AGENT_HARNESS_SESSION_TOKEN"),
        "harness-MCP env is missing the session token key: {env_names:?}",
    );

    host.shutdown().await.unwrap();
    host_task.await.unwrap().unwrap();
    assert!(!node_task.is_finished(), "Harness shutdown stopped the Node");
    assert!(http.ready().await.unwrap().ready, "Harness shutdown stopped C2");

    // Assertion (2), part B -- the grant the harness minted for itself.
    // Pins `80e0869` (a run that asks for harness MCP gets a grant, issued
    // inside dispatch: `default_for_run` is self-only reads, timeline
    // visibility, no writes) and `2551088`/`1983d5a` (the grant's
    // `actor_run_id` is the run's OWN id, a `User`-actor top-level
    // dispatch, not a `ParentRun` child -- and the post-Arm proof accepts
    // that shape instead of refusing it).
    let grant_ids = deterministic_default_grant_ids(&dispatch.dispatch.operation_id).unwrap();
    let harness = HarnessService::open(&fixture.harness).unwrap();
    let grant = harness.engine().grant(&grant_ids.grant_id).cloned()
        .expect("resolve_harness_mcp_grant did not mint the deterministic default grant");
    assert_eq!(grant.revision, HarnessRevision::new(1).unwrap());
    assert_eq!(grant.actor_run_id, dispatch.dispatch.run_id);
    assert_eq!(grant.state, SessionGrantStateV1::Active);
    assert_eq!(grant.read_permissions.tasks, HarnessEntityReadScopeV1::SelfOnly);
    assert_eq!(grant.read_permissions.runs, HarnessEntityReadScopeV1::SelfOnly);
    assert_eq!(grant.read_permissions.operations, HarnessEntityReadScopeV1::SelfOnly);

    // Assertion (3), part B -- the reservation reached (at least) Armed.
    // `deterministic_dispatch_ids`'s `harness_mcp_reservation_id` is keyed
    // on `operation_id` and `plan.harness_mcp == GrantBound` alone (see
    // `derived_mcp_reservation_id`), so any validly-shaped `GrantBound`
    // plan reproduces the SAME id the real derived plan minted; the other
    // fields below stand in for it. Reservation state only moves forward
    // (`Prepared -> Armed -> Bound -> Active`, or `Revoked` on abort/
    // expiry) and this test reads it after the operator-wire window above
    // already proved the run was never `Failed`/`OutcomeUnknown`, so
    // anything short of `Revoked` here proves `Armed` was reached. Pins
    // `3a886b5` directly (its own end-to-end preflight is "a grant minted
    // by resolve_harness_mcp_grant passes begin_run_dispatch_with_
    // harness_mcp up to the Arm").
    let stand_in_plan = HarnessLaunchPlanV1 {
        plan_id: expected_plan_id.clone(),
        revision: HarnessRevision::new(1).unwrap(),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector(profile_id.as_str()),
        provider: AgentId::new("codex").unwrap(),
        mode: HarnessExecutionModeV1::Acp,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: HarnessMcpPolicyV1::GrantBound,
        approval_level: ApprovalLevel::FullAuto,
        deadline_ms: 20_000,
    };
    let reservation_ids = deterministic_dispatch_ids(
        &dispatch.dispatch.operation_id,
        &stand_in_plan,
    ).unwrap();
    let reservation_id = reservation_ids.harness_mcp_reservation_id
        .expect("GrantBound plan derives a harness-MCP reservation id");
    let reservation_state = harness.harness_mcp_reservation_state(&reservation_id);
    assert!(
        matches!(
            reservation_state,
            Some(HarnessMcpReservationStateV1::Armed)
                | Some(HarnessMcpReservationStateV1::Bound)
                | Some(HarnessMcpReservationStateV1::Active)
        ),
        "harness-MCP reservation never reached Armed: {reservation_state:?}",
    );
    harness.close().unwrap();

    std::env::set_current_dir(original_working_directory).unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), node_task).await.expect("Node did not stop").unwrap().unwrap();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait()).await.expect("C2 did not stop").unwrap();
}
