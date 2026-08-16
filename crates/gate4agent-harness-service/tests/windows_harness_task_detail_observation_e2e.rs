#![cfg(windows)]

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::protocol::NodeTransportState;
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    ActivityClassV1, ActivityStateV1, FeatureObservationStateV1,
    HarnessOperatorCredential, HarnessOperatorMutationOutcomeV1,
    HarnessRunCorrelationAvailabilityV1, HarnessRunSessionViewV1,
    InteractionStateV1,
    ProjectionAvailabilityV1, ProjectionFreshnessV1, TimelineCategoryV1,
    TimelineStateV1,
};
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_harness_protocol::{
    HarnessCreateTaskRequestV1, HarnessExecutionModeV1,
    HarnessIdempotencyRef, HarnessOperationId, HarnessOperatorAuthorityV1,
    HarnessRevision, HarnessRunLifecycleV1, HarnessScheduleNextRequestV1,
    HarnessScheduleOutcomeV1, HarnessSelectorV1, HarnessTaskId,
    HarnessTaskStateV1, HarnessWorktreeIntentV1,
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
    NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId,
    SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

const PRIVATE_PROMPT: &str = "g4a-private-prompt-canary";
const PRIVATE_TOOL_INPUT: &str = "g4a-private-tool-input-canary";
const PRIVATE_TOOL_OUTPUT: &str = "g4a-private-tool-output-canary";
const PRIVATE_PROVIDER_SESSION: &str = "g4a-private-provider-session-canary";
const PRIVATE_SUBAGENT_ID: &str = "g4a-private-subagent-id-canary";
const PRIVATE_SUBAGENT_DESCRIPTION: &str = "g4a-private-subagent-description-canary";
const PRIVATE_PERMISSION_INPUT: &str = "g4a-private-permission-input-canary";

struct Fixture {
    root: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-harness-task-detail-{}-{}",
            std::process::id(),
            unix_time_ms(),
        ));
        fs::create_dir(&root).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            root,
        }
    }
}

impl Drop for Fixture {
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
        r"\\.\pipe\gate4agent-task-detail-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn selector(value: impl Into<String>) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn authority(marker: char) -> HarnessOperatorAuthorityV1 {
    HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!(
            "hop_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            marker.to_string().repeat(24),
        )).unwrap(),
        actor_id: selector("operator"),
        now_unix_ms: unix_time_ms(),
    }
}

