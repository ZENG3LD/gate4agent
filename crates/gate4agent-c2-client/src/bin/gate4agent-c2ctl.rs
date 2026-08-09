use std::collections::BTreeSet;
use std::time::Duration;

use gate4agent_c2_client::{connect_local, C2ControlHandle};
use gate4agent_c2_protocol::{
    C2NodeResponse, C2NodeSnapshot, C2SessionSnapshot, C2SessionStatus, NodeRoute,
    NodeTransportState,
};
use gate4agent_node_protocol::{NodeId, NodeRequest, SessionMode, WorkspaceId};
use gate4agent_types::{AgentId, TerminalFrame, TerminalSize};
use thiserror::Error;
use tokio::time::{sleep, timeout};

const C2_TOKEN_ENV: &str = "GATE4AGENT_C2_TOKEN";
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
const SEED_DEADLINE: Duration = Duration::from_secs(100);
const ONLINE_DEADLINE: Duration = Duration::from_secs(15);
const READINESS_DEADLINE: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLOSE_DEADLINE: Duration = Duration::from_secs(8);
const TERMINAL_SIZE: TerminalSize = TerminalSize { rows: 24, columns: 80 };

struct SeedOptions {
    control_endpoint: String,
    node_id: NodeId,
    workspace_id: WorkspaceId,
    providers: Vec<AgentId>,
    token: String,
}

enum ParseOutcome {
    Run(SeedOptions),
    Help,
}

#[derive(Debug)]
struct SeededSession {
    provider: AgentId,
    address: gate4agent_node_protocol::SessionAddress,
}

#[derive(Debug, Error)]
enum SeedError {
    #[error("C2 control connection failed")]
    Connect,
    #[error("selected node did not become online")]
    NodeUnavailable,
    #[error("selected workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("provider {provider} spawn failed")]
    Spawn { provider: AgentId },
    #[error("C2 snapshot failed")]
    Snapshot,
    #[error("provider {provider} session identity did not match")]
    SessionMismatch { provider: AgentId },
    #[error("provider sessions did not produce a visible terminal screen")]
    ReadinessTimedOut,
    #[error("provider seed operation timed out")]
    SeedTimedOut,
    #[error("C2 operator connection did not close cleanly")]
    Close,
}

fn usage() -> &'static str {
    "usage: gate4agent-c2ctl seed-vendor-pty --c2-control ENDPOINT --node NODE_ID --workspace WORKSPACE_ID --provider PROVIDER_ID [--provider ...]\n\
     token env: GATE4AGENT_C2_TOKEN"
}

fn required_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_provider(value: &str) -> Result<AgentId, String> {
    AgentId::new(value).map_err(|_| "invalid --provider ID".to_owned())
}

fn provider_name(provider: &AgentId) -> &str {
    provider.as_str()
}

fn parse_args_from(
    args: &[String],
    mut read_secret: impl FnMut(&str) -> Result<String, String>,
) -> Result<ParseOutcome, String> {
    if args.get(1).is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        return Ok(ParseOutcome::Help);
    }
    if args.get(1).map(String::as_str) != Some("seed-vendor-pty") {
        return Err(usage().to_owned());
    }

    let mut control_endpoint = None;
    let mut node_id = None;
    let mut workspace_id = None;
    let mut providers = Vec::new();
    let mut unique_providers = BTreeSet::new();
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--c2-control" => {
                if control_endpoint.is_some() {
                    return Err("--c2-control can be specified only once".to_owned());
                }
                let value = required_value(args, &mut index, "--c2-control")?;
                if value.is_empty() {
                    return Err("--c2-control must not be empty".to_owned());
                }
                control_endpoint = Some(value);
            }
            "--node" => {
                if node_id.is_some() {
                    return Err("--node can be specified only once".to_owned());
                }
                node_id = Some(
                    NodeId::new(required_value(args, &mut index, "--node")?)
                        .map_err(|_| "invalid --node".to_owned())?,
                );
            }
            "--workspace" => {
                if workspace_id.is_some() {
                    return Err("--workspace can be specified only once".to_owned());
                }
                workspace_id = Some(
                    WorkspaceId::new(required_value(args, &mut index, "--workspace")?)
                        .map_err(|_| "invalid --workspace".to_owned())?,
                );
            }
            "--provider" => {
                let provider = parse_provider(&required_value(args, &mut index, "--provider")?)?;
                if !unique_providers.insert(provider.clone()) {
                    return Err("duplicate --provider".to_owned());
                }
                providers.push(provider);
            }
            "--help" | "-h" => return Ok(ParseOutcome::Help),
            _ => return Err("unknown argument".to_owned()),
        }
        index += 1;
    }

    let control_endpoint = control_endpoint.ok_or_else(|| "seed-vendor-pty requires --c2-control".to_owned())?;
    let node_id = node_id.ok_or_else(|| "seed-vendor-pty requires --node".to_owned())?;
    let workspace_id = workspace_id.ok_or_else(|| "seed-vendor-pty requires --workspace".to_owned())?;
    if providers.is_empty() {
        return Err("seed-vendor-pty requires at least one --provider".to_owned());
    }
    let token = read_secret(C2_TOKEN_ENV)?;
    if token.is_empty() {
        return Err(format!("{C2_TOKEN_ENV} must not be empty"));
    }

    Ok(ParseOutcome::Run(SeedOptions {
        control_endpoint,
        node_id,
        workspace_id,
        providers,
        token,
    }))
}

