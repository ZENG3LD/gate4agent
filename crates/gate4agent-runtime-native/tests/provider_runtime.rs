use std::time::Duration;

use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_testkit::{acp_agent_spec, pipe_agent_spec, ACP_FIXTURE_ID, PIPE_FIXTURE_ID};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, InputAction, PromptFraming, PromptPayload, ProviderEvent, SessionStatus,
    StartRequest, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn runtime(spec: gate4agent_types::AgentSpec) -> (gate4agent_handle::Gate4AgentHandle, NativeRuntime) {
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
    assert_eq!(session.provider.session_id.as_deref(), Some("fixture-thread"));
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
