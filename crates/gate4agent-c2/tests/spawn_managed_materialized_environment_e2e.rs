#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus, NodeId, NodeRoute,
    NodeTransportState, RoutedNodeEvent, C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
    C2_MANAGED_WORKTREE_LIFECYCLE_CAPABILITY, C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_catalog::EnvMutation;
use gate4agent_node::protocol::{
    CapabilityId, ManagedWorktreeLeaseId, ManagedWorktreeLeaseSnapshot,
    ManagedWorktreeLeaseState, ManagedWorktreeRetention, ManagedWorktreeSpawnRequest,
    NodeRequest, SessionAddress, SessionMode, SpawnDeadlineMs, SpawnEnvironmentProfileId,
    SpawnEnvironmentProfileRevision, SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverride,
    SpawnOverrides, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorktreeProfileId,
    WorktreeProfileRevision, WorkspaceId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{
    ManagedWorktreeProfile, NodeEnvironmentProfile, NodeSecretReference,
    NodeSecretResolveError, NodeSecretResolver, NodeSecretValue,
    NodeSessionEnvironmentMutation, NodeSessionFile, NodeSessionMaterializationProfile,
    NodeSessionPathBinding, NodeSessionPathClass, NodeServer, NodeServerConfig,
    SpawnProfileRegistry, WorkspaceConfig, WorktreeServiceMode,
};
use gate4agent_runtime_native::{
    NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver, NativeLaunchProfile,
    NativeLaunchProfileId,
};
use gate4agent_types::{AgentId, TerminalControl, TerminalFrame, TerminalSize, TransportKind};
use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const STATIC_KEY: &str = "GATE4AGENT_F55_STATIC_SENTINEL";
const STATIC_VALUE: &str = "static-child-only-f55";
const MATERIALIZED_KEY: &str = "GATE4AGENT_F55_MATERIALIZED_SENTINEL";
const MATERIALIZED_VALUE: &str = "materialized-child-only-f55";
const PROVIDER_HOME_KEY: &str = "GATE4AGENT_F55_PROVIDER_HOME";
const SECRET_REFERENCE: &str = "fixture-secret-ref-f55";
const SECRET_VALUE: &str = "fixture-secret-value-f55";
const GENERATED_FILE: &str = "profile.ready";
const SECRET_FILE: &str = "credentials/secret.bin";

struct StaticEnvironmentResolver;

impl NativeChildEnvironmentResolver for StaticEnvironmentResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(vec![EnvMutation {
            key: OsString::from(STATIC_KEY),
            value: Some(OsString::from(STATIC_VALUE)),
        }])
    }
}

struct FixtureSecretResolver;

impl NodeSecretResolver for FixtureSecretResolver {
    fn resolve(
        &self,
        reference: &NodeSecretReference,
    ) -> Result<NodeSecretValue, NodeSecretResolveError> {
        if reference.as_str() != SECRET_REFERENCE {
            return Err(NodeSecretResolveError::Unavailable);
        }
        NodeSecretValue::text(SECRET_VALUE).map_err(|_| NodeSecretResolveError::Unavailable)
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
        r"\\.\pipe\gate4agent-f55-materialized-{label}-{}-{nonce}-{}",
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
            .is_some_and(|name| name.starts_with("gate4agent-f55-materialized-e2e-"));
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
        "gate4agent-f55-materialized-e2e-{}-{nonce}",
        std::process::id(),
    ))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn materialization_profile() -> NodeSessionMaterializationProfile {
    NodeSessionMaterializationProfile::new(
        vec![NodeSessionEnvironmentMutation::SetNonSecret {
            key: MATERIALIZED_KEY.to_owned(),
            value: MATERIALIZED_VALUE.to_owned(),
        }],
        vec![NodeSessionPathBinding::new(
            PROVIDER_HOME_KEY,
            NodeSessionPathClass::ProviderHome,
        )
        .unwrap()],
        vec![
            NodeSessionFile::generated(
                NodeSessionPathClass::ProviderHome,
                GENERATED_FILE,
                b"ready\n".to_vec(),
            )
            .unwrap(),
            NodeSessionFile::secret(
                NodeSessionPathClass::ProviderHome,
                SECRET_FILE,
                NodeSecretReference::new(SECRET_REFERENCE).unwrap(),
            )
            .unwrap(),
        ],
    )
    .unwrap()
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
    environment_profile_id: &SpawnEnvironmentProfileId,
) -> NodeServerConfig {
    let managed_profile = ManagedWorktreeProfile::new(
        WorktreeProfileId::new("review").unwrap(),
        WorktreeProfileRevision::new("fixture-v1").unwrap(),
        allocation_root,
        "codex/materialized",
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
        revision: SpawnProfileRevision::new("materialized-r1").unwrap(),
        provider: agent("claude"),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize {
            rows: 40,
            columns: 160,
        },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: Some(environment_profile_id.clone()),
    }])
    .unwrap();
    NodeServerConfig::new(endpoint, token, node_id.clone(), [workspace])
        .unwrap()
        .with_state_path(state_path)
        .unwrap()
        .with_spawn_profiles(profiles)
        .with_session_environment_materialization(
            materialization_root,
            Arc::new(FixtureSecretResolver),
        )
        .unwrap()
}

