//! Controlled, authentication-free provider fixtures for integration tests.

use gate4agent_adapters::builtin_adapter_registry;
use gate4agent_types::{
    AcpTransportSpec, AdapterBinding, AdapterFamily, AgentAdapterCapabilities, AgentCapabilities,
    AgentCommandMode, AgentId, AgentReadinessSpec, AgentSpec, AgentTransportCapabilities,
    DetectionSpec, DraftReadySignal, InitialPromptMode, LaunchSpec, PipePromptDelivery,
    PipeProtocol, PipeTransportSpec, ProcessMatcher, PromptSpec, SpecVerification,
};

pub const CONTROL_FIXTURE_ID: &str = "control-fixture";
pub const PIPE_FIXTURE_ID: &str = "pipe-fixture";
pub const ONE_SHOT_FIXTURE_ID: &str = "one-shot-fixture";
pub const ACP_FIXTURE_ID: &str = "acp-fixture";
pub const PTY_PROVIDER_FIXTURE_ID: &str = "pty-provider-fixture";
pub const HOOK_POSTING_FIXTURE_ID: &str = "hook-posting-fixture";

/// Prevent automated Windows fault paths from opening modal UI.
///
/// Call this before the first native or provider FFI operation in a test
/// process. The process error mode is inherited by fixture children.
pub fn suppress_windows_fault_dialogs_for_test() {
    #[cfg(windows)]
    unsafe {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        const WER_FAULT_REPORTING_NO_UI: u32 = 0x0020;

        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
            fn WerSetFlags(flags: u32) -> i32;
        }

        SetErrorMode(
            SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
        );
        let _ = WerSetFlags(WER_FAULT_REPORTING_NO_UI);
    }
}

/// Refuse to run Windows PTY integration code outside the bounded supervisor.
///
/// The supervisor sets this exact marker only after creating its containment
/// job. Non-Windows tests do not require the Windows-specific containment.
pub fn require_windows_headless_supervisor_for_test() {
    #[cfg(windows)]
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows PTY tests must run through windows-headless-supervisor"
    );
}

pub fn interactive_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write([char]27 + '[?2004h' + [char]27 + '[?25hfixture-ready>'); $line=[Console]::ReadLine(); [Console]::Write('fixture-echo:' + $line); Start-Sleep -Seconds 60";
    #[cfg(not(windows))]
    let script = r#"printf '\033[?2004h\033[?25hfixture-ready>'; IFS= read -r line; printf 'fixture-echo:%s' "$line"; sleep 60"#;
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
                adapter: adapter(AdapterFamily::Pipe, "codex"),
                protocol: PipeProtocol::SemanticNdjson,
                launch_override: Some(launch),
                prompt_delivery: PipePromptDelivery::None,
            }),
            acp: None,
        },
    )
}

pub fn one_shot_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script =
        "$prompt=[Console]::In.ReadToEnd(); [Console]::Write('fixture-one-shot:' + $prompt)";
    #[cfg(not(windows))]
    let script = "prompt=$(cat); printf 'fixture-one-shot:%s' \"$prompt\"";
    let launch = provider_launch(script);
    let binding = adapter(AdapterFamily::OneShot, "claude");
    let mut spec = provider_spec(
        ONE_SHOT_FIXTURE_ID,
        "Control-plane one-shot fixture",
        launch.clone(),
        AgentTransportCapabilities {
            pty: false,
            pty_adapter: None,
            pipe: Some(PipeTransportSpec {
                adapter: binding.clone(),
                protocol: PipeProtocol::OneShotText,
                launch_override: Some(launch),
                prompt_delivery: PipePromptDelivery::None,
            }),
            acp: None,
        },
    );
    spec.capabilities.adapters.one_shot = Some(binding);
    spec
}

pub fn acp_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8
function Write-JsonLine($value) {
    [Console]::WriteLine(($value | ConvertTo-Json -Compress -Depth 12))
}

$initialize = [Console]::ReadLine() | ConvertFrom-Json
$fsCapabilities = $initialize.params.clientCapabilities.fs
$initializeOk = ($initialize.method -eq 'initialize') -and
    ($fsCapabilities.PSObject.Properties.Name -contains 'readTextFile') -and
    ($fsCapabilities.readTextFile -eq $false) -and
    ($initialize.params.clientCapabilities.PSObject.Properties.Name -contains 'terminal') -and
    ($initialize.params.clientCapabilities.terminal -eq $false)
