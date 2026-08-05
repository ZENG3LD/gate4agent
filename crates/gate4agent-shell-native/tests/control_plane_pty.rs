use std::time::Duration;

use gate4agent_catalog::AgentRegistry;
use gate4agent_kernel::Gate4AgentKernel;
use gate4agent_shell_native::{NativeEffectShell, NativeSessionKey};
use gate4agent_testkit::{
    exiting_agent_spec, interactive_agent_spec, pty_provider_agent_spec, CONTROL_FIXTURE_ID,
    PTY_PROVIDER_FIXTURE_ID,
};
use gate4agent_types::{
    AgentCommand, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand,
    ControlEventKind, ControlObservation, DraftReadySignal, InitialPromptMode, InputAction,
    PreparedInputKind, PromptFraming, PromptPayload, SessionStatus, ShellCommand, StartRequest,
    TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

async fn execute_only_effect(
    kernel: &mut Gate4AgentKernel,
    shell: &mut NativeEffectShell,
    command: CommandEnvelope,
) {
    let step = kernel.step([command], []);
    assert!(step.command_outcomes[0].result.is_ok());
    assert_eq!(step.effects.len(), 1);
    let observation = shell.execute(step.effects[0].clone()).await;
    let completed = kernel.step([], [observation]);
    assert!(completed.effects.is_empty());
}

async fn wait_for_contents(shell: &NativeEffectShell, key: NativeSessionKey, expected: &str) {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            if shell
                .terminal_snapshot(key)
                .expect("fixture terminal snapshot")
                .contents
                .contains(expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture output timeout");
}

#[tokio::test]
async fn kernel_effects_drive_real_pty_input_resize_and_tree_stop() {
    let mut spec = interactive_agent_spec();
    #[cfg(windows)]
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h' + [char]27 + '[?25hfixture-ready>'); for($i=0; $i -lt 3; $i++){ $line=[Console]::ReadLine(); [Console]::WriteLine('fixture-echo:' + $line) }; Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let script = "printf '\\033[?2004h\\033[?25hfixture-ready>'; for i in 1 2 3; do IFS= read -r line; printf 'fixture-echo:%s\\n' \"$line\"; done; sleep 60";
    *spec
        .launch
        .fixed_args
        .last_mut()
        .expect("fixture script argument") = script.to_owned();
    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(501);
    let initial_size = TerminalSize {
        rows: 10,
        columns: 40,
    };

    let registered = kernel.step(
        [command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    assert!(registered.command_outcomes[0].result.is_ok());

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
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
                    session_options: None,
                },
            },
        ),
    )
    .await;
    let running = kernel.snapshot().sessions[0].clone();
    assert_eq!(running.status, SessionStatus::Running);
    assert!(running.process_id.is_some());
    assert_eq!(running.terminal_size, Some(initial_size));
    let key = NativeSessionKey {
        instance_id,
        generation: running.generation,
    };
    wait_for_contents(&shell, key, "fixture-ready>").await;

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            3,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::AgentCommand(AgentCommand {
                    agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                    name: "status".to_owned(),
                    arguments: vec!["detail".to_owned()],
                }),
            },
        ),
    )
    .await;
    wait_for_contents(&shell, key, "fixture-echo:/status detail").await;

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            4,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::SubmitPrompt(PromptPayload {
                    text: "No auth type is selected".to_owned(),
                    framing: PromptFraming::BracketedPaste,
                }),
            },
        ),
    )
    .await;
    wait_for_contents(&shell, key, "fixture-echo:No auth type is selected").await;

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            5,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::SubmitPrompt(PromptPayload {
                    text: "followup-after-gate-like-history".to_owned(),
                    framing: PromptFraming::BracketedPaste,
                }),
            },
        ),
    )
    .await;
    wait_for_contents(&shell, key, "fixture-echo:followup-after-gate-like-history").await;

    let resized = TerminalSize {
        rows: 21,
        columns: 71,
    };
    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            6,
            ControlCommand::Resize {
                instance_id,
                size: resized,
            },
        ),
    )
    .await;
    assert_eq!(kernel.snapshot().sessions[0].terminal_size, Some(resized));
    assert_eq!(
        shell.terminal_snapshot(key).unwrap().size.rows,
        resized.rows
    );
    assert_eq!(
        shell.terminal_snapshot(key).unwrap().size.cols,
        resized.columns
    );

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            7,
            ControlCommand::Stop {
                instance_id,
                force: false,
            },
        ),
    )
    .await;
    assert!(matches!(
        kernel.snapshot().sessions[0].status,
        SessionStatus::Exited { .. }
    ));
    assert!(kernel.snapshot().sessions[0]
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame
            .contents
            .contains("fixture-echo:followup-after-gate-like-history")));
    assert_eq!(shell.active_session_count(), 0);
}