fn spawn_request(
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    spawn_profile_id: &SpawnProfileId,
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
                bundle_id: SpawnOverride::Inherit,
                context_id: SpawnOverride::Inherit,
                environment_profile_id: SpawnOverride::Inherit,
            },
            deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("materialized-managed-once").unwrap(),
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
        "{context} exposed private session-environment material",
    );
}

fn assert_state_v6_redaction(
    state_path: &Path,
    environment_profile_id: &SpawnEnvironmentProfileId,
    environment_profile_revision: &SpawnEnvironmentProfileRevision,
    expected_materializations: usize,
) {
    let encoded = std::fs::read_to_string(state_path).expect("durable state is unavailable");
    assert!(!encoded.contains(SECRET_REFERENCE), "V6 state exposed a secret reference");
    assert!(!encoded.contains(SECRET_VALUE), "V6 state exposed a secret value");
    let state: Value = serde_json::from_str(&encoded).expect("durable state is invalid JSON");
    assert_eq!(state["version"], 6);
    let materializations = state["materializations"]
        .as_array()
        .expect("V6 state omitted its materialization registry");
    assert_eq!(materializations.len(), expected_materializations);
    for materialization in materializations {
        assert_eq!(
            materialization["environment_profile"],
            json!({
                "profile_id": environment_profile_id,
                "profile_revision": environment_profile_revision,
            }),
        );
        assert_eq!(materialization["owner"]["kind"], "session");
    }
}

