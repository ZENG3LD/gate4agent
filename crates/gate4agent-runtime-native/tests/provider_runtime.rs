use std::time::Duration;

use gate4agent_catalog::{builtin_adapter_registry, AgentRegistry};
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_testkit::{
    acp_agent_spec, interactive_agent_spec, pipe_agent_spec, pty_provider_agent_spec,
    ACP_FIXTURE_ID, CONTROL_FIXTURE_ID, PIPE_FIXTURE_ID, PTY_PROVIDER_FIXTURE_ID,
};
use gate4agent_types::{
    AdapterFamily, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand,
    ControlEvent, ControlEventKind, ForegroundAuthority, ForegroundProcessKind, InputAction,
    PreparedInputKind, PromptFraming, PromptPayload, ProviderActivity, ProviderEvent,
    ProviderInteractionKind, ProviderInteractionOutcome, ProviderInteractionStatus, ProviderSource,
    SessionStatus, ShellCommand, StartRequest, TerminalControl, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn runtime(
    spec: gate4agent_types::AgentSpec,
) -> (gate4agent_handle::Gate4AgentHandle, NativeRuntime) {
    NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    )
}

async fn drive_until(
    runtime: &mut NativeRuntime,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    mut predicate: impl FnMut(&NativeRuntime, &[ControlEvent]) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if predicate(runtime, events) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controlled provider runtime timeout");
}

#[tokio::test]
async fn pipe_one_shot_reaches_public_snapshot_with_semantic_events() {
    let (handle, mut runtime) = runtime(pipe_agent_spec());
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(41);
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(PIPE_FIXTURE_ID).unwrap(),
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
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: Some("fixture prompt".to_owned()),
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| matches!(session.status, SessionStatus::Exited { .. }))
    })
    .await;

    let snapshot = handle.snapshot();
    let session = snapshot.sessions.first().expect("pipe session snapshot");
    assert_eq!(
        session.provider.session_id.as_deref(),
        Some("fixture-thread")
    );
    assert_eq!(session.provider.completed_turns, 1);
    assert_eq!(session.provider.usage.input_tokens, 3);
    assert_eq!(session.provider.usage.output_tokens, 5);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::ProviderEvent {
            event: ProviderEvent::Text { text, .. },
            ..
        } if text == "fixture-pipe-response"
    )));
}

#[tokio::test]
async fn acp_multi_turn_prompt_streams_and_stops_through_public_handle() {
    let (handle, mut runtime) = runtime(acp_agent_spec());
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(42);
    handle
        .dispatch(command(
            10,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(ACP_FIXTURE_ID).unwrap(),
                transport: TransportKind::Acp,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            11,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| session.status == SessionStatus::Running)
    })
    .await;

    handle
        .dispatch(command(
            12,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::SubmitPrompt(PromptPayload {
                    text: "fixture turn".to_owned(),
                    framing: PromptFraming::Literal,
                }),
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| session.provider.completed_turns >= 1)
    })
    .await;

    let snapshot = handle.snapshot();
    let session = snapshot.sessions.first().expect("ACP session snapshot");
    assert_eq!(
        session.provider.session_id.as_deref(),
        Some("fixture-acp-session")
    );
    assert_eq!(session.provider.usage.input_tokens, 7);
    assert_eq!(session.provider.usage.output_tokens, 11);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::ProviderEvent {
            event: ProviderEvent::Text { text, .. },
            ..
        } if text == "fixture-acp-response"
    )));

    handle
        .dispatch(command(
            13,
            ControlCommand::Stop {
                instance_id,
                force: false,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| matches!(session.status, SessionStatus::Exited { .. }))
    })
    .await;
}

#[tokio::test]
async fn pty_classification_uses_the_same_provider_event_contract() {
    let (handle, mut runtime) = runtime(pty_provider_agent_spec());
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(43);
    handle
        .dispatch(command(
            20,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(PTY_PROVIDER_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            21,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, |_, events| {
        events.iter().any(|event| {
            matches!(
                &event.event,
                ControlEventKind::ProviderEvent {
                    event: ProviderEvent::Text { text, .. },
                    ..
                } if text.contains("fixture-pty-response")
            )
        })
    })
    .await;
    handle
        .dispatch(command(
            22,
            ControlCommand::RefreshForeground { instance_id },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| session.foreground.authority == ForegroundAuthority::Confirmed)
    })
    .await;
    let foreground = handle.snapshot().sessions[0]
        .foreground
        .process
        .clone()
        .expect("confirmed foreground process");
    assert!(matches!(
        foreground.kind,
        ForegroundProcessKind::Agent { agent_id }
            if agent_id.as_str() == PTY_PROVIDER_FIXTURE_ID
    ));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::ForegroundObserved { process }
            if process.process_id == foreground.process_id
    )));

    handle
        .dispatch(command(
            23,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| matches!(session.status, SessionStatus::Exited { .. }))
    })
    .await;
}

