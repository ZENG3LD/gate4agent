#![cfg(windows)]

//! E2E coverage for the `SubscribeEvents` operator-wire push subscription:
//! a subscribed connection receives a `SnapshotBaseline` immediately, then a
//! `TaskChanged` for a task created over an ordinary (non-subscribed)
//! client connection, then `RuntimeInventoryChanged` frames as a directly
//! spawned session (the same typed verb `windows_harness_operator_session_
//! verbs_e2e.rs` exercises) appears and disappears from the runtime
//! inventory. Fixture node + C2 + harness host, the same three-process
//! shape that test already uses.

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    HarnessCreateTaskRequestV1, HarnessIdempotencyRef, HarnessOperationId,
    HarnessOperatorAuthorityV1, HarnessOperatorCredential, HarnessOperatorEventV1,
    HarnessRuntimeNodeInventoryV1, HarnessRuntimeSessionAddressV1, HarnessRuntimeSessionV1,
    HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
};
use gate4agent_harness_client::HarnessOperatorClient;
use gate4agent_harness_protocol::HarnessExecutionModeV1;
use gate4agent_harness_service::{
    runtime::{start_harness_host_with_operator_and_catalogs, HarnessRuntimeCatalogs},
    HarnessService,
};
use gate4agent_node::protocol::{
    NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_observation_service::ObservationService;
use gate4agent_types::{AgentId, TerminalSize};
use tokio::{
    sync::mpsc::UnboundedReceiver,
    time::{sleep, timeout},
};

struct FixturePaths {
    root: PathBuf,
    workspace: PathBuf,
    harness: PathBuf,
    observation: PathBuf,
    node_state: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "gate4agent-harness-event-subscription-{}-{}-{}",
            std::process::id(),
            unix_time_ms(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self {
            harness: root.join("harness.sqlite3"),
            observation: root.join("observation.sqlite3"),
            node_state: root.join("node-state.json"),
            workspace,
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
        r"\\.\pipe\gate4agent-harness-event-subscription-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn node_config(
    fixture: &FixturePaths,
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_id: &WorkspaceId,
    profile_id: &SpawnProfileId,
    profile_revision: &SpawnProfileRevision,
) -> NodeServerConfig {
    let profiles = SpawnProfileRegistry::new([SpawnProfileDefaults {
        profile_id: profile_id.clone(),
        revision: profile_revision.clone(),
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

async fn connect_harness_adapter(
    endpoint: &str,
    token: &str,
) -> (
    gate4agent_harness_service::c2::HarnessC2Adapter,
    gate4agent_harness_service::c2::HarnessC2EventReceiver,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(connected) =
                gate4agent_harness_service::c2::HarnessC2Adapter::connect(endpoint, token).await
            {
                break connected;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C2 did not expose its sole Harness operator slot")
}

fn event_sequence(event: &HarnessOperatorEventV1) -> u64 {
    match event {
        HarnessOperatorEventV1::SnapshotBaseline { sequence, .. }
        | HarnessOperatorEventV1::TaskChanged { sequence, .. }
        | HarnessOperatorEventV1::RunChanged { sequence, .. }
        | HarnessOperatorEventV1::RuntimeInventoryChanged { sequence, .. }
        | HarnessOperatorEventV1::RuntimeInventoryRemoved { sequence, .. }
        | HarnessOperatorEventV1::Lagged { sequence } => *sequence,
    }
}

fn find_session_in_node(
    node: &HarnessRuntimeNodeInventoryV1,
    address: &HarnessRuntimeSessionAddressV1,
) -> Option<HarnessRuntimeSessionV1> {
    if node.node_id != address.node_id {
        return None;
    }
    node.inventory.workspaces.get(&address.workspace_id)?
        .sessions.iter()
        .find(|session| {
            session.instance_id == address.instance_id && session.generation == address.generation
        })
        .cloned()
}

/// Pulls events off the background reader thread's channel (see the test
/// body: `HarnessEventSubscription::next_event` blocks, so it is drained on
/// its own `std::thread`, not directly inside this async test) until one
/// satisfies `matches`, asserting every event observed along the way -- not
/// just the matched one -- carries a strictly increasing `sequence`.
async fn wait_for_event(
    event_rx: &mut UnboundedReceiver<HarnessOperatorEventV1>,
    last_sequence: &mut Option<u64>,
    mut matches: impl FnMut(&HarnessOperatorEventV1) -> bool,
) -> HarnessOperatorEventV1 {
    timeout(Duration::from_secs(15), async {
        loop {
            let event = event_rx.recv().await
                .expect("event subscription reader thread ended early");
            let sequence = event_sequence(&event);
            if let Some(previous) = *last_sequence {
                assert!(
                    sequence > previous,
                    "event sequence must be strictly increasing per subscription",
                );
            }
            *last_sequence = Some(sequence);
            if matches(&event) {
                return event;
            }
        }
    }).await.expect("expected operator event never arrived")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_operator_event_subscription_baseline_task_and_inventory_round_trip() {
    require_headless_supervisor();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_writer(std::io::stderr)
        .try_init();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("event-subscription-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "event-subscription-node-token";
    let c2_token = "event-subscription-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "e".repeat(64),
    )).unwrap();
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("event-subscription-r1").unwrap();

    let node = NodeServer::new_fixture(node_config(
        &fixture,
        &node_endpoint,
        node_token,
        &node_id,
        &workspace_id,
        &profile_id,
        &profile_revision,
    )).unwrap();
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
    let (host, host_task) = start_harness_host_with_operator_and_catalogs(
        HarnessService::open(&fixture.harness).unwrap(),
        ObservationService::open(&fixture.observation).unwrap(),
        adapter,
        events,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        Some(operator_credential.clone()),
        HarnessRuntimeCatalogs::default(),
    ).await.unwrap();
    let harness_endpoint = host.endpoint().socket_addr();
    let client = HarnessOperatorClient::new(harness_endpoint, operator_credential.clone()).unwrap();

    // Subscribe on its own dedicated connection, drained on its own thread
    // -- `next_event` blocks the calling thread, so it must not run
    // directly inside this async test body (see `wait_for_event`'s doc
    // comment).
    let subscription = client.subscribe_events().unwrap();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut subscription = subscription;
        loop {
            match subscription.next_event() {
                Ok(event) => {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let mut last_sequence = None;

    // SnapshotBaseline always arrives first, with no tasks yet.
    let baseline = wait_for_event(&mut event_rx, &mut last_sequence, |event| {
        matches!(event, HarnessOperatorEventV1::SnapshotBaseline { .. })
    }).await;
    match baseline {
        HarnessOperatorEventV1::SnapshotBaseline { tasks, runs, .. } => {
            assert!(tasks.is_empty());
            assert!(runs.is_empty());
        }
        other => panic!("expected SnapshotBaseline, got {other:?}"),
    }

    // Creating a task over an ordinary (non-subscribed) client connection
    // pushes a TaskChanged carrying that exact task.
    let task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();
    let create_request = HarnessCreateTaskRequestV1 {
        authority: HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "a".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{}", "a".repeat(24)))
                .unwrap(),
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
            now_unix_ms: unix_time_ms(),
        },
        task_id: task_id.clone(),
        title: "Event subscription task".to_owned(),
        body: String::new(),
        parent_task_id: None,
        dependencies: Vec::new(),
        initial_state: HarnessTaskStateV1::Backlog,
    };
    client.create_task(create_request).unwrap();

    let task_changed = wait_for_event(&mut event_rx, &mut last_sequence, |event| {
        matches!(event, HarnessOperatorEventV1::TaskChanged { task, .. } if task.task_id == task_id)
    }).await;
    match task_changed {
        HarnessOperatorEventV1::TaskChanged { task, .. } => {
            assert_eq!(task.task_id, task_id);
            assert_eq!(task.title, "Event subscription task");
        }
        other => panic!("expected TaskChanged, got {other:?}"),
    }

    // SpawnSession via the operator wire (direct spawn, no Task/Run/plan --
    // the same typed verb `windows_harness_operator_session_verbs_e2e.rs`
    // exercises) eventually shows up in a RuntimeInventoryChanged push.
    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    ).unwrap();
    assert_eq!(session.node_id, node_id.as_str());

    // Wait for the session to be RUNNING, not merely present: a force-stop
    // issued while the provider child is still starting races the node's
    // own lifecycle (the sibling verbs E2E never hits this because it
    // exercises input/resize first). Running-then-stop is the meaningful
    // product sequence this test pins.
    wait_for_event(&mut event_rx, &mut last_sequence, |event| {
        matches!(
            event,
            HarnessOperatorEventV1::RuntimeInventoryChanged { node, .. }
                if find_session_in_node(node, &session).is_some_and(|found| {
                    found.status == gate4agent_harness_api::HarnessRuntimeSessionStatusV1::Running
                }),
        )
    }).await;

    // StopSession eventually shows up as another inventory push with the
    // session gone.
    client.stop_session(session.clone(), true).unwrap();
    wait_for_event(&mut event_rx, &mut last_sequence, |event| {
        matches!(
            event,
            HarnessOperatorEventV1::RuntimeInventoryChanged { node, .. }
                if node.node_id == session.node_id
                    && find_session_in_node(node, &session).is_none(),
        )
    }).await;

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
