//! A3 result write-back E2E — design §9 of
//! `docs/gate4agent/plans/gate4agent-a3-result-writeback-design-2026-08-17.md`.
//!
//! Sibling to A2's `windows_durable_context_pack_finished_run_continuation_e2e.rs`:
//! same production-path discipline (real named pipes, fixture providers from
//! `gate4agent-testkit`, `windows-headless-supervisor.exe`, isolated
//! `--target-dir`). Unlike A2, this scenario needs no durable context-pack
//! machinery and no second run, so it uses a single `Existing`-worktree plan
//! against a real git-repo workspace — the same worktree shape
//! `windows_harness_run_workspace_read_e2e.rs` already proves `InspectRunWorkspace`
//! works against. `Existing` also means there is no managed-worktree lease to
//! ever get cleaned up, so design §11 risk 1 (the worktree-cleanup race) is
//! structurally impossible in this configuration, not just empirically absent
//! — see the comment at step 4's assertion for the concrete proof this test
//! relies on instead (the captured outcome is asserted `Captured` with exact
//! content, never `Unavailable`).
//!
//! Section map (design step -> code section, search for the literal marker
//! comment in the test body):
//!   step 1 -> "== STEP 1 =="  (create task, issue plan via the V2 launch
//!               pipeline, start_task_v2 -> run A spawns against the
//!               clean-exit fixture bound to a real git-repo workspace)
//!   step 2 -> "== STEP 2 =="  (release the fixture, run A reaches
//!               Completed, task reaches Review, baseline result_disposition)
//!   step 3 -> "== STEP 3 =="  (bounded poll: task.result_refs gains exactly
//!               one entry, reactive reconciliation, §4/§5.1)
//!   step 4 -> "== STEP 4 =="  (bounded poll: run.git_facts lands via the
//!               real periodic InspectRunWorkspace round trip, §2/§3; also
//!               where the risk-1 race check lives, see module doc above)
//!   step 5 -> "== STEP 5 =="  (durability: stop the host, reopen the same
//!               sqlite directly, both fields survive unchanged, §6)
//!   step 6 -> "== STEP 6 =="  (read model: fresh host + operator client,
//!               task_get/run_get carry git_facts/result_refs/context_pack
//!               (None)/result_disposition, §7/§8)
//!   step 7 -> "== STEP 7 =="  (operator flow: move_task Review -> Done)

#![cfg(windows)]

