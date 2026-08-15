#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus, NodeId, NodeRoute,
    NodeTransportState, RoutedNodeEvent, C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
    C2_WORKTREE_SELECTION_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{
    CapabilityId, NodeFailureCode, NodeRequest, OpaqueHostPath, SessionAddress, SessionMode,
    SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides, SpawnProfileId,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceId,
    SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_types::{AgentId, TerminalControl, TerminalSize, TransportKind};
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
        r"\\.\pipe\gate4agent-manual-worktree-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn host_path(value: impl Into<String>) -> OpaqueHostPath {
    OpaqueHostPath::utf8(value.into()).unwrap()
}

fn idempotency_key(value: &str) -> SpawnIdempotencyKey {
    SpawnIdempotencyKey::new(value).unwrap()
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

struct RemoveFixtureDirectory(PathBuf);

impl Drop for RemoveFixtureDirectory {
    fn drop(&mut self) {
        let safe = self
            .0
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-manual-worktree-e2e-"));
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
        "gate4agent-manual-worktree-e2e-{}-{nonce}",
        std::process::id(),
    ))
}

async fn wait_online(client: &C2Client, node_id: &NodeId) -> NodeRoute {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(status) = client.status().await {
                if status.nodes[node_id].transport == NodeTransportState::Online {
                    return NodeRoute {
                        node_id: node_id.clone(),
                        expected_incarnation_id: status.nodes[node_id]
                            .cursor
                            .expect("online fixture node has no cursor")
                            .incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture node did not become online through C2")
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

async fn wait_running(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            let current = snapshot(control, route).await;
            let session = find_session(&current, address)
                .expect("spawned raw PTY session is missing from the C2 snapshot");
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
    expected_text: &str,
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
                if terminal_text
                    .to_ascii_lowercase()
                    .contains(&expected_text.to_ascii_lowercase())
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
                last_observation = "terminal frame had another node, incarnation, or address"
                    .to_owned();
            }
            Some(C2NodeEvent::ResyncRequired { .. }) if observed.as_ref().is_some_and(|event| {
                event.node_id == route.node_id
                    && event.cursor.incarnation_id == route.expected_incarnation_id
            }) => {
                panic!("C2 terminal stream required resync before worktree proof")
            }
            _ => {}
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero()
            || timeout(remaining, events.changed()).await.is_err()
        {
            panic!(
                "expected cmd.exe worktree bytes did not reach the routed C2 terminal stream; last observation: {last_observation:?}"
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
    .expect("cmd.exe raw PTY did not stop before cleanup");
}

fn spawn_spec(
    node_id: &NodeId,
    source_workspace_id: &WorkspaceId,
    worktree_id: WorkspaceId,
    key: &str,
) -> SpawnSpec {
    SpawnSpec {
        target: SpawnTarget {
            node_id: node_id.clone(),
            workspace_id: source_workspace_id.clone(),
            worktree_id: Some(worktree_id),
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
        idempotency_key: idempotency_key(key),
        required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
            SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
        )
        .unwrap()])
        .unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_spec_manual_worktree_selection_is_enforced_end_to_end() {
    require_headless_windows_fixture();
    let test_root = fixture_root();
    std::fs::create_dir_all(&test_root).unwrap();
    let _cleanup = RemoveFixtureDirectory(test_root.clone());
    let repository = test_root.join("repository");
    let ordinary = test_root.join("ordinary-workspace");
    std::fs::create_dir_all(&ordinary).unwrap();
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
    std::fs::write(repository.join("README.md"), b"manual worktree fixture\n").unwrap();
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
    let node_id = NodeId::new("manual-worktree-fixture-node").unwrap();
    let primary_id = WorkspaceId::new("primary").unwrap();
    let ordinary_id = WorkspaceId::new("ordinary").unwrap();
    let worktree_id = WorkspaceId::new("topic-one").unwrap();
    let node_token = "manual-worktree-fixture-node-token";
    let c2_token = "manual-worktree-fixture-c2-token";
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(primary_id.clone(), &repository).unwrap()],
    )
    .unwrap();
    let server = NodeServer::new_exact_launcher_fixture(
        node_config,
        agent("claude"),
        exact_cmd_launcher(),
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
    let route = wait_online(&http, &node_id).await;
    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    assert!(control
        .hello()
        .compatibility
        .as_ref()
        .unwrap()
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == C2_WORKTREE_SELECTION_CAPABILITY));
    assert!(control
        .hello()
        .compatibility
        .as_ref()
        .unwrap()
        .capabilities
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

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::RegisterWorkspace {
                    workspace_id: ordinary_id.clone(),
                    root: host_path(ordinary.to_string_lossy().into_owned()),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::WorkspaceRegistered { .. })
    ));

    let target = test_root.join("topic-one");
    let target_root = host_path(target.to_string_lossy().into_owned());
    let create = control
        .request(
            route.clone(),
            NodeRequest::CreateWorktree {
                source_workspace_id: primary_id.clone(),
                workspace_id: worktree_id.clone(),
                target_root: target_root.clone(),
                branch: "codex/manual-worktree".to_owned(),
                base: Some("HEAD".to_owned()),
            },
        )
        .await
        .unwrap();
    match create.response {
        Ok(C2NodeResponse::WorktreeCreated { workspace, .. }) => {
            assert_eq!(workspace.workspace_id, worktree_id);
        }
        response => panic!("CreateWorktree returned an unexpected response: {response:?}"),
    }
    let marker = "WORKTREE_ONLY_6D89F4";
    std::fs::write(target.join("worktree-only.txt"), format!("{marker}\n")).unwrap();
    assert_git_success(&target, &["add", "--", "worktree-only.txt"]);
    assert_git_success(
        &target,
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
            "worktree marker",
        ],
    );
    assert!(!repository.join("worktree-only.txt").exists());

    let invalid = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec {
                spec: spawn_spec(&node_id, &primary_id, ordinary_id.clone(), "ordinary-refused"),
            },
        )
        .await
        .expect("typed ordinary-workspace refusal closed the C2 connection");
    match invalid.response {
        Err(failure) => assert_eq!(failure.code, NodeFailureCode::WorktreeProtected),
        response => panic!("ordinary workspace was accepted as a worktree: {response:?}"),
    }
    assert_eq!(session_count(&snapshot(&control, &route).await), 0);

    let spec = spawn_spec(&node_id, &primary_id, worktree_id.clone(), "manual-worktree-once");
    let accepted = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec { spec: spec.clone() },
        )
        .await
        .unwrap();
    let receipt = match accepted.response {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => receipt,
        response => panic!("manual-worktree SpawnSpec failed: {response:?}"),
    };
    assert_eq!(receipt.target, spec.target);
    assert_eq!(receipt.session.workspace_id, worktree_id);
    assert_eq!(receipt.provider, agent("claude"));
    assert_eq!(receipt.mode, SessionMode::Pty);
    assert_eq!(session_count(&snapshot(&control, &route).await), 1);
    wait_running(&control, &route, &receipt.session).await;

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: receipt.session.clone(),
                    text: "cd".to_owned(),
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
                    session: receipt.session.clone(),
                    control: TerminalControl::Enter,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    let canonical_target = std::fs::canonicalize(&target).unwrap();
    let canonical_target = canonical_target.to_string_lossy();
    let displayed_target = canonical_target
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical_target)
        .to_owned();
    wait_terminal_proof(
        &mut terminal_events,
        &route,
        &receipt.session,
        &displayed_target,
    )
    .await;
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: receipt.session.clone(),
                    text: "type worktree-only.txt".to_owned(),
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
                    session: receipt.session.clone(),
                    control: TerminalControl::Enter,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    wait_terminal_proof(&mut terminal_events, &route, &receipt.session, marker).await;

    let replay = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec { spec: spec.clone() },
        )
        .await
        .unwrap();
    match replay.response {
        Ok(C2NodeResponse::SpawnSpecAccepted {
            receipt: replayed_receipt,
        }) => assert_eq!(replayed_receipt, receipt),
        response => panic!("manual-worktree idempotent replay failed: {response:?}"),
    }
    assert_eq!(session_count(&snapshot(&control, &route).await), 1);

    let busy = control
        .request(
            route.clone(),
            NodeRequest::RemoveWorktree {
                source_workspace_id: primary_id.clone(),
                target_root: target_root.clone(),
            },
        )
        .await
        .expect("typed busy-worktree refusal closed the C2 connection");
    match busy.response {
        Err(failure) => assert_eq!(failure.code, NodeFailureCode::WorkspaceBusy),
        response => panic!("busy worktree removal was unexpectedly accepted: {response:?}"),
    }

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Stop {
                    session: receipt.session.clone(),
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    wait_stopped(&control, &route, &receipt.session).await;
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Remove {
                    session: receipt.session.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    let removed = control
        .request(
            route.clone(),
            NodeRequest::RemoveWorktree {
                source_workspace_id: primary_id.clone(),
                target_root: target_root.clone(),
            },
        )
        .await
        .unwrap();
    match removed.response {
        Ok(C2NodeResponse::WorktreeRemoved { workspace_id, .. }) => {
            assert_eq!(workspace_id, Some(worktree_id.clone()));
        }
        response => panic!("clean worktree removal failed: {response:?}"),
    }
    assert!(!target.exists());
    assert_git_success(
        &repository,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/codex/manual-worktree",
        ],
    );
    let worktree_list = git_command(&repository, &["worktree", "list", "--porcelain"]);
    assert!(worktree_list.status.success());
    assert_eq!(
        String::from_utf8_lossy(&worktree_list.stdout)
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
    );
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::UnregisterWorkspace {
                    workspace_id: ordinary_id,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::WorkspaceUnregistered { .. })
    ));
    let final_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&final_snapshot), 0);
    assert_eq!(final_snapshot.workspaces.len(), 1);
    assert_eq!(final_snapshot.workspaces[0].workspace_id, primary_id);

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
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .expect("node shutdown timed out")
        .expect("node task panicked")
        .expect("node shutdown failed");
}
