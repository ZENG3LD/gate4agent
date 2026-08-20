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
    HarnessExecutionModeV1, HarnessHostPathV1, HarnessNativeSessionCatalogScopeV1,
    HarnessNativeSessionRouteV1, HarnessOperatorActionV1, HarnessOperatorEventV1,
    HarnessOperatorHostErrorV1, HarnessOperatorIntentV1, HarnessOperatorRequestRefV1,
    HarnessRepositoryPathV1, HarnessRuntimeInventoryPageV1, HarnessRuntimeNodeInventoryV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeSessionStatusV1, HarnessRuntimeSessionV1,
    HarnessRuntimeTerminalSizeV1, HarnessTaskStateV1, HarnessTerminalControlV1,
    HarnessWorkspaceFileContentV1,
};
use gate4agent_harness_client::{HarnessOperatorClient, HarnessOperatorClientError};
use gate4agent_harness_light::start_harness_light;
use gate4agent_node::protocol::{
    NodeId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision, WorkspaceId,
};
use gate4agent_node::{NodeServer, NodeServerConfig, SpawnProfileRegistry, WorkspaceConfig};
use gate4agent_types::{AgentId, TerminalSize};
use tokio::sync::mpsc::UnboundedReceiver;
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

/// Same node -> workspace -> session navigation as [`find_runtime_session`],
/// but against a single `HarnessRuntimeNodeInventoryV1` -- the shape a
/// `SubscribeEvents` push (`SnapshotBaseline`'s `nodes` list,
/// `RuntimeInventoryChanged`'s `node`) carries, not a paged
/// `RuntimeInventoryList` reply. Mirrors
/// `gate4agent-harness-service`'s own subscription E2E's identical helper.
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

/// The `sequence` every `HarnessOperatorEventV1` variant carries -- pulled
/// out once so `wait_for_event` can assert monotonicity without a
/// per-variant match at every call site.
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

/// Pulls events off one subscription's background reader-thread channel
/// (`HarnessEventSubscription::next_event` blocks, so each subscription is
/// drained on its own `std::thread`, not directly inside the async test
/// body -- see the test's own subscribe section) until one satisfies
/// `matches`, asserting every event observed along the way -- not just the
/// matched one -- carries a strictly increasing `sequence` for THIS
/// subscription. Mirrors `gate4agent-harness-service`'s own subscription
/// E2E's identical helper.
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

