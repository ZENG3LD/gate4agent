#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, NodeId, NodeRoute, NodeTransportState,
    C2_OBSERVATION_EVENTS_CAPABILITY, C2_OBSERVATION_MANAGED_TARGET_CAPABILITY,
    C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2EventReceiver};
use gate4agent_node::protocol::{
    ClientRole, NodeCursor, NodeEvent, NodeRequest, ServerFrame, SessionAddress,
    SessionRecordId, SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverrides, SpawnProfileId,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceId,
    NODE_OBSERVATION_EVENTS_CAPABILITY,
    NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY, NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::NamedPipeNodeClient;
use gate4agent_observation_api::{
    ManagedSessionKey, ObservationIngressEnvelope, ObservationIngressPayload, ObservationTarget,
    ObservationTransport, ProjectionAvailability, ProjectionFreshness, RuntimeSessionKey,
};
use gate4agent_observation_engine::{ApplyOutcome, CorrelationState, UsageTotals};
use gate4agent_observation_service::{ObservationService, ObservationStoreLimits};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);

const PROMPT_CANARY: &str = "g4a-qwen-private-prompt-canary";
const TOOL_INPUT_CANARY: &str = "g4a-qwen-private-tool-input-canary";
const TOOL_OUTPUT_CANARY: &str = "g4a-qwen-private-tool-output-canary";
const PROVIDER_SESSION_CANARY: &str = "g4a-qwen-private-provider-session-canary";
const PATH_CANARY: &str = "g4a-qwen-private-path-canary";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    Clean,
    Abrupt,
}

impl Scenario {
    fn argument(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Abrupt => "abrupt",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CapturedObservation {
    Runtime {
        address: SessionAddress,
        observation: gate4agent_node::protocol::ObservationV1,
    },
    Managed {
        record_id: SessionRecordId,
        observation: gate4agent_node::protocol::ObservationV1,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedFact {
    cursor: NodeCursor,
    observation: CapturedObservation,
}

struct FixtureDirectory(PathBuf);

impl FixtureDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-qwen-monitoring-e2e-{label}-{}-{nonce}",
            std::process::id(),
        ));
        fs::create_dir(&path).expect("create Qwen monitoring fixture directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let expected_parent = std::env::temp_dir();
        let safe = self.0.is_absolute()
            && self.0.parent() == Some(expected_parent.as_path())
            && self
                .0
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("gate4agent-qwen-monitoring-e2e-"));
        if safe {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

struct RunningFixture {
    node_shutdown: gate4agent_node::NodeShutdownHandle,
    node_task: tokio::task::JoinHandle<Result<(), gate4agent_node::NodeServerError>>,
    c2: C2Running,
}

fn endpoint(label: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-qwen-monitoring-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed),
    )
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

fn node_config(
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace: &Path,
) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            workspace,
        )
        .unwrap()],
    )
    .unwrap()
}

fn cmd_launcher() -> String {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .expect("Windows system root is unavailable");
    fs::canonicalize(PathBuf::from(root).join("System32").join("cmd.exe"))
    .expect("Windows command launcher is unavailable")
    .into_os_string()
    .into_string()
    .expect("Windows command launcher path is not Unicode")
}

fn fixture_script() -> &'static str {
    r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion
if /I not "%~1"=="--json-file" exit /b 41
if "%~2"=="" exit /b 42
set "SIDECAR=%~2"
> "%CD%\sidecar-argument-proof.txt" <nul set /p "=%SIDECAR%"
> "%SIDECAR%" echo {"type":"system","subtype":"session_start","session_id":"g4a-qwen-private-provider-session-canary","data":{"protocol_version":2,"session_id":"g4a-qwen-private-provider-session-canary","cwd":"g4a-qwen-private-path-canary","supported_events":["control_request","control_response"]}}
>> "%SIDECAR%" echo {"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"g4a-qwen-private-prompt-canary"},{"type":"tool_use","id":"fixture-tool","name":"read_file","input":{"path":"g4a-qwen-private-tool-input-canary"}}]}}
if not exist "%CD%\clean-scenario.marker" goto ready
>> "%SIDECAR%" echo {"type":"control_request","request_id":"fixture-approval","request":{"subtype":"can_use_tool","tool_name":"run_shell_command","tool_use_id":"fixture-tool","input":{"command":"g4a-qwen-private-tool-input-canary"},"prompt":"g4a-qwen-private-prompt-canary"}}
>> "%SIDECAR%" echo {"type":"control_response","response":{"subtype":"success","request_id":"fixture-approval","response":{"allowed":true}}}
>> "%SIDECAR%" echo {"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"fixture-tool","content":"g4a-qwen-private-tool-output-canary","is_error":false,"duration_ms":19}]}}
>> "%SIDECAR%" echo {"type":"result","subtype":"success","usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":5,"cache_creation_input_tokens":7}}
>> "%SIDECAR%" echo {"type":"system","subtype":"session_end"}
:ready
echo fixture-qwen-ready
exit /b 0
"#
}