#[tokio::test]
async fn after_ready_initial_prompt_is_delivered_before_spawn_is_published() {
    let mut spec = interactive_agent_spec();
    spec.prompt.initial = InitialPromptMode::AfterReady;
    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(505);
    kernel.step(
        [command(
            20,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    let starting = kernel.step(
        [command(
            21,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 12,
                        columns: 64,
                    },
                    initial_prompt: Some("deferred-before-spawn".to_owned()),
                    session_options: None,
                },
            },
        )],
        [],
    );
    assert_eq!(kernel.snapshot().sessions[0].status, SessionStatus::Starting);
    let generation = kernel.snapshot().sessions[0].generation;
    let spawn = shell.execute(starting.effects[0].clone()).await;
    assert!(
        matches!(spawn.observation, ControlObservation::Spawned { .. }),
        "unexpected deferred fixture spawn observation: {:?}",
        spawn.observation
    );
    let key = NativeSessionKey {
        instance_id,
        generation,
    };
    wait_for_contents(&shell, key, "fixture-echo:deferred-before-spawn").await;
    assert_eq!(kernel.snapshot().sessions[0].status, SessionStatus::Starting);

    kernel.step([], [spawn]);
    assert_eq!(kernel.snapshot().sessions[0].status, SessionStatus::Running);
    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            22,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ),
    )
    .await;
    assert_eq!(shell.active_session_count(), 0);
}

#[tokio::test]
async fn after_ready_initial_prompt_failure_returns_spawn_failed_without_owned_session() {
    let mut spec = interactive_agent_spec();
    spec.prompt.initial = InitialPromptMode::AfterReady;
    spec.readiness.followup_requires_terminal = true;
    spec.readiness.draft_signal = DraftReadySignal::CodexComposerPrompt;
    spec.readiness.timeout_ms = 250;
    spec.readiness.poll_interval_ms = 20;
    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(506);
    kernel.step(
        [command(
            30,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    let starting = kernel.step(
        [command(
            31,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 12,
                        columns: 64,
                    },
                    initial_prompt: Some("must-not-be-sent".to_owned()),
                    session_options: None,
                },
            },
        )],
        [],
    );
    let generation = kernel.snapshot().sessions[0].generation;
    let spawn = shell.execute(starting.effects[0].clone()).await;
    assert!(matches!(
        &spawn.observation,
        ControlObservation::SpawnFailed { message }
            if message.contains("PTY readiness timed out")
    ));
    assert_eq!(shell.active_session_count(), 0);
    assert!(shell
        .terminal_snapshot(NativeSessionKey {
            instance_id,
            generation,
        })
        .is_err());

    kernel.step([], [spawn]);
    assert!(matches!(
        kernel.snapshot().sessions[0].status,
        SessionStatus::Failed { .. }
    ));
}