/// Spawns a background thread draining `client.subscribe_events()` into an
/// unbounded channel the async test body can `.recv().await` from --
/// `HarnessEventSubscription::next_event` blocks the calling thread, so it
/// must never run directly inside the test's own async task (see
/// `wait_for_event`'s doc comment).
fn spawn_subscription_reader(
    client: &HarnessOperatorClient,
) -> UnboundedReceiver<HarnessOperatorEventV1> {
    let subscription = client.subscribe_events().unwrap();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
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
    event_rx
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

/// A2 coverage: node-scoped workspace read/write (CAS conflict included),
/// management (`RegisterWorkspace`/`UnregisterWorkspace` with inventory
/// convergence, `BrowseHostDirectories`), `TerminalRead` against a live
/// spawned session's fixture echo, and native history (relay proven, not
/// staged -- see this test's own native-history section for why). Same
/// three-process fixture shape as the session-verbs E2E above, with its own
/// atomic-counter-suffixed fixture paths/pipe names so the two tests never
/// collide when run in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_light_operator_workspace_history_management_and_terminal() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("light-a2-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "light-a2-node-token";
    let c2_token = "light-a2-c2-token";
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("light-a2-r1").unwrap();

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

    let running = start_harness_light(&control_endpoint, c2_token).await.unwrap();
    let client = HarnessOperatorClient::new(running.operator_endpoint(), running.operator_credential()).unwrap();

    // --- Node-scoped workspace read/write, including the CAS conflict ---

    let inspection = client
        .inspect_node_workspace(node_id.as_str().to_owned(), workspace_id.as_str().to_owned())
        .unwrap();
    assert!(inspection.entries.is_empty(), "fixture workspace starts empty");
    assert!(!inspection.git.is_repository, "fixture workspace is not git-initialized");

    let file_path = HarnessRepositoryPathV1::new("notes.md").unwrap();
    let created = client.create_node_workspace_file(
        node_id.as_str().to_owned(), workspace_id.as_str().to_owned(), file_path.clone(),
    ).unwrap();
    let initial_revision = created.revision.clone().expect("a freshly created file carries a revision");

    let written = client.write_node_workspace_file(
        node_id.as_str().to_owned(), workspace_id.as_str().to_owned(), file_path.clone(),
        "hello from the A2 slice\n".to_owned(), initial_revision.clone(),
    ).unwrap();
    let fresh_revision = written.revision.clone().expect("a written file carries a revision");
    assert_ne!(fresh_revision, initial_revision, "a successful write advances the CAS revision");

    let read_back = client.read_node_workspace_file(
        node_id.as_str().to_owned(), workspace_id.as_str().to_owned(), file_path.clone(),
    ).unwrap();
    match read_back.content {
        HarnessWorkspaceFileContentV1::Utf8 { text, .. } => {
            assert_eq!(text, "hello from the A2 slice\n");
        }
        other => panic!("expected utf8 file content, got {other:?}"),
    }

    // The stale (pre-write) revision must now be rejected as a typed
    // `Conflict`, not silently overwrite the fresher content.
    let stale_write = client.write_node_workspace_file(
        node_id.as_str().to_owned(), workspace_id.as_str().to_owned(), file_path.clone(),
        "clobber attempt\n".to_owned(), initial_revision,
    );
    assert!(
        matches!(
            stale_write,
            Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Conflict)),
        ),
        "a stale CAS revision must surface as a typed Conflict, got {stale_write:?}",
    );

    let dir_path = HarnessRepositoryPathV1::new("subdir").unwrap();
    client.create_node_workspace_directory(
        node_id.as_str().to_owned(), workspace_id.as_str().to_owned(), dir_path,
    ).unwrap();
    let inspection = client
        .inspect_node_workspace(node_id.as_str().to_owned(), workspace_id.as_str().to_owned())
        .unwrap();
    let entry_names = inspection.entries.iter()
        .map(|entry| entry.relative_path.as_str().to_owned())
        .collect::<Vec<_>>();
    assert!(entry_names.contains(&"notes.md".to_owned()), "inspection lists the created file");
    assert!(entry_names.contains(&"subdir".to_owned()), "inspection lists the created directory");

    // --- BrowseHostDirectories ---

    let host_root = HarnessHostPathV1::new(fixture.root.to_string_lossy().into_owned()).unwrap();
    let listing = client
        .browse_host_directories(node_id.as_str().to_owned(), Some(host_root), None)
        .unwrap();
    assert!(
        listing.entries.iter().any(|entry| entry.display_name == "workspace"),
        "browsing the fixture root lists the workspace directory",
    );

    // --- RegisterWorkspace / UnregisterWorkspace, inventory convergence ---

    let second_workspace_id = "second".to_owned();
    let second_root = fixture.root.join("second-workspace");
    fs::create_dir_all(&second_root).unwrap();
    let second_host_root = HarnessHostPathV1::new(second_root.to_string_lossy().into_owned()).unwrap();
    client.register_workspace(node_id.as_str().to_owned(), second_workspace_id.clone(), second_host_root).unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if page.nodes.iter()
                    .find(|node| node.node_id == node_id.as_str())
                    .is_some_and(|node| node.inventory.workspaces.contains_key(&second_workspace_id))
                {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("registered workspace never appeared in the runtime inventory roster");

    client.unregister_workspace(node_id.as_str().to_owned(), second_workspace_id.clone()).unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if page.nodes.iter()
                    .find(|node| node.node_id == node_id.as_str())
                    .is_some_and(|node| !node.inventory.workspaces.contains_key(&second_workspace_id))
                {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("unregistered workspace never left the runtime inventory roster");

    // --- TerminalRead against a live spawned session's fixture echo ---

    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    ).unwrap();

    let first_page = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.terminal_read(session.clone(), None, 64) {
                if !page.frames.is_empty() {
                    return page;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("the fixture session never produced a terminal frame");
    assert!(
        first_page.frames.windows(2).all(|pair| pair[0].sequence < pair[1].sequence),
        "frame sequences within one page are strictly increasing",
    );
    assert_eq!(first_page.dropped, 0);
    let last_sequence = first_page.frames.last().unwrap().sequence;

    // Paging with `after_sequence` pinned to the last frame already seen:
    // every frame in the next page (if any land before the deadline) must
    // have a strictly greater sequence -- the advancing-cursor contract
    // `crate::terminal`'s own unit tests already prove against the ring
    // directly; this proves the same contract end-to-end through the wire.
    client.write_session_input(session.clone(), "a2-terminal-read-probe".to_owned()).unwrap();
    let second_page = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.terminal_read(session.clone(), Some(last_sequence), 64) {
                if !page.frames.is_empty() {
                    return page;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("no further terminal frame arrived after the input probe");
    assert!(
        second_page.frames.iter().all(|frame| frame.sequence > last_sequence),
        "every frame after the cursor has a strictly greater sequence",
    );

    // --- Native history: relay proven, not staged ---
    //
    // The fixture node carries no staged native-provider history (no
    // `~/.claude/projects/...`-shaped fixture directory is part of this
    // crate's E2E fixtures, unlike the full harness's own native-history
    // E2Es which stage one deliberately); standing one up here purely to
    // exercise `CatalogNativeSessions` would be disproportionate to this
    // slice. What this proves instead: the request reaches the node and
    // comes back as a well-formed, typed reply either way -- an empty
    // catalog (the honest answer for "no history exists") or a typed
    // rejection -- never a wire-level failure or a silently wrong shape.
    let native_route = HarnessNativeSessionRouteV1 {
        node_id: node_id.as_str().to_owned(),
        incarnation_id: session.incarnation_id.clone(),
        scope: HarnessNativeSessionCatalogScopeV1::Unregistered,
        workspace_id: None,
        provider: "claude".to_owned(),
    };
    match client.catalog_native_sessions(native_route, 16) {
        Ok(cataloged) => assert!(
            cataloged.entries.len() <= 16,
            "an unstaged provider history catalogs as empty or small, not overflowing the page",
        ),
        Err(HarnessOperatorClientError::Host(_)) => {}
        Err(other) => panic!("native-history relay failed at the wire/transport level: {other:?}"),
    }

    client.stop_session(session, true).unwrap();

    running.shutdown().await.unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}

/// A3 coverage: `SubscribeEvents` serves the SAME V11 push contract the full
/// harness does (see `gate4agent-harness-service/tests/windows_harness_
/// operator_event_subscription_e2e.rs`, this test's own template) -- a
/// subscriber gets a `SnapshotBaseline` immediately (nodes from the live
/// inventory, tasks/runs always empty: light mode has no task kernel, by
/// canon), then `RuntimeInventoryChanged` pushes as a directly spawned
/// session appears and disappears from the roster (the A1 stop-reap makes
/// the disappearance converge, see `crate::relay::spawn_stop_reap`). Two
/// independent subscribers prove the registry fans the same roster changes
/// out to every live subscriber, each with its own baseline and its own
/// monotonic sequence space. Same three-process fixture shape as the two
/// tests above, with its own atomic-counter-suffixed fixture paths/pipe
/// names so all three never collide when run in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_light_operator_event_subscription_baseline_and_inventory_round_trip() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("light-events-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "light-events-node-token";
    let c2_token = "light-events-c2-token";
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("light-events-r1").unwrap();

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

    let running = start_harness_light(&control_endpoint, c2_token).await.unwrap();
    let client = HarnessOperatorClient::new(running.operator_endpoint(), running.operator_credential()).unwrap();

    // Two independent subscribers, each drained on its own thread -- see
    // `spawn_subscription_reader`'s own doc comment for why this cannot run
    // directly inside this async test body.
    let mut event_rx = spawn_subscription_reader(&client);
    let mut last_sequence = None;
    let mut second_event_rx = spawn_subscription_reader(&client);
    let mut second_last_sequence = None;

    // SnapshotBaseline always arrives first for each subscriber, independently:
    // no tasks/runs (canon: no kernel), and the fixture node already present
    // (the initial sweep in `start_harness_light` completed before this test
    // could even connect).
    for (event_rx, last_sequence) in [
        (&mut event_rx, &mut last_sequence),
        (&mut second_event_rx, &mut second_last_sequence),
    ] {
        let baseline = wait_for_event(event_rx, last_sequence, |event| {
            matches!(event, HarnessOperatorEventV1::SnapshotBaseline { .. })
        }).await;
        match baseline {
            HarnessOperatorEventV1::SnapshotBaseline { tasks, runs, nodes, .. } => {
                assert!(tasks.is_empty(), "light mode has no task kernel: baseline tasks must be empty");
                assert!(runs.is_empty(), "light mode has no task kernel: baseline runs must be empty");
                assert!(
                    nodes.iter().any(|node| node.node_id == node_id.as_str()),
                    "the fixture node must already be present in the baseline",
                );
            }
            other => panic!("expected SnapshotBaseline, got {other:?}"),
        }
    }

    // SpawnSession via the operator wire; both subscribers eventually see a
    // RuntimeInventoryChanged carrying the session Running, not merely
    // present (mirrors the full harness's own subscription E2E: a force-stop
    // issued while the provider child is still starting races the node's
    // own lifecycle).
    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    ).unwrap();
    assert_eq!(session.node_id, node_id.as_str());

    for (event_rx, last_sequence) in [
        (&mut event_rx, &mut last_sequence),
        (&mut second_event_rx, &mut second_last_sequence),
    ] {
        wait_for_event(event_rx, last_sequence, |event| {
            matches!(
                event,
                HarnessOperatorEventV1::RuntimeInventoryChanged { node, .. }
                    if find_session_in_node(node, &session).is_some_and(|found| {
                        found.status == HarnessRuntimeSessionStatusV1::Running
                    }),
            )
        }).await;
    }

    // StopSession, then both subscribers eventually see another inventory
    // push with the session gone -- the A1 stop-reap
    // (`crate::relay::spawn_stop_reap`) is what makes this converge: a
    // settled Stop does not unbind the session on the node side by itself.
    client.stop_session(session.clone(), true).unwrap();
    for (event_rx, last_sequence) in [
        (&mut event_rx, &mut last_sequence),
        (&mut second_event_rx, &mut second_last_sequence),
    ] {
        wait_for_event(event_rx, last_sequence, |event| {
            matches!(
                event,
                HarnessOperatorEventV1::RuntimeInventoryChanged { node, .. }
                    if node.node_id == session.node_id && find_session_in_node(node, &session).is_none(),
            )
        }).await;
    }

    running.shutdown().await.unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