fn parse_args() -> Result<ParseOutcome, String> {
    let args = std::env::args().collect::<Vec<_>>();
    parse_args_from(&args, |name| {
        let value = std::env::var(name)
            .map_err(|_| format!("{name} is required and must be valid Unicode"))?;
        std::env::remove_var(name);
        Ok(value)
    })
}

fn spawn_request(workspace_id: &WorkspaceId, provider: AgentId) -> NodeRequest {
    NodeRequest::Spawn {
        workspace_id: workspace_id.clone(),
        provider,
        mode: SessionMode::Pty,
        terminal_size: TERMINAL_SIZE,
        initial_prompt: None,
    }
}

async fn wait_online_route(
    control: &C2ControlHandle,
    node_id: &NodeId,
) -> Result<NodeRoute, SeedError> {
    let mut topology = control.subscribe_topology();
    timeout(ONLINE_DEADLINE, async {
        loop {
            if let Some(node) = topology.borrow().nodes.iter().find(|node| &node.node_id == node_id) {
                if node.transport == NodeTransportState::Online {
                    if let Some(expected_incarnation_id) = node.current_incarnation_id {
                        return Ok(NodeRoute { node_id: node_id.clone(), expected_incarnation_id });
                    }
                }
            }
            topology.changed().await.map_err(|_| SeedError::NodeUnavailable)?;
        }
    })
    .await
    .map_err(|_| SeedError::NodeUnavailable)?
}

async fn snapshot(
    control: &C2ControlHandle,
    route: &NodeRoute,
) -> Result<C2NodeSnapshot, SeedError> {
    let response = control
        .request(route.clone(), NodeRequest::Snapshot)
        .await
        .map_err(|_| SeedError::Snapshot)?;
    match response.response {
        Ok(C2NodeResponse::Snapshot { snapshot, .. }) => Ok(snapshot),
        _ => Err(SeedError::Snapshot),
    }
}

fn frame_has_output(frame: &TerminalFrame) -> bool {
    !frame.formatted.is_empty()
        && frame
            .contents
            .chars()
            .any(|character| !character.is_whitespace() && !character.is_control())
}

fn session_is_visible(session: &C2SessionSnapshot) -> bool {
    let has_process_or_finished = match session.status {
        C2SessionStatus::Running => session.process_id.is_some(),
        C2SessionStatus::Stopping | C2SessionStatus::Exited { .. } | C2SessionStatus::Failed => true,
        C2SessionStatus::Registered | C2SessionStatus::Starting => false,
    };
    has_process_or_finished && session.terminal_frame.as_ref().is_some_and(frame_has_output)
}

