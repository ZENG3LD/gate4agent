use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, ProviderEvent, ResumeLaunchRequest, ResumeTarget, SessionStatus,
    StartRequest, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_CANARY";
const LIVE_LAUNCHER_ENV: &str = "GATE4AGENT_VENDOR_CANARY_LAUNCHER";
const LIVE_LAUNCHER_AGENT_ENV: &str = "GATE4AGENT_VENDOR_CANARY_AGENT";
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

fn runtime_for_agents(
    agent_ids: &[&str],
) -> (gate4agent_handle::Gate4AgentHandle, NativeRuntime) {
    let specs = agent_ids.iter().map(|agent_id| {
        builtin_registry()
            .get_by_id(agent_id)
            .unwrap_or_else(|| panic!("missing built-in agent spec for {agent_id}"))
            .clone()
    });
    NativeRuntime::new(
        AgentRegistry::new(specs).expect("multi-agent live registry"),
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

async fn drive_until_generation_terminal(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    expected_generation: u64,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if handle.snapshot().sessions.first().is_some_and(|session| {
                session.generation.0 == expected_generation
                    && matches!(
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

async fn drive_until_all_terminal(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    expected_sessions: usize,
    timeout: Duration,
) -> Option<usize> {
    tokio::time::timeout(timeout, async {
        let mut max_active = 0;
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            max_active = max_active.max(runtime.active_native_sessions());
            let snapshot = handle.snapshot();
            if snapshot.sessions.len() == expected_sessions
                && snapshot.sessions.iter().all(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
                    )
                })
            {
                break max_active;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .ok()
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

fn successful_provider_end_count(events: &[ControlEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
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
        .count()
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

fn provider_text_for(events: &[ControlEvent], instance_id: AgentInstanceId) -> String {
    events
        .iter()
        .filter(|event| event.instance_id == instance_id)
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

fn provider_error_count(events: &[ControlEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(
            &event.event,
            ControlEventKind::ProviderEvent {
                event: ProviderEvent::Error { .. },
                ..
            }
        ))
        .count()
}

fn provider_error_categories(events: &[ControlEvent]) -> String {
    let mut categories = Vec::new();
    for message in events.iter().filter_map(|event| match &event.event {
        ControlEventKind::ProviderEvent {
            event: ProviderEvent::Error { message },
            ..
        } => Some(message.to_ascii_lowercase()),
        _ => None,
    }) {
        for (category, marker) in [
            ("authentication", "auth"),
            ("login", "login"),
            ("invalid-session", "invalid session"),
            ("session-not-found", "session not found"),
            ("not-found", "not found"),
            ("does-not-exist", "does not exist"),
            ("in-use", "already in use"),
            ("resume", "resume"),
            ("conversation", "conversation"),
            ("expired", "expired"),
            ("network", "network"),
            ("offline", "offline"),
            ("unavailable", "unavailable"),
            ("overloaded", "overloaded"),
            ("rate-limit", "rate limit"),
            ("permission", "permission"),
            ("unsupported", "unsupported"),
            ("unknown-option", "unknown option"),
        ] {
            if message.contains(marker) && !categories.contains(&category) {
                categories.push(category);
            }
        }
    }
    if categories.is_empty() {
        "unclassified".to_owned()
    } else {
        categories.join(",")
    }
}

fn provider_session_result_count(events: &[ControlEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(
            &event.event,
            ControlEventKind::ProviderEvent {
                event: ProviderEvent::SessionEnded { .. },
                ..
            }
        ))
        .count()
}

fn assert_inherited_path_resolves_vendor_launcher(agent_id: &str) {
    let selected_agent = std::env::var_os(LIVE_LAUNCHER_AGENT_ENV);
    let selected_launcher = std::env::var_os(LIVE_LAUNCHER_ENV);
    if selected_agent.is_none() && selected_launcher.is_none() {
        return;
    }
    let selected_agent = selected_agent
        .expect("inline launcher evidence requires both vendor contract lab variables");
    let selected_launcher = selected_launcher
        .expect("inline launcher evidence requires both vendor contract lab variables");
    assert_eq!(
        selected_agent.to_str(),
        Some(agent_id),
        "vendor contract lab must identify the inline agent"
    );
    let expected = PathBuf::from(selected_launcher);
    assert!(expected.is_absolute());
    assert!(expected.is_file());
    let path = std::env::var_os("PATH").expect("inline canary PATH");
    let directories = std::env::split_paths(&path).collect::<Vec<_>>();
    let resolved = ["cmd", "exe"]
        .into_iter()
        .find_map(|extension| {
            let file_name = format!("{agent_id}.{extension}");
            directories
                .iter()
                .map(|directory| directory.join(&file_name))
                .find(|candidate| candidate.is_file())
        })
        .expect("inline launcher must resolve on the inherited PATH");
    assert_eq!(
        resolved.canonicalize().expect("resolved inline launcher"),
        expected.canonicalize().expect("expected inline launcher"),
        "inline canary inherited PATH resolved a different vendor launcher"
    );
}

async fn run_pipe_canary(agent_id: &str, instance_id: u64) {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor execution"
    );
    assert_inherited_path_resolves_vendor_launcher(agent_id);

    let working_directory = isolated_working_directory(agent_id);
    let marker = format!("GATE4AGENT_{agent_id}_CANARY_OK");
    let prompt = format!(
        "Reply with exactly {marker} and nothing else.\nDo not repeat this transport test data: \"quoted\" 100% ! & | < > ^ Привет"
    );
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
    let first_generation = session.generation.0;
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
        "{agent_id} emitted provider failure: {failure:?}; exit_code={exit_code:?}; provider_error_count={} provider_error_categories={}",
        provider_error_count(&events),
        provider_error_categories(&events),
    );
    assert_eq!(exit_code, Some(0), "{agent_id} exited unsuccessfully");
    assert!(
        text.contains(&marker),
        "{agent_id} response did not contain the unique success marker; response_bytes={}",
        text.len()
    );
    if agent_id != "kimi" {
        assert!(
            completed_turns >= 1,
            "{agent_id} did not publish a completed turn"
        );
    }
    assert!(
        successful_session_end,
        "{agent_id} did not publish a successful provider session end"
    );

    let first_identity = session
        .provider
        .session
        .clone()
        .unwrap_or_else(|| panic!("{agent_id} did not publish a provider session identity"));
    let event_boundary = events.len();
    handle
        .dispatch(command(
            4,
            ControlCommand::Resume {
                instance_id,
                target: ResumeTarget::CurrentProvider,
                request: ResumeLaunchRequest {
                    working_directory: working_directory.path().to_string_lossy().into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: Some(
                        "Reply with exactly the same marker from your previous answer and nothing else."
                            .to_owned(),
                    ),
                },
            },
        ))
        .expect("resume live vendor session");

    let resumed_without_timeout = drive_until_generation_terminal(
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        first_generation + 1,
        LIVE_TIMEOUT,
    )
    .await;
    if !resumed_without_timeout {
        let _ = handle.dispatch(command(
            5,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ));
        let _ = drive_until_terminal(
            &mut runtime,
            &handle,
            &subscription,
            &mut events,
            STOP_TIMEOUT,
        )
        .await;
        panic!("{agent_id} resumed live canary timed out");
    }

    let resumed_snapshot = handle.snapshot();
    let resumed = resumed_snapshot.sessions.first().expect("resumed session snapshot");
    assert_eq!(
        resumed.generation.0,
        first_generation + 1,
        "{agent_id} resume did not advance generation"
    );
    assert_eq!(
        resumed.provider.session.as_ref(),
        Some(&first_identity),
        "{agent_id} resume changed provider session identity"
    );
    assert!(
        matches!(
            resumed.status,
            SessionStatus::Exited { exit_code: Some(0) }
        ),
        "{agent_id} resumed child exited unsuccessfully; status={:?}; failure_kind={:?}; provider_error_count={}; provider_error_categories={}; session_result_count={}; response_bytes={}",
        resumed.status,
        provider_failure_kind(&events[event_boundary..]),
        provider_error_count(&events[event_boundary..]),
        provider_error_categories(&events[event_boundary..]),
        provider_session_result_count(&events[event_boundary..]),
        provider_text(&events[event_boundary..]).len(),
    );
    assert!(
        provider_failure_kind(&events[event_boundary..]).is_none(),
        "{agent_id} resumed turn emitted a provider failure"
    );
    let resumed_text = provider_text(&events[event_boundary..]);
    assert!(
        resumed_text.contains(&marker),
        "{agent_id} resumed response did not retain prior-turn context; response_bytes={} provider_error_count={} event_count={}",
        resumed_text.len(),
        provider_error_count(&events[event_boundary..]),
        events[event_boundary..].len(),
    );
    assert!(
        successful_provider_end_count(&events) >= 2,
        "{agent_id} did not publish successful ends for both inline children"
    );
    assert_eq!(
        runtime.active_native_sessions(),
        0,
        "{agent_id} left a native session active after resume"
    );

    println!(
        "vendor_canary agent={agent_id} transport=pipe fresh_exit_code=0 resume_exit_code=0 completed_turns={} marker_observed=true same_provider_session=true generation={} isolated_cwd=true active_sessions=0",
        resumed.provider.completed_turns.max(completed_turns),
        resumed.generation.0,
    );
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Claude CLI"]
async fn windows_live_claude_inline_contract() {
    run_pipe_canary("claude", 9_001).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Codex CLI"]
async fn windows_live_codex_inline_contract() {
    run_pipe_canary("codex", 9_002).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Kimi Code CLI"]
async fn windows_live_kimi_inline_contract() {
    run_pipe_canary("kimi", 9_003).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Codex CLI"]
async fn windows_live_parallel_codex_inline_process_isolation() {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor execution"
    );

    let directories = [
        isolated_working_directory("codex-parallel-a"),
        isolated_working_directory("codex-parallel-b"),
    ];
    let instances = [AgentInstanceId(9_101), AgentInstanceId(9_102)];
    let markers = ["GATE4AGENT_CODEX_PARALLEL_A", "GATE4AGENT_CODEX_PARALLEL_B"];
    let (handle, mut runtime) = runtime_for_agents(&["codex"]);
    let subscription = handle.subscribe(512);

    for (index, instance_id) in instances.iter().copied().enumerate() {
        handle
            .dispatch(command(
                index as u64 + 1,
                ControlCommand::Register {
                    instance_id,
                    agent_id: AgentId::new("codex").unwrap(),
                    transport: TransportKind::Pipe,
                },
            ))
            .expect("register parallel Codex inline session");
    }
    for (index, instance_id) in instances.iter().copied().enumerate() {
        handle
            .dispatch(command(
                index as u64 + 3,
                ControlCommand::Start {
                    instance_id,
                    request: StartRequest {
                        working_directory: directories[index]
                            .path()
                            .to_string_lossy()
                            .into_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: Some(format!(
                            "Reply with exactly {} and nothing else.",
                            markers[index]
                        )),
                        session_options: None,
                    },
                },
            ))
            .expect("start parallel Codex inline session");
    }

    let mut events = Vec::new();
    let max_active = drive_until_all_terminal(
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        instances.len(),
        LIVE_TIMEOUT,
    )
    .await
    .expect("parallel Codex inline sessions timed out");
    assert_eq!(max_active, 2, "both Codex children were not active concurrently");

    let snapshot = handle.snapshot();
    assert_eq!(snapshot.sessions.len(), 2);
    let mut provider_ids = Vec::new();
    for (index, instance_id) in instances.iter().copied().enumerate() {
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.instance_id == instance_id)
            .expect("parallel Codex session snapshot");
        assert!(matches!(
            session.status,
            SessionStatus::Exited { exit_code: Some(0) }
        ));
        provider_ids.push(
            session
                .provider
                .session
                .as_ref()
                .expect("parallel Codex provider identity")
                .id
                .clone(),
        );
        let text = provider_text_for(&events, instance_id);
        assert!(text.contains(markers[index]));
        assert!(!text.contains(markers[1 - index]));
    }
    assert_ne!(provider_ids[0], provider_ids[1]);
    assert_eq!(runtime.active_native_sessions(), 0);
    println!(
        "vendor_inline_parallel agent=codex concurrent_active=2 distinct_provider_sessions=2 isolated_cwd=true cross_session_marker_leak=false active_sessions=0"
    );
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Codex CLI"]
async fn windows_live_codex_inline_inflight_stop() {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor execution"
    );
    let working_directory = isolated_working_directory("codex-stop");
    let (handle, mut runtime) = runtime_for("codex");
    let subscription = handle.subscribe(256);
    let instance_id = AgentInstanceId(9_103);
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("codex").unwrap(),
                transport: TransportKind::Pipe,
            },
        ))
        .unwrap();
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
                    initial_prompt: Some(
                        "Explain process ownership in exactly fifty short numbered lines."
                            .to_owned(),
                    ),
                    session_options: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if runtime.active_native_sessions() == 1 {
                break;
            }
            assert!(!handle.snapshot().sessions.first().is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
                )
            }));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Codex inline child did not become active before completion");

    handle
        .dispatch(command(
            3,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    assert!(
        drive_until_terminal(
            &mut runtime,
            &handle,
            &subscription,
            &mut events,
            STOP_TIMEOUT,
        )
        .await
    );
    assert_eq!(runtime.active_native_sessions(), 0);
    println!(
        "vendor_inline_stop agent=codex observed_active=true forced_stop=true active_sessions=0"
    );
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated Gemini CLI"]
async fn native_runtime_pipe_live_gemini() {
    run_pipe_canary("gemini", 9_004).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_CANARY=1 and an installed authenticated OpenCode CLI"]
async fn native_runtime_pipe_live_opencode() {
    run_pipe_canary("opencode", 9_005).await;
}
