#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeResponse, C2NodeSnapshot, C2SessionStatus, NodeId, NodeRoute,
    NodeTransportState, C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{
    CapabilityId, NodeFailureCode, NodeRequest, SessionAddress, SessionMode, SpawnBundleId,
    SpawnContextId, SpawnDeadlineMs, SpawnEnvironmentProfileId, SpawnFieldProvenance,
    SpawnIdempotencyKey, SpawnOverride, SpawnOverrides, SpawnProfileDefaults, SpawnProfileId,
    SpawnProfileRevision, SpawnPrompt, SpawnRequiredCapabilities, SpawnSpec, SpawnTarget,
    WorkspaceId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_types::{AgentId, TerminalSize, TransportKind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
        r"\\.\pipe\gate4agent-spawn-spec-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn agent(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

fn profile_id(value: &str) -> SpawnProfileId {
    SpawnProfileId::new(value).unwrap()
}

fn idempotency_key(value: &str) -> SpawnIdempotencyKey {
    SpawnIdempotencyKey::new(value).unwrap()
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

async fn assert_failure_without_session_mutation(
    control: &C2ControlHandle,
    route: &NodeRoute,
    spec: SpawnSpec,
    expected: NodeFailureCode,
    expected_session_count: usize,
) {
    let response = control
        .request(route.clone(), NodeRequest::SpawnSpec { spec })
        .await
        .expect("typed SpawnSpec failure closed the C2 connection");
    match response.response {
        Err(failure) => assert_eq!(failure.code, expected),
        response => panic!("SpawnSpec returned an unexpected response: {response:?}"),
    }
    let current = snapshot(control, route).await;
    assert_eq!(session_count(&current), expected_session_count);
}

async fn wait_raw_pty_session(
    control: &C2ControlHandle,
    route: &NodeRoute,
    address: &SessionAddress,
    expected_size: TerminalSize,
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
                .expect("spawned raw PTY session is missing from the C2 snapshot");
            assert_eq!(session.transport, TransportKind::Pty);
            if session.status == C2SessionStatus::Running
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| {
                        !frame.formatted.is_empty()
                            && frame.contents.contains("fixture-ready>")
                    })
            {
                assert_eq!(session.terminal_size, Some(expected_size));
                return;
            }
            assert!(
                !matches!(
                    session.status,
                    C2SessionStatus::Exited { .. } | C2SessionStatus::Failed
                ),
                "fixture raw PTY exited before publishing its terminal frame",
            );
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture raw PTY did not become healthy through C2");
}

async fn spawn_legacy_after_runtime_probe_settles(
    control: &C2ControlHandle,
    route: &NodeRoute,
    workspace_id: &WorkspaceId,
) -> SessionAddress {
    timeout(Duration::from_secs(10), async {
        loop {
            let response = control
                .request(
                    route.clone(),
                    NodeRequest::Spawn {
                        workspace_id: workspace_id.clone(),
                        provider: agent("claude"),
                        mode: SessionMode::Pty,
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                )
                .await
                .expect("legacy Spawn closed the healthy C2 connection");
            match response.response {
                Ok(C2NodeResponse::SpawnAccepted { session }) => return session,
                Err(failure) if failure.code == NodeFailureCode::BackendBusy => {
                    sleep(Duration::from_millis(20)).await;
                }
                response => {
                    panic!("legacy Spawn returned an unexpected response: {response:?}")
                }
            }
        }
    })
    .await
    .expect("legacy Spawn remained busy after the bounded deadline probe settled")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_spec_defaults_overrides_are_deterministic() {
    require_headless_windows_fixture();
    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("spawn-spec-fixture-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "spawn-spec-fixture-node-token";
    let c2_token = "spawn-spec-fixture-c2-token";
    let selected_profile = profile_id("review");
    let profile_revision = SpawnProfileRevision::new("review-r7").unwrap();

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: selected_profile.clone(),
        revision: profile_revision.clone(),
        provider: agent("claude"),
        mode: SessionMode::Inline,
        terminal_size: TerminalSize {
            rows: 20,
            columns: 70,
        },
        prompt: Some(SpawnPrompt::new("profile-prompt-must-be-cleared").unwrap()),
        bundle_id: Some(SpawnBundleId::new("profile-bundle").unwrap()),
        context_id: Some(SpawnContextId::new("profile-context").unwrap()),
        environment_profile_id: Some(
            SpawnEnvironmentProfileId::new("profile-environment").unwrap(),
        ),
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
    let server = NodeServer::new_fixture(node_config).unwrap();
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
        .any(|capability| {
            capability.as_str() == C2_SPAWN_SPEC_DEFAULTS_OVERRIDES_CAPABILITY
        }));
    let event_drain = tokio::spawn(async move {
        while events.recv().await.is_some() {}
    });

    let initial = snapshot(&control, &route).await;
    assert_eq!(session_count(&initial), 0);
    assert!(initial.session_records.is_empty());

    let resolved_size = TerminalSize {
        rows: 31,
        columns: 97,
    };
    let target = SpawnTarget {
        node_id: node_id.clone(),
        workspace_id: workspace_id.clone(),
        worktree_id: None,
    };
    let required_capabilities = SpawnRequiredCapabilities::new([CapabilityId::new(
        SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
    )
    .unwrap()])
    .unwrap();
    let accepted_spec = SpawnSpec {
        target: target.clone(),
        profile_id: selected_profile.clone(),
        overrides: SpawnOverrides {
            provider: SpawnOverride::Set {
                value: agent("claude"),
            },
            mode: SpawnOverride::Set {
                value: SessionMode::Pty,
            },
            terminal_size: SpawnOverride::Set {
                value: resolved_size,
            },
            prompt: SpawnOverride::Clear,
            bundle_id: SpawnOverride::Clear,
            context_id: SpawnOverride::Clear,
            environment_profile_id: SpawnOverride::Clear,
        },
        deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
        idempotency_key: idempotency_key("accepted-once"),
        required_capabilities: required_capabilities.clone(),
    };

    let first = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec {
                spec: accepted_spec.clone(),
            },
        )
        .await
        .unwrap();
    let receipt = match first.response {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => receipt,
        response => panic!("fixture SpawnSpec returned an unexpected response: {response:?}"),
    };
    assert_eq!(first.node_id, node_id);
    assert_eq!(first.incarnation_id, route.expected_incarnation_id);
    assert_eq!(receipt.incarnation_id, route.expected_incarnation_id);
    assert_eq!(receipt.target, target);
    assert_eq!(receipt.profile_id, selected_profile);
    assert_eq!(receipt.profile_revision, profile_revision);
    assert_eq!(receipt.provider, agent("claude"));
    assert_eq!(receipt.mode, SessionMode::Pty);
    assert_eq!(receipt.terminal_size, resolved_size);
    assert!(!receipt.prompt.present);
    assert_eq!(receipt.prompt.byte_len, 0);
    assert_eq!(receipt.bundle_id, None);
    assert_eq!(receipt.context_id, None);
    assert_eq!(receipt.environment_profile, None);
    assert_eq!(receipt.deadline_ms, accepted_spec.deadline_ms);
    assert_eq!(receipt.idempotency_key, accepted_spec.idempotency_key);
    assert_eq!(receipt.required_capabilities, required_capabilities);
    assert_eq!(receipt.provenance.provider, SpawnFieldProvenance::Override);
    assert_eq!(receipt.provenance.mode, SpawnFieldProvenance::Override);
    assert_eq!(
        receipt.provenance.terminal_size,
        SpawnFieldProvenance::Override,
    );
    assert_eq!(receipt.provenance.prompt, SpawnFieldProvenance::Cleared);
    assert_eq!(receipt.provenance.bundle_id, SpawnFieldProvenance::Cleared);
    assert_eq!(receipt.provenance.context_id, SpawnFieldProvenance::Cleared);
    assert_eq!(
        receipt.provenance.environment_profile_id,
        SpawnFieldProvenance::Cleared,
    );
    wait_raw_pty_session(&control, &route, &receipt.session, resolved_size).await;
    let after_first = snapshot(&control, &route).await;
    assert_eq!(session_count(&after_first), 1);
    assert!(after_first.session_records.is_empty());

    let replay = control
        .request(
            route.clone(),
            NodeRequest::SpawnSpec {
                spec: accepted_spec.clone(),
            },
        )
        .await
        .unwrap();
    match replay.response {
        Ok(C2NodeResponse::SpawnSpecAccepted {
            receipt: replayed_receipt,
        }) => assert_eq!(replayed_receipt, receipt),
        response => panic!("idempotent replay returned an unexpected response: {response:?}"),
    }
    assert_eq!(session_count(&snapshot(&control, &route).await), 1);

    let mut conflicting_spec = accepted_spec.clone();
    conflicting_spec.overrides.terminal_size = SpawnOverride::Set {
        value: TerminalSize {
            rows: 32,
            columns: 98,
        },
    };
    assert_failure_without_session_mutation(
        &control,
        &route,
        conflicting_spec,
        NodeFailureCode::SpawnIdempotencyConflict,
        1,
    )
    .await;

    let mut unknown_profile_spec = accepted_spec.clone();
    unknown_profile_spec.profile_id = profile_id("missing-profile");
    unknown_profile_spec.idempotency_key = idempotency_key("unknown-profile");
    assert_failure_without_session_mutation(
        &control,
        &route,
        unknown_profile_spec,
        NodeFailureCode::UnknownSpawnProfile,
        1,
    )
    .await;

    let mut unavailable_materializer_spec = accepted_spec.clone();
    unavailable_materializer_spec.overrides.bundle_id = SpawnOverride::Set {
        value: SpawnBundleId::new("unavailable-bundle").unwrap(),
    };
    unavailable_materializer_spec.idempotency_key = idempotency_key("materializer-unavailable");
    assert_failure_without_session_mutation(
        &control,
        &route,
        unavailable_materializer_spec,
        NodeFailureCode::UnsupportedSpawnCapability,
        1,
    )
    .await;

    let mut expired_spec = accepted_spec.clone();
    expired_spec.deadline_ms = SpawnDeadlineMs::new(1).unwrap();
    expired_spec.idempotency_key = idempotency_key("deadline-expired");
    assert_failure_without_session_mutation(
        &control,
        &route,
        expired_spec,
        NodeFailureCode::SpawnDeadlineExceeded,
        1,
    )
    .await;

    let healthy_after_failures = snapshot(&control, &route).await;
    assert_eq!(session_count(&healthy_after_failures), 1);
    assert!(healthy_after_failures.session_records.is_empty());

    let legacy_session =
        spawn_legacy_after_runtime_probe_settles(&control, &route, &workspace_id).await;
    wait_raw_pty_session(
        &control,
        &route,
        &legacy_session,
        TerminalSize {
            rows: 24,
            columns: 80,
        },
    )
    .await;
    let final_snapshot = snapshot(&control, &route).await;
    assert_eq!(session_count(&final_snapshot), 2);

    for session in [receipt.session, legacy_session] {
        assert!(matches!(
            control
                .request(
                    route.clone(),
                    NodeRequest::Stop {
                        session,
                        force: true,
                    },
                )
                .await
                .unwrap()
                .response,
            Ok(C2NodeResponse::Accepted)
        ));
    }

    let c2_shutdown = running.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    drop(control);
    timeout(Duration::from_secs(2), event_drain)
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
