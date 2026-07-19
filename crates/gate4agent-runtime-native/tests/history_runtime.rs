use gate4agent_adapters::HistorySourceLayout;
use gate4agent_catalog::builtin_registry;
use gate4agent_runtime_native::{
    NativeHistoryConfig, NativeHistoryRoot, NativeRuntime, NativeRuntimeConfig,
};
use gate4agent_types::{
    AdapterId, AgentId, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEvent,
    ControlEventKind, HistoryQuery, TransportKind, CONTROL_PROTOCOL_VERSION,
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
