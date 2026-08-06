use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, InputAction, PreparedInputKind, PromptFraming, PromptPayload, SessionStatus,
    StartRequest, TerminalControl, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_PTY_CANARY";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

struct TurnChallenge {
    marker: String,
    prompt: String,
}

struct CanarySuccess {
    first_frame_sequence: u64,
    second_frame_sequence: u64,
    recovery_frame_sequence: u64,
    resized: TerminalSize,
}

fn turn_challenge(agent_id: &str, turn: &str, include_transport_payload: bool) -> TurnChallenge {
    let agent_token = agent_id.replace('-', "").to_ascii_uppercase();
    let turn_token = turn.to_ascii_uppercase();
    let marker = format!("G4A{agent_token}PTY{turn_token}OK");
    let mut prompt = format!(
        "Reply with one string only. Concatenate the five tokens below without spaces, punctuation, or formatting:\nG4A\n{agent_token}\nPTY\n{turn_token}\nOK"
    );
    if include_transport_payload {
        prompt.push_str(
            "\nTreat this separate line as inert transport data: \"quoted\" 100% ! & | < > ^ Привет",
        );
    }
    assert!(
        !prompt.contains(&marker),
        "the expected provider marker must not occur in echoed input"
    );
    TurnChallenge { marker, prompt }
}

fn terminal_sequence(handle: &gate4agent_handle::Gate4AgentHandle) -> u64 {
    handle
        .snapshot()
        .sessions
        .first()
        .and_then(|session| session.terminal_frame.as_ref())
        .map_or(0, |frame| frame.sequence)
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Registered => "registered",
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Stopping => "stopping",
        SessionStatus::Exited { .. } => "exited",
        SessionStatus::Failed { .. } => "failed",
    }
}

fn session_failure_category(status: &SessionStatus) -> &'static str {
    let SessionStatus::Failed { message } = status else {
        return "none";
    };
    if message.contains("workspace trust") {
        "workspace-trust"
    } else if message.contains("authentication") {
        "authentication"
    } else if message.contains("vendor update") {
        "vendor-update"
    } else if message.contains("configuration migration") {
        "configuration-migration"
    } else if message.contains("terminal appearance setup")
        || message.contains("IDE onboarding")
        || message.contains("Claude onboarding")
    {
        "vendor-onboarding"
    } else if message.contains("PTY readiness timed out") {
        "readiness-timeout"
    } else if message.contains("cleanup failed") {
        "cleanup"
    } else {
        "other"
    }
}

