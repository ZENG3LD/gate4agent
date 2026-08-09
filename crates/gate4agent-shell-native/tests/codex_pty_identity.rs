#![cfg(windows)]

use std::time::Duration;

use gate4agent_adapters::builtin_adapter_registry;
use gate4agent_catalog::AgentRegistry;
use gate4agent_kernel::Gate4AgentKernel;
use gate4agent_shell_native::{NativeEffectShell, NativeSessionKey};
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
async fn codex_pty_probes_status_and_emits_only_the_authoritative_session_identity() {
    const SESSION_ID: &str = "0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e";

    let mut spec = interactive_agent_spec();
    spec.id = AgentId::new("codex").unwrap();
    spec.display_name = "Controlled Codex identity fixture".to_owned();
    spec.capabilities.transports.pty_adapter = Some(
        builtin_adapter_registry()
            .binding(AdapterFamily::PtySemantic, "codex")
            .expect("Codex PTY semantic binding")
            .clone(),
    );
    spec.readiness = gate4agent_catalog::builtin_registry()
        .get_by_id("codex")
        .expect("built-in Codex spec")
        .readiness
        .clone();
    spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
        name: "codex".to_owned(),
    }];
    *spec
        .launch
        .fixed_args
        .last_mut()
        .expect("PowerShell fixture script") = format!(
        "$esc=[char]27; [Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write($esc + '[?2004h' + [char]0x276F); $line=[Console]::ReadLine(); if($line -ne '/status'){{ [Console]::WriteLine('unexpected-probe:' + $line); exit 41 }}; [Console]::Write($esc + '[2J' + $esc + '[H' + '  ' + [char]0x2502 + '  Session:          {SESSION_ID} ' + [char]0x2502); Start-Sleep -Seconds 60"
    );

    let registry = AgentRegistry::new([spec]).expect("Codex fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(8_032);
    let registered = kernel.step(
        [command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("codex").unwrap(),
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
        "Codex spawn failed: {:?}",
        spawned.observation
    );
    let key = NativeSessionKey {
        instance_id,
        generation: started.effects[0].generation,
    };
    assert!(shell
        .terminal_snapshot(key)
        .expect("Codex status snapshot")
        .contents
        .contains(SESSION_ID));
    kernel.step([], [spawned]);

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
                        if identity.id == SESSION_ID
                )
            }) {
                break events;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Codex PTY identity timeout");

    assert!(observed.iter().any(|event| {
        matches!(
            event,
            ProviderEvent::SessionStarted { session_id, .. } if session_id == SESSION_ID
        )
    }));
    assert!(observed.iter().all(|event| match event {
        ProviderEvent::SessionStarted { session_id, .. } => session_id == SESSION_ID,
        ProviderEvent::SessionIdentityObserved { identity } => {
            identity.key == ProviderSessionKey::SessionId
                && identity.id == SESSION_ID
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

#[tokio::test]
async fn codex_pty_identity_layout_miss_keeps_the_fresh_session_running() {
    let mut spec = interactive_agent_spec();
    spec.id = AgentId::new("codex").unwrap();
    spec.display_name = "Controlled Codex identity layout-miss fixture".to_owned();
    spec.capabilities.transports.pty_adapter = Some(
        builtin_adapter_registry()
            .binding(AdapterFamily::PtySemantic, "codex")
            .expect("Codex PTY semantic binding")
            .clone(),
    );
    spec.readiness = gate4agent_catalog::builtin_registry()
        .get_by_id("codex")
        .expect("built-in Codex spec")
        .readiness
        .clone();
    spec.readiness.timeout_ms = 750;
    spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
        name: "codex".to_owned(),
    }];
    *spec
        .launch
        .fixed_args
        .last_mut()
        .expect("PowerShell fixture script") =
        "$esc=[char]27; [Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write($esc + '[?2004h' + [char]0x276F); $line=[Console]::ReadLine(); if($line -ne '/status'){ [Console]::WriteLine('unexpected-probe:' + $line); exit 41 }; [Console]::WriteLine('  ' + [char]0x2502 + ' Status: ready without an ID ' + [char]0x2502); [Console]::Write($esc + '[?2004h' + [char]0x276F); Start-Sleep -Seconds 60".to_owned();

    let registry = AgentRegistry::new([spec]).expect("Codex fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(8_033);
    let registered = kernel.step(
        [command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("codex").unwrap(),
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
    let spawned = shell.execute(started.effects[0].clone()).await;
    assert!(
        matches!(spawned.observation, ControlObservation::Spawned { .. }),
        "Codex spawn failed after an identity layout miss: {:?}",
        spawned.observation
    );
    assert_eq!(shell.active_session_count(), 1);
    kernel.step([], [spawned]);

    let events = shell
        .collect_provider_events()
        .into_iter()
        .filter_map(|observation| match observation.observation {
            ControlObservation::ProviderEvent { event, .. } => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(events.iter().all(|event| !matches!(
        event,
        ProviderEvent::SessionStarted { .. }
            | ProviderEvent::SessionIdentityObserved { .. }
    )));

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