#[tokio::test]
async fn startup_operator_gate_blocks_deferred_initial_prompt_and_owns_no_session() {
    let mut spec = interactive_agent_spec();
    spec.prompt.initial = InitialPromptMode::AfterReady;
    #[cfg(windows)]
    let gate_script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h' + [char]27 + '[?25hTrust this folder?'); $line=[Console]::ReadLine(); [Console]::Write('unexpected-input:' + $line); Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let gate_script = "printf '\\033[?2004h\\033[?25hTrust this folder?'; IFS= read -r line; printf 'unexpected-input:%s' \"$line\"; sleep 60";
    *spec
        .launch
        .fixed_args
        .last_mut()
        .expect("fixture script argument") = gate_script.to_owned();
    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(507);
    kernel.step(
        [command(
            40,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    let starting = kernel.step(
        [command(
            41,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 12,
                        columns: 64,
                    },
                    initial_prompt: Some("must-not-confirm-the-gate".to_owned()),
                    session_options: None,
                },
            },
        )],
        [],
    );
    let spawn = shell.execute(starting.effects[0].clone()).await;
    assert!(matches!(
        &spawn.observation,
        ControlObservation::SpawnFailed { message }
            if message.contains("operator action")
                && message.contains("workspace trust")
                && message.contains("initial prompt was not submitted")
    ));
    assert_eq!(shell.active_session_count(), 0);

    kernel.step([], [spawn]);
    assert!(matches!(
        kernel.snapshot().sessions[0].status,
        SessionStatus::Failed { .. }
    ));
}

#[cfg(windows)]
#[tokio::test]
async fn late_claude_onboarding_gate_is_not_confirmed_by_initial_prompt() {
    let mut spec = interactive_agent_spec();
    spec.id = AgentId::new("claude").unwrap();
    spec.prompt.initial = InitialPromptMode::AfterReady;
    spec.readiness.followup_requires_terminal = true;
    spec.readiness.draft_signal = DraftReadySignal::ClaudeComposerPrompt;
    spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
        name: "claude".to_owned(),
    }];
    let gate_script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h❯'); Start-Sleep -Milliseconds 500; [Console]::Write([char]27 + '[2JWelcome to Claude Code' + [Environment]::NewLine + 'Choose the text style that looks best with your terminal'); $line=[Console]::ReadLine(); [Console]::Write('unexpected-input:' + $line); Start-Sleep -Seconds 60";
    *spec
        .launch
        .fixed_args
        .last_mut()
        .expect("fixture script argument") = gate_script.to_owned();
    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(509);
    kernel.step(
        [command(
            60,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("claude").unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    let starting = kernel.step(
        [command(
            61,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 12,
                        columns: 80,
                    },
                    initial_prompt: Some("must-not-confirm-late-onboarding".to_owned()),
                    session_options: None,
                },
            },
        )],
        [],
    );
    let generation = kernel.snapshot().sessions[0].generation;
    let spawn = shell.execute(starting.effects[0].clone()).await;
    assert!(matches!(
        &spawn.observation,
        ControlObservation::SpawnFailed { message }
            if message.contains("operator action")
                && message.contains("terminal appearance setup")
                && message.contains("initial prompt was not submitted")
    ));
    assert_eq!(shell.active_session_count(), 0);
    assert!(shell
        .terminal_snapshot(NativeSessionKey {
            instance_id,
            generation,
        })
        .is_err());

    kernel.step([], [spawn]);
    assert!(matches!(
        kernel.snapshot().sessions[0].status,
        SessionStatus::Failed { .. }
    ));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_codex_initial_prompt_waits_for_terminal_render_before_submit() {
    let mut spec = interactive_agent_spec();
    spec.id = AgentId::new("codex").unwrap();
    spec.prompt.initial = InitialPromptMode::AfterReady;
    spec.readiness.draft_signal = DraftReadySignal::CursorAfterBracketedPaste;
    spec.expected_processes = vec![gate4agent_types::ProcessMatcher::Exact {
        name: "codex".to_owned(),
    }];
    let registry = AgentRegistry::new([spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(508);
    kernel.step(
        [command(
            50,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("codex").unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    let starting = kernel.step(
        [command(
            51,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .expect("current directory")
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 12,
                        columns: 80,
                    },
                    initial_prompt: Some("codex-render-ack-before-enter".to_owned()),
                    session_options: None,
                },
            },
        )],
        [],
    );
    let generation = kernel.snapshot().sessions[0].generation;
    let spawn = shell.execute(starting.effects[0].clone()).await;
    assert!(
        matches!(spawn.observation, ControlObservation::Spawned { .. }),
        "unexpected Codex fixture spawn observation: {:?}",
        spawn.observation
    );
    let key = NativeSessionKey {
        instance_id,
        generation,
    };
    wait_for_contents(
        &shell,
        key,
        "fixture-echo:codex-render-ack-before-enter",
    )
    .await;
    assert_eq!(kernel.snapshot().sessions[0].status, SessionStatus::Starting);

    kernel.step([], [spawn]);
    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            52,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ),
    )
    .await;
    assert_eq!(shell.active_session_count(), 0);
}

#[tokio::test]
async fn natural_exit_is_collected_as_generation_bound_observation() {
    let registry = AgentRegistry::new([exiting_agent_spec()]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(502);
    let first = kernel.step(
        [
            command(
                1,
                ControlCommand::Register {
                    instance_id,
                    agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                    transport: TransportKind::Pty,
                },
            ),
            command(
                2,
                ControlCommand::Start {
                    instance_id,
                    request: StartRequest {
                        working_directory: std::env::current_dir()
                            .expect("current directory")
                            .to_string_lossy()
                            .into_owned(),
                        terminal_size: TerminalSize {
                            rows: 10,
                            columns: 40,
                        },
                        initial_prompt: None,
                        session_options: None,
                    },
                },
            ),
        ],
        [],
    );
    let spawn = shell.execute(first.effects[0].clone()).await;
    kernel.step([], [spawn]);

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            let exits = shell.collect_exits().await;
            if !exits.is_empty() {
                kernel.step([], exits);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("natural exit timeout");

    assert_eq!(
        kernel.snapshot().sessions[0].status,
        SessionStatus::Exited { exit_code: Some(7) }
    );
    assert!(kernel.snapshot().sessions[0]
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame.contents.contains("fixture-exit")));
    assert_eq!(shell.active_session_count(), 0);
}

#[tokio::test]
async fn shell_command_writes_only_after_fresh_shell_foreground_proof() {
    let mut shell_spec = interactive_agent_spec();
    shell_spec.detection.command = CONTROL_FIXTURE_ID.to_owned();
    let registry = AgentRegistry::new([shell_spec]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(503);

    kernel.step(
        [command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(CONTROL_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            2,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 10,
                        columns: 40,
                    },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        ),
    )
    .await;
    let running = kernel.snapshot().sessions[0].clone();
    let key = NativeSessionKey {
        instance_id,
        generation: running.generation,
    };
    wait_for_contents(&shell, key, "fixture-ready>").await;

    let pending = kernel.step(
        [command(
            3,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::ShellCommand(ShellCommand {
                    text: "printf shell-route".to_owned(),
                }),
            },
        )],
        [],
    );
    let observation = shell.execute(pending.effects[0].clone()).await;
    let completed = kernel.step([], [observation]);
    assert!(
        completed.events.iter().any(|event| matches!(
            event.event,
            ControlEventKind::InputCompleted {
                input_kind: PreparedInputKind::ShellCommand
            }
        )),
        "shell command did not complete: {:?}",
        completed.events
    );
    wait_for_contents(&shell, key, "fixture-echo:printf shell-route").await;

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            4,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ),
    )
    .await;
}

#[tokio::test]
async fn shell_command_is_rejected_when_the_agent_owns_foreground() {
    let registry =
        AgentRegistry::new([pty_provider_agent_spec()]).expect("provider fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(504);

    kernel.step(
        [command(
            10,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(PTY_PROVIDER_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pty,
            },
        )],
        [],
    );
    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            11,
            ControlCommand::Start {
                instance_id,
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 10,
                        columns: 40,
                    },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        ),
    )
    .await;

    let pending = kernel.step(
        [command(
            12,
            ControlCommand::SendInput {
                instance_id,
                action: InputAction::ShellCommand(ShellCommand {
                    text: "echo must-not-land".to_owned(),
                }),
            },
        )],
        [],
    );
    let observation = shell.execute(pending.effects[0].clone()).await;
    assert!(matches!(
        &observation.observation,
        ControlObservation::InputFailed { message }
            if message.contains("is not a shell")
    ));
    let completed = kernel.step([], [observation]);
    assert!(completed.events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::InputFailed {
            input_kind: PreparedInputKind::ShellCommand,
            message,
        } if message.contains("is not a shell")
    )));

    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            13,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ),
    )
    .await;
}
