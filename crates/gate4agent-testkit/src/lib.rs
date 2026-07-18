//! Controlled, authentication-free provider fixtures for integration tests.

use gate4agent_types::{
    AgentCapabilities, AgentCommandMode, AgentId, AgentReadinessSpec, AgentSpec, DetectionSpec,
    DraftReadySignal, InitialPromptMode, LaunchSpec, ProcessMatcher, PromptSpec, SpecVerification,
};

pub const CONTROL_FIXTURE_ID: &str = "control-fixture";

pub fn interactive_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h' + [char]27 + '[?25hfixture-ready>'); $line=[Console]::ReadLine(); [Console]::Write('fixture-echo:' + $line); Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let script = "printf '\033[?2004h\033[?25hfixture-ready>'; IFS= read -r line; printf 'fixture-echo:%s' \"$line\"; sleep 60";
    fixture_spec(script)
}

pub fn exiting_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script =
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write('fixture-exit'); exit 7";
    #[cfg(not(windows))]
    let script = "printf 'fixture-exit'; exit 7";
    fixture_spec(script)
}

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
        id: AgentId::new(CONTROL_FIXTURE_ID).expect("fixture agent ID"),
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
            name: CONTROL_FIXTURE_ID.to_owned(),
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
