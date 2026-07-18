//! Controlled, authentication-free provider fixtures for integration tests.

use gate4agent_types::{
    AcpTransportSpec, AgentCapabilities, AgentCommandMode, AgentId, AgentReadinessSpec, AgentSpec,
    AgentTransportCapabilities, DetectionSpec, DraftReadySignal, InitialPromptMode, LaunchSpec,
    PipePromptDelivery, PipeTransportSpec, ProcessMatcher, PromptSpec, ProviderAdapter,
    SpecVerification,
};

pub const CONTROL_FIXTURE_ID: &str = "control-fixture";
pub const PIPE_FIXTURE_ID: &str = "pipe-fixture";
pub const ACP_FIXTURE_ID: &str = "acp-fixture";
pub const PTY_PROVIDER_FIXTURE_ID: &str = "pty-provider-fixture";

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

pub fn pipe_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::WriteLine('{"type":"thread.started","thread_id":"fixture-thread"}'); [Console]::WriteLine('{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"fixture-pipe-response"}}'); [Console]::WriteLine('{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":5}}')"#;
    #[cfg(not(windows))]
    let script = r#"printf '%s\n' '{"type":"thread.started","thread_id":"fixture-thread"}' '{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"fixture-pipe-response"}}' '{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":5}}'"#;
    let launch = provider_launch(script);
    provider_spec(
        PIPE_FIXTURE_ID,
        "Control-plane Pipe fixture",
        launch.clone(),
        AgentTransportCapabilities {
            pty: false,
            pty_adapter: None,
            pipe: Some(PipeTransportSpec {
                adapter: ProviderAdapter::Codex,
                launch_override: Some(launch),
                prompt_delivery: PipePromptDelivery::None,
            }),
            acp: None,
        },
    )
}

pub fn acp_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8; while ($true) { $line=[Console]::ReadLine(); if ($null -eq $line) { break }; $request=$line | ConvertFrom-Json; if ($request.method -eq 'initialize') { $result=@{protocolVersion=1;agentCapabilities=@{loadSession=$false};agentInfo=@{name='fixture';title='Fixture ACP';version='1'}}; [Console]::WriteLine((@{jsonrpc='2.0';id=$request.id;result=$result} | ConvertTo-Json -Compress -Depth 8)) } elseif ($request.method -eq 'session/new') { [Console]::WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{sessionId='fixture-acp-session'}} | ConvertTo-Json -Compress -Depth 8)) } elseif ($request.method -eq 'session/prompt') { $update=@{jsonrpc='2.0';method='session/update';params=@{sessionId='fixture-acp-session';update=@{sessionUpdate='agent_message_chunk';content=@{type='text';text='fixture-acp-response'}}}}; [Console]::WriteLine(($update | ConvertTo-Json -Compress -Depth 10)); [Console]::WriteLine((@{jsonrpc='2.0';id=$request.id;result=@{stopReason='end_turn';inputTokens=7;outputTokens=11}} | ConvertTo-Json -Compress -Depth 8)) } }"#;
    #[cfg(not(windows))]
    let script = r#"import json,sys
for line in sys.stdin:
 r=json.loads(line); m=r.get('method'); i=r.get('id')
 if m=='initialize': out={'jsonrpc':'2.0','id':i,'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':False},'agentInfo':{'name':'fixture','title':'Fixture ACP','version':'1'}}}
 elif m=='session/new': out={'jsonrpc':'2.0','id':i,'result':{'sessionId':'fixture-acp-session'}}
 elif m=='session/prompt':
  print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'fixture-acp-session','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'fixture-acp-response'}}}}),flush=True)
  out={'jsonrpc':'2.0','id':i,'result':{'stopReason':'end_turn','inputTokens':7,'outputTokens':11}}
 else: continue
 print(json.dumps(out),flush=True)"#;
    #[cfg(windows)]
    let launch = provider_launch(script);
    #[cfg(not(windows))]
    let launch = LaunchSpec {
        program: "python3".to_owned(),
        fixed_args: vec!["-u".to_owned(), "-c".to_owned(), script.to_owned()],
    };
    provider_spec(
        ACP_FIXTURE_ID,
        "Control-plane ACP fixture",
        launch.clone(),
        AgentTransportCapabilities {
            pty: false,
            pty_adapter: None,
            pipe: None,
            acp: Some(AcpTransportSpec {
                adapter: ProviderAdapter::Gemini,
                launch_override: Some(launch),
            }),
        },
    )
}

pub fn pty_provider_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::WriteLine([char]0x2022 + ' fixture-pty-response'); [Console]::WriteLine([char]0x203A); Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let script = "printf '• fixture-pty-response\\n›\\n'; sleep 60";
    let launch = provider_launch(script);
    provider_spec(
        PTY_PROVIDER_FIXTURE_ID,
        "Control-plane PTY provider fixture",
        launch,
        AgentTransportCapabilities {
            pty: true,
            pty_adapter: Some(ProviderAdapter::Codex),
            pipe: None,
            acp: None,
        },
    )
}

fn provider_launch(script: &str) -> LaunchSpec {
    #[cfg(windows)]
    return LaunchSpec {
        program: "powershell.exe".to_owned(),
        fixed_args: vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-ExecutionPolicy".to_owned(),
            "Bypass".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ],
    };
    #[cfg(not(windows))]
    return LaunchSpec {
        program: "sh".to_owned(),
        fixed_args: vec!["-c".to_owned(), script.to_owned()],
    };
}

fn provider_spec(
    id: &str,
    display_name: &str,
    launch: LaunchSpec,
    transports: AgentTransportCapabilities,
) -> AgentSpec {
    let process_name = launch.program.clone();
    AgentSpec {
        id: AgentId::new(id).expect("fixture agent ID"),
        revision: "fixture-r1".to_owned(),
        display_name: display_name.to_owned(),
        detection: DetectionSpec {
            command: launch.program.clone(),
            aliases: Vec::new(),
            required_commands: Vec::new(),
            unsupported_platforms: Vec::new(),
        },
        launch,
        expected_processes: vec![ProcessMatcher::Exact { name: process_name }],
        prompt: PromptSpec {
            initial: InitialPromptMode::None,
            native_draft: None,
        },
        readiness: AgentReadinessSpec::default(),
        capabilities: AgentCapabilities {
            agent_commands: None,
            transports,
        },
        verification: SpecVerification::Gate4AgentVerified,
    }
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
    let (program, fixed_args) = ("sh", vec!["-c".to_owned(), script.to_owned()]);

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
            ..AgentCapabilities::default()
        },
        verification: SpecVerification::Gate4AgentVerified,
    }
}
