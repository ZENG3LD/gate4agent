#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2ManagedSessionRecord, C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus,
    NodeId, NodeRoute, NodeTransportState, RoutedNodeEvent,
    C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY, C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{
    CapabilityId, ManagedSessionState, ManagedWorktreeCleanupFailure,
    ManagedWorktreeLeaseSnapshot, ManagedWorktreeLeaseState, ManagedWorktreeRetention,
    ManagedWorktreeSpawnRequest, NodeFailureCode, NodeRequest, SessionAddress, SessionMode,
    SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides, SpawnProfileId,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorktreeProfileId,
    WorktreeProfileRevision, WorkspaceId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{
    ManagedWorktreeProfile, NodeServer, NodeServerConfig, WorkspaceConfig,
    WorktreeServiceMode,
};
use gate4agent_types::{AgentId, TerminalControl, TerminalSize, TransportKind};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

fn require_headless_windows_fixture() {
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows PTY tests must run through windows-headless-supervisor",
    );
    unsafe {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        const WER_FAULT_REPORTING_NO_UI: u32 = 0x0020;

        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
            fn WerSetFlags(flags: u32) -> i32;
        }

        SetErrorMode(
            SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
        );
        let _ = WerSetFlags(WER_FAULT_REPORTING_NO_UI);
    }
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-managed-worktree-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn exact_cmd_launcher() -> String {
    let configured = std::env::var_os("ComSpec").expect("Windows ComSpec is unavailable");
    std::fs::canonicalize(Path::new(&configured))
        .expect("Windows command processor is unavailable")
        .into_os_string()
        .into_string()
        .expect("Windows command processor path is not Unicode")
}

fn git_command(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_git_success(root: &Path, arguments: &[&str]) {
    let output = git_command(root, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[derive(Debug)]
struct ListedWorktree {
    path: PathBuf,
    branch: Option<String>,
}

fn listed_worktrees(repository: &Path) -> Vec<ListedWorktree> {
    let output = git_command(repository, &["worktree", "list", "--porcelain"]);
    assert!(
        output.status.success(),
        "git worktree list failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let text = String::from_utf8(output.stdout).expect("Git worktree list was not UTF-8");
    let mut listed = Vec::new();
    let mut path = None;
    let mut branch = None;
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_owned());
        } else if line.is_empty() {
            if let Some(path) = path.take() {
                listed.push(ListedWorktree {
                    path,
                    branch: branch.take(),
                });
            }
        }
    }
    listed
}

struct RemoveFixtureDirectory(PathBuf);

impl Drop for RemoveFixtureDirectory {
    fn drop(&mut self) {
        let safe = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-managed-worktree-e2e-"));
        if safe {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn fixture_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "gate4agent-managed-worktree-e2e-{}-{nonce}",
        std::process::id(),
    ))
}

fn node_config(
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    repository: &Path,
    allocation_root: &Path,
    state_path: &Path,
) -> NodeServerConfig {
    let profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("review").unwrap(),
        WorktreeProfileRevision::new("fixture-v1").unwrap(),
        allocation_root,
        "codex/managed",
        "HEAD",
        ManagedWorktreeRetention::Retain,
    )
    .unwrap();
    let workspace = WorkspaceConfig::new(workspace_id.clone(), repository)
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(profile)
        .unwrap();
    NodeServerConfig::new(endpoint, token, node_id.clone(), [workspace])
        .unwrap()
        .with_state_path(state_path)
        .unwrap()
}

fn spawn_request(node_id: &NodeId, workspace_id: &WorkspaceId) -> ManagedWorktreeSpawnRequest {
    ManagedWorktreeSpawnRequest {
        worktree_profile_id: WorktreeProfileId::new("review").unwrap(),
        spawn_spec: SpawnSpec {
            target: SpawnTarget {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("default").unwrap(),
            expected_profile_revision:
                gate4agent_node::protocol::SpawnProfileRevision::new("builtin-v1").unwrap(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Set {
                    value: agent("claude"),
                },
                mode: SpawnOverride::Set {
                    value: SessionMode::Pty,
                },
                terminal_size: SpawnOverride::Set {
                    value: TerminalSize {
                        rows: 80,
                        columns: 200,
                    },
                },
                prompt: SpawnOverride::Clear,
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Clear,
                environment_profile_id: SpawnOverride::Clear,
            },
            deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("managed-worktree-once").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
                SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
            )
            .unwrap()])
            .unwrap(),
        },
    }
}