#[tokio::test]
async fn public_handle_shell_command_requires_and_uses_live_shell_route() {
    let mut spec = interactive_agent_spec();
    spec.detection.command = CONTROL_FIXTURE_ID.to_owned();
    let (handle, mut runtime) = runtime(spec);
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(45);
    handle
        .dispatch(command(
            40,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            41,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle.snapshot().sessions.first().is_some_and(|session| {
            session.status == SessionStatus::Running
                && session
                    .terminal_frame
                    .as_ref()
                    .is_some_and(|frame| frame.contents.contains("fixture-ready>"))
        })
    })
    .await;

    handle
        .dispatch(command(
            42,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::ShellCommand(ShellCommand {
                    text: "printf public-shell-route".to_owned(),
                }),
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, events| {
        events.iter().any(|event| {
            matches!(
                event.event,
                ControlEventKind::InputCompleted {
                    input_kind: PreparedInputKind::ShellCommand
                }
            )
        }) && handle.snapshot().sessions[0]
            .terminal_frame
            .as_ref()
            .is_some_and(|frame| {
                frame
                    .contents
                    .contains("fixture-echo:printf public-shell-route")
            })
    })
    .await;

    handle
        .dispatch(command(
            43,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| matches!(session.status, SessionStatus::Exited { .. }))
    })
    .await;
}

#[tokio::test]
async fn external_hook_ingress_reaches_the_public_snapshot_without_shell_authority() {
    let mut spec = pty_provider_agent_spec();
    let hook_binding = builtin_adapter_registry()
        .binding(AdapterFamily::Hook, "grok")
        .unwrap()
        .clone();
    spec.capabilities.adapters.hook = Some(hook_binding.clone());
    let (handle, mut runtime) = runtime(spec);
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(44);
    handle
        .dispatch(command(
            30,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(PTY_PROVIDER_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            31,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| session.status == SessionStatus::Running)
    })
    .await;
    let generation = handle.snapshot().sessions[0].generation;
    handle
        .dispatch(command(
            32,
            ControlCommand::IngestProvider {
                instance_id,
                generation,
                source: ProviderSource {
                    family: AdapterFamily::Hook,
                    binding: hook_binding.clone(),
                },
                source_sequence: 1,
                events: vec![
                    ProviderEvent::TurnStarted {
                        prompt: Some("external hook turn".to_owned()),
                    },
                    ProviderEvent::InteractionRequested {
                        request_id: Some("question-1".to_owned()),
                        interaction_kind: ProviderInteractionKind::Question,
                        tool_name: "ask_user_question".to_owned(),
                        prompt: "{\"question\":\"choose\"}".to_owned(),
                    },
                ],
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| session.provider.activity == ProviderActivity::WaitingForInput)
    })
    .await;

    let snapshot = handle.snapshot();
    let provider = &snapshot.sessions[0].provider;
    assert_eq!(
        provider.current_prompt.as_deref(),
        Some("external hook turn")
    );
    assert!(provider
        .sources
        .iter()
        .any(|cursor| cursor.source.family == AdapterFamily::Hook && cursor.sequence == 1));
    assert!(matches!(
        provider.interactions.as_slice(),
        [interaction]
            if interaction.interaction_kind == ProviderInteractionKind::Question
                && interaction.status == ProviderInteractionStatus::Pending
    ));
    let interaction_id = provider.interactions[0].id;
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::InteractionRequested { interaction }
            if interaction.id == interaction_id
    )));

    handle
        .dispatch(command(
            33,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::TerminalControl(TerminalControl::Interrupt),
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, events| {
        events.iter().any(|event| {
            matches!(
                event.event,
                ControlEventKind::InteractionResolved {
                    interaction_id: observed,
                    outcome: ProviderInteractionOutcome::Interrupted,
                } if observed == interaction_id
            )
        })
    })
    .await;
    assert_eq!(
        handle.snapshot().sessions[0].provider.interactions[0].status,
        ProviderInteractionStatus::Resolved {
            outcome: ProviderInteractionOutcome::Interrupted
        }
    );

    handle
        .dispatch(command(
            34,
            ControlCommand::IngestProvider {
                instance_id,
                generation,
                source: ProviderSource {
                    family: AdapterFamily::Hook,
                    binding: hook_binding.clone(),
                },
                source_sequence: 2,
                events: vec![
                    ProviderEvent::SubagentStarted {
                        agent_id: "child-public-1".to_owned(),
                        agent_type: Some("reviewer".to_owned()),
                        description: Some("public path review".to_owned()),
                    },
                    ProviderEvent::TurnCompleted {
                        usage: Default::default(),
                        is_cumulative: false,
                    },
                ],
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        let provider = &handle.snapshot().sessions[0].provider;
        provider.lead_activity == ProviderActivity::Idle
            && provider.activity == ProviderActivity::Working
            && provider.subagents.len() == 1
    })
    .await;

    handle
        .dispatch(command(
            35,
            ControlCommand::IngestProvider {
                instance_id,
                generation,
                source: ProviderSource {
                    family: AdapterFamily::Hook,
                    binding: hook_binding,
                },
                source_sequence: 3,
                events: vec![ProviderEvent::SubagentStopped {
                    agent_id: "child-public-1".to_owned(),
                }],
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        let provider = &handle.snapshot().sessions[0].provider;
        provider.activity == ProviderActivity::Idle && provider.subagents.is_empty()
    })
    .await;

    handle
        .dispatch(command(
            36,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, |_, _| {
        handle
            .snapshot()
            .sessions
            .first()
            .is_some_and(|session| matches!(session.status, SessionStatus::Exited { .. }))
    })
    .await;
}