use std::{
    fs,
    io::Write as _,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    HarnessExpectedExecutionSpecRevisionV1, HarnessMoveTaskRequestV1, HarnessOperatorCredential,
    HarnessOperatorMutationOutcomeV1, HarnessReplaceTaskExecutionSpecRequestV2,
    HarnessReviewedTaskLaunchSelectionV1, HarnessReviewedWorktreeSelectionV1,
    HarnessStartTaskRequestV2,
};
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_protocol::{
    HarnessCreateTaskRequestV1, HarnessExecutionModeV1, HarnessIdempotencyRef, HarnessOperationId,
    HarnessOperatorAuthorityV1, HarnessResultDispositionV1, HarnessResultRef, HarnessRevision,
    HarnessRunGitCommitSummaryV1, HarnessRunGitFactsOutcomeV1, HarnessRunGitSummaryV1,
    HarnessRunLifecycleV1, HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
    HarnessWorktreeIntentV1,
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
    NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

const AUTHOR_EMAIL_CANARY: &str = "fixture-author-email-result-writeback@gate4agent.invalid";
const COMMIT_SUBJECT: &str = "gate4agent A3 result-writeback fixture commit";

struct FixturePaths {
    root: PathBuf,
    workspace: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    node_state: PathBuf,
    started: PathBuf,
    release: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-run-result-writeback-e2e-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            node_state: root.join("node-state.json"),
            started: root.join("started.marker"),
            release: root.join("release.signal"),
            workspace,
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
        r"\\.\pipe\gate4agent-run-result-writeback-e2e-{label}-{}-{}",
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

fn fixture_task_id() -> HarnessTaskId {
    HarnessTaskId::new(format!("htask_{}", "9".repeat(24))).unwrap()
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

fn assert_git_success(repository: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(repository).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn git_stdout(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(repository).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// One clean commit on `main`, nothing left uncommitted -- gives the
/// periodic git-facts sweep a fully deterministic, exactly-assertable
/// summary to capture (empty status, one recent commit).
fn prepare_repository(workspace: &Path) -> String {
    let init = Command::new("git").args(["init", "-b", "main"]).arg(workspace).output().unwrap();
    assert!(init.status.success(), "git init failed: {}", String::from_utf8_lossy(&init.stderr));
    fs::write(
        workspace.join("README.md"),
        b"gate4agent A3 result-writeback fixture workspace\n",
    )
    .unwrap();
    assert_git_success(workspace, &["add", "--", "README.md"]);
    assert_git_success(
        workspace,
        &[
            "-c",
            "user.name=Gate4Agent Fixture",
            "-c",
            &format!("user.email={AUTHOR_EMAIL_CANARY}"),
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=NUL",
            "commit",
            "--quiet",
            "-m",
            COMMIT_SUBJECT,
        ],
    );
    git_stdout(workspace, &["rev-parse", "HEAD"])
}

fn plan_catalog(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchCatalog {
    HarnessLaunchCatalog::new([HarnessLaunchPlanV1 {
        plan_id: selector("result-writeback"),
        revision: revision(1),
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
    }])
    .unwrap()
}

fn node_config(
    fixture: &FixturePaths,
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
) -> NodeServerConfig {
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("clean-exit").unwrap(),
        revision: SpawnProfileRevision::new("result-writeback-r1").unwrap(),
        provider: AgentId::new("claude").unwrap(),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }])
    .unwrap();
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), fixture.workspace.clone()).unwrap()],
    )
    .unwrap()
    .with_state_path(fixture.node_state.clone())
    .unwrap()
    .with_spawn_profiles(profiles)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_run_result_writeback_survives_reopen_and_reaches_operator_done() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let head_commit = prepare_repository(&fixture.workspace);
    let task_id = fixture_task_id();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("run-result-writeback-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "node-secret-run-result-writeback";
    let c2_token = "c2-secret-run-result-writeback";
    let operator_secret = format!("g4aho_{}", "9".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret).unwrap();

    let catalogs = HarnessRuntimeCatalogs::new(
        plan_catalog(&node_id, &workspace_id),
        DeliveryCatalogV2::default(),
    )
    .unwrap();

    let node = NodeServer::new_clean_exit_fixture(
        node_config(&fixture, &node_endpoint, node_token, &node_id, &workspace_id),
        fixture.root.clone(),
        fixture.started.clone(),
        fixture.release.clone(),
    )
    .unwrap();
    let node_shutdown = node.shutdown_handle();
    let node_task = tokio::spawn(node.run());

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
    let c2_client = C2Client::new(c2.api_addr(), c2_token).unwrap().with_deadline(Duration::from_secs(1));
    wait_online(&c2_client, &node_id).await;

    // == STEP 1 ==
    // Create the task, issue its plan through the V2 launch pipeline
    // (task_launch_options_get -> replace_task_execution_spec_v2 ->
    // start_task_v2), and dispatch run A against the clean-exit fixture
    // bound to the real git-repo workspace prepared above.
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
                task_id: task_id.clone(),
                title: "A3 result write-back".to_owned(),
                body: String::new(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Ready,
            })
            .unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let options = client_a.task_launch_options_get(task_id.clone()).unwrap();
    let plan_option = options
        .plans
        .iter()
        .find(|plan| plan.plan.plan_id.as_str() == "result-writeback")
        .unwrap()
        .clone();
    let selection = HarnessReviewedTaskLaunchSelectionV1 {
        plan: plan_option,
        worktree: HarnessReviewedWorktreeSelectionV1::Existing,
        context_source: None,
        delivery: None,
        review_policy: gate4agent_harness_protocol::HarnessTaskReviewPolicyV1::OperatorReview,
    };
    assert_eq!(
        client_a
            .replace_task_execution_spec_v2(HarnessReplaceTaskExecutionSpecRequestV2 {
                authority: authority('b'),
                task_id: task_id.clone(),
                expected_task_revision: options.task_revision,
                expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                selection,
            })
            .unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let issued_options = client_a.task_launch_options_get(task_id.clone()).unwrap();
    let issued_summary = issued_options
        .current_issued_spec
        .expect("ReplaceTaskExecutionSpecV2 must issue a current spec");
    let started = client_a
        .start_task_v2(HarnessStartTaskRequestV2 {
            authority: authority('c'),
            task_id: task_id.clone(),
            expected_task_revision: options.task_revision,
            expected_execution_spec_revision: issued_summary.revision,
            expected_launch_issuance: issued_summary.launch_issuance,
        })
        .unwrap();
    assert!(!started.replayed);
    let run_a_id = started.dispatch.run_id;

    timeout(Duration::from_secs(15), async {
        loop {
            if fs::read(&fixture.started).ok().as_deref() == Some(b"started\n")
                && client_a.run_get(run_a_id.clone()).ok()
                    .is_some_and(|run| run.lifecycle == HarnessRunLifecycleV1::Running)
            {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("run A did not become Running against the fixture workspace");

    // == STEP 2 ==
    // Release the fixture; run A exits cleanly (exit 0), which drives the
    // already-existing, unchanged lifecycle projection: run -> Completed,
    // task -> Review, run.result_disposition -> Succeeded.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fixture.release)
        .unwrap()
        .write_all(b"release\n")
        .unwrap();

    timeout(Duration::from_secs(15), async {
        loop {
            if client_a.run_get(run_a_id.clone()).unwrap().lifecycle == HarnessRunLifecycleV1::Completed {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("run A did not become Completed after its clean exit");
    assert_eq!(client_a.task_get(task_id.clone()).unwrap().state, HarnessTaskStateV1::Review);
    assert_eq!(
        client_a.run_get(run_a_id.clone()).unwrap().result_disposition,
        Some(HarnessResultDispositionV1::Succeeded),
    );

    // == STEP 3 ==
    // Reactive reconciliation (A3 design §4/§5.1): the task's result_refs
    // gains exactly one entry, deterministically derived from run A's id,
    // the moment result_disposition landed above -- no sweep involved here.
    let expected_ref = HarnessResultRef::for_run(&run_a_id);
    timeout(Duration::from_secs(10), async {
        loop {
            if client_a.task_get(task_id.clone()).unwrap().result_refs == vec![expected_ref.clone()] {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("task did not accumulate run A's result_refs entry after completion");
    assert_eq!(expected_ref.run_id().unwrap(), run_a_id);

    // == STEP 4 ==
    // Periodic sweep (A3 design §2/§3, RUN_GIT_FACTS_SWEEP_PERIOD = 5s):
    // bounded poll comfortably covering several sweep ticks, never a blind
    // sleep. The captured outcome is asserted exactly `Captured` with the
    // fixture repository's real content (not `Unavailable`, not `is_some()`)
    // -- with an `Existing`-worktree plan there is no managed-worktree lease
    // for `reconcile_managed_worktrees` to ever remove, so design §11 risk 1
    // cannot occur in this configuration; this exact-content assertion is
    // the concrete, non-flaky proof that no such race happened.
    let before_release_unix_ms = unix_time_ms();
    let git_facts_a = timeout(Duration::from_secs(25), async {
        loop {
            if let Some(facts) = client_a.run_get(run_a_id.clone()).unwrap().git_facts {
                break facts;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("periodic git-facts sweep did not capture run A's workspace in time");
    let after_capture_unix_ms = unix_time_ms();
    assert!(git_facts_a.captured_at_unix_ms >= before_release_unix_ms);
    assert!(git_facts_a.captured_at_unix_ms <= after_capture_unix_ms);
    let expected_summary = HarnessRunGitSummaryV1 {
        is_repository: true,
        branch: Some("main".to_owned()),
        status: Vec::new(),
        recent_commits: vec![HarnessRunGitCommitSummaryV1 {
            id: head_commit.clone(),
            summary: COMMIT_SUBJECT.to_owned(),
        }],
        truncated: false,
    };
    assert_eq!(git_facts_a.outcome, HarnessRunGitFactsOutcomeV1::Captured(expected_summary));

    // == STEP 5 ==
    // Durability (A3 design §6): stop the host, reopen the same sqlite
    // directly -- both fields must survive unchanged, no migration.
    stop_host(host_a, host_a_task).await;
    drop(client_a);

    let store = HarnessService::open(&fixture.harness).unwrap();
    assert_eq!(store.engine().run(&run_a_id).unwrap().git_facts.as_ref(), Some(&git_facts_a));
    assert_eq!(
        store.engine().task(&task_id).unwrap().result_refs,
        vec![expected_ref.clone()],
    );
    drop(store);

    // == STEP 6 ==
    // Read model (A3 design §7/§8): fresh host + operator client, the
    // redacted views carry git_facts/result_refs/result_disposition, and
    // context_pack is present-and-typed `None` (no context source was ever
    // selected in this scenario).
    let (adapter_b, events_b) = connect_harness_adapter(&control_endpoint, c2_token, &node_id).await;
    let (host_b, host_b_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter_b,
        events_b,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        catalogs,
    )
    .await
    .unwrap();
    let client_b = HarnessOperatorClient::new(host_b.endpoint().socket_addr(), operator_credential)
        .unwrap();

    let task_view = client_b.task_get(task_id.clone()).unwrap();
    assert_eq!(task_view.result_refs, vec![expected_ref.clone()]);
    assert_eq!(task_view.state, HarnessTaskStateV1::Review);

    let run_view = client_b.run_get(run_a_id.clone()).unwrap();
    assert_eq!(run_view.git_facts.as_ref(), Some(&git_facts_a));
    assert_eq!(run_view.context_pack, None);
    assert_eq!(run_view.result_disposition, Some(HarnessResultDispositionV1::Succeeded));

    // == STEP 7 ==
    // Operator flow: Review -> Done through the real move_task API, with
    // real result content (result_refs/git_facts) already behind the task,
    // not an empty scaffold.
    assert_eq!(
        client_b
            .move_task(HarnessMoveTaskRequestV1 {
                authority: authority('d'),
                task_id: task_id.clone(),
                expected_revision: task_view.revision,
                state: HarnessTaskStateV1::Done,
            })
            .unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    assert_eq!(client_b.task_get(task_id.clone()).unwrap().state, HarnessTaskStateV1::Done);

    stop_host(host_b, host_b_task).await;
    drop(client_b);

    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
    drop(node_shutdown);
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
}