fn assert_request_is_path_free(request: &ManagedWorktreeSpawnRequest) {
    let encoded = serde_json::to_value(NodeRequest::SpawnManagedWorktree {
        request: request.clone(),
    })
    .unwrap();
    let outer = encoded.as_object().unwrap();
    assert_eq!(outer.len(), 2);
    assert_eq!(outer["kind"], "spawn-managed-worktree");
    let public_request = outer["request"].as_object().unwrap();
    let mut keys = public_request.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(keys, ["spawn_spec", "worktree_profile_id"]);
    let encoded = serde_json::to_string(&encoded).unwrap();
    for forbidden in ["target_root", "allocation_root", "branch", "base_commit", "gitdir"] {
        assert!(
            !encoded.contains(forbidden),
            "managed spawn request leaked {forbidden}",
        );
    }
}

fn assert_public_lease_is_path_free(lease: &ManagedWorktreeLeaseSnapshot, target: &Path) {
    fn inspect(value: &Value, target: &str) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    for forbidden in ["path", "root", "branch", "base", "gitdir", "canonical"] {
                        assert!(
                            !key.contains(forbidden),
                            "public managed lease exposed private key {key}",
                        );
                    }
                    inspect(value, target);
                }
            }
            Value::Array(values) => {
                for value in values {
                    inspect(value, target);
                }
            }
            Value::String(value) => assert!(
                !value.eq_ignore_ascii_case(target),
                "public managed lease exposed its host path",
            ),
            _ => {}
        }
    }

    let target = target.to_string_lossy();
    inspect(&serde_json::to_value(lease).unwrap(), &target);
}

async fn wait_online_after(
    client: &C2Client,
    node_id: &NodeId,
    previous: Option<&NodeRoute>,
) -> NodeRoute {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(status) = client.status().await {
                if status.nodes[node_id].transport == NodeTransportState::Online {
                    let incarnation_id = status.nodes[node_id]
                        .cursor
                        .expect("online fixture node has no cursor")
                        .incarnation_id;
                    if previous.is_none_or(|route| route.expected_incarnation_id != incarnation_id) {
                        return NodeRoute {
                            node_id: node_id.clone(),
                            expected_incarnation_id: incarnation_id,
                        };
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture node did not become online through C2")
}

async fn wait_offline(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes[node_id].transport != NodeTransportState::Online
            }) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture node remained online after shutdown");
}

async fn snapshot(control: &C2ControlHandle, route: &NodeRoute) -> C2NodeSnapshot {
    let response = control
        .request(route.clone(), NodeRequest::Snapshot)
        .await
        .expect("C2 snapshot route failed");
    match response.response {
        Ok(C2NodeResponse::Snapshot { snapshot, .. }) => snapshot,
        response => panic!("C2 snapshot returned an unexpected response: {response:?}"),
    }
}

async fn wait_relay_ready(control: &C2ControlHandle, route: &NodeRoute) {
    timeout(Duration::from_secs(10), async {
        loop {
            match control.request(route.clone(), NodeRequest::Snapshot).await {
                Ok(response) => match response.response {
                    Ok(C2NodeResponse::Snapshot { .. }) => return,
                    response => {
                        panic!("C2 relay readiness probe returned an unexpected response: {response:?}")
                    }
                },
                Err(_) => sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("C2 status became online before its production relay became ready");
}

fn session_count(snapshot: &C2NodeSnapshot) -> usize {
    snapshot
        .workspaces
        .iter()
        .map(|workspace| workspace.sessions.len())
        .sum()
}

fn find_session<'a>(
    snapshot: &'a C2NodeSnapshot,
    address: &SessionAddress,
) -> Option<&'a gate4agent_c2::protocol::C2SessionSnapshot> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == address.workspace_id)?
        .sessions
        .iter()
        .find(|session| {
            session.instance_id == address.session.instance_id
                && session.generation == address.session.generation
        })
}

fn find_lease<'a>(
    snapshot: &'a C2NodeSnapshot,
    lease: &ManagedWorktreeLeaseSnapshot,
) -> Option<&'a ManagedWorktreeLeaseSnapshot> {
    snapshot
        .managed_worktrees
        .iter()
        .find(|candidate| candidate.lease_id == lease.lease_id)
}

fn find_record<'a>(
    snapshot: &'a C2NodeSnapshot,
    record_id: &gate4agent_node::protocol::SessionRecordId,
) -> Option<&'a C2ManagedSessionRecord> {
    snapshot
        .session_records
        .iter()
        .find(|record| &record.record_id == record_id)
}