if (-not $initializeOk) {
    Write-JsonLine @{jsonrpc='2.0';id=$initialize.id;error=@{code=-32003;message='fixture expected fail-closed client capabilities'}}
    exit 41
}
Write-JsonLine @{jsonrpc='2.0';id=$initialize.id;result=@{protocolVersion=1;agentCapabilities=@{loadSession=$false};agentInfo=@{name='fixture';title='Fixture ACP';version='1'}}}

$newSession = [Console]::ReadLine() | ConvertFrom-Json
$newSessionOk = ($newSession.method -eq 'session/new') -and
    ($newSession.params.PSObject.Properties.Name -contains 'mcpServers') -and
    (@($newSession.params.mcpServers).Count -eq 0)
if (-not $newSessionOk) {
    Write-JsonLine @{jsonrpc='2.0';id=$newSession.id;error=@{code=-32003;message='fixture expected an empty MCP server list'}}
    exit 42
}
Write-JsonLine @{jsonrpc='2.0';id=$newSession.id;result=@{sessionId='fixture-acp-session'}}

while ($true) {
    $line = [Console]::ReadLine()
    if ($null -eq $line) { break }
    $request = $line | ConvertFrom-Json
    if ($request.method -ne 'session/prompt') { continue }

    Write-JsonLine @{jsonrpc='2.0';id=9101;method='fs/read_text_file';params=@{path='fixture-forbidden.txt'}}
    $fsResponse = [Console]::ReadLine() | ConvertFrom-Json
    Write-JsonLine @{jsonrpc='2.0';id=9102;method='terminal/create';params=@{cwd='.';env=@{}}}
    $terminalResponse = [Console]::ReadLine() | ConvertFrom-Json
    Write-JsonLine @{jsonrpc='2.0';id=9103;method='session/request_permission';params=@{toolName='fixture-tool';description='fixture permission';sessionId='fixture-acp-session'}}
    $permissionResponse = [Console]::ReadLine() | ConvertFrom-Json

    $callbacksDenied = ($fsResponse.id -eq 9101) -and
        ($fsResponse.error.code -eq -32002) -and
        ($terminalResponse.id -eq 9102) -and
        ($terminalResponse.error.code -eq -32004) -and
        ($permissionResponse.id -eq 9103) -and
        ($null -eq $permissionResponse.error) -and
        ($permissionResponse.result.allowed -eq $false)
    if (-not $callbacksDenied) {
        Write-JsonLine @{jsonrpc='2.0';id=$request.id;error=@{code=-32003;message='fixture observed ACP host authority'}}
        continue
    }

    Write-JsonLine @{jsonrpc='2.0';method='session/update';params=@{sessionId='fixture-acp-session';update=@{sessionUpdate='agent_message_chunk';content=@{type='text';text='fixture-acp-response'}}}}
    Write-JsonLine @{jsonrpc='2.0';id=$request.id;result=@{stopReason='end_turn';inputTokens=7;outputTokens=11}}
}"#;
    #[cfg(not(windows))]
    let script = r#"import json,sys
def read_message():
 message=sys.stdin.readline()
 if not message: sys.exit(0)
 return json.loads(message)
def write_message(message):
 print(json.dumps(message),flush=True)
def fail(request,message,code):
 write_message({'jsonrpc':'2.0','id':request.get('id'),'error':{'code':-32003,'message':message}})
 sys.exit(code)

initialize=read_message()
client=initialize.get('params',{}).get('clientCapabilities',{})
fs=client.get('fs',{})
if initialize.get('method')!='initialize' or fs.get('readTextFile') is not False or client.get('terminal') is not False:
 fail(initialize,'fixture expected fail-closed client capabilities',41)
write_message({'jsonrpc':'2.0','id':initialize.get('id'),'result':{'protocolVersion':1,'agentCapabilities':{'loadSession':False},'agentInfo':{'name':'fixture','title':'Fixture ACP','version':'1'}}})

new_session=read_message()
new_params=new_session.get('params',{})
if new_session.get('method')!='session/new' or new_params.get('mcpServers')!=[]:
 fail(new_session,'fixture expected an empty MCP server list',42)
write_message({'jsonrpc':'2.0','id':new_session.get('id'),'result':{'sessionId':'fixture-acp-session'}})

