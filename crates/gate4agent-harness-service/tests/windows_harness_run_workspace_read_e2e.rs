#![cfg(windows)]

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::protocol::C2RelayFailureCode;
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlError};
use gate4agent_harness_api::{
    HarnessExpectedExecutionSpecRevisionV1, HarnessGitDiffModeV1,
    HarnessGitObjectIdV1, HarnessLaunchAuthorityRefV1, HarnessOperatorActionV1,
    HarnessOperatorCredential, HarnessOperatorHostErrorV1,
    HarnessOperatorIntentV1, HarnessOperatorMutationOutcomeV1,
    HarnessOperatorRequestRefV1, HarnessOperatorResponseV1, HarnessRepositoryPathV1,
    HarnessRunCorrelationAvailabilityV1, HarnessRunWorkspaceOriginV1,
    HarnessTaskExecutionSpecInputV1, HarnessTaskReviewPolicyV1,
    HarnessWorkspaceFileContentV1, HARNESS_WORKSPACE_FILE_MAX_BYTES,
};
use gate4agent_harness_client::{HarnessOperatorClient, HarnessOperatorClientError};
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_protocol::{
    HarnessExecutionModeV1, HarnessRevision, HarnessRunLifecycleV1,
    HarnessSelectorV1, HarnessTaskStateV1, HarnessWorktreeIntentV1,
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
    WorkspaceId, MAX_WORKSPACE_FILE_BYTES,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

const WORKSPACE_ROOT_CANARY: &str = "workspace-root-secret-canary";
const WORKTREE_ROOT_CANARY: &str = "worktree-root-secret-canary";
const DIAGNOSTIC_CANARY: &str = "raw-git-diagnostic-secret-canary";
const AUTHOR_EMAIL_CANARY: &str = "fixture-author-email-secret@gate4agent.invalid";

struct FixturePaths {
    root: PathBuf,
    workspace: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    started: PathBuf,
    release: PathBuf,
    node_state: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-harness-run-workspace-read-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        let workspace = root.join(WORKSPACE_ROOT_CANARY);
        fs::create_dir_all(&workspace).unwrap();
        Self {
            workspace,
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
        r"\\.\pipe\gate4agent-harness-run-read-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn path(value: &str) -> HarnessRepositoryPathV1 {
    HarnessRepositoryPathV1::new(value).unwrap()
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

fn git_stdout(repository: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn prepare_repository(
    fixture: &FixturePaths,
) -> (HarnessGitObjectIdV1, HarnessGitObjectIdV1) {
    assert_eq!(MAX_WORKSPACE_FILE_BYTES, HARNESS_WORKSPACE_FILE_MAX_BYTES);
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&fixture.workspace)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr),
    );

    fs::create_dir_all(fixture.workspace.join("notes")).unwrap();
    fs::write(
        fixture.workspace.join("notes").join("utf8.txt"),
        b"visible harness workspace text\n",
    ).unwrap();
    fs::write(
        fixture.workspace.join("tracked.txt"),
        b"committed line\n",
    ).unwrap();
    assert_git_success(&fixture.workspace, &["add", "--", "notes/utf8.txt", "tracked.txt"]);
    assert_git_success(
        &fixture.workspace,
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
            "fixture initial commit",
        ],
    );
    let first = HarnessGitObjectIdV1::new(git_stdout(
        &fixture.workspace,
        &["rev-parse", "HEAD"],
    )).unwrap();
    fs::write(
        fixture.workspace.join("tracked.txt"),
        b"committed line\nsecond committed line\n",
    ).unwrap();
    assert_git_success(&fixture.workspace, &["add", "--", "tracked.txt"]);
    assert_git_success(
        &fixture.workspace,
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
            "fixture second commit",
        ],
    );
    let head = HarnessGitObjectIdV1::new(git_stdout(
        &fixture.workspace,
        &["rev-parse", "HEAD"],
    )).unwrap();

    let worktree = fixture.root.join(WORKTREE_ROOT_CANARY);
    let worktree_text = worktree.to_string_lossy().into_owned();
    assert_git_success(
        &fixture.workspace,
        &["worktree", "add", "--quiet", "-b", "fixture/worktree-canary", &worktree_text, "HEAD"],
    );

    fs::write(
        fixture.workspace.join("notes").join("binary.bin"),
        [0xff, 0xfe, 0x00, 0x61],
    ).unwrap();
    fs::write(
        fixture.workspace.join("notes").join("over-limit.txt"),
        vec![b'x'; MAX_WORKSPACE_FILE_BYTES + 1],
    ).unwrap();
    fs::write(
        fixture.workspace.join("tracked.txt"),
        b"committed line\nsecond committed line\nworking line\n",
    ).unwrap();
    fs::write(
        fixture.workspace.join("staged.txt"),
        b"staged line\n",
    ).unwrap();
    assert_git_success(&fixture.workspace, &["add", "--", "staged.txt"]);
    (head, first)
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