fn assert_raw_inventory_record(
    record: &C2ManagedSessionRecord,
    session: Option<&SessionAddress>,
    workspace_id: &WorkspaceId,
    expected_state: ManagedSessionState,
    expected_name: &str,
) {
    assert_eq!(record.display_name, expected_name);
    assert_eq!(record.provider, agent("claude"));
    assert_eq!(record.mode, SessionMode::Pty);
    assert_eq!(record.state, expected_state);
    assert_eq!(&record.workspace_id, workspace_id);
    assert_eq!(record.active_session.as_ref(), session);
    assert!(record.environment_profile.is_none());
    assert!(record.bundle.is_none());
    assert!(record.context_id.is_none());
    assert!(record.context.is_none());
    assert!(!record.provider_identity_present);
    assert!(record.created_at_unix_ms > 0);
    assert!(record.updated_at_unix_ms >= record.created_at_unix_ms);
}

async fn wait_running(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            let current = snapshot(control, route).await;
            let session = find_session(&current, address)
                .expect("managed raw PTY session is missing from the C2 snapshot");
            assert_eq!(session.transport, TransportKind::Pty);
            if session.status == C2SessionStatus::Running {
                return;
            }
            assert!(
                !matches!(
                    session.status,
                    C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
                ),
                "cmd.exe raw PTY exited before becoming healthy",
            );
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cmd.exe raw PTY did not become healthy through C2");
}

