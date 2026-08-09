#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus, NodeId, NodeRoute,
    NodeTransportState, RoutedNodeEvent, C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
    C2_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY, C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{
    CapabilityId, ManagedWorktreeLeaseId, ManagedWorktreeLeaseSnapshot,
    ManagedWorktreeLeaseState, ManagedWorktreeRetention, ManagedWorktreeSpawnRequest,
    NodeFailureCode, NodeRequest, SessionAddress, SessionMode, SpawnBundleDigest, SpawnBundleId,
    SpawnBundleRevision, SpawnDeadlineMs, SpawnFieldProvenance, SpawnIdempotencyKey,
    SpawnOverride, SpawnOverrides, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorktreeProfileId,
    WorktreeProfileRevision, WorkspaceId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{
    ManagedWorktreeProfile, NodeBundle, NodeBundleError, NodeSecretReference,
    NodeSecretResolveError, NodeSecretResolver, NodeSecretValue, NodeServer, NodeServerConfig,
    SpawnProfileRegistry, WorkspaceConfig, WorktreeServiceMode,
};
use gate4agent_types::{AgentId, TerminalControl, TerminalFrame, TerminalSize};
use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const BUNDLE_ID: &str = "review-tools";
const BUNDLE_REVISION: &str = "review-tools-r1";
const BUNDLE_DIGEST: &str =
    "sha256:941f20868a6ef49b4329bf1bb1368515763f5b64d8279b05cd9700966083d707";
const ROOT_MANIFEST: &[u8] = br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#;
const CLAUDE_MANIFEST: &[u8] =
    br#"{"name":"review-tools","version":"1.0.0","description":"Review helpers"}"#;
const SKILL: &[u8] = b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n";
const CHANGED_SKILL: &[u8] = b"---\nname: review-code\ndescription: Changed after the digest was pinned.\n---\n";

struct UnusedSecretResolver;

impl NodeSecretResolver for UnusedSecretResolver {
    fn resolve(
        &self,
        _: &NodeSecretReference,
    ) -> Result<NodeSecretValue, NodeSecretResolveError> {
        Err(NodeSecretResolveError::Unavailable)
    }
}

fn require_headless_windows_fixture() {
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(OsStr::new("1")),
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
        r"\\.\pipe\gate4agent-f61-bundle-{label}-{}-{nonce}-{}",
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
        "fixture Git command failed: {}",
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
            .is_some_and(|name| name.starts_with("gate4agent-f61-bundle-e2e-"));
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
        "gate4agent-f61-bundle-e2e-{}-{nonce}",
        std::process::id(),
    ))
}

fn write_bundle_fixture(root: &Path) {
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(root.join("skills/review-code")).unwrap();
    std::fs::write(root.join("plugin.json"), ROOT_MANIFEST).unwrap();
    std::fs::write(root.join(".claude-plugin/plugin.json"), CLAUDE_MANIFEST).unwrap();
    std::fs::write(root.join("skills/review-code/SKILL.md"), SKILL).unwrap();
}

