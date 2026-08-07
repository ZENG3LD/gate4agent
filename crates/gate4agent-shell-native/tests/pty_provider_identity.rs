use std::time::Duration;

use gate4agent_catalog::AgentRegistry;
use gate4agent_kernel::Gate4AgentKernel;
use gate4agent_shell_native::NativeEffectShell;
use gate4agent_testkit::{pty_provider_agent_spec, PTY_PROVIDER_FIXTURE_ID};
use gate4agent_types::{
    AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlObservation,
    ProviderEvent, StartRequest, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

#[tokio::test]
async fn fresh_codex_pty_does_not_publish_a_transport_local_provider_identity() {
    let registry = AgentRegistry::new([pty_provider_agent_spec()]).expect("Codex fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(8_032);
    let registered = kernel.step(
        [command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(PTY_PROVIDER_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    assert!(registered.command_outcomes[0].result.is_ok());
    let started = kernel.step(
        [command(
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
                    initial_prompt: None,
                    session_options: None,
                },
            },
        )],
        [],
    );
    let spawned = shell.execute(started.effects[0].clone()).await;
    kernel.step([], [spawned]);

    let observed = tokio::time::timeout(Duration::from_secs(15), async {
        let mut observed = Vec::new();
        loop {
            let batch = shell.collect_provider_events();
            observed.extend(batch.iter().filter_map(|observation| match &observation.observation {
                ControlObservation::ProviderEvent { event, .. } => Some(event.clone()),
                _ => None,
            }));
            kernel.step([], batch);
            if observed.iter().any(|event| {
                matches!(
                    event,
                    ProviderEvent::Text { text, .. }
                        if text.contains("fixture-pty-response")
                )
            }) {
                break observed;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Codex PTY provider output timeout");

    assert!(observed.iter().all(|event| !matches!(
        event,
        ProviderEvent::SessionStarted { .. }
            | ProviderEvent::SessionIdentityObserved { .. }
    )));
    assert!(kernel.snapshot().sessions[0].provider.session.is_none());

    let stopped = kernel.step(
        [command(
            3,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        )],
        [],
    );
    let observation = shell.execute(stopped.effects[0].clone()).await;
    kernel.step([], [observation]);
    assert_eq!(shell.active_session_count(), 0);
}
