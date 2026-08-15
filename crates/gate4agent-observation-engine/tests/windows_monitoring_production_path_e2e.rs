#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, NodeId, NodeRoute, NodeTransportState,
    C2_OBSERVATION_EVENTS_CAPABILITY, C2_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2EventReceiver};
use gate4agent_node::protocol::{
    ClientRole, NodeCursor, NodeEvent, NodeRequest, ServerFrame, SessionAddress, SessionMode,
    SessionRecordId, WorkspaceId, NODE_OBSERVATION_EVENTS_CAPABILITY,
    NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::NamedPipeNodeClient;
use gate4agent_observation_api::{
    ManagedSessionKey, ObservationIngressEnvelope, ObservationIngressPayload,
    ObservationResyncBatch, ObservationTarget, ObservationTransport, RuntimeSessionKey,
};
use gate4agent_observation_engine::{
    gap, ApplyOutcome, CorrelationState, ObservationEngine, UsageTotals,
};
use gate4agent_testkit::{
    MONITORING_PROMPT_CANARY, MONITORING_PROVIDER_SESSION_CANARY,
    MONITORING_TOOL_INPUT_CANARY, MONITORING_TOOL_OUTPUT_CANARY,
};
use gate4agent_types::{AgentId, TerminalSize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct DirectCapture {
    observations: Vec<(NodeCursor, SessionAddress, gate4agent_node::protocol::ObservationV1)>,
    managed_observations:
        Vec<(NodeCursor, SessionRecordId, gate4agent_node::protocol::ObservationV1)>,
    raw_controls: Vec<String>,
}

#[derive(Debug)]
struct C2Capture {
    observations: Vec<(NodeCursor, SessionAddress, gate4agent_node::protocol::ObservationV1)>,
    managed_observations:
        Vec<(NodeCursor, SessionRecordId, gate4agent_node::protocol::ObservationV1)>,
    projected_controls: Vec<String>,
}

fn endpoint(label: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-monitoring-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed),
    )
}

fn node_config(endpoint: &str, token: &str, node_id: &NodeId) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
}

fn timings() -> C2Timings {
    C2Timings {
        poll_interval: Duration::from_millis(20),
        fresh_for: Duration::from_secs(2),
        attempt_deadline: Duration::from_secs(2),
        transient_backoffs: [Duration::from_millis(20); 5],
        parked_backoff: Duration::from_millis(100),
        http_io_deadline: Duration::from_secs(1),
    }
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
                            .expect("online monitoring node has no cursor")
                            .incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("monitoring fixture node did not become online through C2")
}

fn has_capability(capabilities: &[gate4agent_node::protocol::CapabilityId], expected: &str) -> bool {
    capabilities
        .iter()
        .any(|capability| capability.as_str() == expected)
}

