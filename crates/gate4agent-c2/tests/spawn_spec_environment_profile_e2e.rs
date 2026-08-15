#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2ManagedSessionRecord, C2NodeEvent, C2NodeResponse, C2NodeSnapshot, C2SessionStatus,
    NodeId, NodeRoute, NodeTransportState, RoutedNodeEvent,
    C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY, C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_catalog::EnvMutation;
use gate4agent_node::protocol::{
    CapabilityId, ManagedSessionState, NodeRequest, SessionAddress, SessionMode,
    SpawnDeadlineMs, SpawnEnvironmentProfileId, SpawnEnvironmentProfileRevision,
    SpawnFieldProvenance, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides,
    SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, SpawnRequiredCapabilities,
    SpawnSpec, SpawnTarget, WorkspaceId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{
    NodeEnvironmentProfile, NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig,
};
use gate4agent_runtime_native::{
    NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver, NativeLaunchProfile,
    NativeLaunchProfileId,
};
use gate4agent_types::{AgentId, TerminalControl, TerminalFrame, TerminalSize, TransportKind};
use serde_json::json;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

const SENTINEL_KEY: &str = "GATE4AGENT_TEST_PROFILE_SENTINEL";
const SENTINEL_VALUE: &str = "profile-child-only-7c39a1";

struct SentinelResolver;

impl NativeChildEnvironmentResolver for SentinelResolver {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
        Ok(vec![EnvMutation {
            key: OsString::from(SENTINEL_KEY),
            value: Some(OsString::from(SENTINEL_VALUE)),
        }])
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
        r"\\.\pipe\gate4agent-environment-profile-{label}-{}-{nonce}-{}",
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

async fn snapshot(
    control: &C2ControlHandle,
    route: &NodeRoute,
    context: &str,
) -> C2NodeSnapshot {
    let response = control
        .request(route.clone(), NodeRequest::Snapshot)
        .await
        .unwrap_or_else(|error| panic!("C2 snapshot route failed during {context}: {error:?}"));
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

fn assert_raw_inventory_record(
    record: &C2ManagedSessionRecord,
    session: Option<&SessionAddress>,
    workspace_id: &WorkspaceId,
    expected_state: ManagedSessionState,
) {
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
            let current = snapshot(control, route, "running wait").await;
            let session = current
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.sessions)
                .find(|session| {
                    session.instance_id == address.session.instance_id
                        && session.generation == address.session.generation
                })
                .expect("spawned environment-profile session is missing from the C2 snapshot");
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

async fn wait_stopped(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(control, route, "stopped wait").await;
            let session = current
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.sessions)
                .find(|session| {
                    session.instance_id == address.session.instance_id
                        && session.generation == address.session.generation
                })
                .expect("stopped environment-profile session disappeared before Remove");
            if matches!(
                session.status,
                C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
            ) {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cmd.exe raw PTY did not stop through C2");
}

async fn wait_sentinel_terminal_frame(
    events: &mut watch::Receiver<Option<RoutedNodeEvent>>,
    route: &NodeRoute,
    address: &SessionAddress,
) -> TerminalFrame {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let observed = events.borrow_and_update().clone();
        match observed {
            Some(RoutedNodeEvent {
                node_id,
                cursor,
                event:
                    C2NodeEvent::TerminalFrame {
                        address: event_address,
                        frame,
                    },
            }) if node_id == route.node_id
                && cursor.incarnation_id == route.expected_incarnation_id
                && event_address == *address
                && frame.contents.contains(SENTINEL_VALUE) => return frame,
            Some(RoutedNodeEvent {
                node_id,
                cursor,
                event: C2NodeEvent::ResyncRequired { .. },
            }) if node_id == route.node_id
                && cursor.incarnation_id == route.expected_incarnation_id =>
            {
                panic!("C2 terminal stream required resync before sentinel proof")
            }
            _ => {}
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || timeout(remaining, events.changed()).await.is_err() {
            panic!("profile sentinel did not reach a routed C2 TerminalFrame");
        }
        if events.has_changed().is_err() {
            panic!("authenticated C2 event stream closed before terminal proof");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_fixture_spawn_spec_environment_profile_runs_exact_pty_end_to_end() {
    require_headless_windows_fixture();
    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("environment-profile-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "environment-profile-fixture-node-token";
    let c2_token = "environment-profile-fixture-c2-token";
    let spawn_profile_id = SpawnProfileId::new("environment-profile").unwrap();
    let spawn_profile_revision =
        SpawnProfileRevision::new("environment-profile-r1").unwrap();
    let environment_profile_id =
        SpawnEnvironmentProfileId::new("sentinel-environment").unwrap();
    let environment_profile_revision =
        SpawnEnvironmentProfileRevision::new("sentinel-environment-r1").unwrap();
    let terminal_size = TerminalSize {
        rows: 30,
        columns: 120,
    };

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: spawn_profile_id.clone(),
        revision: spawn_profile_revision.clone(),
        provider: agent("claude"),
        mode: SessionMode::Pty,
        terminal_size,
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: Some(environment_profile_id.clone()),
    }])
    .unwrap();
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
    .with_spawn_profiles(profiles);
    let mut server = NodeServer::new_exact_launcher_fixture(
        node_config,
        agent("claude"),
        exact_cmd_launcher(),
    )
    .unwrap();
    let native_profile = NativeLaunchProfile::new(
        NativeLaunchProfileId::new("sentinel-environment-pty").unwrap(),
        agent("claude"),
        TransportKind::Pty,
        vec![OsString::from(SENTINEL_KEY)],
        Arc::new(SentinelResolver),
    )
    .unwrap();
    server
        .install_environment_profile(
            NodeEnvironmentProfile::new(
                environment_profile_id.clone(),
                environment_profile_revision.clone(),
                agent("claude"),
                [native_profile],
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
    let config = C2Config::new(
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
    let running = C2Running::start(config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;
    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    let negotiated = &control.hello().compatibility.as_ref().unwrap().capabilities;
    assert!(negotiated.iter().any(|capability| {
        capability.as_str() == C2_CHILD_ENVIRONMENT_PROFILE_CAPABILITY
    }));
    assert!(negotiated.iter().any(|capability| {
        capability.as_str() == C2_TERMINAL_FRAME_EVENTS_CAPABILITY
    }));
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
    let initial = snapshot(&control, &route, "initial health check").await;
    assert_eq!(session_count(&initial), 0);
    assert!(initial.session_records.is_empty());

    let required_capabilities = SpawnRequiredCapabilities::new([CapabilityId::new(
        SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
    )
    .unwrap()])
    .unwrap();
    let spec = SpawnSpec {
        target: SpawnTarget {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            worktree_id: None,
        },
        profile_id: spawn_profile_id,
        expected_profile_revision: spawn_profile_revision,
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
        idempotency_key: SpawnIdempotencyKey::new("environment-profile-once").unwrap(),
        required_capabilities,
    };
    let first = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec { spec: spec.clone() },
        )
        .await
        .unwrap();
    let receipt = match first.response {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => receipt,
        response => panic!("environment-profile SpawnSpec failed: {response:?}"),
    };
    let environment_receipt = receipt
        .environment_profile
        .as_ref()
        .expect("SpawnSpec receipt omitted the selected environment profile");
    assert_eq!(
        serde_json::to_value(environment_receipt).unwrap(),
        json!({
            "profile_id": environment_profile_id,
            "profile_revision": environment_profile_revision,
        }),
    );
    assert!(!serde_json::to_string(&receipt)
        .unwrap()
        .contains(SENTINEL_VALUE));
    assert_eq!(receipt.provider, agent("claude"));
    assert_eq!(receipt.mode, SessionMode::Pty);
    assert_eq!(receipt.terminal_size, terminal_size);
    assert_eq!(
        receipt.provenance.environment_profile_id,
        SpawnFieldProvenance::Profile,
    );
    wait_running(&control, &route, &receipt.session).await;

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: receipt.session.clone(),
                    text: format!("echo %{SENTINEL_KEY}%"),
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
    let sentinel_frame =
        wait_sentinel_terminal_frame(&mut terminal_events, &route, &receipt.session).await;
    assert!(!sentinel_frame.formatted.is_empty());
    assert!(sentinel_frame.contents.contains(SENTINEL_VALUE));
    let live = snapshot(&control, &route, "raw inventory check").await;
    assert_eq!(live.session_records.len(), 1);
    let live_record = &live.session_records[0];
    assert_raw_inventory_record(
        live_record,
        Some(&receipt.session),
        &workspace_id,
        ManagedSessionState::Live,
    );
    let record_id = live_record.record_id.clone();
    let display_name = live_record.display_name.clone();

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
        response => panic!("environment-profile idempotent replay failed: {response:?}"),
    }
    let after_replay = snapshot(&control, &route, "idempotent replay check").await;
    assert_eq!(session_count(&after_replay), 1);
    assert_eq!(after_replay.session_records.len(), 1);
    assert_eq!(after_replay.session_records[0].record_id, record_id);
    assert_eq!(after_replay.session_records[0].display_name, display_name);
    assert_raw_inventory_record(
        &after_replay.session_records[0],
        Some(&receipt.session),
        &workspace_id,
        ManagedSessionState::Live,
    );

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
    let stopped = snapshot(&control, &route, "post-stop inventory check").await;
    assert_eq!(stopped.session_records.len(), 1);
    assert_eq!(stopped.session_records[0].record_id, record_id);
    assert_eq!(stopped.session_records[0].display_name, display_name);
    assert_raw_inventory_record(
        &stopped.session_records[0],
        None,
        &workspace_id,
        ManagedSessionState::Unavailable,
    );
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Remove {
                    session: receipt.session,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    let healthy = snapshot(&control, &route, "post-remove health check").await;
    assert_eq!(session_count(&healthy), 0);
    assert_eq!(healthy.session_records.len(), 1);
    assert_eq!(healthy.session_records[0].record_id, record_id);
    assert_eq!(healthy.session_records[0].display_name, display_name);
    assert_raw_inventory_record(
        &healthy.session_records[0],
        None,
        &workspace_id,
        ManagedSessionState::Unavailable,
    );
    assert_eq!(
        http.status().await.unwrap().nodes[&node_id].transport,
        NodeTransportState::Online,
    );

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
        .expect("C2 event drain failed");
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .expect("node shutdown timed out")
        .expect("node task panicked")
        .expect("node shutdown failed");
}