fn has_capability(capabilities: &[gate4agent_node::protocol::CapabilityId], expected: &str) -> bool {
    capabilities
        .iter()
        .any(|capability| capability.as_str() == expected)
}

fn is_scenario_terminal(
    scenario: Scenario,
    observation: &gate4agent_node::protocol::ObservationV1,
) -> bool {
    use gate4agent_node::protocol::{ObservationEvidenceV1, ObservationKindV1};
    if observation.evidence != ObservationEvidenceV1::StructuredProvider {
        return false;
    }
    match scenario {
        Scenario::Clean => matches!(observation.kind, ObservationKindV1::Exited { .. }),
        Scenario::Abrupt => matches!(observation.kind, ObservationKindV1::Gap { .. }),
    }
}

async fn capture_direct(
    client: &mut NamedPipeNodeClient,
    incarnation_id: gate4agent_node::protocol::NodeIncarnationId,
    expected_address: &SessionAddress,
    scenario: Scenario,
) -> Vec<CapturedFact> {
    let mut facts = Vec::new();
    let mut runtime_terminal = false;
    let mut managed_terminal = false;
    let completed = timeout(Duration::from_secs(5), async {
        loop {
            match client.recv().await.expect("direct Node event stream failed") {
                ServerFrame::Event(envelope) => match envelope.event {
                    NodeEvent::Observation { address, observation }
                        if address == *expected_address =>
                    {
                        runtime_terminal |= is_scenario_terminal(scenario, &observation);
                        facts.push(CapturedFact {
                            cursor: NodeCursor {
                                incarnation_id,
                                sequence: envelope.sequence,
                            },
                            observation: CapturedObservation::Runtime {
                                address,
                                observation,
                            },
                        });
                    }
                    NodeEvent::ManagedObservation { record_id, observation } => {
                        managed_terminal |= is_scenario_terminal(scenario, &observation);
                        facts.push(CapturedFact {
                            cursor: NodeCursor {
                                incarnation_id,
                                sequence: envelope.sequence,
                            },
                            observation: CapturedObservation::Managed {
                                record_id,
                                observation,
                            },
                        });
                    }
                    NodeEvent::ResyncRequired { .. } => {
                        panic!("direct Qwen monitoring stream required resync")
                    }
                    _ => {}
                },
                frame => panic!("direct Node sent an unexpected frame: {frame:?}"),
            }
            if runtime_terminal && managed_terminal {
                return;
            }
        }
    })
    .await;
    assert!(
        completed.is_ok(),
        "direct Qwen monitoring omitted a runtime or managed terminator: {facts:#?}"
    );
    facts
}

async fn capture_c2(
    events: &mut C2EventReceiver,
    node_id: &NodeId,
    expected_address: &SessionAddress,
    scenario: Scenario,
) -> Vec<CapturedFact> {
    let mut facts = Vec::new();
    let mut runtime_terminal = false;
    let mut managed_terminal = false;
    let completed = timeout(Duration::from_secs(5), async {
        loop {
            let routed = events
                .recv()
                .await
                .expect("authenticated C2 event stream closed early");
            if &routed.node_id != node_id {
                continue;
            }
            match routed.event {
                C2NodeEvent::Observation { address, observation }
                    if address == *expected_address =>
                {
                    runtime_terminal |= is_scenario_terminal(scenario, &observation);
                    facts.push(CapturedFact {
                        cursor: routed.cursor,
                        observation: CapturedObservation::Runtime {
                            address,
                            observation,
                        },
                    });
                }
                C2NodeEvent::ManagedObservation { record_id, observation } => {
                    managed_terminal |= is_scenario_terminal(scenario, &observation);
                    facts.push(CapturedFact {
                        cursor: routed.cursor,
                        observation: CapturedObservation::Managed {
                            record_id,
                            observation,
                        },
                    });
                }
                C2NodeEvent::ResyncRequired { .. } => {
                    panic!("C2 Qwen monitoring stream required resync")
                }
                _ => {}
            }
            if runtime_terminal && managed_terminal {
                return;
            }
        }
    })
    .await;
    assert!(
        completed.is_ok(),
        "C2 Qwen monitoring omitted a runtime or managed terminator: {facts:#?}"
    );
    facts
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
                            .expect("online Qwen fixture node has no cursor")
                            .incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Qwen fixture node did not become online through C2")
}

