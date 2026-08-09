use gate4agent_adapters::HistorySourceLayout;
use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_runtime_native::{
    NativeHistoryConfig, NativeHistoryRoot, NativeRuntime, NativeRuntimeConfig,
};
use gate4agent_types::{
    AdapterId, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, HistoryQuery, LaunchSpec, ProcessMatcher, ProviderRuntimePolicy,
    ResumeLaunchRequest, ResumeTarget, SessionStatus, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-history-runtime-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn controlled_resume_registry(fixture: &FixtureDir) -> AgentRegistry {
    let mut spec = builtin_registry().get_by_id("grok").unwrap().clone();
    #[cfg(windows)]
    let launch = {
        let script = fixture.0.join("resume-fixture.ps1");
        write(
            &script,
            "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write('fixture-resume:' + ($args -join '|')); Start-Sleep -Seconds 60",
        );
        LaunchSpec {
            program: "powershell.exe".to_owned(),
            fixed_args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-File".to_owned(),
                script.to_string_lossy().into_owned(),
            ],
        }
    };
    #[cfg(not(windows))]
    let launch = {
        let script = fixture.0.join("resume-fixture.sh");
        write(
            &script,
            "printf 'fixture-resume:%s|%s' \"$1\" \"$2\"; sleep 60",
        );
        LaunchSpec {
            program: "sh".to_owned(),
            fixed_args: vec![script.to_string_lossy().into_owned()],
        }
    };
    spec.detection.command = launch.program.clone();
    spec.expected_processes = vec![ProcessMatcher::Exact {
        name: launch.program.clone(),
    }];
    spec.launch = launch;
    AgentRegistry::new([spec]).unwrap()
}

async fn drive_until(
    runtime: &mut NativeRuntime,
    subscription: &gate4agent_handle::EventSubscription,
    events: &mut Vec<ControlEvent>,
    predicate: impl Fn() -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if predicate() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controlled history runtime timeout");
}

