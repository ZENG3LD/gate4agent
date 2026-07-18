use std::time::Duration;

use gate4agent::agent::{
    AgentCapabilities, AgentId, AgentReadinessSpec, AgentSpec, DetectionSpec, InitialPromptMode,
    LaunchSpec, ProcessMatcher, PromptSpec, RuntimePlatform, SpecVerification,
};
use gate4agent::pty::{
    PtyAttachError, PtyEvent, PtyEventEnvelope, PtyEventRecvError, PtySession, PtySize,
};
use gate4agent::LaunchRequest;

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(15);

fn fixture_spec(script: &str) -> AgentSpec {
    #[cfg(windows)]
    let (program, fixed_args, process_name) = (
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
        "powershell",
    );

    #[cfg(not(windows))]
    let (program, fixed_args, process_name) =
        ("sh", vec!["-c".to_owned(), script.to_owned()], "sh");

    AgentSpec {
        id: AgentId::new("pty-fixture").expect("fixture agent ID"),
        revision: "fixture-r1".to_owned(),
        display_name: "PTY lifecycle fixture".to_owned(),
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
            name: process_name.to_owned(),
        }],
        prompt: PromptSpec {
            initial: InitialPromptMode::None,
            native_draft: None,
        },
        readiness: AgentReadinessSpec::default(),
        capabilities: AgentCapabilities::default(),
        verification: SpecVerification::Gate4AgentVerified,
    }
}

async fn spawn_fixture(script: &str, rows: u16, cols: u16) -> PtySession {
    let spec = fixture_spec(script);
    PtySession::spawn_agent_with_size(
        &spec,
        LaunchRequest {
            working_dir: std::env::current_dir().expect("current directory"),
            platform: RuntimePlatform::current(),
            ..LaunchRequest::default()
        },
        rows,
        cols,
    )
    .await
    .expect("spawn controlled PTY fixture")
}

async fn collect_through_exit(
    mut replay: Vec<PtyEventEnvelope>,
    mut receiver: gate4agent::pty::PtyEventReceiver,
) -> Vec<PtyEventEnvelope> {
    if replay
        .iter()
        .any(|event| matches!(event.event, PtyEvent::Exited { .. }))
    {
        return replay;
    }

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            let event = receiver.recv().await.expect("event before PTY exit");
            let exited = matches!(event.event, PtyEvent::Exited { .. });
            replay.push(event);
            if exited {
                return replay;
            }
        }
    })
    .await
    .expect("controlled PTY fixture must exit")
}

fn output_bytes(events: &[PtyEventEnvelope]) -> Vec<u8> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            PtyEvent::Output(data) => Some(data.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

async fn next_resize(
    receiver: &mut gate4agent::pty::PtyEventReceiver,
) -> PtyEventEnvelope {
    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            let event = receiver.recv().await.expect("event before PTY resize");
            if matches!(event.event, PtyEvent::Resized(_)) {
                return event;
            }
        }
    })
    .await
    .expect("resize event timeout")
}

#[cfg(windows)]
const EXIT_SCRIPT: &str =
    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Write('fixture-output'); exit 7";
#[cfg(not(windows))]
const EXIT_SCRIPT: &str = "printf fixture-output; exit 7";

#[tokio::test]
async fn output_exit_snapshot_and_post_exit_attach_are_ordered() {
    let session = spawn_fixture(EXIT_SCRIPT, 12, 48).await;
    let attachment = session
        .attach_events(session.beginning_cursor())
        .expect("initial attach");
    let events = collect_through_exit(attachment.replay, attachment.receiver).await;

    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    let output_position = events
        .iter()
        .position(|event| matches!(event.event, PtyEvent::Output(_)))
        .expect("fixture output event");
    let exit_position = events
        .iter()
        .position(|event| matches!(event.event, PtyEvent::Exited { code: 7 }))
        .expect("fixture exit event");
    assert!(output_position < exit_position);
    assert!(String::from_utf8_lossy(&output_bytes(&events)).contains("fixture-output"));

    let snapshot = session.terminal_snapshot().expect("terminal snapshot");
    assert!(snapshot.contents.contains("fixture-output"));
    assert_eq!(snapshot.size, PtySize { rows: 12, cols: 48 });

    let replay_after_exit = session
        .attach_events(session.beginning_cursor())
        .expect("post-exit attach")
        .replay;
    assert!(replay_after_exit
        .iter()
        .any(|event| matches!(event.event, PtyEvent::Exited { code: 7 })));

    let mut stale = session.beginning_cursor();
    stale.generation = stale.generation.saturating_add(1);
    assert!(matches!(
        session.attach_events(stale),
        Err(PtyAttachError::StaleGeneration { .. })
    ));

    let outcome = session.shutdown().await.expect("join exited fixture");
    assert_eq!(outcome.exit_code, Some(7));
    assert!(outcome.termination.is_none());
    assert!(outcome.terminal.contents.contains("fixture-output"));
}

#[cfg(windows)]
const WAIT_SCRIPT: &str = "$child=Start-Process -FilePath powershell.exe -ArgumentList '-NoLogo','-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60' -PassThru; Wait-Process -Id $child.Id";
#[cfg(not(windows))]
const WAIT_SCRIPT: &str = "sleep 60 & child=$!; wait $child";

#[tokio::test]
async fn resize_and_shutdown_have_observable_terminal_states() {
    let session = spawn_fixture(WAIT_SCRIPT, 10, 40).await;
    let mut receiver = session.subscribe_events().expect("subscribe");

    session.resize(20, 70).await.expect("first resize");
    session.resize(21, 71).await.expect("second resize");
    let first = next_resize(&mut receiver).await;
    let second = next_resize(&mut receiver).await;
    assert!(matches!(
        first.event,
        PtyEvent::Resized(PtySize { rows: 20, cols: 70 })
    ));
    assert!(matches!(
        second.event,
        PtyEvent::Resized(PtySize { rows: 21, cols: 71 })
    ));
    assert_eq!(
        session.terminal_snapshot().expect("resized snapshot").size,
        PtySize { rows: 21, cols: 71 }
    );

    let outcome = tokio::time::timeout(FIXTURE_TIMEOUT, session.shutdown())
        .await
        .expect("bounded fixture shutdown")
        .expect("fixture shutdown");
    let report = outcome
        .termination
        .expect("live fixture termination report");
    assert!(report.root_pid.is_some());
    assert!(
        report.captured_descendants >= 1,
        "controlled fixture must capture its sleeping child"
    );
    assert!(
        report.degraded_reason.is_none(),
        "controlled fixture must have process-table ownership evidence: {:?}",
        report.degraded_reason
    );

    tokio::time::timeout(FIXTURE_TIMEOUT, async {
        loop {
            let event = receiver.recv().await.expect("event before forced exit");
            if matches!(event.event, PtyEvent::Exited { .. }) {
                break;
            }
        }
    })
    .await
    .expect("exit event timeout");
    assert!(matches!(
        receiver.recv().await,
        Err(PtyEventRecvError::Closed)
    ));
}