fn node_config(
    fixture: &FixturePaths,
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
) -> NodeServerConfig {
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("clean-exit").unwrap(),
        revision: SpawnProfileRevision::new("harness-run-read-r1").unwrap(),
        provider: AgentId::new("claude").unwrap(),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }]).unwrap();
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), fixture.workspace.clone()).unwrap()],
    ).unwrap()
        .with_state_path(fixture.node_state.clone()).unwrap()
        .with_spawn_profiles(profiles)
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

fn assert_origin(
    origin: &HarnessRunWorkspaceOriginV1,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
    run_revision: HarnessRevision,
    node_id: &NodeId,
    incarnation_id: &str,
    workspace_id: &WorkspaceId,
) {
    assert_eq!(&origin.run_id, run_id);
    assert_eq!(origin.run_revision, run_revision);
    assert_eq!(origin.node_id.as_str(), node_id.as_str());
    assert_eq!(origin.node_incarnation_id.as_str(), incarnation_id);
    assert_eq!(origin.workspace_id.as_str(), workspace_id.as_str());
}

fn assert_private_reply(value: &impl serde::Serialize, canonical_workspace: &str) {
    let json = serde_json::to_string(value).unwrap();
    assert!(!json.contains(canonical_workspace));
    assert!(!json.contains(WORKSPACE_ROOT_CANARY));
    assert!(!json.contains(WORKTREE_ROOT_CANARY));
    assert!(!json.contains(DIAGNOSTIC_CANARY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_like_harness_client_reads_exact_run_workspace_and_rejects_stale_incarnation() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let (head, first_commit) = prepare_repository(&fixture);
    let canonical_workspace = fs::canonicalize(&fixture.workspace).unwrap()
        .to_string_lossy().into_owned();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("harness-run-read-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "harness-run-read-node-token";
    let c2_token = "harness-run-read-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "e".repeat(64),
    )).unwrap();

    let node = NodeServer::new_clean_exit_fixture(
        node_config(&fixture, &node_endpoint, node_token, &node_id, &workspace_id),
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
        vec![C2NodeConfig::new(node_id.clone(), node_endpoint.clone(), node_token).unwrap()],
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
    let tui = HarnessOperatorClient::new(harness_endpoint, operator_credential.clone()).unwrap();

    let direct_c2 = match connect_local(&control_endpoint, c2_token).await {
        Err(error) => error,
        Ok(_) => panic!("direct C2 operator connected while Harness owned the operator slot"),
    };
    assert!(matches!(
        direct_c2,
        C2ControlError::Relay(ref failure)
            if failure.code == C2RelayFailureCode::OperatorAlreadyConnected
    ));

    assert_eq!(
        tui.submit_intent(intent('a', HarnessOperatorActionV1::CreateTask {
            title: "Run workspace read production E2E".to_owned(),
            body: "Bind a real Harness run before reading its workspace".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Ready,
        })).unwrap(),
        HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
    );
    let task = tui.tasks_list(None, Some(HarnessTaskStateV1::Ready), 10).unwrap()
        .tasks.into_iter()
        .find(|task| task.title == "Run workspace read production E2E")
        .expect("Harness did not materialize the Ready task");
    let plan = tui.launch_plans_list(None, 16).unwrap().plans
        .into_iter().next().expect("ordinary launch plan is missing");
    assert_eq!(plan.scheduled_launch.authority, HarnessLaunchAuthorityRefV1::OrdinaryOperator);
    assert_eq!(
        tui.submit_intent(intent('b', HarnessOperatorActionV1::ReplaceTaskExecutionSpec {
            task_id: task.task_id.clone(),
            expected_task_revision: task.revision,
            expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            spec: HarnessTaskExecutionSpecInputV1 {
                scheduled_launch: plan.scheduled_launch,
                review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
            },
        })).unwrap(),
        HarnessOperatorResponseV1::ExecutionSpecMutation(
            HarnessOperatorMutationOutcomeV1::Applied,
        ),
    );
    let execution_spec = tui.task_execution_spec_get(task.task_id.clone()).unwrap()
        .expect("task execution spec was not retained");
    let task = tui.task_get(task.task_id).unwrap();
    let HarnessOperatorResponseV1::TaskStarted(started) = tui.submit_intent(intent(
        'c',
        HarnessOperatorActionV1::StartTask {
            task_id: task.task_id.clone(),
            expected_task_revision: task.revision,
            expected_execution_spec_revision: execution_spec.revision,
            expected_scheduled_launch_digest: execution_spec.scheduled_launch_digest,
        },
    )).unwrap() else {
        panic!("StartTask did not return TaskStarted");
    };
    let run_id = started.dispatch.run_id;

    timeout(Duration::from_secs(15), async {
        loop {
            if fs::read(&fixture.started).ok().as_deref() == Some(b"started\n")
                && tui.run_get(run_id.clone()).ok()
                    .is_some_and(|run| run.lifecycle == HarnessRunLifecycleV1::Running)
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness did not bind and start the workspace fixture run");
    let run = tui.run_get(run_id.clone()).unwrap();
    let correlation = tui.run_correlation_get(run_id.clone()).unwrap();
    assert_eq!(correlation.availability, HarnessRunCorrelationAvailabilityV1::Available);
    assert_eq!(correlation.run_id, run_id);
    assert_eq!(correlation.run_revision, run.revision);
    assert_eq!(correlation.node_id.as_str(), node_id.as_str());
    assert_eq!(correlation.workspace_id.as_str(), workspace_id.as_str());
    let incarnation_id = correlation.node_incarnation_id.as_str().to_owned();

    let inspection = tui.inspect_run_workspace(run_id.clone()).unwrap();
    assert_origin(
        &inspection.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert!(inspection.git.is_repository);
    assert_eq!(inspection.git.branch.as_deref(), Some("main"));
    assert!(inspection.entries.iter().any(|entry| entry.relative_path.as_str() == "notes/utf8.txt"));
    assert!(inspection.git.recent_commits.iter().any(|commit| commit.id == head));
    assert_private_reply(&inspection, &canonical_workspace);

    let utf8 = tui.read_run_workspace_file(run_id.clone(), path("notes/utf8.txt")).unwrap();
    assert_origin(
        &utf8.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert_eq!(
        utf8.content,
        HarnessWorkspaceFileContentV1::Utf8 {
            text: "visible harness workspace text\n".to_owned(),
            byte_len: 31,
        },
    );
    assert!(utf8.revision.is_some());
    assert_private_reply(&utf8, &canonical_workspace);

    let binary = tui.read_run_workspace_file(run_id.clone(), path("notes/binary.bin")).unwrap();
    assert_origin(
        &binary.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert_eq!(binary.content, HarnessWorkspaceFileContentV1::NonUtf8 { byte_len: 4 });
    assert_eq!(binary.revision, None);
    assert_private_reply(&binary, &canonical_workspace);

    let oversized = tui.read_run_workspace_file(
        run_id.clone(),
        path("notes/over-limit.txt"),
    ).unwrap();
    assert_origin(
        &oversized.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert_eq!(
        oversized.content,
        HarnessWorkspaceFileContentV1::TooLarge {
            limit_bytes: HARNESS_WORKSPACE_FILE_MAX_BYTES as u32,
        },
    );
    assert_eq!(oversized.revision, None);
    assert_private_reply(&oversized, &canonical_workspace);

    let history = tui.read_run_git_history(
        run_id.clone(),
        Some(path("tracked.txt")),
        None,
        1,
    ).unwrap();
    assert_origin(
        &history.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert_eq!(history.path.as_ref().map(HarnessRepositoryPathV1::as_str), Some("tracked.txt"));
    assert_eq!(history.commits.len(), 1);
    assert_eq!(history.commits[0].id, head);
    assert_eq!(history.commits[0].subject, "fixture second commit");
    assert_eq!(history.next_before.as_ref(), Some(&head));
    assert!(!history.truncated, "pagination must not be reported as byte truncation");
    assert_private_reply(&history, &canonical_workspace);
    assert!(!serde_json::to_string(&history).unwrap().contains(AUTHOR_EMAIL_CANARY));

    let history_next = tui.read_run_git_history(
        run_id.clone(),
        Some(path("tracked.txt")),
        history.next_before.clone(),
        1,
    ).unwrap();
    assert_origin(
        &history_next.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert_eq!(history_next.commits.len(), 1);
    assert_eq!(history_next.commits[0].id, first_commit);
    assert_eq!(history_next.commits[0].subject, "fixture initial commit");
    assert_eq!(history_next.next_before, None);
    assert!(!history_next.truncated);
    assert_private_reply(&history_next, &canonical_workspace);
    assert!(!serde_json::to_string(&history_next).unwrap().contains(AUTHOR_EMAIL_CANARY));

    let working = tui.read_run_git_diff(
        run_id.clone(),
        HarnessGitDiffModeV1::Working,
        Some(path("tracked.txt")),
    ).unwrap();
    assert_origin(
        &working.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert!(working.text.contains("+working line"));
    assert_private_reply(&working, &canonical_workspace);

    let staged = tui.read_run_git_diff(
        run_id.clone(),
        HarnessGitDiffModeV1::Staged,
        Some(path("staged.txt")),
    ).unwrap();
    assert_origin(
        &staged.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert!(staged.text.contains("+staged line"));
    assert_private_reply(&staged, &canonical_workspace);

    let committed = tui.read_run_git_diff(
        run_id.clone(),
        HarnessGitDiffModeV1::Commit { revision: head.clone() },
        Some(path("tracked.txt")),
    ).unwrap();
    assert_origin(
        &committed.origin, &run_id, run.revision, &node_id, &incarnation_id, &workspace_id,
    );
    assert!(committed.text.contains("+second committed line"));
    assert_private_reply(&committed, &canonical_workspace);

    drop(tui);
    assert!(!host_task.is_finished(), "dropping TUI-like client stopped Harness");
    assert!(!node_task.is_finished(), "dropping TUI-like client stopped Node");
    assert!(c2_client.ready().await.unwrap().ready, "dropping TUI-like client stopped C2");

    let reconnected = HarnessOperatorClient::new(harness_endpoint, operator_credential).unwrap();
    let after_reconnect = reconnected.inspect_run_workspace(run_id.clone()).unwrap();
    assert_eq!(after_reconnect, inspection);
    let file_after_reconnect = reconnected.read_run_workspace_file(
        run_id.clone(),
        path("notes/utf8.txt"),
    ).unwrap();
    assert_eq!(file_after_reconnect, utf8);

    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
    drop(node_shutdown);
    let restarted_node = NodeServer::new(node_config(
        &fixture,
        &node_endpoint,
        node_token,
        &node_id,
        &workspace_id,
    )).unwrap();
    let restarted_shutdown = restarted_node.shutdown_handle();
    let restarted_task = tokio::spawn(restarted_node.run());
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(page) = reconnected.runtime_inventory_list(None, 16) {
                if page.nodes.iter().any(|node| {
                    node.node_id == node_id.as_str()
                        && node.incarnation_id != incarnation_id
                }) {
                    break;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Harness did not observe the replacement Node incarnation");
    assert!(matches!(
        reconnected.inspect_run_workspace(run_id),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)),
    ));

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    restarted_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), restarted_task).await.unwrap().unwrap().unwrap();
}
