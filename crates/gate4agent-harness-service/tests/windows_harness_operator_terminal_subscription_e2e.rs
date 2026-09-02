#![cfg(windows)]

//! E2E coverage for the `SubscribeTerminal` operator-wire push subscription
//! (see `docs/gate4agent/plans/gate4agent-terminal-push-channel-2026-08-26.md`
//! step 5): a real fixture node + C2 + harness host, the same three-process
//! shape `windows_harness_operator_event_subscription_e2e.rs` already uses
//! for `SubscribeEvents`. This is deliberately NOT a reducer-level test --
//! it drives `handle_connection`'s new branch, the `SubscribeTerminal` wire
//! encode/decode, `deny_unknown_fields`, and the v12 version gate all at
//! once, through a real `gate4agent-harness-client::subscribe_terminal`
//! socket, exactly the "green while broken" trap the plan calls out that a
//! `TerminalSubscriberRegistry::publish` unit test alone would miss.
//!
//! Asserts two things: the pushed `TerminalFrame` carries the exact session
//! and real PTY bytes (the fixture agent's own `fixture-ready>` banner, the
//! same stable checkpoint `gate4agent-c2`'s own
//! `windows_fixture_terminal_frame_push_e2e.rs` already uses to prove a real
//! terminal frame reaches a downstream consumer), and that a SECOND,
//! ordinary `TerminalRead` poll against the same session -- the fallback
//! path -- returns that exact frame too, proving the push and the poll read
//! the same underlying `TerminalBufferRegistry` ring and agree.

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
    HarnessOperatorCredential, HarnessOperatorTerminalEventV1, HARNESS_TERMINAL_PAGE_LIMIT_MAX,
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
            "gate4agent-harness-terminal-subscription-{}-{}-{}",
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
        r"\\.\pipe\gate4agent-harness-terminal-subscription-{label}-{}-{}",
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

fn terminal_event_sequence(event: &HarnessOperatorTerminalEventV1) -> u64 {
    match event {
        HarnessOperatorTerminalEventV1::TerminalFrame { sequence, .. }
        | HarnessOperatorTerminalEventV1::Ping { sequence } => *sequence,
    }
}

/// Pulls events off the background reader thread's channel (see the test
/// body: `HarnessTerminalSubscription::next_event` blocks, so it is drained
/// on its own `std::thread`, not directly inside this async test, mirroring
/// `windows_harness_operator_event_subscription_e2e.rs`'s own `wait_for_
/// event`) until one satisfies `matches`, asserting every event observed
/// along the way carries a strictly increasing per-subscription `sequence`.
async fn wait_for_terminal_event(
    event_rx: &mut UnboundedReceiver<HarnessOperatorTerminalEventV1>,
    last_sequence: &mut Option<u64>,
    mut matches: impl FnMut(&HarnessOperatorTerminalEventV1) -> bool,
) -> HarnessOperatorTerminalEventV1 {
    timeout(Duration::from_secs(15), async {
        loop {
            let event = event_rx.recv().await
                .expect("terminal subscription reader thread ended early");
            let sequence = terminal_event_sequence(&event);
            if let Some(previous) = *last_sequence {
                assert!(
                    sequence > previous,
                    "terminal event sequence must be strictly increasing per subscription",
                );
            }
            *last_sequence = Some(sequence);
            if matches(&event) {
                return event;
            }
        }
    }).await.expect("expected terminal event never arrived")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_operator_terminal_subscription_pushes_the_rings_newest_frame() {
    require_headless_supervisor();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_writer(std::io::stderr)
        .try_init();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("terminal-subscription-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "terminal-subscription-node-token";
    let c2_token = "terminal-subscription-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "f".repeat(64),
    )).unwrap();
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("terminal-subscription-r1").unwrap();

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

    // A directly spawned session (the same typed verb `windows_harness_
    // operator_event_subscription_e2e.rs`/`windows_harness_operator_session_
    // verbs_e2e.rs` already exercise) -- its scripted `fixture-ready>` banner
    // is the same stable, real-PTY checkpoint `windows_fixture_terminal_
    // frame_push_e2e.rs` (gate4agent-c2) already uses to prove a real
    // terminal frame reaches a downstream consumer.
    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
        None,
    ).unwrap();
    assert_eq!(session.node_id, node_id.as_str());

    // Subscribed AFTER spawning, deliberately: the seed this subscription
    // receives on registration must come from `TerminalBufferRegistry::
    // latest` -- whatever the ring already holds by the time `SubscribeTerminal`
    // registers -- not a frame this subscription happens to race live.
    let subscription = client.subscribe_terminal(vec![session.clone()]).unwrap();
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
    let pushed_event = wait_for_terminal_event(&mut event_rx, &mut last_sequence, |event| {
        matches!(
            event,
            HarnessOperatorTerminalEventV1::TerminalFrame { session: event_session, frame, .. }
                if event_session == &session
                    && String::from_utf8_lossy(&frame.formatted).contains("fixture-ready"),
        )
    }).await;
    let HarnessOperatorTerminalEventV1::TerminalFrame { session: pushed_session, frame: pushed_frame, .. } =
        pushed_event
    else {
        panic!("expected a TerminalFrame event");
    };
    assert_eq!(pushed_session, session);
    assert!(!pushed_frame.formatted.is_empty());

    // The fallback path (`TerminalRead`, the ordinary one-shot request) must
    // agree with what the push already delivered -- both read the exact
    // same underlying `TerminalBufferRegistry` ring, proven directly rather
    // than inferred.
    let page = client
        .terminal_read(session.clone(), None, HARNESS_TERMINAL_PAGE_LIMIT_MAX)
        .unwrap();
    let matching_frame = page.frames.iter()
        .find(|frame| frame.sequence == pushed_frame.sequence)
        .expect("TerminalRead page must contain the exact frame the push already delivered");
    assert_eq!(matching_frame.formatted, pushed_frame.formatted);

    client.stop_session(session, true).unwrap();

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