fn evaluate_readiness(
    snapshot: &C2NodeSnapshot,
    workspace_id: &WorkspaceId,
    sessions: &[SeededSession],
) -> Result<bool, SeedError> {
    let Some(workspace) = snapshot.workspaces.iter().find(|workspace| &workspace.workspace_id == workspace_id) else {
        return Err(SeedError::WorkspaceUnavailable);
    };
    let mut all_ready = true;
    for seeded in sessions {
        let Some(session) = workspace.sessions.iter().find(|session| {
            session.instance_id == seeded.address.session.instance_id
                && session.generation == seeded.address.session.generation
        }) else {
            all_ready = false;
            continue;
        };
        if session.agent_id != seeded.provider {
            return Err(SeedError::SessionMismatch { provider: seeded.provider.clone() });
        }
        all_ready &= session_is_visible(session);
    }
    Ok(all_ready)
}

async fn seed_connected(
    control: &C2ControlHandle,
    options: &SeedOptions,
) -> Result<Vec<SeededSession>, SeedError> {
    let route = wait_online_route(control, &options.node_id).await?;
    let initial = snapshot(control, &route).await?;
    if !initial.workspaces.iter().any(|workspace| workspace.workspace_id == options.workspace_id) {
        return Err(SeedError::WorkspaceUnavailable);
    }

    let mut sessions = Vec::with_capacity(options.providers.len());
    for provider in options.providers.iter().cloned() {
        let response = control
            .request(route.clone(), spawn_request(&options.workspace_id, provider.clone()))
            .await
            .map_err(|_| SeedError::Spawn { provider: provider.clone() })?;
        let address = match response.response {
            Ok(C2NodeResponse::SpawnAccepted { session }) => session,
            _ => return Err(SeedError::Spawn { provider: provider.clone() }),
        };
        sessions.push(SeededSession { provider, address });
    }

    timeout(READINESS_DEADLINE, async {
        loop {
            let current = snapshot(control, &route).await?;
            if evaluate_readiness(&current, &options.workspace_id, &sessions)? {
                return Ok(());
            }
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| SeedError::ReadinessTimedOut)??;
    Ok(sessions)
}

async fn run_seed(mut options: SeedOptions) -> Result<Vec<SeededSession>, SeedError> {
    let (control, mut events) = timeout(
        CONNECT_DEADLINE,
        connect_local(&options.control_endpoint, &options.token),
    )
        .await
        .map_err(|_| SeedError::Connect)?
        .map_err(|_| SeedError::Connect)?;
    options.token.clear();
    let mut event_drain = tokio::spawn(async move {
        while events.recv().await.is_some() {}
    });

    let result = timeout(SEED_DEADLINE, seed_connected(&control, &options))
        .await
        .map_err(|_| SeedError::SeedTimedOut)
        .and_then(|result| result);
    drop(control);
    match timeout(CLOSE_DEADLINE, &mut event_drain).await {
        Ok(Ok(())) => result,
        Ok(Err(_)) => Err(SeedError::Close),
        Err(_) => {
            event_drain.abort();
            Err(SeedError::Close)
        }
    }
}

fn main() {
    let outcome = match parse_args() {
        Ok(outcome) => outcome,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let ParseOutcome::Run(options) = outcome else {
        println!("{}", usage());
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime construction failed");
    match runtime.block_on(run_seed(options)) {
        Ok(sessions) => {
            for session in sessions {
                println!(
                    "visible provider={} session={}",
                    provider_name(&session.provider),
                    session.address.session.instance_id.0,
                );
            }
        }
        Err(error) => {
            eprintln!("gate4agent-c2ctl: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_c2_protocol::C2WorkspaceSnapshot;
    use gate4agent_node_protocol::SessionKey;
    use gate4agent_types::{
        AgentId, AgentInstanceId, ProviderActivity, SessionGeneration,
        TerminalMouseProtocolEncoding, TransportKind,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn fixture_session(provider: AgentId, instance: u64) -> C2SessionSnapshot {
        C2SessionSnapshot {
            instance_id: AgentInstanceId(instance),
            agent_id: provider,
            transport: TransportKind::Pty,
            generation: SessionGeneration(1),
            status: C2SessionStatus::Running,
            pending_operation: None,
            pending_input: None,
            process_id: Some(7000 + instance as u32),
            terminal_size: Some(TERMINAL_SIZE),
            terminal_frame: Some(TerminalFrame {
                sequence: 1,
                size: TERMINAL_SIZE,
                cursor_row: 0,
                cursor_column: 0,
                contents: "vendor ready".to_owned(),
                formatted: b"vendor ready".to_vec(),
                scrollback_formatted: Vec::new(),
                alternate_screen: false,
                mouse_protocol_enabled: false,
                mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
            }),
            provider_activity: ProviderActivity::Idle,
            provider_interaction_pending: false,
            provider_identity_present: false,
        }
    }

    #[test]
    fn seed_vendor_pty_args_use_only_env_secret_and_preserve_requested_provider_order() {
        let parsed = parse_args_from(
            &args(&[
                "gate4agent-c2ctl",
                "seed-vendor-pty",
                "--c2-control",
                "fixture-control",
                "--node",
                "node-a",
                "--workspace",
                "primary",
                "--provider",
                "claude",
                "--provider",
                "codex",
                "--provider",
                "kimi",
                "--provider",
                "qwen-code",
                "--provider",
                "grok",
            ]),
            |name| {
                assert_eq!(name, C2_TOKEN_ENV);
                Ok("fixture-secret".to_owned())
            },
        )
        .unwrap();
        let ParseOutcome::Run(parsed) = parsed else { panic!("expected run options") };
        assert_eq!(
            parsed.providers,
            [agent("claude"), agent("codex"), agent("kimi"), agent("qwen-code"), agent("grok")],
        );
        assert_eq!(parsed.token, "fixture-secret");
    }

    #[test]
    fn seed_vendor_pty_requests_are_raw_pty_without_prompt_or_auth_data() {
        let workspace = WorkspaceId::new("primary").unwrap();
        for provider in [agent("claude"), agent("codex"), agent("kimi"), agent("qwen-code"), agent("grok")] {
            assert!(matches!(
                spawn_request(&workspace, provider.clone()),
                NodeRequest::Spawn {
                    workspace_id,
                    provider: actual_provider,
                    mode: SessionMode::Pty,
                    terminal_size: TERMINAL_SIZE,
                    initial_prompt: None,
                } if workspace_id == workspace && actual_provider == provider
            ));
        }
    }

    #[test]
    fn fixture_c2_readiness_accepts_live_or_finished_real_terminal_output() {
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let address = gate4agent_node_protocol::SessionAddress {
            workspace_id: workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(9),
                generation: SessionGeneration(1),
            },
        };
        let seeded = [SeededSession { provider: agent("codex"), address }];
        let mut session = fixture_session(agent("codex"), 9);
        let mut snapshot = C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: Vec::new(),
            provider_runtime_statuses: Default::default(),
            workspaces: vec![C2WorkspaceSnapshot {
                workspace_id: workspace_id.clone(),
                canonical_root: gate4agent_node_protocol::OpaqueHostPath::utf8(
                    "fixture-root".to_owned(),
                ).unwrap(),
                sessions: vec![session.clone()],
            }],
            session_records: Vec::new(),
        };
        assert_eq!(evaluate_readiness(&snapshot, &workspace_id, &seeded).unwrap(), true);

        session.status = C2SessionStatus::Exited { exit_code: Some(1) };
        session.process_id = None;
        snapshot.workspaces[0].sessions[0] = session.clone();
        assert_eq!(evaluate_readiness(&snapshot, &workspace_id, &seeded).unwrap(), true);

        session.status = C2SessionStatus::Running;
        session.process_id = None;
        snapshot.workspaces[0].sessions[0] = session.clone();
        assert_eq!(evaluate_readiness(&snapshot, &workspace_id, &seeded).unwrap(), false);

        session.process_id = Some(7009);
        session.terminal_frame.as_mut().unwrap().formatted.clear();
        snapshot.workspaces[0].sessions[0] = session;
        assert_eq!(evaluate_readiness(&snapshot, &workspace_id, &seeded).unwrap(), false);

        let frame = snapshot.workspaces[0].sessions[0].terminal_frame.as_mut().unwrap();
        frame.formatted = b"\x1b[2J".to_vec();
        frame.contents.clear();
        frame.scrollback_formatted = vec![b"\x1b[2J".to_vec()];
        assert_eq!(evaluate_readiness(&snapshot, &workspace_id, &seeded).unwrap(), false);
    }
}