fn protect_bundle_tree(root: &Path) {
    fn collect(path: &Path, paths: &mut Vec<PathBuf>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            for entry in std::fs::read_dir(path).unwrap() {
                collect(&entry.unwrap().path(), paths);
            }
        }
        paths.push(path.to_path_buf());
    }

    let identity = Command::new("whoami").output().unwrap();
    assert!(identity.status.success(), "whoami failed for bundle ACL fixture");
    let principal = String::from_utf8(identity.stdout).unwrap().trim().to_owned();
    assert!(!principal.is_empty(), "whoami returned an empty bundle owner");
    let mut paths = Vec::new();
    collect(root, &mut paths);
    for path in paths {
        let output = Command::new("icacls")
            .arg(&path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{principal}:(F)"))
            .arg("/q")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "owner-only bundle ACL failed for {path:?}: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn pinned_bundle(root: &Path) -> Result<NodeBundle, NodeBundleError> {
    NodeBundle::new(
        SpawnBundleId::new(BUNDLE_ID).unwrap(),
        SpawnBundleRevision::new(BUNDLE_REVISION).unwrap(),
        SpawnBundleDigest::new(BUNDLE_DIGEST).unwrap(),
        root,
    )
}

fn node_config(
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    repository: &Path,
    allocation_root: &Path,
    state_path: &Path,
    materialization_root: &Path,
    spawn_profile_id: &SpawnProfileId,
) -> NodeServerConfig {
    let managed_profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("review").unwrap(),
        WorktreeProfileRevision::new("fixture-v1").unwrap(),
        allocation_root,
        "codex/bundle",
        "HEAD",
        ManagedWorktreeRetention::RemoveWhenReleased,
    )
    .unwrap();
    let workspace = WorkspaceConfig::new(workspace_id.clone(), repository)
        .unwrap()
        .with_worktree_service_mode(WorktreeServiceMode::Managed)
        .with_managed_worktree_profile(managed_profile)
        .unwrap();
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: spawn_profile_id.clone(),
        revision: SpawnProfileRevision::new("bundle-r1").unwrap(),
        provider: agent("claude"),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize {
            rows: 40,
            columns: 160,
        },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }])
    .unwrap();
    NodeServerConfig::new(endpoint, token, node_id.clone(), [workspace])
        .unwrap()
        .with_state_path(state_path)
        .unwrap()
        .with_spawn_profiles(profiles)
        .with_session_environment_materialization(
            materialization_root,
            Arc::new(UnusedSecretResolver),
        )
        .unwrap()
}

fn spawn_request(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    spawn_profile_id: &SpawnProfileId,
    bundle_id: &str,
    idempotency_key: &str,
) -> ManagedWorktreeSpawnRequest {
    ManagedWorktreeSpawnRequest {
        worktree_profile_id: WorktreeProfileId::new("review").unwrap(),
        spawn_spec: SpawnSpec {
            target: SpawnTarget {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                worktree_id: None,
            },
            profile_id: spawn_profile_id.clone(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Inherit,
                mode: SpawnOverride::Inherit,
                terminal_size: SpawnOverride::Inherit,
                prompt: SpawnOverride::Inherit,
                bundle_id: SpawnOverride::Set {
                    value: SpawnBundleId::new(bundle_id).unwrap(),
                },
                context_id: SpawnOverride::Inherit,
                environment_profile_id: SpawnOverride::Inherit,
            },
            deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new(idempotency_key).unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new([CapabilityId::new(
                SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
            )
            .unwrap()])
            .unwrap(),
        },
    }
}

fn assert_redacted<T: Serialize>(value: &T, forbidden: &[&str], context: &str) {
    let encoded = serde_json::to_string(value).unwrap();
    let decoded_backslashes = encoded.replace("\\\\", "\\");
    assert!(
        forbidden.iter().all(|value| {
            !encoded.contains(value) && !decoded_backslashes.contains(value)
        }),
        "{context} exposed private bundle material",
    );
}

fn assert_state_v7_bundle_correlation(state_path: &Path, expected_materializations: usize) {
    let encoded = std::fs::read_to_string(state_path).expect("durable state is unavailable");
    assert!(!encoded.contains(std::str::from_utf8(SKILL).unwrap()));
    assert!(!encoded.contains(std::str::from_utf8(ROOT_MANIFEST).unwrap()));
    let state: Value = serde_json::from_str(&encoded).expect("durable state is invalid JSON");
    assert_eq!(state["version"], 7);
    let materializations = state["materializations"]
        .as_array()
        .expect("V7 state omitted its materialization registry");
    assert_eq!(materializations.len(), expected_materializations);
    for materialization in materializations {
        assert_eq!(materialization["environment_profile"], Value::Null);
        assert_eq!(
            materialization["bundle"],
            json!({
                "id": BUNDLE_ID,
                "revision": BUNDLE_REVISION,
                "digest": BUNDLE_DIGEST,
            }),
        );
        assert_eq!(materialization["owner"]["kind"], "session");
    }
}