fn launch_catalog(node_id: &NodeId, workspace_id: &WorkspaceId) -> HarnessLaunchCatalog {
    HarnessLaunchCatalog::new([HarnessLaunchPlanV1 {
        plan_id: selector("default"),
        revision: HarnessRevision::new(1).unwrap(),
        node_id: selector(node_id.as_str()),
        workspace_id: selector(workspace_id.as_str()),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("claude"),
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

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            if client.status().await.is_ok_and(|status| {
                status.nodes[node_id].transport == NodeTransportState::Online
            }) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("fixture Node did not become online through C2");
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

fn assert_redacted(serialized: &str, forbidden: &[&str]) {
    for value in forbidden {
        assert!(
            !serialized.contains(value),
            "operator observation reply leaked forbidden bytes: {value}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_like_task_detail_observes_exact_bound_provider_timeline() {
    require_headless_supervisor();
    let fixture = Fixture::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("harness-task-detail-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "task-detail-node-token";
    let c2_token = "task-detail-c2-token";
    let operator_secret = format!("g4aho_{}", "e".repeat(64));
    let operator_credential = HarnessOperatorCredential::parse(operator_secret.clone()).unwrap();
    let workspace = std::env::current_dir().unwrap();
    let absolute_workspace = workspace.to_string_lossy().into_owned();

    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: SpawnProfileId::new("claude").unwrap(),
        revision: SpawnProfileRevision::new("task-detail-r1").unwrap(),
        provider: AgentId::new("claude").unwrap(),
        mode: SessionMode::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt: None,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
    }]).unwrap();
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id.clone(), workspace).unwrap()],
    ).unwrap().with_spawn_profiles(profiles);
    let node = NodeServer::new_monitoring_hook_fixture(node_config).unwrap();
    let node_shutdown = node.shutdown_handle();
    let node_task = tokio::spawn(node.run());

    let c2 = C2Running::start(C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(
            node_id.clone(),
            node_endpoint,
            node_token,
        ).unwrap()],
    ).unwrap()
        .with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(C2Timings {
            poll_interval: Duration::from_millis(20),
            fresh_for: Duration::from_secs(2),
            attempt_deadline: Duration::from_secs(2),
            transient_backoffs: [Duration::from_millis(20); 5],
            parked_backoff: Duration::from_millis(100),
            http_io_deadline: Duration::from_secs(1),
        })).await.unwrap();
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
    let tui = HarnessOperatorClient::new(
        harness_endpoint,
        operator_credential.clone(),
    ).unwrap();

    let task_id = HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap();
    assert_eq!(
        tui.create_task(HarnessCreateTaskRequestV1 {
            authority: authority('a'),
            task_id: task_id.clone(),
            title: "Exact bound provider observation".to_owned(),
            body: "Observe only redacted structured progress".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Ready,
        }).unwrap(),
        HarnessOperatorMutationOutcomeV1::Applied,
    );
    let HarnessScheduleOutcomeV1::Dispatch(dispatch) = tui.schedule_next(
        HarnessScheduleNextRequestV1 {
            authority: authority('b'),
            plan_id: Some(selector("default")),
        },
    ).unwrap() else {
        panic!("ScheduleNext did not dispatch the exact Ready task");
    };
    assert_eq!(dispatch.task_id, task_id);
    let run_id = dispatch.run_id;

    timeout(Duration::from_secs(15), async {
        loop {
            let run = tui.run_get(run_id.clone()).unwrap();
            let monitor = tui.monitor_get(run_id.clone()).unwrap();
            let rich_hook_events_completed = monitor.detail.as_ref().is_some_and(|detail| {
                detail.tool_facts.iter().any(|fact| {
                    fact.state == ActivityStateV1::Completed
                }) && detail.subagent_facts.iter().any(|fact| {
                    fact.state == ActivityStateV1::Completed
                }) && detail.interaction_facts.iter().any(|fact| {
                    fact.state == InteractionStateV1::Responded
                })
            });
            if run.lifecycle == HarnessRunLifecycleV1::Running && rich_hook_events_completed {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("real Node/provider event path did not expose the tool lifecycle");

    sleep(Duration::from_millis(250)).await;
    let correlation = tui.run_correlation_get(run_id.clone()).unwrap();
    let monitor = tui.monitor_get(run_id.clone()).unwrap();
    let timeline = tui.timeline_read(run_id.clone(), None, 128).unwrap();
    drop(tui);
    let reconnected = HarnessOperatorClient::new(
        harness_endpoint,
        operator_credential,
    ).unwrap();
    let monitor_after_reconnect = reconnected.monitor_get(run_id.clone()).unwrap();
    let timeline_after_reconnect = reconnected.timeline_read(run_id.clone(), None, 128).unwrap();

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();

    assert_eq!(correlation.run_id, run_id);
    assert_eq!(correlation.task_id, task_id);
    assert_eq!(correlation.node_id.as_str(), node_id.as_str());
    assert_eq!(correlation.workspace_id.as_str(), workspace_id.as_str());
    assert_eq!(
        correlation.availability,
        HarnessRunCorrelationAvailabilityV1::Available,
    );
    assert!(matches!(correlation.session, HarnessRunSessionViewV1::Managed(_)));

    assert_eq!(monitor.run_id, run_id);
    assert_eq!(monitor.availability, ProjectionAvailabilityV1::Current);
    assert_eq!(monitor.freshness, ProjectionFreshnessV1::Live);
    assert!(!monitor.transport_incomplete);
    assert_eq!(monitor.features.tools, FeatureObservationStateV1::Observed);
    assert_eq!(monitor.features.subagents, FeatureObservationStateV1::Observed);
    assert_eq!(monitor.features.interactions, FeatureObservationStateV1::Observed);
    assert_eq!(
        monitor.features.usage,
        FeatureObservationStateV1::NotSupportedByObservedSources,
    );
    assert_eq!(
        monitor.features.history,
        FeatureObservationStateV1::NotSupportedByObservedSources,
    );
    assert_eq!(
        monitor.features.todo,
        FeatureObservationStateV1::NotSupportedByObservedSources,
    );
    assert_eq!(
        monitor.features.files,
        FeatureObservationStateV1::NotSupportedByObservedSources,
    );
    assert_eq!(monitor.todo_total, 0);
    assert_eq!(monitor.todo_completed, 0);
    assert_eq!(monitor.input_tokens, 0);
    assert_eq!(monitor.output_tokens, 0);
    assert_eq!(monitor.cache_read_tokens, 0);
    assert_eq!(monitor.cache_write_tokens, 0);
    assert_eq!(monitor.reasoning_tokens, 0);
    assert_eq!(monitor.context_window_tokens, None);
    assert_eq!(monitor.active_tools, 0);
    assert_eq!(monitor.active_subagents, 0);
    assert_eq!(monitor.active_interactions, 0);
    let detail = monitor.detail.as_ref().expect("Timeline visibility omitted monitor detail");
    assert!(detail.todo_facts.is_empty());
    assert!(detail.file_facts.is_empty());
    assert!(detail.tool_facts.iter().any(|fact| {
        fact.class == ActivityClassV1::Tool
            && fact.state == ActivityStateV1::Completed
            && fact.label.as_deref() == Some("Shell")
            && fact.correlation.is_some()
    }));
    assert!(detail.subagent_facts.iter().any(|fact| {
        fact.class == ActivityClassV1::Subagent
            && fact.state == ActivityStateV1::Completed
            && fact.label.as_deref() == Some("Search")
            && fact.correlation.is_some()
    }));
    assert!(detail.interaction_facts.iter().any(|fact| {
        fact.state == InteractionStateV1::Responded
    }));

    assert_eq!(timeline.run_id, run_id);
    assert_eq!(timeline.availability, ProjectionAvailabilityV1::Current);
    assert_eq!(timeline.freshness, ProjectionFreshnessV1::Live);
    assert!(!timeline.transport_incomplete);
    assert!(timeline.entries.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));
    let tool_entries = timeline.entries.iter()
        .filter(|entry| entry.category == TimelineCategoryV1::Tool)
        .collect::<Vec<_>>();
    assert!(tool_entries.iter().any(|entry| entry.state == TimelineStateV1::Started));
    assert!(tool_entries.iter().any(|entry| entry.state == TimelineStateV1::Completed));
    let started_tool = tool_entries.iter()
        .find(|entry| entry.state == TimelineStateV1::Started).unwrap();
    let completed_tool = tool_entries.iter()
        .find(|entry| entry.state == TimelineStateV1::Completed).unwrap();
    assert_eq!(started_tool.correlation, completed_tool.correlation);
    assert!(started_tool.correlation.is_some());
    let started_subagent = timeline.entries.iter().find(|entry| {
        entry.category == TimelineCategoryV1::Subagent
            && entry.state == TimelineStateV1::Started
            && entry.correlation.is_some()
    }).unwrap();
    let completed_subagent = timeline.entries.iter().find(|entry| {
        entry.category == TimelineCategoryV1::Subagent
            && entry.state == TimelineStateV1::Completed
            && entry.correlation.is_some()
    }).unwrap();
    assert_eq!(started_subagent.correlation, completed_subagent.correlation);
    assert_eq!(started_subagent.label.as_deref(), Some("Search"));
    assert_eq!(completed_subagent.label.as_deref(), Some("Search"));
    let requested_interaction = timeline.entries.iter().find(|entry| {
        entry.category == TimelineCategoryV1::Interaction
            && entry.state == TimelineStateV1::Required
            && entry.correlation.is_some()
    }).unwrap();
    let resolved_interaction = timeline.entries.iter().find(|entry| {
        entry.category == TimelineCategoryV1::Interaction
            && entry.state == TimelineStateV1::Completed
            && entry.correlation.is_some()
    }).unwrap();
    assert_eq!(requested_interaction.correlation, resolved_interaction.correlation);
    assert!(
        completed_tool.sequence < started_subagent.sequence
            && started_subagent.sequence < completed_subagent.sequence
            && completed_subagent.sequence < requested_interaction.sequence
            && requested_interaction.sequence < resolved_interaction.sequence
    );
    assert!(!timeline.entries.iter().any(|entry| matches!(
        entry.category,
        TimelineCategoryV1::Todo
            | TimelineCategoryV1::File
            | TimelineCategoryV1::Usage
            | TimelineCategoryV1::History
    )));

    assert_eq!(monitor_after_reconnect, monitor);
    assert_eq!(timeline_after_reconnect, timeline);
    let serialized = serde_json::to_string(&(
        correlation,
        monitor,
        timeline,
        monitor_after_reconnect,
        timeline_after_reconnect,
    )).unwrap();
    assert_redacted(&serialized, &[
        PRIVATE_PROMPT,
        PRIVATE_TOOL_INPUT,
        PRIVATE_TOOL_OUTPUT,
        PRIVATE_PROVIDER_SESSION,
        PRIVATE_SUBAGENT_ID,
        PRIVATE_SUBAGENT_DESCRIPTION,
        PRIVATE_PERMISSION_INPUT,
        "fixture-permission-1",
        absolute_workspace.as_str(),
        node_token,
        c2_token,
        operator_secret.as_str(),
        "credentials",
        "provider-config",
    ]);
}