#[tokio::test]
async fn public_handle_discovers_and_loads_history_without_blocking_the_tick() {
    let fixture = FixtureDir::new();
    let sessions = fixture.0.join("sessions");
    let session = sessions.join("repo").join("grok-runtime-1");
    write(
        &session.join("summary.json"),
        r#"{"info":{"id":"grok-runtime-1","cwd":"/repo"},"generated_title":"Runtime title"}"#,
    );
    write(
        &session.join("chat_history.jsonl"),
        concat!(
            r#"{"type":"user","content":"runtime question"}"#,
            "\n",
            r#"{"type":"assistant","content":"runtime answer"}"#
        ),
    );
    let history = NativeHistoryConfig::new(vec![NativeHistoryRoot::new(
        AdapterId::new("grok").unwrap(),
        HistorySourceLayout::SummaryJsonWithSiblingNdjson,
        sessions,
    )
    .unwrap()])
    .unwrap();
    let (handle, mut runtime) = NativeRuntime::new_with_history(
        builtin_registry().clone(),
        NativeRuntimeConfig::default(),
        history,
    );
    assert!(runtime.history_enabled());
    let subscription = handle.subscribe(32);
    let instance_id = AgentInstanceId(8801);
    handle
        .dispatch(command(
            1,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("grok").unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            2,
            ControlCommand::DiscoverHistory {
                instance_id,
                query: HistoryQuery {
                    working_directory: None,
                    limit: 8,
                },
            },
        ))
        .unwrap();

    let first_tick = tokio::time::timeout(Duration::from_millis(500), runtime.tick())
        .await
        .expect("history dispatch must not await filesystem work");
    assert_eq!(first_tick.effects_dispatched, 1);
    assert!(first_tick
        .command_outcomes
        .iter()
        .all(|outcome| outcome.result.is_ok()));
    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, || {
        handle.snapshot().sessions[0].history.pending.is_none()
            && handle.snapshot().sessions[0].history.candidates.len() == 1
    })
    .await;

    let candidate_id = handle.snapshot().sessions[0].history.candidates[0]
        .id
        .clone();
    handle
        .dispatch(command(
            3,
            ControlCommand::LoadHistory {
                instance_id,
                candidate_id,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, || {
        handle.snapshot().sessions[0].history.loaded.is_some()
    })
    .await;

    let snapshot = handle.snapshot();
    let history = &snapshot.sessions[0].history;
    let loaded = history.loaded.as_ref().unwrap();
    assert_eq!(loaded.session_id, "grok-runtime-1");
    assert_eq!(loaded.title.as_deref(), Some("Runtime title"));
    assert_eq!(loaded.cwd.as_deref(), Some("/repo"));
    assert_eq!(loaded.messages.len(), 2);
    assert!(events.iter().any(|event| matches!(
        event.event,
        ControlEventKind::HistoryDiscovered { count: 1 }
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::HistoryLoaded { session_id } if session_id == "grok-runtime-1"
    )));
}

#[tokio::test]
async fn unconfigured_history_authority_fails_through_the_same_snapshot() {
    let (handle, mut runtime) =
        NativeRuntime::new(builtin_registry().clone(), NativeRuntimeConfig::default());
    assert!(!runtime.history_enabled());
    let instance_id = AgentInstanceId(8802);
    handle
        .dispatch(command(
            10,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("grok").unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            11,
            ControlCommand::DiscoverHistory {
                instance_id,
                query: HistoryQuery {
                    working_directory: None,
                    limit: 8,
                },
            },
        ))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            runtime.tick().await;
            if handle.snapshot().sessions[0].history.last_error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unconfigured history failure timeout");
    assert_eq!(
        handle.snapshot().sessions[0].history.last_error.as_deref(),
        Some("native history authority is not configured")
    );
}

#[tokio::test]
async fn loaded_history_resume_is_authorized_then_spawned_with_exact_provider_argv() {
    let fixture = FixtureDir::new();
    let sessions = fixture.0.join("sessions");
    let session = sessions.join("repo").join("grok-resume-1");
    write(
        &session.join("summary.json"),
        r#"{"info":{"id":"grok-resume-1","cwd":"/repo"},"generated_title":"Resume fixture"}"#,
    );
    write(
        &session.join("chat_history.jsonl"),
        r#"{"type":"user","content":"resume this"}"#,
    );
    let history = NativeHistoryConfig::new(vec![NativeHistoryRoot::new(
        AdapterId::new("grok").unwrap(),
        HistorySourceLayout::SummaryJsonWithSiblingNdjson,
        sessions,
    )
    .unwrap()])
    .unwrap();
    let (handle, mut runtime) = NativeRuntime::new_with_history(
        controlled_resume_registry(&fixture),
        NativeRuntimeConfig::default(),
        history,
    );
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(8803);
    let working_directory = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    handle
        .dispatch(command(
            20,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("grok").unwrap(),
                transport: TransportKind::Pty,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            21,
            ControlCommand::DiscoverHistory {
                instance_id,
                query: HistoryQuery {
                    working_directory: None,
                    limit: 8,
                },
            },
        ))
        .unwrap();
    let mut events = Vec::new();
    drive_until(&mut runtime, &subscription, &mut events, || {
        handle.snapshot().sessions[0].history.candidates.len() == 1
    })
    .await;
    let candidate_id = handle.snapshot().sessions[0].history.candidates[0]
        .id
        .clone();
    handle
        .dispatch(command(
            22,
            ControlCommand::LoadHistory {
                instance_id,
                candidate_id: candidate_id.clone(),
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, || {
        handle.snapshot().sessions[0].history.loaded.is_some()
    })
    .await;
    handle
        .dispatch(command(
            23,
            ControlCommand::Resume {
                instance_id,
                target: ResumeTarget::HistoryCandidate { candidate_id },
                runtime_policy: ProviderRuntimePolicy::new(true, false, false, true, true)
                    .unwrap(),
                request: ResumeLaunchRequest {
                    working_directory,
                    terminal_size: TerminalSize {
                        rows: 12,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
            },
        ))
        .unwrap();

    drive_until(&mut runtime, &subscription, &mut events, || {
        handle.snapshot().sessions[0].status == SessionStatus::Running
    })
    .await;
    drive_until(&mut runtime, &subscription, &mut events, || {
        handle.snapshot().sessions[0]
            .terminal_frame
            .as_ref()
            .is_some_and(|frame| {
                frame
                    .contents
                    .contains("fixture-resume:--resume|grok-resume-1")
            })
    })
    .await;

    let snapshot = handle.snapshot();
    let resumed = &snapshot.sessions[0];
    assert_eq!(resumed.generation.0, 1);
    assert_eq!(
        resumed
            .resume
            .last_session
            .as_ref()
            .map(|session| session.id.as_str()),
        Some("grok-resume-1")
    );
    assert_eq!(
        resumed
            .provider
            .session
            .as_ref()
            .map(|session| session.id.as_str()),
        Some("grok-resume-1")
    );
    assert!(events
        .iter()
        .any(|event| matches!(event.event, ControlEventKind::ResumeRequested { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event.event, ControlEventKind::ResumeAuthorized { .. })));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::Resumed { session, .. } if session.id == "grok-resume-1"
    )));

    handle
        .dispatch(command(
            24,
            ControlCommand::Stop {
                instance_id,
                force: true,
            },
        ))
        .unwrap();
    drive_until(&mut runtime, &subscription, &mut events, || {
        matches!(
            handle.snapshot().sessions[0].status,
            SessionStatus::Exited { .. }
        )
    })
    .await;
    assert_eq!(runtime.active_native_sessions(), 0);
}
