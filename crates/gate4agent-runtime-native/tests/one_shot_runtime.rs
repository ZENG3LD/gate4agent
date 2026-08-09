use std::time::Duration;

use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_testkit::{one_shot_agent_spec, ONE_SHOT_FIXTURE_ID};
use gate4agent_types::{
    AdapterFamily, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand,
    ControlEventKind, ProviderEvent, ProviderRuntimePolicy, ProviderSource, SessionStatus,
    StartRequest, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
};

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

#[tokio::test]
async fn bounded_plain_text_one_shot_crosses_the_public_control_plane() {
    let spec = one_shot_agent_spec();
    let (handle, mut runtime) = NativeRuntime::new(
        AgentRegistry::new([spec]).expect("fixture registry"),
        NativeRuntimeConfig {
            worker_poll_interval_ms: 5,
            ..NativeRuntimeConfig::default()
        },
    );
    let subscription = handle.subscribe(64);
    let instance_id = AgentInstanceId(411);
    handle
        .dispatch(command(
            101,
            ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new(ONE_SHOT_FIXTURE_ID).unwrap(),
                transport: TransportKind::Pipe,
            },
        ))
        .unwrap();
    handle
        .dispatch(command(
            102,
            ControlCommand::Start {
                instance_id,
                runtime_policy: ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
                request: StartRequest {
                    working_directory: std::env::current_dir()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: Some("fixture prompt".to_owned()),
                    session_options: None,
                },
            },
        ))
        .unwrap();

    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            runtime.tick().await;
            while let Ok(event) = subscription.try_recv() {
                events.push(event);
            }
            if handle
                .snapshot()
                .sessions
                .first()
                .is_some_and(|session| matches!(session.status, SessionStatus::Exited { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controlled one-shot runtime timeout");

    let snapshot = handle.snapshot();
    let session = snapshot.sessions.first().unwrap();
    assert_eq!(session.provider.completed_turns, 1);
    assert!(session.provider.session.is_none());
    assert_eq!(
        session
            .session_options
            .as_ref()
            .map(|selection| selection.model.as_str()),
        Some("sonnet")
    );
    assert!(events.iter().any(|event| matches!(
        &event.event,
        ControlEventKind::ProviderEvent {
            source: ProviderSource {
                family: AdapterFamily::OneShot,
                ..
            },
            event: ProviderEvent::Text { text, .. },
            ..
        } if text == "fixture-one-shot:fixture prompt"
    )));
}
