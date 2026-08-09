#![cfg(windows)]

use std::time::Duration;

use gate4agent_adapters::builtin_adapter_registry;
use gate4agent_catalog::AgentRegistry;
use gate4agent_kernel::Gate4AgentKernel;
use gate4agent_shell_native::NativeEffectShell;
use gate4agent_testkit::interactive_agent_spec;
use gate4agent_types::{
    AdapterFamily, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand,
    ControlObservation, ProviderEvent, ProviderRuntimePolicy, ProviderSessionKey, StartRequest,
    TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

fn semantic_runtime_policy() -> ProviderRuntimePolicy {
    ProviderRuntimePolicy::new(true, true, true, true, true).unwrap()
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

#[tokio::test]
async fn kimi_pty_probes_status_and_emits_only_the_authoritative_session_identity() {
    let mut spec = interactive_agent_spec();
    spec.id = AgentId::new("kimi").unwrap();
    spec.display_name = "Controlled Kimi identity fixture".to_owned();
    spec.capabilities.transports.pty_adapter = Some(
        builtin_adapter_registry()
            .binding(AdapterFamily::PtySemantic, "kimi")
            .expect("Kimi PTY semantic binding")
            .clone(),
    );
    spec.readiness = gate4agent_catalog::builtin_registry()
        .get_by_id("kimi")
        .expect("built-in Kimi spec")
        .readiness
        .clone();
    spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
        name: "kimi".to_owned(),
    }];
    *spec
        .launch
        .fixed_args
        .last_mut()
        .expect("PowerShell fixture script") = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h' + [char]27 + '[?25hkimi-ready>'); $line=[Console]::ReadLine(); if($line -notmatch '/status'){ [Console]::WriteLine('unexpected-probe:' + $line); exit 41 }; [Console]::WriteLine('  ' + [char]0x2502 + ' Session       session_fixture-19 ' + [char]0x2502); Start-Sleep -Seconds 60".to_owned();

    let registry = AgentRegistry::new([spec]).expect("Kimi fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(8_031);
    let registered = kernel.step(
        [command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("kimi").unwrap(),
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
                runtime_policy: semantic_runtime_policy(),
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
    assert!(started.command_outcomes[0].result.is_ok());
    assert_eq!(started.effects.len(), 1);
    let spawned = shell.execute(started.effects[0].clone()).await;
    assert!(
        matches!(spawned.observation, ControlObservation::Spawned { .. }),
        "Kimi spawn failed: {:?}",
        spawned.observation
    );
    assert!(shell
        .terminal_snapshot(gate4agent_shell_native::NativeSessionKey {
            instance_id,
            generation: started.effects[0].generation,
        })
        .expect("Kimi status snapshot")
        .contents
        .contains("session_fixture-19"));
    let running = kernel.step([], [spawned]);
    assert!(running.command_outcomes.is_empty());

    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let events = shell
                .collect_provider_events()
                .into_iter()
                .filter_map(|observation| match observation.observation {
                    ControlObservation::ProviderEvent { event, .. } => Some(event),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if events.iter().any(|event| {
                matches!(
                    event,
                    ProviderEvent::SessionIdentityObserved { identity }
                        if identity.id == "session_fixture-19"
                )
            }) {
                break events;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Kimi PTY identity timeout");

    assert!(observed.iter().any(|event| {
        matches!(
            event,
            ProviderEvent::SessionStarted { session_id, .. }
                if session_id == "session_fixture-19"
        )
    }));
    assert!(observed.iter().all(|event| match event {
        ProviderEvent::SessionStarted { session_id, .. } => session_id == "session_fixture-19",
        ProviderEvent::SessionIdentityObserved { identity } => {
            identity.key == ProviderSessionKey::SessionId
                && identity.id == "session_fixture-19"
                && identity.transcript_path.is_none()
        }
        _ => true,
    }));

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
