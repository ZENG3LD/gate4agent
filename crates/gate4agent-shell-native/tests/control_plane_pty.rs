use std::time::Duration;

use gate4agent_catalog::AgentRegistry;
use gate4agent_kernel::Gate4AgentKernel;
use gate4agent_shell_native::{NativeEffectShell, NativeSessionKey};
use gate4agent_types::{
    AgentCapabilities, AgentCommand, AgentCommandMode, AgentId, AgentInstanceId,
    AgentReadinessSpec, AgentSpec, CommandEnvelope, CommandId, ControlCommand, DetectionSpec,
    DraftReadySignal, InitialPromptMode, InputAction, LaunchSpec, ProcessMatcher, PromptSpec,
    SessionStatus, SpecVerification, StartRequest, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(windows)]
const INTERACTIVE_SCRIPT: &str = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h' + [char]27 + '[?25hfixture-ready>'); $line=[Console]::ReadLine(); [Console]::Write('fixture-echo:' + $line); Start-Sleep -Seconds 60";
#[cfg(not(windows))]
const INTERACTIVE_SCRIPT: &str =
    "printf '\033[?2004h\033[?25hfixture-ready>'; IFS= read -r line; printf 'fixture-echo:%s' \"$line\"; sleep 60";

#[cfg(windows)]
const EXIT_SCRIPT: &str =
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write('fixture-exit'); exit 7";
#[cfg(not(windows))]
const EXIT_SCRIPT: &str = "printf 'fixture-exit'; exit 7";

fn fixture_spec(script: &str) -> AgentSpec {
    #[cfg(windows)]
    let (program, fixed_args) = (
        "powershell.exe",
        vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-ExecutionPolicy".to_owned(),
            "Bypass".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ],
    );

    #[cfg(not(windows))]
    let (program, fixed_args) = (
        "sh",
        vec!["-c".to_owned(), script.to_owned()],
    );

    AgentSpec {
        id: AgentId::new("control-fixture").expect("fixture agent ID"),
        revision: "fixture-r1".to_owned(),
        display_name: "Control-plane PTY fixture".to_owned(),
        detection: DetectionSpec {
            command: program.to_owned(),
            aliases: Vec::new(),
            required_commands: Vec::new(),
            unsupported_platforms: Vec::new(),
        },
        launch: LaunchSpec {
            program: program.to_owned(),
            fixed_args,
        },
        expected_processes: vec![ProcessMatcher::Exact {
            name: "control-fixture".to_owned(),
        }],
        prompt: PromptSpec {
            initial: InitialPromptMode::None,
            native_draft: None,
        },
        readiness: AgentReadinessSpec {
            draft_signal: DraftReadySignal::CursorAfterBracketedPaste,
            ..AgentReadinessSpec::default()
        },
        capabilities: AgentCapabilities {
            agent_commands: Some(AgentCommandMode::SlashLine),
        },
        verification: SpecVerification::Gate4AgentVerified,
    }
}

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

async fn wait_for_contents(
    shell: &NativeEffectShell,
    key: NativeSessionKey,
    expected: &str,
) {
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
    let registry = AgentRegistry::new([fixture_spec(INTERACTIVE_SCRIPT)]).expect("fixture registry");
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
                agent_id: AgentId::new("control-fixture").unwrap(),
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
                    agent_id: AgentId::new("control-fixture").unwrap(),
                    name: "status".to_owned(),
                    arguments: vec!["detail".to_owned()],
                }),
            },
        ),
    )
    .await;
    wait_for_contents(&shell, key, "fixture-echo:/status detail").await;

    let resized = TerminalSize {
        rows: 21,
        columns: 71,
    };
    execute_only_effect(
        &mut kernel,
        &mut shell,
        command(
            4,
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
            5,
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
    assert_eq!(shell.active_session_count(), 0);
}

#[tokio::test]
async fn natural_exit_is_collected_as_generation_bound_observation() {
    let registry = AgentRegistry::new([fixture_spec(EXIT_SCRIPT)]).expect("fixture registry");
    let mut kernel = Gate4AgentKernel::new(registry.clone());
    let mut shell = NativeEffectShell::new(registry);
    let instance_id = AgentInstanceId(502);
    let first = kernel.step(
        [
            command(
                1,
                ControlCommand::Register {
                    instance_id,
                    agent_id: AgentId::new("control-fixture").unwrap(),
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
    assert_eq!(shell.active_session_count(), 0);
}
