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
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::C2Client;
use gate4agent_harness_api::{
    HarnessHostPathV1, HarnessOperatorCredential, HarnessOperatorHostErrorV1,
    HarnessRuntimeSessionAddressV1,
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
        None,
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
    // `map_session_control_error`'s dedicated, permanent-refusal arm every
    // other session-control verb's capability rejection already uses (not
    // `Unavailable`, which would misread a "this can never succeed" refusal
    // as a transient one worth retrying).
    let paste_rejected = client.paste_session(session.clone(), "rejected paste".to_owned());
    assert!(matches!(
        paste_rejected,
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::UnsupportedCapability)),
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
        None,
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

/// Finds a managed-session record inside a `RuntimeInventoryList` page by
/// node + record id -- the managed-record sibling of `find_runtime_session`.
fn find_managed_session<'a>(
    page: &'a gate4agent_harness_api::HarnessRuntimeInventoryPageV1,
    node_id: &str,
    record_id: &str,
) -> Option<&'a gate4agent_harness_api::HarnessRuntimeManagedSessionV1> {
    page.nodes.iter()
        .find(|node| node.node_id == node_id)?
        .inventory.managed_sessions.iter()
        .find(|record| record.record_id == record_id)
}

/// E2E coverage for the session-record operator verb family added alongside
/// the nine direct session verbs above (`PreviewSessionRecord`/
/// `ResumeSessionRecord`/`RenameSessionRecord`/`SetSessionTask`/
/// `ForgetSessionRecord`/`IndexProviderSession`/`IndexNativeSession`). Same
/// fixture shape, but no PTY fixture spawn is needed: `IndexProviderSession`
/// with no `transcript_path` (the only shape `NodeRequest::
/// IndexProviderSession`'s handler accepts -- see `index_provider_session_
/// with_policy`'s `allow_validated_transcript_path: false`) creates a
/// `ManagedSessionRecord` directly against the node's session-record store,
/// no process involved. That gives a real, live `record_id` to exercise the
/// mutation family's full round trip (index -> discover -> rename -> forget)
/// against genuine node state.
///
/// `ResumeSessionRecord`/`PreviewSessionRecord`/`SetSessionTask`/
/// `IndexNativeSession` are covered only through their real wire-relay and
/// node-rejection paths, not a full success round trip: a record indexed
/// this way carries no resumable provider transcript (staging one would mean
/// fabricating a real on-disk native-session file matching a provider's
/// exact format, and `SetSessionTask` binding to a real Harness task is a
/// second, unrelated fixture concern) -- see the module doc comment's
/// `PasteSession` precedent for the same "prove the rejection relays
/// faithfully, not a success this fixture cannot honestly produce" choice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_operator_session_record_family_index_rename_forget_round_trip_and_rejections() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("session-record-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "session-record-node-token";
    let c2_token = "session-record-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "e".repeat(64),
    )).unwrap();
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("session-record-r1").unwrap();

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
    let client = HarnessOperatorClient::new(harness_endpoint, operator_credential).unwrap();

    // IndexProviderSession: real node-side write, no process spawned.
    let identity = gate4agent_types::ProviderSessionIdentity {
        key: gate4agent_types::ProviderSessionKey::SessionId,
        id: "external-provider-session-id".to_owned(),
        transcript_path: None,
    };
    let indexed = client.index_provider_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        gate4agent_harness_api::HarnessProviderSessionIdentityV1 {
            key: gate4agent_harness_api::HarnessProviderSessionKeyV1::SessionId,
            id: identity.id.clone(),
            transcript_path: identity.transcript_path.clone(),
        },
        "Indexed provider session".to_owned(),
    ).unwrap();
    assert_eq!(indexed.workspace_id, workspace_id.as_str());
    assert!(indexed.provider_identity_present);
    let record_id = indexed.record_id.clone();

    // Discoverable through the runtime inventory (RuntimeInventoryList) --
    // the same route a harness-mode TUI sidebar reads the roster through.
    let discovered = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if let Some(found) = find_managed_session(&page, node_id.as_str(), &record_id) {
                    return found.clone();
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("indexed record never appeared in the runtime inventory");
    assert_eq!(discovered.display_name, "Indexed provider session");

    // RenameSessionRecord: real success, reflected in the reply and in a
    // subsequent runtime-inventory read (the server-side route invalidation
    // `HostCommand::SessionRecordMutationFinished` triggers on every settled
    // mutation).
    let renamed = client.rename_session_record(
        node_id.as_str().to_owned(),
        record_id.clone(),
        "Renamed provider session".to_owned(),
    ).unwrap();
    assert_eq!(renamed.record_id, record_id);
    assert_eq!(renamed.display_name, "Renamed provider session");
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if find_managed_session(&page, node_id.as_str(), &record_id)
                    .is_some_and(|record| record.display_name == "Renamed provider session")
                {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("rename never converged in the runtime inventory");

    // ResumeSessionRecord/PreviewSessionRecord against a record with no
    // resumable transcript: real rejection, not a success this fixture
    // cannot honestly produce -- see the doc comment above.
    let resume_rejected = client.resume_session_record(
        node_id.as_str().to_owned(),
        record_id.clone(),
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
        None,
    );
    assert!(resume_rejected.is_err(), "resuming a non-resumable indexed record must be rejected");
    let preview_rejected = client.preview_session_record(node_id.as_str().to_owned(), record_id.clone(), 8);
    assert!(preview_rejected.is_err(), "previewing a record with no transcript must be rejected");

    // ForgetSessionRecord: real success, gone from a subsequent read.
    let forgotten_record_id = client.forget_session_record(
        node_id.as_str().to_owned(),
        record_id.clone(),
    ).unwrap();
    assert_eq!(forgotten_record_id, record_id);
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if find_managed_session(&page, node_id.as_str(), &record_id).is_none() {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("forgotten record never left the runtime inventory");

    // Every verb's real wire-relay + node-rejection path against a
    // `record_id` the node has never seen: `NodeFailureCode::
    // UnknownSessionRecord` relayed end to end, mapped to
    // `HarnessOperatorHostErrorV1::NotFound` for the read family
    // (`map_native_history_error`) and the mutation family
    // (`map_session_record_mutation_error`) alike.
    let unknown_record_id = "record-never-indexed".to_owned();
    assert!(matches!(
        client.preview_session_record(node_id.as_str().to_owned(), unknown_record_id.clone(), 8),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));
    assert!(matches!(
        client.resume_session_record(
            node_id.as_str().to_owned(),
            unknown_record_id.clone(),
            gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
            None,
        ),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));
    assert!(matches!(
        client.rename_session_record(
            node_id.as_str().to_owned(),
            unknown_record_id.clone(),
            "Nobody home".to_owned(),
        ),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));
    assert!(matches!(
        client.set_session_task(
            node_id.as_str().to_owned(),
            unknown_record_id.clone(),
            1,
            gate4agent_harness_api::HarnessSessionTaskTargetV1::Clear,
        ),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));
    assert!(matches!(
        client.forget_session_record(node_id.as_str().to_owned(), unknown_record_id.clone()),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));

    // `IndexNativeSession` against a selection the node's native-session
    // catalog can never contain (garbage `selection_id`, `catalog_revision`
    // above zero to pass wire validation): the node's own catalog-lookup
    // rejection relays through faithfully. Not asserting the exact mapped
    // host error here (unlike the five above): unlike an unknown
    // `record_id`, "no native session catalog entry matches this selection"
    // is not exercised by the fixture's real catalog paging path, only by
    // this synthetic selection, so only "the wire relay round-trips and the
    // node rejects it" is asserted -- fabricating a real on-disk native-
    // session file to pin the exact code would be staging a fixture this
    // module's own doc comment already says is out of scope.
    let bogus_native_selection = gate4agent_harness_api::HarnessNativeSessionSelectionV1 {
        route: gate4agent_harness_api::HarnessNativeSessionRouteV1 {
            node_id: node_id.as_str().to_owned(),
            incarnation_id: "0".repeat(32),
            scope: gate4agent_harness_api::HarnessNativeSessionCatalogScopeV1::Workspace,
            workspace_id: Some(workspace_id.as_str().to_owned()),
            provider: "claude".to_owned(),
        },
        catalog_revision: 1,
        recent_cutoff_unix_ms: 1,
        selection_id: "never-cataloged".to_owned(),
    };
    assert!(
        client.index_native_session(bogus_native_selection, "Never cataloged".to_owned()).is_err(),
        "indexing a selection outside the node's real native-session catalog must be rejected",
    );

    host.shutdown().await.unwrap();
    timeout(Duration::from_secs(5), host_task).await.unwrap().unwrap().unwrap();
    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
}

fn assert_git_success(repository: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Minimal one-commit repository, just enough for `CreateWorktree` to have a
/// real `HEAD` to branch a worktree from -- unlike `windows_harness_run_
/// workspace_read_e2e.rs`'s own `prepare_repository`, this test's git
/// summary/diff/status content is never inspected, so no tracked/staged/
/// binary files are staged here.
fn init_git_repository(workspace: &Path) {
    let init = Command::new("git")
        .args(["init", "-b", "main"])
        .arg(workspace)
        .output()
        .unwrap();
    assert!(init.status.success(), "git init failed: {}", String::from_utf8_lossy(&init.stderr));
    fs::write(workspace.join("tracked.txt"), b"committed line\n").unwrap();
    assert_git_success(workspace, &["add", "--", "tracked.txt"]);
    assert_git_success(
        workspace,
        &[
            "-c", "user.name=Gate4Agent Fixture",
            "-c", "user.email=fixture@gate4agent.invalid",
            "-c", "commit.gpgSign=false",
            "-c", "core.hooksPath=NUL",
            "commit", "--quiet", "-m", "fixture initial commit",
        ],
    );
}

/// E2E coverage for the resource-mutation operator verb family
/// (`BrowseHostDirectories`/`RegisterWorkspace`/`UnregisterWorkspace`/
/// `CreateStandaloneWorkspace`/`CreateWorktree`/`RemoveWorktree`/
/// `ExportContextPack`/`ForgetContextPack`) added alongside the two verb
/// families above. Same fixture shape, plus a one-commit git repository in
/// the fixture's primary workspace (`init_git_repository`): `CreateWorktree`
/// needs a real `HEAD` to branch from.
///
/// `ExportContextPack` is exercised only through its real wire-relay and
/// node-rejection path, not a full success round trip: unlike the session-
/// record family's `IndexProviderSession` (a direct node-side store write,
/// no process involved), a context pack is exported from a session's actual
/// provider-history buffer, which this fixture's plain PTY-echo session
/// never populates with anything the node's own context-pack machinery
/// recognizes as exportable -- see the module doc comment's `PasteSession`
/// precedent for the same "prove the rejection relays faithfully, not a
/// success this fixture cannot honestly produce" choice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_harness_operator_resource_mutation_family_round_trip_and_rejections() {
    require_headless_supervisor();
    let fixture = FixturePaths::new();
    init_git_repository(&fixture.workspace);
    let node_endpoint = pipe("node");
    let control_endpoint = pipe("control");
    let node_id = NodeId::new("resource-mutation-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "resource-mutation-node-token";
    let c2_token = "resource-mutation-c2-token";
    let operator_credential = HarnessOperatorCredential::parse(format!(
        "g4aho_{}",
        "d".repeat(64),
    )).unwrap();
    let profile_id = SpawnProfileId::new("interactive-default").unwrap();
    let profile_revision = SpawnProfileRevision::new("resource-mutation-r1").unwrap();

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
    let client = HarnessOperatorClient::new(harness_endpoint, operator_credential).unwrap();

    // BrowseHostDirectories: the fixture root contains exactly the primary
    // workspace directory this test's own `FixturePaths::new` created.
    let root_listing = client.browse_host_directories(
        node_id.as_str().to_owned(),
        Some(HarnessHostPathV1::new(fixture.root.to_string_lossy().into_owned()).unwrap()),
        None,
    ).unwrap();
    assert!(
        root_listing.entries.iter().any(|entry| entry.display_name == "workspace"),
        "host directory page did not contain the fixture workspace directory: {:?}",
        root_listing.entries,
    );

    // RegisterWorkspace: a second, plain (non-git) directory -- discoverable
    // through the runtime inventory afterward, the same convergence route
    // the session-record family's own `IndexProviderSession` test already
    // proves.
    let second_workspace_dir = fixture.root.join("second-workspace");
    fs::create_dir_all(&second_workspace_dir).unwrap();
    let registered = client.register_workspace(
        node_id.as_str().to_owned(),
        "second".to_owned(),
        HarnessHostPathV1::new(second_workspace_dir.to_string_lossy().into_owned()).unwrap(),
    ).unwrap();
    assert_eq!(registered.workspace_id, "second");
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if page.nodes.iter().any(|node| {
                    node.node_id == node_id.as_str()
                        && node.inventory.workspaces.contains_key("second")
                }) {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("registered workspace never appeared in the runtime inventory");

    // CreateWorktree: branched from the primary workspace's own `HEAD`
    // (`init_git_repository`). `InspectNodeWorkspace` (the V9 node-scoped
    // read family) confirms the new workspace is a real repository on the
    // requested branch -- `HarnessGitSummaryV1` carries no worktree list of
    // its own (only the run-scoped/node-scoped tree+status+commits already
    // covered by `windows_harness_run_workspace_read_e2e.rs`), so this reads
    // the created worktree's own workspace identity instead of a worktree
    // listing.
    let worktree_target = fixture.root.join("worktree-target");
    let (worktree, worktree_workspace) = client.create_worktree(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "worktree-ws".to_owned(),
        HarnessHostPathV1::new(worktree_target.to_string_lossy().into_owned()).unwrap(),
        "feature/e2e-worktree".to_owned(),
        None,
    ).unwrap();
    assert_eq!(worktree_workspace.workspace_id, "worktree-ws");
    assert_eq!(worktree.branch.as_deref(), Some("feature/e2e-worktree"));
    assert!(worktree_target.is_dir(), "git worktree add did not create the target directory");
    let inspected = client.inspect_node_workspace(
        node_id.as_str().to_owned(),
        "worktree-ws".to_owned(),
    ).unwrap();
    assert!(inspected.git.is_repository);
    assert_eq!(inspected.git.branch.as_deref(), Some("feature/e2e-worktree"));

    // RemoveWorktree: the target directory `git worktree remove` deletes on
    // disk is the direct, filesystem-level proof this test can check without
    // a dedicated worktree-listing read (see the comment above).
    let (removed_target, _removed_workspace_id) = client.remove_worktree(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        HarnessHostPathV1::new(worktree_target.to_string_lossy().into_owned()).unwrap(),
    ).unwrap();
    assert_eq!(removed_target, worktree_target.to_string_lossy().into_owned());
    assert!(!worktree_target.exists(), "removed worktree directory is still present on disk");

    // UnregisterWorkspace: the second workspace registered above leaves the
    // runtime inventory.
    let unregistered = client.unregister_workspace(
        node_id.as_str().to_owned(),
        "second".to_owned(),
    ).unwrap();
    assert_eq!(unregistered, "second");
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(page) = client.runtime_inventory_list(None, 16) {
                if page.nodes.iter().any(|node| node.node_id == node_id.as_str())
                    && !page.nodes.iter().any(|node| {
                        node.node_id == node_id.as_str()
                            && node.inventory.workspaces.contains_key("second")
                    })
                {
                    return;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("unregistered workspace never left the runtime inventory");

    // ExportContextPack: real wire relay against a live (unmanaged, PTY-
    // echo) session -- see the module doc comment for why only the
    // rejection path is asserted here, not a success.
    let session = client.spawn_session(
        node_id.as_str().to_owned(),
        workspace_id.as_str().to_owned(),
        "claude".to_owned(),
        profile_id.as_str().to_owned(),
        HarnessExecutionModeV1::Pty,
        gate4agent_harness_api::HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
        None,
    ).unwrap();
    assert!(
        client.export_context_pack(session).is_err(),
        "exporting a context pack from a plain PTY-echo session with no exportable history must be rejected",
    );

    // ForgetContextPack against a context id the node never exported:
    // `NodeFailureCode::UnknownContextPack` relays through
    // `map_resource_mutation_error` to `NotFound`, the same "unknown X"
    // mapping every other family on this wire already uses.
    assert!(matches!(
        client.forget_context_pack(node_id.as_str().to_owned(), "context-never-exported".to_owned()),
        Err(HarnessOperatorClientError::Host(HarnessOperatorHostErrorV1::NotFound)),
    ));

    // Negative: `CreateWorktree` against a source workspace the node has
    // never registered relays the node's own rejection faithfully.
    assert!(matches!(
        client.create_worktree(
            node_id.as_str().to_owned(),
            "no-such-workspace".to_owned(),
            "worktree-ws-2".to_owned(),
            HarnessHostPathV1::new(fixture.root.join("worktree-target-2").to_string_lossy().into_owned())
                .unwrap(),
            "feature/e2e-worktree-2".to_owned(),
            None,
        ),
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
