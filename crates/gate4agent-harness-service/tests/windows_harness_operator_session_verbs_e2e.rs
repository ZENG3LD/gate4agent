#![cfg(windows)]

//! E2E coverage for the nine typed operator session verbs (`SpawnSession`,
//! `WriteSessionInput`, `ResizeSession`, `StopSession`, `ControlSession`,
//! `WriteSessionBytes`, `PasteSession`, `RemoveSession`, `ResumeSession`)
//! added alongside the existing node-scoped read family. Fixture node + C2 +
//! harness host, the same three-process shape `windows_harness_run_
//! workspace_read_e2e.rs`/`windows_harness_mode_hierarchy_e2e.rs` already
//! use, but exercising the direct-spawn path instead of the Task/Run/
//! launch-plan catalog: no `HarnessLaunchCatalog` entry is registered,
//! because `SpawnSession` never consults one (see the doc comment on
//! `HarnessOperatorRequestV1::SpawnSession`).
//!
//! `RemoveSession`/`ResumeSession` are exercised against the session this
//! test's own `StopSession { force: true }` step already tears down, not a
//! freshly present one: a settled `StopSession` (forced or graceful) already
//! triggers the harness's own best-effort node-side reap (see
//! `HarnessC2Adapter::remove_stopped_session`'s doc comment), so by the time
//! that step's own roster-clearance wait below completes, the node no
//! longer recognizes the address at all. This is deterministic (no race
//! against that reap: the wait already settled) and reproduces something
//! `RemoveSession`/`ResumeSession` must both handle correctly on their own
//! -- an operator call against a session the node has already forgotten --
//! rather than a race-prone "beat the reap to it" positive-clearance
//! assertion. It also answers, empirically, whether the node supports
//! resuming a removed session: it does not (`ResumeSession`, like every
//! other session-control verb, first re-validates the address the same way
//! `RemoveSession` just found gone).
//!
//! `PasteSession` is exercised as a negative case, not alongside
//! `ControlSession`/`WriteSessionBytes`'s positive round trip: unlike those
//! two (relayed with no runtime-policy gate at all -- see the node's
//! `NodeRequest::TerminalBytes`/`TerminalControl` handlers), the node's own
//! `NodeRequest::Paste` handler requires `ProviderRuntimeRequirement::
//! SemanticPrompt`, which this fixture's plain PTY-echo `AgentSpec` (no
//! declared semantic PTY adapter) never admits -- the same policy gate
//! `windows_fixture_runtime_policy_e2e.rs` (gate4agent-node) already proves
//! rejects a semantic `NodeRequest::Prompt`/`Paste`/`ResumeWithPrompt` under
//! a raw-PTY-only policy. So this fixture's session can only ever prove
//! `PasteSession` relays the node's rejection faithfully end to end, not
//! that a paste actually lands -- the real, empirically verified behavior.

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
    HarnessOperatorCredential, HarnessOperatorHostErrorV1, HarnessRuntimeSessionAddressV1,
    HarnessRuntimeSessionV1, HarnessTerminalControlV1, HARNESS_TERMINAL_PAGE_LIMIT_MAX,
};
use gate4agent_harness_client::{HarnessOperatorClient, HarnessOperatorClientError};
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
use tokio::time::{sleep, timeout};

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
            "gate4agent-harness-session-verbs-{}-{}-{}",
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
        r"\\.\pipe\gate4agent-harness-session-verbs-{label}-{}-{}",
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

/// Finds the spawned session inside a `RuntimeInventoryList` page by its
/// harness wire address (node/workspace/instance/generation, all typed
/// bounds already validated on the way in by `HarnessRuntimeSessionAddressV1
/// ::validate()`), navigating the same
/// node -> workspace -> session path a harness-mode TUI sidebar would.
fn find_runtime_session<'a>(
    page: &'a gate4agent_harness_api::HarnessRuntimeInventoryPageV1,
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

