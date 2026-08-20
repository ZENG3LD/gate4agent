#![cfg(windows)]

//! E2E coverage for `gate4agent-harness-light`'s A1 slice: fixture node +
//! real C2 + `start_harness_light` (no SQLite task kernel, no
//! `HarnessC2Adapter` -- this crate owns its own C2 connection directly),
//! then an ordinary `HarnessOperatorClient` (the exact client the full
//! harness's own operator E2Es use, see
//! `gate4agent-harness-service/tests/windows_harness_operator_session_verbs_e2e.rs`)
//! connects with the credential `start_harness_light` generated in-process.
//!
//! Same three-process fixture shape (node/C2/host) and the same
//! atomic-counter-suffixed fixture paths/pipe names that reference test
//! uses, but with `start_harness_light` in place of
//! `start_harness_host_with_operator_and_catalogs` and no
//! `HarnessService`/`ObservationService` at all: light mode has neither.
//!
//! `WriteSessionInput`/`ControlSession(Enter)` are exercised for acceptance
//! only, not for their terminal effect: A1 has no `TerminalRead`
//! (`TerminalBufferRegistry` is deliberately deferred past this slice, see
//! `gate4agent-harness-light`'s crate doc and the coordinator report), so
//! unlike the full harness's own session-verbs E2E this test cannot poll a
//! terminal frame for the echoed/submitted text -- only that both verbs
//! relay and return their typed acks.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    HarnessExecutionModeV1, HarnessOperatorActionV1, HarnessOperatorHostErrorV1,
    HarnessOperatorIntentV1, HarnessOperatorRequestRefV1, HarnessRuntimeInventoryPageV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeSessionStatusV1, HarnessRuntimeSessionV1,
    HarnessRuntimeTerminalSizeV1, HarnessTaskStateV1, HarnessTerminalControlV1,
};
use gate4agent_harness_client::{HarnessOperatorClient, HarnessOperatorClientError};
use gate4agent_harness_light::start_harness_light;
use gate4agent_node::protocol::{
    NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_types::{AgentId, TerminalSize};
use tokio::time::{sleep, timeout};

struct FixturePaths {
    root: PathBuf,
    workspace: PathBuf,
    node_state: PathBuf,
}

impl FixturePaths {
    fn new() -> Self {
        static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "gate4agent-harness-light-operator-{}-{}-{}",
            std::process::id(),
            unix_time_ms(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self { node_state: root.join("node-state.json"), workspace, root }
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
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().try_into().unwrap()
}

fn pipe(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        r"\\.\pipe\gate4agent-harness-light-{label}-{}-{}",
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

/// Finds the spawned session inside a `RuntimeInventoryList` page -- same
/// node -> workspace -> session navigation as the full harness's own
/// operator session-verbs E2E.
fn find_runtime_session<'a>(
    page: &'a HarnessRuntimeInventoryPageV1,
    address: &HarnessRuntimeSessionAddressV1,
) -> Option<&'a HarnessRuntimeSessionV1> {
    page.nodes.iter()
        .find(|node| node.node_id == address.node_id)?
        .inventory.workspaces.get(&address.workspace_id)?
        .sessions.iter()
        .find(|session| {
            session.instance_id == address.instance_id && session.generation == address.generation
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_light_operator_session_verbs_and_typed_rejections() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("light-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "light-node-token";
    let c2_token = "light-c2-token";
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("light-r1").unwrap();

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

    // No SQLite, no `HarnessC2Adapter`: `start_harness_light` owns its own
    // C2 connection directly and mints its own operator credential.
    let running = start_harness_light(&control_endpoint, c2_token).await.unwrap();
    let client = HarnessOperatorClient::new(running.operator_endpoint(), running.operator_credential()).unwrap();

    // RuntimeInventoryList shows the node -- served from the maintained
    // roster, no `HarnessRuntimeInventoryCache`/observation-resync involved.
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if page.nodes.iter().any(|node| node.node_id == node_id.as_str()) {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("node never appeared in the light harness's runtime inventory");

    // SpawnSession via the operator wire, relayed straight to C2/Node.
    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    ).unwrap();
    assert_eq!(session.node_id, node_id.as_str());
    assert_eq!(session.workspace_id, workspace_id.as_str());
    assert_ne!(session.instance_id, 0);
    assert_ne!(session.generation, 0);

    // The spawn eagerly refreshed the roster (see `crate::relay`'s doc
    // comment); this waits for the node to actually report the session
    // `Running`, not just present.
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if find_runtime_session(&page, &session)
                    .is_some_and(|found| found.status == HarnessRuntimeSessionStatusV1::Running)
                {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("spawned session never reported Running in the light harness's runtime inventory");

    // WriteSessionInput + ControlSession(Enter): accepted and relayed --
    // see the module doc comment for why this cannot also assert the
    // terminal effect in A1.
    client.write_session_input(session.clone(), "session-verb-e2e-probe".to_owned()).unwrap();
    client.control_session(session.clone(), HarnessTerminalControlV1::Enter).unwrap();

    // StopSession, then the session leaves the runtime inventory roster
    // (the eager post-mutation refresh again, this time dropping it).
    client.stop_session(session.clone(), true).unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if find_runtime_session(&page, &session).is_none() {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("stopped session never left the light harness's runtime inventory roster");

    // TasksList: light mode has no task kernel, so this is an honestly
    // empty page, not a typed rejection.
    let tasks = client.tasks_list(None, None, 16).unwrap();
    assert!(tasks.tasks.is_empty());
    assert!(tasks.next_cursor.is_none());

    // SubmitIntent (create-task): a typed `Unsupported` rejection -- the
    // task-kernel mutation family this A1 slice does not implement. The
    // action itself is a real, independently-validating `CreateTask` (the
    // same fixture `gate4agent-harness-api`'s own `operator_v3_intent_is_
    // authority_free_and_fails_closed_on_v2` test uses), so this proves the
    // light harness's own dispatcher rejects it -- not that the request
    // never made it onto the wire.
    let intent = HarnessOperatorIntentV1 {
        request_ref: HarnessOperatorRequestRefV1::new(format!("hireq_{}", "1".repeat(24))).unwrap(),
        submitted_at_unix_ms: unix_time_ms(),
        action: HarnessOperatorActionV1::CreateTask {
            title: "Harness-owned identity".to_owned(),
            body: "Typed user intent".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Backlog,
        },
    };
    assert!(matches!(
        client.submit_intent(intent),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Unsupported)),
    ));

    running.shutdown().await.unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