async fn capture_direct(
    client: &mut NamedPipeNodeClient,
    incarnation_id: gate4agent_node::protocol::NodeIncarnationId,
    expected_address: &SessionAddress,
) -> DirectCapture {
    let mut capture = DirectCapture {
        observations: Vec::new(),
        managed_observations: Vec::new(),
        raw_controls: Vec::new(),
    };
    let mut runtime_turn_completed = false;
    let mut managed_turn_completed = false;
    let result = timeout(Duration::from_secs(15), async {
        loop {
            match client.recv().await.expect("direct Node event stream failed") {
                ServerFrame::Event(envelope) => match envelope.event {
                    NodeEvent::Control { address, event } if address == *expected_address => {
                        capture
                            .raw_controls
                            .push(serde_json::to_string(&event).unwrap());
                    }
                    NodeEvent::Observation {
                        address,
                        observation,
                    } if address == *expected_address => {
                        runtime_turn_completed |= matches!(
                            observation.kind,
                            gate4agent_node::protocol::ObservationKindV1::TurnCompleted
                        );
                        capture.observations.push((
                            NodeCursor {
                                incarnation_id,
                                sequence: envelope.sequence,
                            },
                            address,
                            observation,
                        ));
                        if runtime_turn_completed && managed_turn_completed {
                            return;
                        }
                    }
                    NodeEvent::ManagedObservation { record_id, observation } => {
                        managed_turn_completed |= matches!(
                            observation.kind,
                            gate4agent_node::protocol::ObservationKindV1::TurnCompleted
                        );
                        capture.managed_observations.push((
                            NodeCursor {
                                incarnation_id,
                                sequence: envelope.sequence,
                            },
                            record_id,
                            observation,
                        ));
                        if runtime_turn_completed && managed_turn_completed {
                            return;
                        }
                    }
                    NodeEvent::ResyncRequired { .. } => {
                        panic!("direct Node monitoring stream required resync")
                    }
                    _ => {}
                },
                frame => panic!("direct Node sent an unexpected idle frame: {frame:?}"),
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "direct Node stream omitted the monitoring turn-completed boundary: {capture:#?}",
    );
    capture
}

async fn capture_c2(
    events: &mut C2EventReceiver,
    node_id: &NodeId,
    expected_address: &SessionAddress,
) -> C2Capture {
    let mut capture = C2Capture {
        observations: Vec::new(),
        managed_observations: Vec::new(),
        projected_controls: Vec::new(),
    };
    let mut runtime_turn_completed = false;
    let mut managed_turn_completed = false;
    let result = timeout(Duration::from_secs(15), async {
        loop {
            let routed = events
                .recv()
                .await
                .expect("authenticated C2 event stream closed early");
            if &routed.node_id != node_id {
                continue;
            }
            match routed.event {
                C2NodeEvent::Control { address, event } if address == *expected_address => {
                    capture
                        .projected_controls
                        .push(serde_json::to_string(&event).unwrap());
                }
                C2NodeEvent::Observation {
                    address,
                    observation,
                } if address == *expected_address => {
                    runtime_turn_completed |= matches!(
                        observation.kind,
                        gate4agent_node::protocol::ObservationKindV1::TurnCompleted
                    );
                    capture
                        .observations
                        .push((routed.cursor, address, observation));
                    if runtime_turn_completed && managed_turn_completed {
                        return;
                    }
                }
                C2NodeEvent::ManagedObservation { record_id, observation } => {
                    managed_turn_completed |= matches!(
                        observation.kind,
                        gate4agent_node::protocol::ObservationKindV1::TurnCompleted
                    );
                    capture
                        .managed_observations
                        .push((routed.cursor, record_id, observation));
                    if runtime_turn_completed && managed_turn_completed {
                        return;
                    }
                }
                C2NodeEvent::ResyncRequired { .. } => {
                    panic!("C2 monitoring stream required resync")
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "C2 stream omitted the routed monitoring turn-completed boundary: {capture:#?}",
    );
    capture
}

fn target(node_id: &NodeId, cursor: NodeCursor, address: &SessionAddress) -> ObservationTarget {
    ObservationTarget::Runtime {
        key: RuntimeSessionKey {
            node_id: node_id.clone(),
            incarnation_id: cursor.incarnation_id,
            workspace_id: address.workspace_id.clone(),
            instance_id: address.session.instance_id,
            generation: address.session.generation,
        },
    }
}

fn ingress(
    node_id: &NodeId,
    transport: ObservationTransport,
    cursor: NodeCursor,
    address: &SessionAddress,
    observation: &gate4agent_node::protocol::ObservationV1,
) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node_id.clone(),
        cursor,
        received_at_ms: observation.observed_at_unix_ms.unwrap_or(1),
        transport,
        payload: ObservationIngressPayload::Observation {
            address: target(node_id, cursor, address),
            observation: observation.clone(),
        },
    }
}

fn managed_target(
    node_id: &NodeId,
    cursor: NodeCursor,
    record_id: &SessionRecordId,
) -> ObservationTarget {
    ObservationTarget::Managed {
        key: ManagedSessionKey {
            node_id: node_id.clone(),
            incarnation_id: cursor.incarnation_id,
            record_id: record_id.clone(),
        },
    }
}

fn managed_ingress(
    node_id: &NodeId,
    transport: ObservationTransport,
    cursor: NodeCursor,
    record_id: &SessionRecordId,
    observation: &gate4agent_node::protocol::ObservationV1,
) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node_id.clone(),
        cursor,
        received_at_ms: observation.observed_at_unix_ms.unwrap_or(1),
        transport,
        payload: ObservationIngressPayload::Observation {
            address: managed_target(node_id, cursor, record_id),
            observation: observation.clone(),
        },
    }
}

fn apply_capture(
    node_id: &NodeId,
    transport: ObservationTransport,
    observations: &[(NodeCursor, SessionAddress, gate4agent_node::protocol::ObservationV1)],
) -> ObservationEngine {
    let mut engine = ObservationEngine::new();
    for (cursor, address, observation) in observations {
        let prepared = engine
            .prepare(ingress(node_id, transport, *cursor, address, observation))
            .expect("production observation must be accepted by the engine");
        assert_eq!(prepared.outcomes(), &[ApplyOutcome::Applied]);
        engine.accept(prepared);
    }
    engine
}

fn apply_managed_capture(
    node_id: &NodeId,
    transport: ObservationTransport,
    observations: &[(NodeCursor, SessionRecordId, gate4agent_node::protocol::ObservationV1)],
) -> ObservationEngine {
    let mut engine = ObservationEngine::new();
    for (cursor, record_id, observation) in observations {
        let prepared = engine
            .prepare(managed_ingress(
                node_id,
                transport,
                *cursor,
                record_id,
                observation,
            ))
            .expect("production managed observation must be accepted by the engine");
        assert_eq!(prepared.outcomes(), &[ApplyOutcome::Applied]);
        engine.accept(prepared);
    }
    engine
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_fixture_hook_monitoring_direct_c2_engine_privacy_e2e() {
    gate4agent_testkit::suppress_windows_fault_dialogs_for_test();
    gate4agent_testkit::require_windows_headless_supervisor_for_test();

    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("c2");
    let node_id = NodeId::new("monitoring-production-node").unwrap();
    let node_token = "monitoring-production-node-token";
    let c2_token = "monitoring-production-c2-token";

    let server = NodeServer::new_monitoring_hook_fixture(node_config(
        &node_endpoint,
        node_token,
        &node_id,
    ))
    .unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

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
    .with_timings(timings());
    let running = C2Running::start(config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;
    let (control, mut c2_events) = connect_local(&control_endpoint, c2_token).await.unwrap();

    let c2_compatibility = control
        .hello()
        .compatibility
        .as_ref()
        .expect("C2 monitoring path did not negotiate compatibility");
    assert!(has_capability(
        &c2_compatibility.capabilities,
        C2_OBSERVATION_EVENTS_CAPABILITY,
    ));
    assert!(has_capability(
        &c2_compatibility.capabilities,
        C2_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    ));
    assert!(has_capability(
        &c2_compatibility.capabilities,
        C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    ));

    let mut direct = NamedPipeNodeClient::connect(
        &node_endpoint,
        &node_id,
        ClientRole::Observer,
        node_token,
    )
    .await
    .unwrap();
    let direct_incarnation = direct.hello().incarnation_id;
    assert_eq!(direct_incarnation, route.expected_incarnation_id);
    let direct_compatibility = direct
        .hello()
        .compatibility
        .as_ref()
        .expect("direct monitoring path did not negotiate compatibility");
    assert!(has_capability(
        &direct_compatibility.capabilities,
        NODE_OBSERVATION_EVENTS_CAPABILITY,
    ));
    assert!(has_capability(
        &direct_compatibility.capabilities,
        NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    ));
    assert!(has_capability(
        &direct_compatibility.capabilities,
        NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    ));
    println!("checkpoint: direct+C2 negotiated base, managed-target, and workflow-detail observation capabilities");

    let spawned = control
        .request(
            route.clone(),
            NodeRequest::Spawn {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                provider: AgentId::new("claude").unwrap(),
                mode: SessionMode::Pty,
                terminal_size: TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                initial_prompt: None,
            },
        )
        .await
        .unwrap();
    let address = match spawned.response {
        Ok(C2NodeResponse::SpawnAccepted { session }) => session,
        response => panic!("monitoring fixture spawn returned an unexpected response: {response:?}"),
    };

    let (direct_capture, c2_capture) = tokio::join!(
        capture_direct(&mut direct, direct_incarnation, &address),
        capture_c2(&mut c2_events, &node_id, &address),
    );

    assert!(matches!(
        control
            .request(
                route,
                NodeRequest::Stop {
                    session: address.clone(),
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    drop(direct);
    let c2_shutdown = running.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    drop(control);
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .expect("Node shutdown timed out")
        .expect("Node task panicked")
        .expect("Node shutdown failed");

    assert_eq!(direct_capture.observations, c2_capture.observations);
    assert_eq!(
        direct_capture.managed_observations,
        c2_capture.managed_observations,
    );
    println!(
        "checkpoint: transport parity runtime={} managed={} raw_control={} c2_control={}",
        direct_capture.observations.len(),
        direct_capture.managed_observations.len(),
        direct_capture.raw_controls.len(),
        c2_capture.projected_controls.len(),
    );
    assert!(!direct_capture.managed_observations.is_empty());
    let managed_record_id = direct_capture.managed_observations[0].1.clone();
    assert!(direct_capture
        .managed_observations
        .iter()
        .all(|(_, record_id, _)| record_id == &managed_record_id));
    let mut parity_engine = ObservationEngine::new();
    for (cursor, event_address, observation) in &direct_capture.observations {
        let direct_ingress = ingress(
            &node_id,
            ObservationTransport::DirectNode,
            *cursor,
            event_address,
            observation,
        );
        let mut c2_ingress = ingress(
            &node_id,
            ObservationTransport::C2,
            *cursor,
            event_address,
            observation,
        );
        c2_ingress.received_at_ms = c2_ingress.received_at_ms.saturating_add(1_000);
        assert!(direct_ingress.canonical_eq(&c2_ingress));
        let direct_prepared = parity_engine.prepare(direct_ingress).unwrap();
        assert_eq!(direct_prepared.outcomes(), &[ApplyOutcome::Applied]);
        parity_engine.accept(direct_prepared);
        let c2_prepared = parity_engine.prepare(c2_ingress).unwrap();
        assert_eq!(c2_prepared.outcomes(), &[ApplyOutcome::Duplicate]);
        parity_engine.accept(c2_prepared);
    }
    let mut managed_parity_engine = ObservationEngine::new();
    for (cursor, record_id, observation) in &direct_capture.managed_observations {
        let direct_ingress = managed_ingress(
            &node_id,
            ObservationTransport::DirectNode,
            *cursor,
            record_id,
            observation,
        );
        let mut c2_ingress = managed_ingress(
            &node_id,
            ObservationTransport::C2,
            *cursor,
            record_id,
            observation,
        );
        c2_ingress.received_at_ms = c2_ingress.received_at_ms.saturating_add(1_000);
        assert!(direct_ingress.canonical_eq(&c2_ingress));
        let direct_prepared = managed_parity_engine.prepare(direct_ingress).unwrap();
        assert_eq!(direct_prepared.outcomes(), &[ApplyOutcome::Applied]);
        managed_parity_engine.accept(direct_prepared);
        let c2_prepared = managed_parity_engine.prepare(c2_ingress).unwrap();
        assert_eq!(c2_prepared.outcomes(), &[ApplyOutcome::Duplicate]);
        managed_parity_engine.accept(c2_prepared);
    }
    assert!(direct_capture.raw_controls.len() >= 5);
    let raw_control_json = direct_capture.raw_controls.join("\n");
    for canary in [
        MONITORING_PROMPT_CANARY,
        MONITORING_TOOL_INPUT_CANARY,
        MONITORING_TOOL_OUTPUT_CANARY,
        MONITORING_PROVIDER_SESSION_CANARY,
    ] {
        assert!(raw_control_json.contains(canary));
    }
    let projected_control_json = c2_capture.projected_controls.join("\n");
    let observation_json = serde_json::to_string(&(
        &direct_capture.observations,
        &direct_capture.managed_observations,
    )).unwrap();
    for canary in [
        MONITORING_PROMPT_CANARY,
        MONITORING_TOOL_INPUT_CANARY,
        MONITORING_TOOL_OUTPUT_CANARY,
        MONITORING_PROVIDER_SESSION_CANARY,
    ] {
        assert!(!projected_control_json.contains(canary));
        assert!(!observation_json.contains(canary));
    }
    println!("checkpoint: four privacy canaries confined to direct raw Control capture");

    use gate4agent_node::protocol::{
        ObservationEvidenceV1, ObservationKindV1, ObservationSourceFamilyV1,
    };
    let runtime_observations = direct_capture
        .observations
        .iter()
        .map(|(_, _, observation)| observation)
        .collect::<Vec<_>>();
    assert!(runtime_observations.iter().any(|observation| matches!(
        (&observation.evidence, &observation.kind),
        (
            ObservationEvidenceV1::NodeLifecycle,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::NodeLifecycle,
                source_adapter,
                capabilities,
            },
        ) if source_adapter == "node" && capabilities.owned_processes
    )));
    assert!(runtime_observations.iter().any(|observation| matches!(
        (&observation.evidence, &observation.kind),
        (ObservationEvidenceV1::NodeLifecycle, ObservationKindV1::SessionStarted)
    )));
    assert!(runtime_observations.iter().any(|observation| matches!(
        (&observation.evidence, &observation.kind),
        (ObservationEvidenceV1::NodeLifecycle, ObservationKindV1::OwnedProcessStarted { .. })
    )));

    let managed_hook_observations = direct_capture
        .managed_observations
        .iter()
        .map(|(_, _, observation)| observation)
        .filter(|observation| observation.evidence == ObservationEvidenceV1::ManagedHook)
        .collect::<Vec<_>>();
    assert!(managed_hook_observations.iter().any(|observation| matches!(
        &observation.kind,
        ObservationKindV1::SourceCapabilities {
            source_family: ObservationSourceFamilyV1::Hook,
            source_adapter,
            capabilities,
        } if source_adapter == "claude-code"
            && capabilities.tools
            && capabilities.attention
            && capabilities.subagents
            && !capabilities.usage
    )));
    for expected in ["session-started", "turn-started", "turn-completed"] {
        assert!(managed_hook_observations.iter().any(|observation| match expected {
            "session-started" => matches!(observation.kind, ObservationKindV1::SessionStarted),
            "turn-started" => matches!(observation.kind, ObservationKindV1::TurnStarted),
            "turn-completed" => matches!(observation.kind, ObservationKindV1::TurnCompleted),
            _ => unreachable!(),
        }), "missing managed Hook observation: {expected}");
    }
    assert!(managed_hook_observations
        .iter()
        .all(|observation| !matches!(observation.kind, ObservationKindV1::Usage { .. })));
    assert!(managed_hook_observations.iter().any(|observation| matches!(
        &observation.kind,
        ObservationKindV1::ToolStarted { class, .. } if class == "Shell"
    )));
    assert!(managed_hook_observations.iter().any(|observation| matches!(
        observation.kind,
        ObservationKindV1::ToolCompleted {
            success: true,
            duration_ms: Some(17),
            ..
        }
    )));

    let direct_engine = apply_capture(
        &node_id,
        ObservationTransport::DirectNode,
        &direct_capture.observations,
    );
    let c2_engine = apply_capture(
        &node_id,
        ObservationTransport::C2,
        &c2_capture.observations,
    );
    let direct_managed_engine = apply_managed_capture(
        &node_id,
        ObservationTransport::DirectNode,
        &direct_capture.managed_observations,
    );
    let c2_managed_engine = apply_managed_capture(
        &node_id,
        ObservationTransport::C2,
        &c2_capture.managed_observations,
    );
    let projection_target = target(
        &node_id,
        direct_capture.observations[0].0,
        &address,
    );
    let direct_projection = direct_engine
        .projection(&projection_target)
        .expect("direct engine projection");
    let c2_projection = c2_engine
        .projection(&projection_target)
        .expect("C2 engine projection");
    assert_eq!(direct_projection, c2_projection);
    assert_eq!(
        parity_engine
            .projection(&projection_target)
            .expect("deduplicated direct/C2 projection"),
        direct_projection,
    );
    assert_eq!(direct_projection.tools.len(), 1);
    assert_eq!(direct_projection.tools[0].class.as_deref(), Some("Shell"));
    assert_eq!(
        direct_projection.tools[0].state,
        CorrelationState::Completed {
            success: Some(true),
        }
    );
    assert_eq!(direct_projection.usage.observed_delta, UsageTotals::default());
    let runtime_timeline_facts = direct_capture
        .observations
        .iter()
        .filter(|(_, _, observation)| {
            !matches!(observation.kind, ObservationKindV1::SourceCapabilities { .. })
        })
        .count();
    assert_eq!(
        direct_projection.timeline.len(),
        runtime_timeline_facts,
        "timeline must contain observation facts only; raw Control and capability metadata are excluded",
    );
    assert!(direct_projection.source_capabilities.iter().any(|source| {
        source.source_family == ObservationSourceFamilyV1::NodeLifecycle
            && source.source_adapter == "node"
            && source.capabilities.owned_processes
    }));
    assert!(direct_projection
        .timeline
        .iter()
        .all(|entry| !matches!(entry.kind, ObservationKindV1::Usage { .. })));

    let managed_projection_target = managed_target(
        &node_id,
        direct_capture.managed_observations[0].0,
        &managed_record_id,
    );
    let direct_managed_projection = direct_managed_engine
        .projection(&managed_projection_target)
        .expect("direct managed engine projection");
    let c2_managed_projection = c2_managed_engine
        .projection(&managed_projection_target)
        .expect("C2 managed engine projection");
    assert_eq!(direct_managed_projection, c2_managed_projection);
    assert_eq!(
        managed_parity_engine
            .projection(&managed_projection_target)
            .expect("deduplicated direct/C2 managed projection"),
        direct_managed_projection,
    );
    assert_eq!(direct_managed_projection.tools.len(), 1);
    assert!(direct_managed_projection.source_capabilities.iter().any(|source| {
        source.source_family == ObservationSourceFamilyV1::Hook
            && source.source_adapter == "claude-code"
            && source.capabilities.tools
            && source.capabilities.attention
            && source.capabilities.subagents
            && !source.capabilities.usage
    }));
    assert!(direct_managed_projection
        .timeline
        .iter()
        .all(|entry| !matches!(entry.kind, ObservationKindV1::Usage { .. })));

    let (resync_cursor, resync_address, resync_observation) = direct_capture
        .observations
        .iter()
        .rev()
        .find(|(cursor, _, _)| cursor.sequence >= 3)
        .expect("real transport did not provide a resync-capable cursor");
    let mut resync_engine = ObservationEngine::new();
    let (seed_cursor, seed_address, seed_observation) = direct_capture
        .observations
        .iter()
        .find(|(cursor, address, _)| {
            address == resync_address && cursor.sequence < resync_cursor.sequence
        })
        .expect("real transport did not provide a pre-gap observation");
    let prepared = resync_engine
        .prepare(ingress(
            &node_id,
            ObservationTransport::DirectNode,
            *seed_cursor,
            seed_address,
            seed_observation,
        ))
        .unwrap();
    assert_eq!(prepared.outcomes(), &[ApplyOutcome::Applied]);
    resync_engine.accept(prepared);
    let partial = ObservationResyncBatch {
        node_id: node_id.clone(),
        incarnation_id: resync_cursor.incarnation_id,
        requested_after: resync_cursor.sequence - 2,
        high_watermark: *resync_cursor,
        oldest_available_sequence: resync_cursor.sequence,
        records: Vec::new(),
        records_complete: false,
        gaps: vec![gap(resync_cursor.sequence - 1, resync_cursor.sequence - 1)],
        events: vec![ingress(
            &node_id,
            ObservationTransport::DirectNode,
            *resync_cursor,
            resync_address,
            resync_observation,
        )],
    };
    let prepared = resync_engine.prepare_resync(&partial).unwrap();
    assert_eq!(prepared.outcomes(), &[ApplyOutcome::Applied]);
    resync_engine.accept(prepared);
    assert!(resync_engine
        .projection(&target(&node_id, *resync_cursor, resync_address))
        .expect("partial resync projection")
        .transport_incomplete);

    let complete = ObservationResyncBatch {
        node_id: node_id.clone(),
        incarnation_id: resync_cursor.incarnation_id,
        requested_after: resync_cursor.sequence - 1,
        high_watermark: *resync_cursor,
        oldest_available_sequence: resync_cursor.sequence,
        records: Vec::new(),
        records_complete: false,
        gaps: Vec::new(),
        events: vec![ingress(
            &node_id,
            ObservationTransport::C2,
            *resync_cursor,
            resync_address,
            resync_observation,
        )],
    };
    let prepared = resync_engine.prepare_resync(&complete).unwrap();
    assert_eq!(prepared.outcomes(), &[ApplyOutcome::Duplicate]);
    resync_engine.accept(prepared);
    assert!(!resync_engine
        .projection(&target(&node_id, *resync_cursor, resync_address))
        .expect("complete resync projection")
        .transport_incomplete);
    println!(
        "checkpoint: engine runtime_timeline={} managed_timeline={} tools={} partial_gap=true complete_gap=false",
        direct_projection.timeline.len(),
        direct_managed_projection.timeline.len(),
        direct_managed_projection.tools.len(),
    );
}