while True:
 request=read_message()
 if request.get('method')!='session/prompt': continue
 write_message({'jsonrpc':'2.0','id':9101,'method':'fs/read_text_file','params':{'path':'fixture-forbidden.txt'}})
 fs_response=read_message()
 write_message({'jsonrpc':'2.0','id':9102,'method':'terminal/create','params':{'cwd':'.','env':{}}})
 terminal_response=read_message()
 write_message({'jsonrpc':'2.0','id':9103,'method':'session/request_permission','params':{'toolName':'fixture-tool','description':'fixture permission','sessionId':'fixture-acp-session'}})
 permission_response=read_message()
 callbacks_denied=(fs_response.get('id')==9101 and fs_response.get('error',{}).get('code')==-32002 and terminal_response.get('id')==9102 and terminal_response.get('error',{}).get('code')==-32004 and permission_response.get('id')==9103 and permission_response.get('error') is None and permission_response.get('result',{}).get('allowed') is False)
 if not callbacks_denied:
  write_message({'jsonrpc':'2.0','id':request.get('id'),'error':{'code':-32003,'message':'fixture observed ACP host authority'}})
  continue
 write_message({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'fixture-acp-session','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'fixture-acp-response'}}}})
 write_message({'jsonrpc':'2.0','id':request.get('id'),'result':{'stopReason':'end_turn','inputTokens':7,'outputTokens':11}})"#;
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
                adapter: adapter(AdapterFamily::Acp, "gemini"),
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
            pty_adapter: Some(adapter(AdapterFamily::PtySemantic, "codex")),
            pipe: None,
            acp: None,
        },
    )
}

pub fn hook_posting_agent_spec() -> AgentSpec {
    #[cfg(windows)]
    let script = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8; $headers=@{'X-Gate4Agent-Hook-Token'=$env:GATE4AGENT_HOOK_TOKEN;'X-Gate4Agent-Hook-Route'=$env:GATE4AGENT_HOOK_ROUTE}; $body='{"hook_event_name":"UserPromptSubmit","event_id":"fixture-hook-1","payload":{"hook_event_name":"UserPromptSubmit","prompt":"fixture hook prompt"}}'; Invoke-WebRequest -UseBasicParsing -Method Post -Uri $env:GATE4AGENT_HOOK_URL -Headers $headers -ContentType 'application/json' -Body $body | Out-Null; [Console]::Write('fixture-hook-posted'); Start-Sleep -Seconds 60"#;
    #[cfg(not(windows))]
    let script = r#"python3 -c 'import json,os,urllib.request; body=json.dumps({"hook_event_name":"UserPromptSubmit","event_id":"fixture-hook-1","payload":{"hook_event_name":"UserPromptSubmit","prompt":"fixture hook prompt"}}).encode(); request=urllib.request.Request(os.environ["GATE4AGENT_HOOK_URL"],data=body,headers={"Content-Type":"application/json","X-Gate4Agent-Hook-Token":os.environ["GATE4AGENT_HOOK_TOKEN"],"X-Gate4Agent-Hook-Route":os.environ["GATE4AGENT_HOOK_ROUTE"]},method="POST"); urllib.request.urlopen(request,timeout=2).read()'; printf 'fixture-hook-posted'; sleep 60"#;
    let launch = provider_launch(script);
    let mut spec = provider_spec(
        HOOK_POSTING_FIXTURE_ID,
        "Control-plane Hook posting fixture",
        launch,
        AgentTransportCapabilities {
            pty: true,
            pty_adapter: None,
            pipe: None,
            acp: None,
        },
    );
    spec.capabilities.adapters.hook = Some(adapter(AdapterFamily::Hook, "grok"));
    spec
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
            adapters: AgentAdapterCapabilities::default(),
        },
        verification: SpecVerification::Gate4AgentVerified,
    }
}

fn adapter(family: AdapterFamily, id: &str) -> AdapterBinding {
    builtin_adapter_registry()
        .binding(family, id)
        .unwrap_or_else(|| panic!("missing controlled {family:?} adapter {id}"))
        .clone()
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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn unix_interactive_fixture_argv_is_nul_free_and_retains_terminal_escapes() {
        let spec = interactive_agent_spec();
        assert!(spec
            .launch
            .fixed_args
            .iter()
            .all(|argument| !argument.as_bytes().contains(&0)));
        let script = spec.launch.fixed_args.last().unwrap();
        assert!(script.contains(r"\033[?2004h"));
        assert!(script.contains(r"\033[?25h"));
    }
}
