use gate4agent_handle::bounded_port;
use gate4agent_kernel::Gate4AgentKernel;
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEffect,
    ProviderRuntimePolicy, SessionStatus, StartRequest, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};

#[test]
fn handle_drives_kernel_and_publishes_one_authoritative_snapshot() {
    let (handle, kernel_port) = bounded_port(8);
    let subscription = handle.subscribe(8);
    let instance_id = AgentInstanceId(41);
    handle
        .dispatch(CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(1),
            command: ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("claude").unwrap(),
                transport: TransportKind::Pty,
            },
        })
        .unwrap();
    handle
        .dispatch(CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(2),
            command: ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
                request: StartRequest {
                    working_directory: ".".to_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        })
        .unwrap();

    let mut kernel = Gate4AgentKernel::default();
    let step = kernel.step(kernel_port.drain_commands(8), []);
    kernel_port.publish_snapshot(step.snapshot.clone());
    let report = kernel_port.publish_events(step.events.clone());

    assert!(step.command_outcomes.iter().all(|outcome| outcome.result.is_ok()));
    assert!(matches!(step.effects[0].effect, ControlEffect::Spawn { .. }));
    assert_eq!(handle.snapshot().sessions[0].status, SessionStatus::Starting);
    assert_eq!(report.delivered, 2);
    assert_eq!(subscription.try_recv().unwrap().sequence, 1);
    assert_eq!(subscription.try_recv().unwrap().sequence, 2);
}