async fn wait_for_terminal_text(
    client: &HarnessOperatorClient,
    session: &HarnessRuntimeSessionAddressV1,
    expected_text: &str,
) {
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.terminal_read(
                session.clone(),
                None,
                HARNESS_TERMINAL_PAGE_LIMIT_MAX,
            ) {
                let seen = page.frames.iter().any(|frame| {
                    String::from_utf8_lossy(&frame.formatted).contains(expected_text)
                        || frame.scrollback_formatted.iter().any(|line| {
                            String::from_utf8_lossy(line).contains(expected_text)
                        })
                });
                if seen {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap_or_else(|_| {
        panic!("terminal never rendered the expected text: {expected_text}")
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_operator_session_verbs_spawn_input_resize_control_stop_remove_resume_round_trip() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("session-verbs-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "session-verbs-node-token";
    let c2_token = "session-verbs-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "f".repeat(64),
    )).unwrap();
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("session-verbs-r1").unwrap();

    // `new_fixture` (not `new_clean_exit_fixture`): the interactive PTY
    // fixture prints `fixture-ready>`, then echoes typed input back through
    // the PTY's own local echo -- exactly the round trip
    // `WriteSessionInput`/`TerminalRead` need to exercise.
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
    // No `HarnessLaunchCatalog` entry: a direct `SpawnSession` never
    // resolves one (unlike the Task/Run path's `StartTaskV2`), so the
    // default (empty) catalogs are sufficient for this test.
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
    let client = HarnessOperatorClient::new(harness_endpoint, operator_credential).unwrap();

    // SpawnSession via the operator wire -- no Task/Run/plan, direct spawn.
    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    ).unwrap();
    assert_eq!(session.node_id, node_id.as_str());
    assert_eq!(session.workspace_id, workspace_id.as_str());
    assert_ne!(session.instance_id, 0);
    assert_ne!(session.generation, 0);

    // The spawned session appears in the runtime inventory -- confirming
    // the read model (`HarnessRuntimeInventoryCache`) mirrors an
    // operator-spawned session with no Task/Run binding, exactly as the
    // seam map's read-model analysis (seam 6) concluded.
    let inventory_session = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if let Some(found) = find_runtime_session(&page, &session) {
                    return found.clone();
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("spawned session never appeared in the runtime inventory");
    assert_eq!(inventory_session.provider, "claude");

    // Wait for the fixture's own ready prompt before sending input.
    wait_for_terminal_text(&client, &session, "fixture-ready>").await;

    // WriteSessionInput -> TerminalRead shows the echoed bytes: the PTY's
    // own local echo of the typed characters, not yet the fixture script's
    // application-level `fixture-echo:` response (that needs a submitted
    // line -- see `ControlSession`'s `TerminalControl::Enter` below, which
    // submits it).
    let probe_text = "session-verb-e2e-probe";
    client.write_session_input(session.clone(), probe_text.to_owned()).unwrap();
    wait_for_terminal_text(&client, &session, probe_text).await;

    // ResizeSession ack, then the next terminal frame reports the new size.
    client.resize_session(
        session.clone(),
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 30, columns: 100 },
    ).unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.terminal_read(
                session.clone(),
                None,
                HARNESS_TERMINAL_PAGE_LIMIT_MAX,
            ) {
                if page.frames.iter().any(|frame| frame.size.rows == 30 && frame.size.columns == 100) {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("resized terminal size never appeared in a terminal frame");

    // WriteSessionBytes appends to the same still-unsubmitted line via a raw
    // node-level byte write, then ControlSession's `TerminalControl::Enter`
    // submits it. Asserting on the fixture's application-level `fixture-
    // echo:` response (rather than the raw terminal rendering of either
    // fragment) proves both verbs actually reached the PTY's input stream,
    // in order.
    client.write_session_bytes(session.clone(), b"-bytes".to_vec()).unwrap();
    client.control_session(session.clone(), HarnessTerminalControlV1::Enter).unwrap();
    let expected_line = format!("fixture-echo:{probe_text}-bytes");
    wait_for_terminal_text(&client, &session, &expected_line).await;

    // PasteSession, negative: see the module doc comment for why this
    // fixture's plain PTY-echo `AgentSpec` never admits a semantic paste --
    // `NodeFailureCode::UnsupportedCapability` relays through
    // `map_session_control_error`'s same catch-all every other session-
    // control verb's capability rejection already uses.
    let paste_rejected = client.paste_session(session.clone(), "rejected paste".to_owned());
    assert!(matches!(
        paste_rejected,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Unavailable)),
    ));

    // StopSession, then the session leaves the runtime inventory roster.
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
    }).await.expect("stopped session never left the runtime inventory roster");

    // RemoveSession/ResumeSession against the now-stopped-and-reaped session
    // -- see the module doc comment for why this is the deterministic case
    // to assert rather than a race against the reap that already ran. Both
    // fail the same way every other session-control verb already fails
    // against an unrecognized address (`controlled_session`'s `validate_
    // address` check, `NodeFailureCode::UnknownSession` -> the same
    // `map_session_control_error` catch-all `HarnessOperatorHostErrorV1::
    // Internal` an unknown-session `WriteSessionInput`/`ResizeSession`/
    // `StopSession` would also get -- not a dedicated "not found" host
    // error; that pre-existing catch-all is unchanged by this session's
    // addition of `RemoveSession`/`ResumeSession` alongside the other four).
    let remove_after_reap = client.remove_session(session.clone());
    assert!(matches!(
        remove_after_reap,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Internal)),
    ));
    let resume_after_reap = client.resume_session(
        session.clone(),
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    );
    assert!(matches!(
        resume_after_reap,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::Internal)),
    ));

    // Negative: a bogus provider profile is a typed rejection, not a spawn.
    let rejected = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        "no-such-profile".to_owned(),
        HarnessExecutionModeV1::Pty,
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
    );
    assert!(matches!(
        rejected,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}