async fn start_fixture(
    root: &Path,
    scenario: Scenario,
) -> (
    RunningFixture,
    NodeId,
    String,
    String,
    NodeRoute,
    gate4agent_c2_client::C2ControlHandle,
    C2EventReceiver,
    NamedPipeNodeClient,
) {
    let node_endpoint = endpoint(scenario.argument());
    let control_endpoint = endpoint("c2");
    let node_id = NodeId::new(format!("qwen-monitoring-{}", scenario.argument())).unwrap();
    let node_token = format!("qwen-monitoring-node-token-{}", scenario.argument());
    let c2_token = format!("qwen-monitoring-c2-token-{}", scenario.argument());
    if scenario == Scenario::Clean {
        fs::write(root.join("clean-scenario.marker"), b"clean")
            .expect("create controlled Qwen clean-scenario marker");
    }
    let fixture_path = root.join("controlled-qwen.cmd");
    fs::write(&fixture_path, fixture_script().as_bytes())
        .expect("create controlled Qwen command fixture");
    let fixed_args = vec![
        "/D".to_owned(),
        "/Q".to_owned(),
        "/C".to_owned(),
        fixture_path.to_string_lossy().into_owned(),
    ];
    let server = NodeServer::new_qwen_dual_output_fixture(
        node_config(&node_endpoint, &node_token, &node_id, root),
        cmd_launcher(),
        fixed_args,
    )
    .unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

    let config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        &c2_token,
        vec![C2NodeConfig::new(
            node_id.clone(),
            node_endpoint.clone(),
            &node_token,
        )
        .unwrap()],
    )
    .unwrap()
    .with_control_endpoint(control_endpoint.clone())
    .unwrap()
    .with_timings(timings());
    let c2 = C2Running::start(config).await.unwrap();
    let http = C2Client::new(c2.api_addr(), &c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;
    let (control, events) = connect_local(&control_endpoint, &c2_token).await.unwrap();
    let direct = NamedPipeNodeClient::connect(
        &node_endpoint,
        &node_id,
        ClientRole::Observer,
        &node_token,
    )
    .await
    .unwrap();
    assert_eq!(direct.hello().incarnation_id, route.expected_incarnation_id);

    let direct_capabilities = &direct
        .hello()
        .compatibility
        .as_ref()
        .expect("direct monitoring compatibility")
        .capabilities;
    for capability in [
        NODE_OBSERVATION_EVENTS_CAPABILITY,
        NODE_OBSERVATION_MANAGED_TARGET_CAPABILITY,
        NODE_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    ] {
        assert!(has_capability(direct_capabilities, capability));
    }
    let c2_capabilities = &control
        .hello()
        .compatibility
        .as_ref()
        .expect("C2 monitoring compatibility")
        .capabilities;
    for capability in [
        C2_OBSERVATION_EVENTS_CAPABILITY,
        C2_OBSERVATION_MANAGED_TARGET_CAPABILITY,
        C2_OBSERVATION_WORKFLOW_DETAIL_CAPABILITY,
    ] {
        assert!(has_capability(c2_capabilities, capability));
    }

    (
        RunningFixture {
            node_shutdown,
            node_task,
            c2,
        },
        node_id,
        node_token,
        c2_token,
        route,
        control,
        events,
        direct,
    )
}

fn target(
    node_id: &NodeId,
    cursor: NodeCursor,
    captured: &CapturedObservation,
) -> ObservationTarget {
    match captured {
        CapturedObservation::Runtime { address, .. } => ObservationTarget::Runtime {
            key: RuntimeSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                workspace_id: address.workspace_id.clone(),
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            },
        },
        CapturedObservation::Managed { record_id, .. } => ObservationTarget::Managed {
            key: ManagedSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                record_id: record_id.clone(),
            },
        },
    }
}