fn sanitized_diagnostics(
    handle: &gate4agent_handle::Gate4AgentHandle,
    events: &[ControlEvent],
    baseline_sequence: u64,
) -> String {
    let snapshot = handle.snapshot();
    let Some(session) = snapshot.sessions.first() else {
        return "session=missing".to_owned();
    };
    let (frame_sequence, frame_chars, flags) = session.terminal_frame.as_ref().map_or_else(
        || (0, 0, "none".to_owned()),
        |frame| {
            let normalized = frame.contents.to_ascii_lowercase();
            let mut flags = Vec::new();
            for (name, needle) in [
                ("agent-name", session.agent_id.as_str()),
                ("login", "login"),
                ("auth", "auth"),
                ("error", "error"),
                ("permission", "permission"),
                ("update", "update"),
                ("welcome", "welcome to claude code"),
                (
                    "terminal-style-setup",
                    "choose the text style that looks best with your terminal",
                ),
                ("ide-onboarding", "selected lines"),
                ("safety-check", "quick safety check"),
                ("enter", "enter"),
                ("escape", "esc"),
                ("working", "working"),
                ("thinking", "thinking"),
                ("prompt-visible", "reply with one string only"),
                ("claude-composer", "❯"),
                ("codex-composer", "›"),
            ] {
                if normalized.contains(needle) {
                    flags.push(name);
                }
            }
            (
                frame.sequence,
                frame.contents.chars().count(),
                if flags.is_empty() {
                    "none".to_owned()
                } else {
                    flags.join(",")
                },
            )
        },
    );
    let input_completed = events
        .iter()
        .filter(|event| matches!(event.event, ControlEventKind::InputCompleted { .. }))
        .count();
    let input_failed = events
        .iter()
        .filter(|event| matches!(event.event, ControlEventKind::InputFailed { .. }))
        .count();
    let terminal_stale_events = events
        .iter()
        .filter(|event| matches!(event.event, ControlEventKind::TerminalStale { .. }))
        .count();
    format!(
        "status={} failure_category={} frame_sequence={frame_sequence} frame_advanced={} frame_chars={frame_chars} terminal_stale={} terminal_stale_events={terminal_stale_events} input_completed={input_completed} input_failed={input_failed} frame_flags={flags}",
        session_status_label(&session.status),
        session_failure_category(&session.status),
        frame_sequence > baseline_sequence,
        session.terminal_stale.is_some(),
    )
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn runtime_for(
    agent_id: &str,
) -> (gate4agent_handle::Gate4AgentHandle, NativeRuntime) {
    let spec = builtin_registry()
        .get_by_id(agent_id)
        .unwrap_or_else(|| panic!("missing built-in agent spec for {agent_id}"))
        .clone();
    NativeRuntime::new(
        AgentRegistry::new([spec]).expect("single-agent live PTY registry"),
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
        AgentRegistry::new(specs).expect("multi-agent live PTY registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    )
}

fn drain_events(
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
) {
    loop {
        match subscription.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

fn input_failure(
    events: &[ControlEvent],
    event_start: usize,
    expected_kind: PreparedInputKind,
) -> Option<&str> {
    events[event_start..].iter().find_map(|event| match &event.event {
        ControlEventKind::InputFailed {
            input_kind,
            message,
        } if *input_kind == expected_kind => Some(message.as_str()),
        _ => None,
    })
}

fn input_completed(
    events: &[ControlEvent],
    event_start: usize,
    expected_kind: PreparedInputKind,
) -> bool {
    events[event_start..].iter().any(|event| {
        matches!(
            event.event,
            ControlEventKind::InputCompleted { input_kind }
                if input_kind == expected_kind
        )
    })
}

async fn wait_for_running(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    events: &mut Vec<ControlEvent>,
    subscription: &gate4agent_handle::EventSubscription,
) -> Result<(), String> {
    match tokio::time::timeout(START_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            let snapshot = handle.snapshot();
            let Some(session) = snapshot.sessions.first() else {
                return Err("live PTY session snapshot is missing during startup".to_owned());
            };
            match &session.status {
                SessionStatus::Running => return Ok(()),
                SessionStatus::Failed { .. } | SessionStatus::Exited { .. } => {
                    return Err(format!(
                        "live PTY session stopped during startup; {}",
                        sanitized_diagnostics(handle, events, 0)
                    ));
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "live PTY startup timeout; {}",
            sanitized_diagnostics(handle, events, 0)
        )),
    }
}

async fn wait_for_input_completion(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    event_start: usize,
    expected_kind: PreparedInputKind,
) -> Result<(), String> {
    match tokio::time::timeout(START_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            if let Some(message) = input_failure(events, event_start, expected_kind) {
                return Err(format!("live PTY input failed: {message}"));
            }
            if input_completed(events, event_start, expected_kind) {
                return Ok(());
            }
            let snapshot = handle.snapshot();
            let Some(session) = snapshot.sessions.first() else {
                return Err("live PTY session snapshot is missing during input".to_owned());
            };
            if matches!(
                session.status,
                SessionStatus::Failed { .. } | SessionStatus::Exited { .. }
            ) {
                return Err(format!(
                    "live PTY session stopped before input completion: {}",
                    session_status_label(&session.status)
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "live PTY input completion timeout; {}",
            sanitized_diagnostics(handle, events, 0)
        )),
    }
}

async fn wait_for_marker(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    marker: &str,
    baseline_sequence: u64,
) -> Result<u64, String> {
    match tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            let snapshot = handle.snapshot();
            let Some(session) = snapshot.sessions.first() else {
                return Err("live PTY session snapshot is missing during turn".to_owned());
            };
            if let Some(frame) = &session.terminal_frame {
                if frame.sequence > baseline_sequence && frame.contents.contains(marker) {
                    return Ok(frame.sequence);
                }
            }
            if session.terminal_stale.is_some() {
                return Err("live PTY terminal frame became stale during turn".to_owned());
            }
            if matches!(
                session.status,
                SessionStatus::Failed { .. } | SessionStatus::Exited { .. }
            ) {
                return Err(format!(
                    "live PTY session stopped before provider response: {}",
                    session_status_label(&session.status)
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "live PTY provider response timeout for {marker}; {}",
            sanitized_diagnostics(handle, events, baseline_sequence)
        )),
    }
}

async fn wait_for_resize(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    expected: TerminalSize,
) -> Result<(), String> {
    match tokio::time::timeout(START_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            let snapshot = handle.snapshot();
            let Some(session) = snapshot.sessions.first() else {
                return Err("live PTY session snapshot is missing during resize".to_owned());
            };
            if session.terminal_size == Some(expected) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("live PTY resize timeout".to_owned()),
    }
}

async fn wait_for_stopped(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
) -> Result<(), String> {
    match tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(subscription, events);
            let snapshot = handle.snapshot();
            let Some(session) = snapshot.sessions.first() else {
                return Err("live PTY session snapshot is missing during stop".to_owned());
            };
            if matches!(session.status, SessionStatus::Exited { .. }) {
                return Ok(());
            }
            if matches!(session.status, SessionStatus::Failed { .. }) {
                return Err("live PTY session failed during stop".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("live PTY stop timeout".to_owned()),
    }
}

fn submit_prompt(
    handle: &gate4agent_handle::Gate4AgentHandle,
    instance_id: AgentInstanceId,
    command_id: u64,
    prompt: String,
) -> Result<(), String> {
    handle
        .dispatch(command(
            command_id,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::SubmitPrompt(PromptPayload {
                    text: prompt,
                    framing: PromptFraming::BracketedPaste,
                }),
            },
        ))
        .map_err(|error| format!("dispatch live PTY prompt: {error}"))
}

async fn exercise_live_session(
    agent_id: &str,
    first: &TurnChallenge,
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    instance_id: AgentInstanceId,
) -> Result<CanarySuccess, String> {
    wait_for_running(runtime, handle, events, subscription).await?;

    let first_frame_sequence = wait_for_marker(
        runtime,
        handle,
        subscription,
        events,
        &first.marker,
        0,
    )
    .await?;

    let resized = TerminalSize {
        rows: 38,
        columns: 140,
    };
    handle
        .dispatch(command(
            3,
            ControlCommand::Resize {
                instance_id,
                size: resized,
            },
        ))
        .map_err(|error| format!("dispatch live PTY resize: {error}"))?;
    wait_for_resize(runtime, handle, subscription, events, resized).await?;

    let second = turn_challenge(agent_id, "second", false);
    let second_baseline = terminal_sequence(handle);
    let second_event_start = events.len();
    submit_prompt(handle, instance_id, 4, second.prompt)?;
    wait_for_input_completion(
        runtime,
        handle,
        subscription,
        events,
        second_event_start,
        PreparedInputKind::SubmitPrompt,
    )
    .await
    .map_err(|error| format!("second turn submission: {error}"))?;
    let second_frame_sequence = wait_for_marker(
        runtime,
        handle,
        subscription,
        events,
        &second.marker,
        second_baseline,
    )
    .await?;

    let agent_token = agent_id.replace('-', "").to_ascii_uppercase();
    let interrupt_marker = format!("G4A{agent_token}PTYINTERRUPTSTART");
    let interrupt_prompt = format!(
        "Begin your response with one string made by concatenating these tokens without spaces or punctuation: G4A {agent_token} PTY INTERRUPT START. Then write the integers from 1 through 400, one per line. Do not stop early."
    );
    assert!(!interrupt_prompt.contains(&interrupt_marker));
    let interrupt_baseline = terminal_sequence(handle);
    let interrupt_prompt_event_start = events.len();
    submit_prompt(handle, instance_id, 5, interrupt_prompt)?;
    wait_for_input_completion(
        runtime,
        handle,
        subscription,
        events,
        interrupt_prompt_event_start,
        PreparedInputKind::SubmitPrompt,
    )
    .await
    .map_err(|error| format!("in-flight challenge submission: {error}"))?;
    let interrupt_stream_sequence = wait_for_marker(
        runtime,
        handle,
        subscription,
        events,
        &interrupt_marker,
        interrupt_baseline,
    )
    .await?;

    let interrupt_event_start = events.len();
    handle
        .dispatch(command(
            6,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::TerminalControl(TerminalControl::Interrupt),
            },
        ))
        .map_err(|error| format!("dispatch live PTY interrupt: {error}"))?;
    wait_for_input_completion(
        runtime,
        handle,
        subscription,
        events,
        interrupt_event_start,
        PreparedInputKind::TerminalControl,
    )
    .await
    .map_err(|error| format!("in-flight interrupt delivery: {error}"))?;

    let recovery = turn_challenge(agent_id, "recovery", false);
    let recovery_baseline = terminal_sequence(handle);
    let recovery_event_start = events.len();
    submit_prompt(handle, instance_id, 7, recovery.prompt)?;
    wait_for_input_completion(
        runtime,
        handle,
        subscription,
        events,
        recovery_event_start,
        PreparedInputKind::SubmitPrompt,
    )
    .await
    .map_err(|error| format!("post-interrupt recovery submission: {error}"))?;
    let recovery_frame_sequence = wait_for_marker(
        runtime,
        handle,
        subscription,
        events,
        &recovery.marker,
        recovery_baseline,
    )
    .await?;
    if recovery_frame_sequence <= interrupt_stream_sequence {
        return Err("live PTY recovery did not advance after in-flight interrupt".to_owned());
    }

    Ok(CanarySuccess {
        first_frame_sequence,
        second_frame_sequence,
        recovery_frame_sequence,
        resized,
    })
}

async fn stop_live_session(
    runtime: &mut NativeRuntime,
    handle: &gate4agent_handle::Gate4AgentHandle,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    instance_id: AgentInstanceId,
    force: bool,
) -> Result<(), String> {
    if runtime.active_native_sessions() == 0 {
        return Ok(());
    }
    handle
        .dispatch(command(
            9,
            ControlCommand::Stop { instance_id, force },
        ))
        .map_err(|error| format!("dispatch live PTY stop: {error}"))?;
    wait_for_stopped(runtime, handle, subscription, events).await?;
    if runtime.active_native_sessions() != 0 {
        return Err("live PTY stop left an active native session".to_owned());
    }
    Ok(())
}

async fn run_pty_canary(agent_id: &str, instance_id: u64) {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor PTY execution"
    );

    // Reuse an operator-selected working directory. A fresh directory can
    // trigger vendor trust dialogs; the canary must never approve those or
    // mutate vendor-owned trust configuration on the operator's behalf.
    let working_directory = std::env::current_dir().expect("vendor PTY canary current directory");
    let (handle, mut runtime) = runtime_for(agent_id);
    let subscription = handle.subscribe(512);
    let instance_id = AgentInstanceId(instance_id);
    let initial_size = TerminalSize {
        rows: 32,
        columns: 132,
    };
    let first = turn_challenge(agent_id, "first", true);
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(agent_id).expect("valid built-in agent ID"),
                transport: TransportKind::Pty,
            },
        ))
        .expect("register live vendor PTY session");
    handle
        .dispatch(command(
            2,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: working_directory.to_string_lossy().into_owned(),
                    terminal_size: initial_size,
                    initial_prompt: Some(first.prompt.clone()),
                    session_options: None,
                },
            },
        ))
        .expect("start live vendor PTY session");

    let mut events = Vec::new();
    let result = exercise_live_session(
        agent_id,
        &first,
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        instance_id,
    )
    .await;
    let cleanup = stop_live_session(
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        instance_id,
        result.is_err(),
    )
    .await;
    let success = match result {
        Ok(success) => success,
        Err(error) => panic!(
            "{error}; cleanup={}",
            cleanup.as_ref().map_or_else(|failure| failure.as_str(), |_| "ok")
        ),
    };
    cleanup.expect("live PTY cleanup after successful scenario");

    assert_eq!(runtime.active_native_sessions(), 0);
    assert!(success.first_frame_sequence > 0);
    assert!(success.second_frame_sequence > success.first_frame_sequence);
    assert!(success.recovery_frame_sequence > success.second_frame_sequence);
    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    assert!(events.iter().any(|event| matches!(
        event.event,
        ControlEventKind::Resized { size } if size == success.resized
    )));
    assert!(!events.iter().any(|event| matches!(
        event.event,
        ControlEventKind::InputFailed { .. }
    )));
    println!(
        "vendor_pty_canary agent={agent_id} initial_prompt_response=true followup_response=true recovery_response=true first_frame_sequence={} second_frame_sequence={} recovery_frame_sequence={} complex_multiline_prompt=true resize=true interrupt_in_flight=true active_sessions=0",
        success.first_frame_sequence,
        success.second_frame_sequence,
        success.recovery_frame_sequence,
    );
}