async fn wait_terminal_proof(
    events: &mut watch::Receiver<Option<RoutedNodeEvent>>,
    route: &NodeRoute,
    address: &SessionAddress,
    expected: &[&str],
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last_observation = "no routed terminal frame observed".to_owned();
    loop {
        let observed = events.borrow_and_update().clone();
        match observed.as_ref().map(|event| &event.event) {
            Some(C2NodeEvent::TerminalFrame {
                address: event_address,
                frame,
            }) if observed.as_ref().is_some_and(|event| {
                event.node_id == route.node_id
                    && event.cursor.incarnation_id == route.expected_incarnation_id
            }) && event_address == address => {
                let mut terminal_text = frame.contents.clone();
                for row in &frame.scrollback_formatted {
                    terminal_text.push_str(&String::from_utf8_lossy(row));
                }
                let folded = terminal_text.to_ascii_lowercase();
                if expected
                    .iter()
                    .all(|text| folded.contains(&text.to_ascii_lowercase()))
                    && !frame.formatted.is_empty()
                {
                    return;
                }
                last_observation = terminal_text
                    .chars()
                    .map(|character| if character.is_control() { ' ' } else { character })
                    .take(512)
                    .collect();
            }
            Some(C2NodeEvent::TerminalFrame { .. }) => {
                last_observation =
                    "terminal frame had another node, incarnation, or address".to_owned();
            }
            Some(C2NodeEvent::ResyncRequired { .. }) if observed.as_ref().is_some_and(|event| {
                event.node_id == route.node_id
                    && event.cursor.incarnation_id == route.expected_incarnation_id
            }) => panic!("C2 terminal stream required resync before managed worktree proof"),
            _ => {}
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || timeout(remaining, events.changed()).await.is_err() {
            panic!(
                "expected cmd.exe managed-worktree bytes did not reach the routed C2 terminal stream; last observation: {last_observation:?}"
            );
        }
        if events.has_changed().is_err() {
            panic!("authenticated C2 event stream closed before terminal proof");
        }
    }
}

async fn wait_stopped(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(control, route).await;
            if find_session(&current, address).is_some_and(|session| {
                matches!(
                    session.status,
                    C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
                )
            }) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cmd.exe raw PTY did not stop");
}

async fn wait_released(
    control: &C2ControlHandle,
    route: &NodeRoute,
    lease: &ManagedWorktreeLeaseSnapshot,
) -> ManagedWorktreeLeaseSnapshot {
    timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(control, route).await;
            if session_count(&current) == 0 {
                let current_lease = find_lease(&current, lease)
                    .expect("managed lease disappeared during release");
                if current_lease.active_session_count == 0
                    && current_lease.managed_record_count == 0
                    && current_lease.state == ManagedWorktreeLeaseState::Retained
                {
                    return current_lease.clone();
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("managed worktree did not reach released Retained state")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_managed_worktree_is_durable_path_free_and_safely_cleaned_end_to_end() {
    require_headless_windows_fixture();
    let test_root = fixture_root();
    std::fs::create_dir_all(&test_root).unwrap();
    let _cleanup = RemoveFixtureDirectory(test_root.clone());
    let repository = test_root.join("repository");
    let allocation_root = test_root.join("managed");
    let state_path = test_root.join("node-state.json");
    std::fs::create_dir_all(&allocation_root).unwrap();
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&repository)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr),
    );
    std::fs::write(repository.join("README.md"), b"managed worktree fixture\n").unwrap();
    assert_git_success(&repository, &["add", "--", "README.md"]);
    assert_git_success(
        &repository,
        &[
            "-c",
            "user.name=Gate4Agent Fixture",
            "-c",
            "user.email=fixture@gate4agent.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=NUL",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );

    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("managed-worktree-fixture-node").unwrap();
    let source_workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "managed-worktree-fixture-node-token";
    let c2_token = "managed-worktree-fixture-c2-token";
    let launcher = exact_cmd_launcher();
    let server = NodeServer::new_exact_launcher_fixture(
        node_config(
            &node_endpoint,
            node_token,
            &node_id,
            &source_workspace_id,
            &repository,
            &allocation_root,
            &state_path,
        ),
        agent("claude"),
        launcher.clone(),
    )
    .unwrap();
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
        vec![C2NodeConfig::new(
            node_id.clone(),
            node_endpoint.clone(),
            node_token,
        )
        .unwrap()],
    )
    .unwrap()
    .with_control_endpoint(control_endpoint.clone())
    .unwrap()
    .with_timings(timings);
    let running = C2Running::start(config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let mut route = wait_online_after(&http, &node_id, None).await;
    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    let capabilities = &control.hello().compatibility.as_ref().unwrap().capabilities;
    assert!(capabilities
        .iter()
        .any(|capability| capability.as_str() == C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY));
    assert!(capabilities
        .iter()
        .any(|capability| capability.as_str() == C2_TERMINAL_FRAME_EVENTS_CAPABILITY));
    let (terminal_events_tx, mut terminal_events) = watch::channel(None::<RoutedNodeEvent>);
    let event_collector = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(
                &event.event,
                C2NodeEvent::TerminalFrame { .. } | C2NodeEvent::ResyncRequired { .. }
            ) {
                terminal_events_tx.send_replace(Some(event));
            }
        }
    });
    wait_relay_ready(&control, &route).await;
    let initial = snapshot(&control, &route).await;
    assert_eq!(session_count(&initial), 0);
    assert!(initial.session_records.is_empty());
    assert!(initial.managed_worktrees.is_empty());

    let managed_request = spawn_request(&node_id, &source_workspace_id);
    assert_request_is_path_free(&managed_request);
    let accepted = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree {
                request: managed_request.clone(),
            },
        )
        .await
        .unwrap();
    let receipt = match accepted.response {
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => receipt,
        response => panic!("managed worktree spawn failed: {response:?}"),
    };
    assert_eq!(receipt.lease.source_workspace_id, source_workspace_id);
    assert_eq!(receipt.lease.profile_id.as_str(), "review");
    assert_eq!(receipt.lease.profile_revision.as_str(), "fixture-v1");
    assert_eq!(receipt.lease.retention, ManagedWorktreeRetention::Retain);
    assert_eq!(receipt.lease.state, ManagedWorktreeLeaseState::InUse);
    assert_eq!(receipt.lease.active_session_count, 1);
    assert_eq!(receipt.lease.managed_record_count, 0);
    assert_eq!(receipt.spawn.session.workspace_id, receipt.lease.workspace_id);
    wait_running(&control, &route, &receipt.spawn.session).await;
    let active = snapshot(&control, &route).await;
    assert_eq!(session_count(&active), 1);
    assert_eq!(active.session_records.len(), 1);
    let live_record = &active.session_records[0];
    let record_id = live_record.record_id.clone();
    let record_name = live_record.display_name.clone();
    assert_raw_inventory_record(
        live_record,
        Some(&receipt.spawn.session),
        &receipt.lease.workspace_id,
        ManagedSessionState::Live,
        &record_name,
    );
    assert_eq!(
        find_lease(&active, &receipt.lease)
            .expect("active managed lease disappeared")
            .managed_record_count,
        0,
    );

    let listed = listed_worktrees(&repository);
    assert_eq!(listed.len(), 2);
    let canonical_repository = std::fs::canonicalize(&repository).unwrap();
    let managed = listed
        .iter()
        .find(|worktree| std::fs::canonicalize(&worktree.path).unwrap() != canonical_repository)
        .expect("managed worktree was absent from authoritative Git list");
    let target = std::fs::canonicalize(&managed.path).unwrap();
    let branch = format!("codex/managed/{}", receipt.lease.lease_id.as_str());
    assert_eq!(managed.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(
        target.file_name().and_then(|name| name.to_str()),
        Some(receipt.lease.lease_id.as_str()),
    );
    assert_public_lease_is_path_free(&receipt.lease, &target);
    let marker = "WORKTREE_ONLY_F5_7B84D2";
    let marker_path = target.join("worktree-only.txt");
    std::fs::write(&marker_path, format!("{marker}\n")).unwrap();
    assert!(!repository.join("worktree-only.txt").exists());

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: receipt.spawn.session.clone(),
                    text: "cd & type worktree-only.txt".to_owned(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::TerminalControl {
                    session: receipt.spawn.session.clone(),
                    control: TerminalControl::Enter,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    let target_text = target.to_string_lossy();
    let displayed_target = target_text.strip_prefix(r"\\?\").unwrap_or(&target_text);
    wait_terminal_proof(
        &mut terminal_events,
        &route,
        &receipt.spawn.session,
        &[displayed_target, marker],
    )
    .await;

    let replay = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree {
                request: managed_request,
            },
        )
        .await
        .unwrap();
    match replay.response {
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted {
            receipt: replayed_receipt,
        }) => assert_eq!(replayed_receipt, receipt),
        response => panic!("managed worktree idempotent replay failed: {response:?}"),
    }
    let after_replay = snapshot(&control, &route).await;
    assert_eq!(session_count(&after_replay), 1);
    assert_eq!(after_replay.session_records.len(), 1);
    assert_raw_inventory_record(
        find_record(&after_replay, &record_id)
            .expect("idempotent replay replaced the raw inventory record"),
        Some(&receipt.spawn.session),
        &receipt.lease.workspace_id,
        ManagedSessionState::Live,
        &record_name,
    );
    assert_eq!(after_replay.managed_worktrees.len(), 1);

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Stop {
                    session: receipt.spawn.session.clone(),
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    wait_stopped(&control, &route, &receipt.spawn.session).await;
    let after_stop = snapshot(&control, &route).await;
    let stopped_lease = find_lease(&after_stop, &receipt.lease)
        .expect("Stop alone removed the managed lease");
    assert_eq!(stopped_lease.active_session_count, 1);
    assert_eq!(stopped_lease.managed_record_count, 0);
    assert_eq!(after_stop.session_records.len(), 1);
    assert_raw_inventory_record(
        find_record(&after_stop, &record_id)
            .expect("stopped raw inventory record disappeared"),
        None,
        &receipt.lease.workspace_id,
        ManagedSessionState::Unavailable,
        &record_name,
    );
    assert!(target.is_dir(), "Stop alone removed the physical worktree");
    assert_eq!(listed_worktrees(&repository).len(), 2);

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Remove {
                    session: receipt.spawn.session.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    let released = wait_released(&control, &route, &receipt.lease).await;
    assert!(target.is_dir(), "Retain policy removed the released worktree");
    let after_remove = snapshot(&control, &route).await;
    assert_eq!(session_count(&after_remove), 0);
    assert_eq!(after_remove.session_records.len(), 1);
    assert_raw_inventory_record(
        find_record(&after_remove, &record_id)
            .expect("Remove discarded the durable raw inventory record"),
        None,
        &receipt.lease.workspace_id,
        ManagedSessionState::Unavailable,
        &record_name,
    );

    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .expect("node shutdown timed out")
        .expect("node task panicked")
        .expect("node shutdown failed");
    wait_offline(&http, &node_id).await;
    let restarted = NodeServer::new_exact_launcher_fixture(
        node_config(
            &node_endpoint,
            node_token,
            &node_id,
            &source_workspace_id,
            &repository,
            &allocation_root,
            &state_path,
        ),
        agent("claude"),
        launcher,
    )
    .unwrap();
    let restarted_shutdown = restarted.shutdown_handle();
    let restarted_task = tokio::spawn(restarted.run());
    let previous_route = route.clone();
    route = wait_online_after(&http, &node_id, Some(&previous_route)).await;
    wait_relay_ready(&control, &route).await;
    let recovered = snapshot(&control, &route).await;
    let recovered_lease = find_lease(&recovered, &released)
        .expect("durable managed lease was absent after Node restart");
    assert_eq!(recovered_lease.state, ManagedWorktreeLeaseState::Retained);
    assert_eq!(recovered_lease.active_session_count, 0);
    assert_eq!(recovered_lease.managed_record_count, 0);
    assert_eq!(recovered.session_records.len(), 1);
    assert_raw_inventory_record(
        find_record(&recovered, &record_id)
            .expect("durable raw inventory record was absent after Node restart"),
        None,
        &receipt.lease.workspace_id,
        ManagedSessionState::Unavailable,
        &record_name,
    );
    assert!(target.is_dir());
    assert_eq!(listed_worktrees(&repository).len(), 2);

    let dirty = control
        .request(
            route.clone(),
            NodeRequest::CleanupManagedWorktree {
                lease_id: receipt.lease.lease_id.clone(),
            },
        )
        .await
        .expect("typed dirty-worktree refusal closed the C2 connection");
    match dirty.response {
        Err(failure) => assert_eq!(failure.code, NodeFailureCode::WorktreeDirty),
        response => panic!("dirty managed worktree was unexpectedly cleaned: {response:?}"),
    }
    let blocked = snapshot(&control, &route).await;
    let blocked_lease = find_lease(&blocked, &receipt.lease)
        .expect("dirty cleanup removed the managed lease");
    assert_eq!(blocked_lease.state, ManagedWorktreeLeaseState::CleanupBlocked);
    assert_eq!(
        blocked_lease.cleanup_failure,
        Some(ManagedWorktreeCleanupFailure::Dirty),
    );
    assert!(target.is_dir());

    std::fs::remove_file(&marker_path).unwrap();
    let cleaned = control
        .request(
            route.clone(),
            NodeRequest::CleanupManagedWorktree {
                lease_id: receipt.lease.lease_id.clone(),
            },
        )
        .await
        .unwrap();
    match cleaned.response {
        Ok(C2NodeResponse::ManagedWorktreeCleanup { lease }) => {
            assert_eq!(lease.lease_id, receipt.lease.lease_id);
            assert_eq!(lease.state, ManagedWorktreeLeaseState::Removed);
            assert_eq!(lease.active_session_count, 0);
            assert_eq!(lease.managed_record_count, 0);
        }
        response => panic!("managed worktree cleanup failed: {response:?}"),
    }
    assert!(!target.exists());
    assert_git_success(
        &repository,
        &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    );
    let final_worktrees = listed_worktrees(&repository);
    assert_eq!(final_worktrees.len(), 1);
    assert_eq!(
        std::fs::canonicalize(&final_worktrees[0].path).unwrap(),
        canonical_repository,
    );
    assert_eq!(final_worktrees[0].branch.as_deref(), Some("main"));
    let final_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&final_snapshot), 0);
    assert_eq!(final_snapshot.session_records.len(), 1);
    assert_raw_inventory_record(
        find_record(&final_snapshot, &record_id)
            .expect("worktree cleanup discarded the durable raw inventory record"),
        None,
        &receipt.lease.workspace_id,
        ManagedSessionState::Unavailable,
        &record_name,
    );
    assert!(final_snapshot.managed_worktrees.is_empty());
    assert_eq!(final_snapshot.workspaces.len(), 1);
    assert_eq!(final_snapshot.workspaces[0].workspace_id, source_workspace_id);

    let c2_shutdown = running.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    drop(control);
    timeout(Duration::from_secs(2), event_collector)
        .await
        .expect("C2 event stream did not close")
        .expect("C2 event collector failed");
    restarted_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), restarted_task)
        .await
        .expect("restarted node shutdown timed out")
        .expect("restarted node task panicked")
        .expect("restarted node shutdown failed");
}