fn materialization_directories(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    std::fs::read_dir(root)
        .expect("materialization root is unavailable")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

fn git_worktree_count(repository: &Path) -> usize {
    let output = git_command(repository, &["worktree", "list", "--porcelain"]);
    assert!(output.status.success(), "Git worktree listing failed");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
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

async fn wait_running(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            let current = snapshot(control, route).await;
            let session = current
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.sessions)
                .find(|session| {
                    session.instance_id == address.session.instance_id
                        && session.generation == address.session.generation
                })
                .expect("bundle-bound session is missing from the C2 snapshot");
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

async fn wait_stopped(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(control, route).await;
            if current
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.sessions)
                .find(|session| {
                    session.instance_id == address.session.instance_id
                        && session.generation == address.session.generation
                })
                .is_some_and(|session| {
                    matches!(
                        session.status,
                        C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
                    )
                })
            {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cmd.exe raw PTY did not stop through C2");
}

async fn send_terminal_line(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    text: &str,
) {
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: address.clone(),
                    text: text.to_owned(),
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
                    session: address.clone(),
                    control: TerminalControl::Enter,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
}

async fn wait_terminal_line(
    events: &mut mpsc::UnboundedReceiver<RoutedNodeEvent>,
    route: &NodeRoute,
    address: &SessionAddress,
    expected: &str,
) {
    fn contains_line(frame: &TerminalFrame, expected: &str) -> bool {
        !frame.formatted.is_empty()
            && frame.contents.lines().map(str::trim).any(|line| line == expected)
    }

    timeout(Duration::from_secs(15), async {
        loop {
            let event = events.recv().await.expect("authenticated C2 event stream closed");
            if event.node_id != route.node_id
                || event.cursor.incarnation_id != route.expected_incarnation_id
            {
                continue;
            }
            match event.event {
                C2NodeEvent::TerminalFrame {
                    address: event_address,
                    frame,
                } if event_address == *address && contains_line(&frame, expected) => return,
                C2NodeEvent::ResyncRequired { .. } => {
                    panic!("C2 terminal stream required resync before liveness proof")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("exact {expected} line did not reach a routed TerminalFrame"));
}

async fn wait_managed_cleanup_events(
    events: &mut mpsc::UnboundedReceiver<RoutedNodeEvent>,
    route: &NodeRoute,
    lease_id: &ManagedWorktreeLeaseId,
) {
    timeout(Duration::from_secs(15), async {
        let mut released_sequence = None;
        loop {
            let event = events.recv().await.expect("authenticated C2 event stream closed");
            if event.node_id != route.node_id
                || event.cursor.incarnation_id != route.expected_incarnation_id
            {
                continue;
            }
            match event.event {
                C2NodeEvent::ManagedWorktreeUpserted { lease }
                    if lease.lease_id == *lease_id
                        && lease.state == ManagedWorktreeLeaseState::Ready
                        && lease.active_session_count == 0
                        && lease.managed_record_count == 0 =>
                {
                    released_sequence = Some(event.cursor.sequence);
                }
                C2NodeEvent::ManagedWorktreeRemoved { lease_id: removed }
                    if removed == *lease_id =>
                {
                    assert!(
                        released_sequence.is_some_and(|sequence| sequence < event.cursor.sequence),
                        "managed worktree removal preceded its holder-release event",
                    );
                    return;
                }
                C2NodeEvent::ResyncRequired { .. } => {
                    panic!("C2 event stream required resync before managed cleanup proof")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("managed worktree cleanup events were not routed through C2");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_managed_materialized_bundle_is_immutable_private_and_cleaned_end_to_end() {
    require_headless_windows_fixture();
    let test_root = fixture_root();
    std::fs::create_dir_all(&test_root).unwrap();
    let _cleanup = RemoveFixtureDirectory(test_root.clone());
    let repository = test_root.join("repository");
    let allocation_root = test_root.join("managed");
    let materialization_root = test_root.join("private-session-bundles");
    let bundle_source = test_root.join("bundle-source");
    let state_path = test_root.join("node-state.json");
    std::fs::create_dir_all(&allocation_root).unwrap();
    write_bundle_fixture(&bundle_source);
    protect_bundle_tree(&bundle_source);
    let bundle = pinned_bundle(&bundle_source).expect("pinned bundle did not validate");
    assert_eq!(bundle.receipt().digest.as_str(), BUNDLE_DIGEST);

    std::fs::write(
        bundle_source.join("skills/review-code/SKILL.md"),
        CHANGED_SKILL,
    )
    .unwrap();
    let changed = pinned_bundle(&bundle_source).unwrap_err();
    assert!(matches!(changed, NodeBundleError::DigestMismatch { .. }));

    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&repository)
        .output()
        .unwrap();
    assert!(init.status.success(), "fixture Git repository initialization failed");
    std::fs::write(repository.join("README.md"), b"materialized bundle fixture\n").unwrap();
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
    let node_id = NodeId::new("materialized-bundle-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "materialized-bundle-node-token";
    let c2_token = "materialized-bundle-c2-token";
    let spawn_profile_id = SpawnProfileId::new("materialized-bundle").unwrap();
    let config = node_config(
        &node_endpoint,
        node_token,
        &node_id,
        &workspace_id,
        &repository,
        &allocation_root,
        &state_path,
        &materialization_root,
        &spawn_profile_id,
    );
    let mut server = NodeServer::new_exact_launcher_fixture(
        config,
        agent("claude"),
        exact_cmd_launcher(),
    )
    .unwrap();
    server.install_bundle(bundle).unwrap();
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
    let c2_config = C2Config::new(
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
    .with_timings(timings);
    let running = C2Running::start(c2_config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;
    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    let capabilities = &control.hello().compatibility.as_ref().unwrap().capabilities;
    for required in [
        C2_SESSION_BUNDLE_MATERIALIZATION_CAPABILITY,
        C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY,
        C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
    ] {
        assert!(capabilities.iter().any(|capability| capability.as_str() == required));
    }
    let (event_tx, mut collected_events) = mpsc::unbounded_channel();
    let event_collector = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if event_tx.send(event).is_err() {
                return;
            }
        }
    });

    let unknown_request = spawn_request(
        &node_id,
        &workspace_id,
        &spawn_profile_id,
        "unknown-bundle",
        "unknown-bundle-once",
    );
    let unknown = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree {
                request: unknown_request,
            },
        )
        .await
        .expect("typed unknown-bundle failure closed the C2 connection");
    match unknown.response {
        Err(failure) => assert_eq!(failure.code, NodeFailureCode::UnknownBundle),
        response => panic!("unknown bundle unexpectedly spawned: {response:?}"),
    }
    let after_unknown = snapshot(&control, &route).await;
    assert_eq!(session_count(&after_unknown), 0);
    assert!(after_unknown.managed_worktrees.is_empty());
    assert!(materialization_directories(&materialization_root).is_empty());
    assert_eq!(git_worktree_count(&repository), 1);

    let request = spawn_request(
        &node_id,
        &workspace_id,
        &spawn_profile_id,
        BUNDLE_ID,
        "materialized-bundle-once",
    );
    let accepted = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree {
                request: request.clone(),
            },
        )
        .await
        .unwrap();
    let receipt = match accepted.response {
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt }) => receipt,
        response => panic!("managed materialized bundle spawn failed: {response:?}"),
    };
    assert_eq!(receipt.spawn.bundle_id.as_ref().unwrap().as_str(), BUNDLE_ID);
    assert_eq!(
        receipt.spawn.bundle,
        Some(gate4agent_node::protocol::ResolvedBundleReceipt {
            id: SpawnBundleId::new(BUNDLE_ID).unwrap(),
            revision: SpawnBundleRevision::new(BUNDLE_REVISION).unwrap(),
            digest: SpawnBundleDigest::new(BUNDLE_DIGEST).unwrap(),
        }),
    );
    assert_eq!(
        receipt.spawn.provenance.bundle_id,
        SpawnFieldProvenance::Override,
    );
    assert_eq!(receipt.lease.retention, ManagedWorktreeRetention::RemoveWhenReleased);
    wait_running(&control, &route, &receipt.spawn.session).await;

    let managed_worktrees = materialization_directories(&allocation_root);
    assert_eq!(managed_worktrees.len(), 1);
    let managed_worktree = std::fs::canonicalize(&managed_worktrees[0]).unwrap();
    assert_eq!(git_worktree_count(&repository), 2);
    let materializations = materialization_directories(&materialization_root);
    assert_eq!(materializations.len(), 1);
    let materialized = materializations[0].clone();
    let private_bundle = materialized.join("bundle");
    assert_eq!(std::fs::read(private_bundle.join("plugin.json")).unwrap(), ROOT_MANIFEST);
    assert_eq!(
        std::fs::read(private_bundle.join(".claude-plugin/plugin.json")).unwrap(),
        CLAUDE_MANIFEST,
    );
    assert_eq!(
        std::fs::read(private_bundle.join("skills/review-code/SKILL.md")).unwrap(),
        SKILL,
    );
    assert_state_v7_bundle_correlation(&state_path, 1);

    let source_text = bundle_source.to_string_lossy().into_owned();
    let private_text = materialized.to_string_lossy().into_owned();
    let skill_text = std::str::from_utf8(SKILL).unwrap();
    let forbidden = [source_text.as_str(), private_text.as_str(), skill_text];
    assert_redacted(&request, &forbidden, "managed spawn request");
    assert_redacted(&receipt, &forbidden, "managed spawn receipt");
    let active_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&active_snapshot), 1);
    assert_redacted(&active_snapshot, &forbidden, "public C2 snapshot");
    assert_redacted(&http.status().await.unwrap(), &forbidden, "public C2 status");

    send_terminal_line(
        &control,
        &route,
        &receipt.spawn.session,
        "echo F61_RUNTIME_READY",
    )
    .await;
    wait_terminal_line(
        &mut collected_events,
        &route,
        &receipt.spawn.session,
        "F61_RUNTIME_READY",
    )
    .await;

    let replay = control
        .request(
            route.clone(),
            NodeRequest::SpawnManagedWorktree { request },
        )
        .await
        .unwrap();
    match replay.response {
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted { receipt: replayed }) => {
            assert_eq!(replayed, receipt)
        }
        response => panic!("managed bundle idempotent replay failed: {response:?}"),
    }
    assert_eq!(session_count(&snapshot(&control, &route).await), 1);
    assert_eq!(materialization_directories(&materialization_root), vec![materialized.clone()]);

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
    assert!(private_bundle.is_dir(), "Stop removed the session-owned bundle");
    assert!(managed_worktree.is_dir(), "Stop removed the managed worktree");
    let stopped_snapshot = snapshot(&control, &route).await;
    let stopped_lease = find_lease(&stopped_snapshot, &receipt.lease)
        .expect("Stop removed the managed worktree lease");
    assert_eq!(stopped_lease.active_session_count, 1);
    assert_state_v7_bundle_correlation(&state_path, 1);

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
    wait_managed_cleanup_events(
        &mut collected_events,
        &route,
        &receipt.lease.lease_id,
    )
    .await;
    assert!(!materialized.exists(), "Remove retained session-owned bundle bytes");
    assert!(!managed_worktree.exists(), "Remove retained the managed worktree");
    assert_eq!(git_worktree_count(&repository), 1);
    assert_state_v7_bundle_correlation(&state_path, 0);
    let final_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&final_snapshot), 0);
    assert!(final_snapshot.managed_worktrees.is_empty());
    assert_redacted(&final_snapshot, &forbidden, "final public C2 snapshot");

    let c2_shutdown = running.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    drop(control);
    drop(collected_events);
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
