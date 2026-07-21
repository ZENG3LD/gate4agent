use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, ProviderEvent, SessionStatus, StartRequest, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};

const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_CANARY";
const LIVE_TIMEOUT: Duration = Duration::from_secs(120);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

struct CanaryDirectory(PathBuf);

impl CanaryDirectory {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CanaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn isolated_working_directory(agent_id: &str) -> CanaryDirectory {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must follow the Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gate4agent-vendor-canary-{agent_id}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&path).expect("create isolated vendor-canary working directory");
    CanaryDirectory(path)
}

fn runtime_for(
    agent_id: &str,
) -> (gate4agent_handle::Gate4AgentHandle, NativeRuntime) {
    let spec = builtin_registry()
        .get_by_id(agent_id)
        .unwrap_or_else(|| panic!("missing built-in agent spec for {agent_id}"))
        .clone();
    NativeRuntime::new(
        AgentRegistry::new([spec]).expect("single-agent live registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    )
}

async fn drive_until_terminal(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if handle.snapshot().sessions.first().is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

fn provider_failure_kind(events: &[ControlEvent]) -> Option<String> {
    events.iter().find_map(|event| match &event.event {
        ControlEventKind::CommandRejected { .. } => Some("command-rejected".to_owned()),
        ControlEventKind::Failed { .. } => Some("control-failed".to_owned()),
        ControlEventKind::ProviderGap { missed, .. } => {
            Some(format!("provider-gap:{missed}"))
        }
        ControlEventKind::ProviderEvent {
            event: ProviderEvent::Error { .. },
            ..
        } => Some("provider-error".to_owned()),
        ControlEventKind::ProviderEvent {
            event:
                ProviderEvent::SessionEnded {
                    is_error: true,
                    ..
                },
            ..
        } => Some("session-ended-error".to_owned()),
        ControlEventKind::ProviderEvent {
            event:
                ProviderEvent::ToolCompleted {
                    is_error: true, ..
                },
            ..
        } => Some("tool-error".to_owned()),
        ControlEventKind::ProviderEvent {
            event: ProviderEvent::RateLimited { limit_type, .. },
            ..
        } => Some(format!("rate-limited:{limit_type}")),
        _ => None,
    })
}

fn provider_has_successful_end(events: &[ControlEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.event,
            ControlEventKind::ProviderEvent {
                event: ProviderEvent::SessionEnded {
                    is_error: false,
                    ..
                },
                ..
            }
        )
    })
}

fn provider_text(events: &[ControlEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.event {
            ControlEventKind::ProviderEvent {
                event: ProviderEvent::Text { text, .. },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn run_pipe_canary(agent_id: &str, instance_id: u64) {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor execution"
    );

    let working_directory = isolated_working_directory(agent_id);
    let marker = format!("GATE4AGENT_{agent_id}_CANARY_OK");
    let prompt = format!("Reply with exactly {marker} and nothing else.");
    let (handle, mut runtime) = runtime_for(agent_id);
    let subscription = handle.subscribe(256);
    let instance_id = AgentInstanceId(instance_id);

    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(agent_id).expect("valid built-in agent ID"),
                transport: TransportKind::Pipe,
            },
        ))
        .expect("register live vendor session");
    handle
        .dispatch(command(
            2,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: working_directory.path().to_string_lossy().into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: Some(prompt),
                    session_options: None,
                },
            },
        ))
        .expect("start live vendor session");

    let mut events = Vec::new();
    let completed_without_timeout = drive_until_terminal(
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        LIVE_TIMEOUT,
    )
    .await;
    let forced_stop_observed = if completed_without_timeout {
        None
    } else {
        let _ = handle.dispatch(command(
            3,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ));
        let stopped = drive_until_terminal(
            &mut runtime,
            &handle,
            &subscription,
            &mut events,
            STOP_TIMEOUT,
        )
        .await;
        Some(stopped)
    };

    let snapshot = handle.snapshot();
    let session = snapshot.sessions.first().expect("live session snapshot");
    let status = session.status.clone();
    let completed_turns = session.provider.completed_turns;
    let failure = provider_failure_kind(&events);
    let successful_session_end = provider_has_successful_end(&events);
    let text = provider_text(&events);
    let active_sessions = runtime.active_native_sessions();

    assert_eq!(
        active_sessions, 0,
        "{agent_id} left a native session active after terminal handling"
    );
    if let Some(stopped) = forced_stop_observed {
        panic!("{agent_id} live canary timed out; forced_stop_observed={stopped}");
    }
    let exit_code = match status {
        SessionStatus::Exited { exit_code } => exit_code,
        SessionStatus::Failed { .. } => {
            panic!("{agent_id} native runtime entered Failed; failure_kind={failure:?}")
        }
        _ => panic!("{agent_id} did not reach a terminal session state"),
    };

    assert!(
        failure.is_none(),
        "{agent_id} emitted provider failure: {failure:?}; exit_code={exit_code:?}"
    );
    assert_eq!(exit_code, Some(0), "{agent_id} exited unsuccessfully");
    assert!(
        text.contains(&marker),
        "{agent_id} response did not contain the unique success marker; response_bytes={}",
        text.len()
    );
    assert!(
        completed_turns >= 1,
        "{agent_id} did not publish a completed turn"
    );
    assert!(
        successful_session_end,
        "{agent_id} did not publish a successful provider session end"
    );

    println!(
        "vendor_canary agent={agent_id} transport=pipe exit_code=0 completed_turns={completed_turns} marker_observed=true isolated_cwd=true"
    );
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Claude CLI"]
async fn native_runtime_pipe_live_claude() {
    run_pipe_canary("claude", 9_001).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Codex CLI"]
async fn native_runtime_pipe_live_codex() {
    run_pipe_canary("codex", 9_002).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Gemini CLI"]
async fn native_runtime_pipe_live_gemini() {
    run_pipe_canary("gemini", 9_003).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated OpenCode CLI"]
async fn native_runtime_pipe_live_opencode() {
    run_pipe_canary("opencode", 9_004).await;
}
