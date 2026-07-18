use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_testkit::{interactive_agent_spec, CONTROL_FIXTURE_ID};
use gate4agent_types::{
    AgentCommand, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand,
    ControlEvent, InputAction, SessionStatus, StartRequest, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
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

#[tokio::test]
async fn public_handle_drives_embedded_runtime_to_real_pty_and_back() {
    let registry = AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(
        registry,
        NativeRuntimeConfig {
            command_capacity: 16,
            max_commands_per_tick: 16,
            ..NativeRuntimeConfig::default()
        },
    );
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(701);
    let initial_size = TerminalSize {
        rows: 12,
        columns: 48,
    };
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
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
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: initial_size,
                    initial_prompt: None,
                },
            },
        ))
        .unwrap();

    let started = tokio::time::timeout(Duration::from_millis(500), runtime.tick())
        .await
        .expect("start dispatch must not await PTY spawn");
    assert!(started
        .command_outcomes
        .iter()
        .all(|outcome| outcome.result.is_ok()));
    assert_eq!(started.effects_dispatched, 1);
    assert_eq!(handle.snapshot().sessions[0].status, SessionStatus::Starting);

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            if handle.snapshot().sessions[0].status == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("PTY spawn completion timeout");
    assert_eq!(runtime.active_native_sessions(), 1);

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if handle.snapshot().sessions[0]
                .terminal_frame
                .as_ref()
                .is_some_and(|frame| frame.contents.contains("fixture-ready>"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            runtime.tick().await;
        }
    })
    .await
    .expect("initial terminal frame timeout");

    handle
        .dispatch(command(
            3,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::AgentCommand(AgentCommand {
                    agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                    name: "status".to_owned(),
                    arguments: vec!["detail".to_owned()],
                }),
            },
        ))
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), runtime.tick())
        .await
        .expect("input dispatch must not await readiness");
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if handle.snapshot().sessions[0]
                .terminal_frame
                .as_ref()
                .is_some_and(|frame| frame.contents.contains("fixture-echo:/status detail"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            runtime.tick().await;
        }
    })
    .await
    .expect("agent-command output timeout");

    let resized = TerminalSize {
        rows: 21,
        columns: 71,
    };
    handle
        .dispatch(command(
            4,
            ControlCommand::Resize {
                instance_id,
                size: resized,
            },
        ))
        .unwrap();
    runtime.tick().await;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            if handle.snapshot().sessions[0].terminal_size == Some(resized) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("resize completion timeout");

    handle
        .dispatch(command(
            5,
            ControlCommand::Stop {
                instance_id,
                force: false,
            },
        ))
        .unwrap();
    runtime.tick().await;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            if matches!(
                handle.snapshot().sessions[0].status,
                SessionStatus::Exited { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stop completion timeout");
    assert!(matches!(
        handle.snapshot().sessions[0].status,
        SessionStatus::Exited { .. }
    ));
    assert!(handle.snapshot().sessions[0]
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame.contents.contains("fixture-echo:/status detail")));
    assert_eq!(runtime.active_native_sessions(), 0);

    let mut events = Vec::new();
    drain_events(&subscription, &mut events);
    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    assert!(events.iter().any(|event| matches!(
        event.event,
        gate4agent_types::ControlEventKind::InputCompleted { .. }
    )));
    assert!(events.iter().any(|event| matches!(
        event.event,
        gate4agent_types::ControlEventKind::Resized { .. }
    )));
}

#[tokio::test]
async fn rejected_commands_are_visible_on_the_ordered_event_port() {
    let registry = AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
    let subscription = handle.subscribe(4);
    handle
        .dispatch(command(
            90,
            ControlCommand::Register {
                instance_id: AgentInstanceId(999),
                agent_id: AgentId::new("absent-agent").unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();

    let tick = runtime.tick().await;
    assert!(tick.command_outcomes[0].result.is_err());
    assert!(handle.snapshot().sessions.is_empty());
    let event = subscription.try_recv().expect("command rejection event");
    assert_eq!(event.command_id, Some(CommandId(90)));
    assert!(matches!(
        event.event,
        gate4agent_types::ControlEventKind::CommandRejected { .. }
    ));
}

#[tokio::test]
async fn two_agent_instances_progress_on_independent_effect_workers() {
    let registry = AgentRegistry::new([interactive_agent_spec()]).expect("fixture registry");
    let (handle, mut runtime) = NativeRuntime::new(registry, NativeRuntimeConfig::default());
    let instances = [AgentInstanceId(801), AgentInstanceId(802)];
    for (index, instance_id) in instances.into_iter().enumerate() {
        handle
            .dispatch(command(
                100 + (index as u64 * 2),
                ControlCommand::Register {
                    instance_id,
                    agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                    transport: TransportKind::Pty,
                },
            ))
            .unwrap();
        handle
            .dispatch(command(
                101 + (index as u64 * 2),
                ControlCommand::Start {
                    instance_id,
                    request: StartRequest {
                        working_directory: std::env::current_dir()
                            .expect("current directory")
                            .to_string_lossy()
                            .into_owned(),
                        terminal_size: TerminalSize {
                            rows: 12,
                            columns: 48,
                        },
                        initial_prompt: None,
                    },
                },
            ))
            .unwrap();
    }
    tokio::time::timeout(Duration::from_millis(500), runtime.tick())
        .await
        .expect("multi-agent spawn dispatch must not await workers");

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            let snapshot = handle.snapshot();
            if snapshot
                .sessions
                .iter()
                .all(|session| session.status == SessionStatus::Running)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("multi-agent spawn completion timeout");
    assert_eq!(runtime.active_native_sessions(), 2);

    for (index, instance_id) in instances.into_iter().enumerate() {
        handle
            .dispatch(command(
                110 + index as u64,
                ControlCommand::SendInput {
                    instance_id,
                    action: InputAction::AgentCommand(AgentCommand {
                        agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                        name: "status".to_owned(),
                        arguments: vec![format!("worker-{index}")],
                    }),
                },
            ))
            .unwrap();
    }
    tokio::time::timeout(Duration::from_millis(500), runtime.tick())
        .await
        .expect("multi-agent input dispatch must not await readiness workers");

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            let snapshot = handle.snapshot();
            if snapshot.sessions.iter().enumerate().all(|(index, session)| {
                session.terminal_frame.as_ref().is_some_and(|frame| {
                    frame
                        .contents
                        .contains(&format!("fixture-echo:/status worker-{index}"))
                })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("independent agent-command completion timeout");

    for (index, instance_id) in instances.into_iter().enumerate() {
        handle
            .dispatch(command(
                120 + index as u64,
                ControlCommand::Stop {
                    instance_id,
                    force: false,
                },
            ))
            .unwrap();
    }
    runtime.tick().await;
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            runtime.tick().await;
            if handle
                .snapshot()
                .sessions
                .iter()
                .all(|session| matches!(session.status, SessionStatus::Exited { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("multi-agent stop timeout");
    assert_eq!(runtime.active_native_sessions(), 0);
}