async fn run_expected_startup_block_canary(
    agent_id: &str,
    instance_id: u64,
    expected_failure_category: &str,
) {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor PTY execution"
    );

    let working_directory = std::env::current_dir().expect("vendor PTY canary current directory");
    let (handle, mut runtime) = runtime_for(agent_id);
    let subscription = handle.subscribe(256);
    let instance_id = AgentInstanceId(instance_id);
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(agent_id).expect("valid built-in agent ID"),
                transport: TransportKind::Pty,
            },
        ))
        .expect("register startup-gate vendor PTY session");
    handle
        .dispatch(command(
            2,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: working_directory.to_string_lossy().into_owned(),
                    terminal_size: TerminalSize {
                        rows: 32,
                        columns: 132,
                    },
                    initial_prompt: Some("must-not-be-sent-before-vendor-login".to_owned()),
                    session_options: None,
                },
            },
        ))
        .expect("start startup-gate vendor PTY session");

    let mut events = Vec::new();
    let outcome = tokio::time::timeout(START_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(&subscription, &mut events);
            let snapshot = handle.snapshot();
            let Some(session) = snapshot.sessions.first() else {
                return Err("startup-gate session snapshot is missing".to_owned());
            };
            match &session.status {
                SessionStatus::Failed { .. } => {
                    return Ok(session_failure_category(&session.status));
                }
                SessionStatus::Running => {
                    return Err("vendor session bypassed the expected startup gate".to_owned());
                }
                SessionStatus::Exited { .. } => {
                    return Err("vendor session exited before reporting the startup gate".to_owned());
                }
                SessionStatus::Registered | SessionStatus::Starting | SessionStatus::Stopping => {}
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "vendor startup-gate canary timed out".to_owned())
    .and_then(|result| result);
    let cleanup = stop_live_session(
        &mut runtime,
        &handle,
        &subscription,
        &mut events,
        instance_id,
        true,
    )
    .await;
    let category = outcome.unwrap_or_else(|error| {
        panic!(
            "{error}; cleanup={}",
            cleanup.as_ref().map_or_else(|failure| failure.as_str(), |_| "ok")
        )
    });
    cleanup.expect("startup-gate canary cleanup");

    assert_eq!(category, expected_failure_category);
    assert_eq!(runtime.active_native_sessions(), 0);
    assert!(!events.iter().any(|event| matches!(
        event.event,
        ControlEventKind::InputCompleted { .. } | ControlEventKind::InputFailed { .. }
    )));
    println!(
        "vendor_pty_startup_block_canary agent={agent_id} failure_category={category} input_events=0 active_sessions=0"
    );
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_PTY_CANARY=1 and an installed authenticated Claude CLI"]
async fn windows_live_claude_pty_contract() {
    run_pty_canary("claude", 10_001).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_PTY_CANARY=1 and an installed authenticated Codex CLI"]
async fn windows_live_codex_pty_contract() {
    run_pty_canary("codex", 10_002).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_PTY_CANARY=1 and an installed authenticated Kimi Code CLI"]
async fn windows_live_kimi_pty_contract() {
    run_pty_canary("kimi", 10_003).await;
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_PTY_CANARY=1 and authenticated Codex and Kimi Code CLIs"]
async fn windows_live_parallel_codex_kimi_pty_process_isolation() {
    assert_eq!(
        std::env::var(LIVE_CANARY_ENV).as_deref(),
        Ok("1"),
        "set {LIVE_CANARY_ENV}=1 to opt into authenticated vendor PTY execution"
    );
    let working_directory = std::env::current_dir().expect("parallel PTY canary current directory");
    let (handle, mut runtime) = runtime_for_agents(&["codex", "kimi"]);
    let subscription = handle.subscribe(1_024);
    let codex_id = AgentInstanceId(20_001);
    let kimi_id = AgentInstanceId(20_002);
    let codex_turn = turn_challenge("codex", "parallel", false);
    let kimi_turn = turn_challenge("kimi", "parallel", false);

    for (command_id, instance_id, agent_id) in [
        (1, codex_id, "codex"),
        (2, kimi_id, "kimi"),
    ] {
        handle
            .dispatch(command(
                command_id,
                ControlCommand::Register {
                    instance_id,
                    agent_id: AgentId::new(agent_id).expect("valid parallel agent ID"),
                    transport: TransportKind::Pty,
                },
            ))
            .expect("register parallel PTY session");
    }
    for (command_id, instance_id, prompt) in [
        (3, codex_id, codex_turn.prompt.clone()),
        (4, kimi_id, kimi_turn.prompt.clone()),
    ] {
        handle
            .dispatch(command(
                command_id,
                ControlCommand::Start {
                    instance_id,
                    request: StartRequest {
                        working_directory: working_directory.to_string_lossy().into_owned(),
                        terminal_size: TerminalSize {
                            rows: 32,
                            columns: 132,
                        },
                        initial_prompt: Some(prompt),
                        session_options: None,
                    },
                },
            ))
            .expect("start parallel PTY session");
    }

    let mut events = Vec::new();
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(&subscription, &mut events);
            let snapshot = handle.snapshot();
            let codex = snapshot
                .sessions
                .iter()
                .find(|session| session.instance_id == codex_id)
                .expect("parallel Codex snapshot");
            let kimi = snapshot
                .sessions
                .iter()
                .find(|session| session.instance_id == kimi_id)
                .expect("parallel Kimi snapshot");
            let codex_ready = codex
                .terminal_frame
                .as_ref()
                .is_some_and(|frame| frame.contents.contains(&codex_turn.marker));
            let kimi_ready = kimi
                .terminal_frame
                .as_ref()
                .is_some_and(|frame| frame.contents.contains(&kimi_turn.marker));
            if codex_ready && kimi_ready {
                assert!(!codex
                    .terminal_frame
                    .as_ref()
                    .unwrap()
                    .contents
                    .contains(&kimi_turn.marker));
                assert!(!kimi
                    .terminal_frame
                    .as_ref()
                    .unwrap()
                    .contents
                    .contains(&codex_turn.marker));
                break;
            }
            assert!(!matches!(
                codex.status,
                SessionStatus::Failed { .. } | SessionStatus::Exited { .. }
            ));
            assert!(!matches!(
                kimi.status,
                SessionStatus::Failed { .. } | SessionStatus::Exited { .. }
            ));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("parallel PTY initial turns timed out");
    assert_eq!(runtime.active_native_sessions(), 2);

    handle
        .dispatch(command(
            5,
            ControlCommand::Stop {
                instance_id: codex_id,
                force: false,
            },
        ))
        .expect("stop only parallel Codex session");
    tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(&subscription, &mut events);
            let snapshot = handle.snapshot();
            let codex = snapshot
                .sessions
                .iter()
                .find(|session| session.instance_id == codex_id)
                .expect("stopped Codex snapshot");
            let kimi = snapshot
                .sessions
                .iter()
                .find(|session| session.instance_id == kimi_id)
                .expect("surviving Kimi snapshot");
            if matches!(codex.status, SessionStatus::Exited { .. }) {
                assert!(matches!(kimi.status, SessionStatus::Running));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stopping Codex affected parallel session lifecycle");
    assert_eq!(runtime.active_native_sessions(), 1);

    let recovery = turn_challenge("kimi", "survivor", false);
    let recovery_boundary = events.len();
    submit_prompt(&handle, kimi_id, 6, recovery.prompt).expect("submit surviving Kimi prompt");
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(&subscription, &mut events);
            if let Some(message) = events[recovery_boundary..].iter().find_map(|event| {
                match &event.event {
                    ControlEventKind::InputFailed { message, .. }
                        if event.instance_id == kimi_id => Some(message.as_str()),
                    _ => None,
                }
            }) {
                panic!("surviving Kimi input failed: {message}");
            }
            let snapshot = handle.snapshot();
            let kimi = snapshot
                .sessions
                .iter()
                .find(|session| session.instance_id == kimi_id)
                .expect("surviving Kimi snapshot");
            if kimi
                .terminal_frame
                .as_ref()
                .is_some_and(|frame| frame.contents.contains(&recovery.marker))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("surviving Kimi did not answer after Codex stopped");

    handle
        .dispatch(command(
            7,
            ControlCommand::Stop {
                instance_id: kimi_id,
                force: false,
            },
        ))
        .expect("stop surviving Kimi session");
    tokio::time::timeout(STOP_TIMEOUT, async {
        loop {
            runtime.tick().await;
            drain_events(&subscription, &mut events);
            if runtime.active_native_sessions() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("parallel PTY cleanup timed out");

    println!(
        "vendor_pty_parallel agents=codex,kimi concurrent_active=2 stop_one_survivor_running=true survivor_followup=true cross_session_marker_leak=false active_sessions=0"
    );
}

#[tokio::test]
#[ignore = "requires GATE4AGENT_VENDOR_PTY_CANARY=1 and an installed Qwen Code CLI without completed login"]
async fn windows_live_qwen_pty_without_login_fails_closed() {
    run_expected_startup_block_canary("qwen-code", 10_004, "readiness-timeout").await;
}