fn ingress(
    node_id: &NodeId,
    transport: ObservationTransport,
    fact: &CapturedFact,
) -> ObservationIngressEnvelope {
    let observation = match &fact.observation {
        CapturedObservation::Runtime { observation, .. }
        | CapturedObservation::Managed { observation, .. } => observation.clone(),
    };
    ObservationIngressEnvelope {
        node_id: node_id.clone(),
        cursor: fact.cursor,
        received_at_ms: observation.observed_at_unix_ms.unwrap_or(1),
        transport,
        payload: ObservationIngressPayload::Observation {
            address: target(node_id, fact.cursor, &fact.observation),
            observation,
        },
    }
}

async fn stop_fixture(running: RunningFixture) {
    let c2_shutdown = running.c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.c2.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    running.node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), running.node_task)
        .await
        .expect("Node shutdown timed out")
        .expect("Node task panicked")
        .expect("Node shutdown failed");
}

async fn run_scenario(root: &Path, scenario: Scenario) -> (NodeId, Vec<CapturedFact>) {
    let (
        running,
        node_id,
        _node_token,
        _c2_token,
        route,
        control,
        mut c2_events,
        mut direct,
    ) = start_fixture(root, scenario).await;
    let incarnation_id = direct.hello().incarnation_id;
    let spawned = control
        .request(
            route,
            NodeRequest::SpawnSpec {
                spec: SpawnSpec {
                    target: SpawnTarget {
                        node_id: node_id.clone(),
                        workspace_id: WorkspaceId::new("primary").unwrap(),
                        worktree_id: None,
                    },
                    profile_id: SpawnProfileId::new("default").unwrap(),
                    expected_profile_revision:
                        gate4agent_node::protocol::SpawnProfileRevision::new("builtin-v1")
                            .unwrap(),
                    overrides: SpawnOverrides::default(),
                    deadline_ms: SpawnDeadlineMs::new(30_000).unwrap(),
                    idempotency_key: SpawnIdempotencyKey::new(format!(
                        "qwen-monitoring-{}",
                        scenario.argument(),
                    ))
                    .unwrap(),
                    required_capabilities: SpawnRequiredCapabilities::default(),
                },
            },
        )
        .await
        .unwrap();
    let address = match spawned.response {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => receipt.session,
        response => panic!("Qwen fixture spawn returned an unexpected response: {response:?}"),
    };
    let (direct_facts, c2_facts) = tokio::join!(
        capture_direct(&mut direct, incarnation_id, &address, scenario),
        capture_c2(&mut c2_events, &node_id, &address, scenario),
    );
    assert_eq!(direct_facts, c2_facts);
    let terminal_debug = match direct.request(NodeRequest::Snapshot).await.unwrap() {
        gate4agent_node::protocol::NodeResponse::Snapshot { snapshot, .. } => snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.sessions.iter())
            .find(|session| {
                session.instance_id == address.session.instance_id
                    && session.generation == address.session.generation
            })
            .and_then(|session| session.terminal_frame.as_ref())
            .map(|frame| frame.contents.clone()),
        response => panic!("direct Qwen snapshot returned an unexpected response: {response:?}"),
    };
    drop(direct);
    drop(control);
    stop_fixture(running).await;
    let sidecar_path = fs::read_to_string(root.join("sidecar-argument-proof.txt"))
        .unwrap_or_else(|error| {
            panic!(
                "controlled Qwen fixture did not receive --json-file: {error}; terminal={terminal_debug:#?}; facts={direct_facts:#?}"
            )
        });
    let sidecar_path = PathBuf::from(sidecar_path);
    assert!(sidecar_path.is_absolute());
    assert_eq!(
        sidecar_path.file_name().and_then(|name| name.to_str()),
        Some("events.jsonl"),
    );
    assert!(sidecar_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("gate4agent-qwen-sidecar-")));
    assert!(!sidecar_path.exists(), "Qwen sidecar file survived session cleanup");
    (node_id, direct_facts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_qwen_pty_node_c2_durable_monitoring_privacy_and_gap_e2e() {
    gate4agent_testkit::suppress_windows_fault_dialogs_for_test();
    gate4agent_testkit::require_windows_headless_supervisor_for_test();

    let clean_root = FixtureDirectory::new("clean");
    let (clean_node, clean_facts) = run_scenario(clean_root.path(), Scenario::Clean).await;
    assert!(!clean_facts.is_empty());

    use gate4agent_node::protocol::{
        ObservationEvidenceV1, ObservationInteractionOutcomeV1, ObservationKindV1,
        ObservationSourceFamilyV1,
    };
    let all_clean_observations = clean_facts
        .iter()
        .map(|fact| match &fact.observation {
            CapturedObservation::Runtime { observation, .. }
            | CapturedObservation::Managed { observation, .. } => observation,
        })
        .collect::<Vec<_>>();
    let clean_observations = all_clean_observations
        .iter()
        .copied()
        .filter(|observation| observation.evidence == ObservationEvidenceV1::StructuredProvider)
        .collect::<Vec<_>>();
    assert!(clean_observations.iter().any(|observation| matches!(
        &observation.kind,
        ObservationKindV1::SourceCapabilities {
            source_family: ObservationSourceFamilyV1::Pipe,
            source_adapter,
            capabilities,
        } if source_adapter == "qwen-code"
            && capabilities.tools
            && capabilities.attention
            && capabilities.usage
            && !capabilities.subagents
            && !capabilities.todo
            && !capabilities.file_changes
    )));
    for expected in [
        "ready",
        "turn",
        "tool-start",
        "tool-end",
        "approval",
        "usage",
        "exit",
    ] {
        assert!(clean_observations.iter().any(|observation| match expected {
            "ready" => matches!(observation.kind, ObservationKindV1::Ready),
            "turn" => matches!(observation.kind, ObservationKindV1::TurnStarted),
            "tool-start" => matches!(
                &observation.kind,
                ObservationKindV1::ToolStarted { class, .. } if class == "Read"
            ),
            "tool-end" => matches!(
                observation.kind,
                ObservationKindV1::ToolCompleted {
                    success: true,
                    duration_ms: Some(19),
                    ..
                }
            ),
            "approval" => matches!(
                &observation.kind,
                ObservationKindV1::ApprovalRequested { tool_class, .. } if tool_class == "Shell"
            ),
            "usage" => matches!(
                observation.kind,
                ObservationKindV1::Usage {
                    input_tokens: 2,
                    output_tokens: 3,
                    cache_read_tokens: 5,
                    cache_write_tokens: 7,
                    ..
                }
            ),
            "exit" => matches!(
                observation.kind,
                ObservationKindV1::Exited { success: Some(true) }
            ),
            _ => unreachable!(),
        }), "missing controlled Qwen observation: {expected}");
    }
    let approval_correlation = clean_observations
        .iter()
        .find_map(|observation| match &observation.kind {
            ObservationKindV1::ApprovalRequested { correlation_id, .. } => Some(correlation_id),
            _ => None,
        })
        .expect("missing controlled Qwen approval correlation");
    let resolved_correlation = all_clean_observations
        .iter()
        .find_map(|observation| match &observation.kind {
            ObservationKindV1::InteractionResolved {
                correlation_id,
                outcome: ObservationInteractionOutcomeV1::Approved,
            } if observation.evidence == ObservationEvidenceV1::NodeLifecycle => {
                Some(correlation_id)
            }
            _ => None,
        })
        .expect("missing canonical controlled Qwen approval resolution");
    assert_eq!(resolved_correlation, approval_correlation);

    let database = clean_root.path().join("observations.sqlite3");
    let mut service = ObservationService::open_with_limits(
        &database,
        ObservationStoreLimits {
            tail_operations: 8,
            tail_bytes: 4 * 1024 * 1024,
        },
    )
    .unwrap();
    for fact in &clean_facts {
        assert_eq!(
            service
                .apply_ingress(ingress(
                    &clean_node,
                    ObservationTransport::DirectNode,
                    fact,
                ))
                .unwrap(),
            ApplyOutcome::Applied,
        );
    }
    for fact in &clean_facts {
        let mut duplicate = ingress(&clean_node, ObservationTransport::C2, fact);
        duplicate.received_at_ms = duplicate.received_at_ms.saturating_add(1_000);
        assert_eq!(
            service.apply_ingress(duplicate).unwrap(),
            ApplyOutcome::Duplicate,
        );
    }
    let before_restart = service.committed_snapshot();
    let serialized = format!("{before_restart:#?}");
    let clean_root_canary = clean_root.path().to_string_lossy();
    for canary in [
        PROMPT_CANARY,
        TOOL_INPUT_CANARY,
        TOOL_OUTPUT_CANARY,
        PROVIDER_SESSION_CANARY,
        PATH_CANARY,
        clean_root_canary.as_ref(),
    ] {
        assert!(!serialized.contains(canary), "private canary leaked: {canary}");
    }
    let runtime_projection = before_restart
        .projections
        .iter()
        .find(|projection| matches!(projection.target, ObservationTarget::Runtime { .. }))
        .expect("durable Qwen runtime projection");
    assert_eq!(runtime_projection.tools.len(), 1);
    assert_eq!(runtime_projection.tools[0].class.as_deref(), Some("Read"));
    assert_eq!(
        runtime_projection.tools[0].state,
        CorrelationState::Completed {
            success: Some(true),
        }
    );
    assert_eq!(runtime_projection.interactions.len(), 1);
    assert_eq!(runtime_projection.interactions[0].class.as_deref(), Some("Shell"));
    assert_eq!(
        runtime_projection.interactions[0].state,
        CorrelationState::Resolved {
            outcome: ObservationInteractionOutcomeV1::Approved,
        }
    );
    assert_eq!(
        runtime_projection.usage.observed_delta,
        UsageTotals {
            input_tokens: 2,
            output_tokens: 3,
            cache_read_tokens: 5,
            cache_write_tokens: 7,
            reasoning_tokens: 0,
        }
    );
    service.close().unwrap();
    let reopened = ObservationService::open_with_limits(
        &database,
        ObservationStoreLimits {
            tail_operations: 8,
            tail_bytes: 4 * 1024 * 1024,
        },
    )
    .unwrap();
    assert_eq!(reopened.committed_snapshot(), before_restart);
    reopened.close().unwrap();

    let abrupt_root = FixtureDirectory::new("abrupt");
    let (abrupt_node, abrupt_facts) = run_scenario(abrupt_root.path(), Scenario::Abrupt).await;
    let structured_gaps = abrupt_facts
        .iter()
        .filter(|fact| match &fact.observation {
            CapturedObservation::Runtime { observation, .. }
            | CapturedObservation::Managed { observation, .. } => {
                observation.evidence == ObservationEvidenceV1::StructuredProvider
                    && matches!(observation.kind, ObservationKindV1::Gap { missed: 1 })
            }
        })
        .count();
    assert_eq!(structured_gaps, 2, "expected one exact Qwen gap per target");
    let abrupt_database = abrupt_root.path().join("observations.sqlite3");
    let mut abrupt_service = ObservationService::open(&abrupt_database).unwrap();
    for fact in &abrupt_facts {
        assert_eq!(
            abrupt_service
                .apply_ingress(ingress(
                    &abrupt_node,
                    ObservationTransport::DirectNode,
                    fact,
                ))
                .unwrap(),
            ApplyOutcome::Applied,
        );
    }
    for fact in &abrupt_facts {
        let mut duplicate = ingress(&abrupt_node, ObservationTransport::C2, fact);
        duplicate.received_at_ms = duplicate.received_at_ms.saturating_add(1_000);
        assert_eq!(
            abrupt_service.apply_ingress(duplicate).unwrap(),
            ApplyOutcome::Duplicate,
        );
    }
    let abrupt_before_restart = abrupt_service.committed_snapshot();
    let abrupt_runtime = abrupt_before_restart
        .projections
        .iter()
        .find(|projection| matches!(projection.target, ObservationTarget::Runtime { .. }))
        .expect("abrupt Qwen runtime projection");
    assert_eq!(abrupt_runtime.availability, ProjectionAvailability::Partial);
    assert_eq!(
        abrupt_runtime.freshness,
        ProjectionFreshness::IncompleteAfterGap,
    );
    assert_eq!(
        abrupt_runtime.incomplete_evidence,
        [ObservationEvidenceV1::StructuredProvider],
    );
    let abrupt_json = format!("{abrupt_before_restart:#?}");
    let abrupt_root_canary = abrupt_root.path().to_string_lossy();
    for canary in [
        PROMPT_CANARY,
        TOOL_INPUT_CANARY,
        TOOL_OUTPUT_CANARY,
        PROVIDER_SESSION_CANARY,
        PATH_CANARY,
        abrupt_root_canary.as_ref(),
    ] {
        assert!(!abrupt_json.contains(canary), "private abrupt canary leaked: {canary}");
    }
    abrupt_service.close().unwrap();
    let reopened_abrupt = ObservationService::open(&abrupt_database).unwrap();
    assert_eq!(reopened_abrupt.committed_snapshot(), abrupt_before_restart);
    reopened_abrupt.close().unwrap();

    println!(
        "checkpoint: qwen clean facts={} projections={} abrupt facts={} exact_gaps={} durable_restart=true privacy_canaries=5",
        clean_facts.len(),
        before_restart.projections.len(),
        abrupt_facts.len(),
        structured_gaps,
    );
}