fn only_materialization_directory(root: &Path) -> PathBuf {
    let directories = std::fs::read_dir(root)
        .expect("materialization root is unavailable")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(directories.len(), 1, "unexpected materialization directory count");
    directories.into_iter().next().unwrap()
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
                .expect("materialized session is missing from the C2 snapshot");
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
    text: String,
) {
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: address.clone(),
                    text,
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

async fn wait_exact_terminal_line(
    control: &C2ControlHandle,
    events: &mut mpsc::UnboundedReceiver<RoutedNodeEvent>,
    route: &NodeRoute,
    address: &SessionAddress,
    after_sequence: u64,
    expected: &str,
    rejected: Option<&str>,
) -> u64 {
    fn exact_line(
        frame: &TerminalFrame,
        after_sequence: u64,
        expected: &str,
        rejected: Option<&str>,
    ) -> Option<u64> {
        if frame.sequence <= after_sequence {
            return None;
        }
        let lines = frame.contents.lines().map(str::trim).collect::<Vec<_>>();
        if rejected.is_some_and(|rejected| lines.contains(&rejected)) {
            panic!("materialized child emitted the bounded failure marker");
        }
        (!frame.formatted.is_empty() && lines.contains(&expected)).then_some(frame.sequence)
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
                } if event_address == *address => {
                    if let Some(sequence) = exact_line(
                        &frame,
                        after_sequence,
                        expected,
                        rejected,
                    ) {
                        return sequence;
                    }
                }
                C2NodeEvent::ResyncRequired { .. } => {
                    loop {
                        let current = snapshot(control, route).await;
                        let frame = current
                            .workspaces
                            .iter()
                            .flat_map(|workspace| &workspace.sessions)
                            .find(|session| {
                                session.instance_id == address.session.instance_id
                                    && session.generation == address.session.generation
                            })
                            .and_then(|session| session.terminal_frame.as_ref());
                        if let Some(sequence) = frame.and_then(|frame| {
                            exact_line(frame, after_sequence, expected, rejected)
                        }) {
                            return sequence;
                        }
                        sleep(Duration::from_millis(20)).await;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("exact {expected} child output line did not reach a routed C2 TerminalFrame")
    })
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
                C2NodeEvent::ManagedWorktreeRemoved {
                    lease_id: removed,
                } if removed == *lease_id => {
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
async fn spawn_managed_materialized_environment_is_private_and_cleaned_end_to_end() {
    require_headless_windows_fixture();
    let test_root = fixture_root();
    std::fs::create_dir_all(&test_root).unwrap();
    let _cleanup = RemoveFixtureDirectory(test_root.clone());
    let repository = test_root.join("repository");
    let allocation_root = test_root.join("managed");
    let materialization_root = test_root.join("private-session-environments");
    let state_path = test_root.join("node-state.json");
    std::fs::create_dir_all(&allocation_root).unwrap();
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(&repository)
        .output()
        .unwrap();
    assert!(init.status.success(), "fixture Git repository initialization failed");
    std::fs::write(repository.join("README.md"), b"materialized environment fixture\n").unwrap();
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
    let node_id = NodeId::new("materialized-managed-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "materialized-managed-node-token";
    let c2_token = "materialized-managed-c2-token";
    let spawn_profile_id = SpawnProfileId::new("materialized-managed").unwrap();
    let environment_profile_id =
        SpawnEnvironmentProfileId::new("materialized-local-profile").unwrap();
    let environment_profile_revision =
        SpawnEnvironmentProfileRevision::new("materialized-local-r1").unwrap();
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
        &environment_profile_id,
    );
    let mut server = NodeServer::new_exact_launcher_fixture(
        config,
        agent("claude"),
        exact_cmd_launcher(),
    )
    .unwrap();
    let native_profile = NativeLaunchProfile::new(
        NativeLaunchProfileId::new("materialized-local-pty").unwrap(),
        agent("claude"),
        TransportKind::Pty,
        vec![OsString::from(STATIC_KEY)],
        Arc::new(StaticEnvironmentResolver),
    )
    .unwrap();
    server
        .install_environment_profile(
            NodeEnvironmentProfile::new_with_materialization(
                environment_profile_id.clone(),
                environment_profile_revision.clone(),
                agent("claude"),
                [native_profile],
                Some(materialization_profile()),
            )
            .unwrap(),
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
        C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY,
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

    let request = spawn_request(&node_id, &workspace_id, &spawn_profile_id);
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
        response => panic!("managed materialized spawn failed: {response:?}"),
    };
    let environment_receipt = receipt
        .spawn
        .environment_profile
        .as_ref()
        .expect("spawn receipt omitted the inherited environment profile");
    assert_eq!(
        serde_json::to_value(environment_receipt).unwrap(),
        json!({
            "profile_id": environment_profile_id,
            "profile_revision": environment_profile_revision,
        }),
    );
    assert_eq!(
        receipt.spawn.provenance.environment_profile_id,
        SpawnFieldProvenance::Profile,
    );
    assert_eq!(receipt.lease.retention, ManagedWorktreeRetention::RemoveWhenReleased);
    wait_running(&control, &route, &receipt.spawn.session).await;

    let worktrees = git_command(&repository, &["worktree", "list", "--porcelain"]);
    assert!(worktrees.status.success(), "Git worktree listing failed");
    let worktree_text = String::from_utf8(worktrees.stdout).unwrap();
    let target = std::fs::read_dir(&allocation_root)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .expect("managed worktree allocation is absent")
        .path();
    let canonical_target = std::fs::canonicalize(&target).unwrap();
    assert!(worktree_text.contains(receipt.lease.lease_id.as_str()));
    let materialized = only_materialization_directory(&materialization_root);
    let provider_home = materialized.join("home");
    assert!(provider_home.is_dir(), "provider home was not materialized");
    assert!(provider_home.join(GENERATED_FILE).is_file(), "generated profile file is absent");
    let secret_bytes = std::fs::read(provider_home.join(SECRET_FILE))
        .expect("materialized secret file is unavailable");
    assert!(
        fnv1a64(&secret_bytes) == fnv1a64(SECRET_VALUE.as_bytes()),
        "materialized secret file digest mismatch",
    );
    assert_state_v6_redaction(
        &state_path,
        &environment_profile_id,
        &environment_profile_revision,
        1,
    );

    let private_root_text = materialization_root.to_string_lossy().into_owned();
    let private_home_text = provider_home.to_string_lossy().into_owned();
    let forbidden = [
        SECRET_REFERENCE,
        SECRET_VALUE,
        private_root_text.as_str(),
        private_home_text.as_str(),
    ];
    assert_redacted(&receipt, &forbidden, "managed spawn receipt");
    let active_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&active_snapshot), 1);
    assert!(active_snapshot.session_records.is_empty());
    assert_redacted(&active_snapshot, &forbidden, "public C2 snapshot");
    let public_status = http.status().await.unwrap();
    assert_redacted(&public_status, &forbidden, "public C2 status");

    let target_text = canonical_target.to_string_lossy();
    let displayed_target = target_text.strip_prefix(r"\\?\").unwrap_or(&target_text);
    send_terminal_line(
        &control,
        &route,
        &receipt.spawn.session,
        "@echo off".to_owned(),
    )
    .await;
    send_terminal_line(
        &control,
        &route,
        &receipt.spawn.session,
        "echo F55_BASELINE_READY".to_owned(),
    )
    .await;
    let baseline_sequence = wait_exact_terminal_line(
        &control,
        &mut collected_events,
        &route,
        &receipt.spawn.session,
        0,
        "F55_BASELINE_READY",
        None,
    )
    .await;
    for proof_command in [
        "set \"F55_RESULT=OK\"".to_owned(),
        format!(
            "if /I not \"%CD%\"==\"{displayed_target}\" set \"F55_RESULT=FAIL\""
        ),
        format!(
            "if not exist \"%{PROVIDER_HOME_KEY}%\\{GENERATED_FILE}\" set \"F55_RESULT=FAIL\""
        ),
        format!(
            "if not exist \"%{PROVIDER_HOME_KEY}%\\{SECRET_FILE}\" set \"F55_RESULT=FAIL\""
        ),
        format!(
            "if not \"%{MATERIALIZED_KEY}%\"==\"{MATERIALIZED_VALUE}\" set \"F55_RESULT=FAIL\""
        ),
        format!(
            "if not \"%{STATIC_KEY}%\"==\"{STATIC_VALUE}\" set \"F55_RESULT=FAIL\""
        ),
        "cmd /v:on /c \"echo F55_PROOF_!F55_RESULT!\"".to_owned(),
    ] {
        send_terminal_line(
            &control,
            &route,
            &receipt.spawn.session,
            proof_command,
        )
        .await;
    }
    wait_exact_terminal_line(
        &control,
        &mut collected_events,
        &route,
        &receipt.spawn.session,
        baseline_sequence,
        "F55_PROOF_OK",
        Some("F55_PROOF_FAIL"),
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
        Ok(C2NodeResponse::ManagedWorktreeSpawnAccepted {
            receipt: replayed,
        }) => assert_eq!(replayed, receipt),
        response => panic!("managed materialized idempotent replay failed: {response:?}"),
    }
    assert_eq!(session_count(&snapshot(&control, &route).await), 1);
    assert!(
        only_materialization_directory(&materialization_root) == materialized,
        "idempotent replay allocated another materialization",
    );

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
    assert!(provider_home.is_dir(), "Stop removed the session-owned provider home");
    assert!(canonical_target.is_dir(), "Stop removed the managed worktree");
    let stopped_snapshot = snapshot(&control, &route).await;
    let stopped_lease = find_lease(&stopped_snapshot, &receipt.lease)
        .expect("Stop removed the managed worktree lease");
    assert_eq!(stopped_lease.active_session_count, 1);
    assert_state_v6_redaction(
        &state_path,
        &environment_profile_id,
        &environment_profile_revision,
        1,
    );

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
    assert!(!materialized.exists(), "Remove retained session-owned materialization");
    assert!(!canonical_target.exists(), "Remove retained the managed worktree");
    assert_state_v6_redaction(
        &state_path,
        &environment_profile_id,
        &environment_profile_revision,
        0,
    );
    let final_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&final_snapshot), 0);
    assert!(final_snapshot.session_records.is_empty());
    assert!(final_snapshot.managed_worktrees.is_empty());
    assert_redacted(&final_snapshot, &forbidden, "final public C2 snapshot");
    assert_redacted(&http.status().await.unwrap(), &forbidden, "final public C2 status");

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
