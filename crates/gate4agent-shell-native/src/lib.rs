//! Native effect execution for gate4agent control-plane sessions.

use gate4agent::pty::{PtyEvent, PtySession, PtyTerminalSnapshot};
use gate4agent::agent::ReadinessStatus;
use gate4agent::{
    LaunchRequest, ReadinessIntent, ReadinessPermit, ReadinessTracker, RuntimePlatform,
};
use gate4agent_catalog::{AgentRegistry, AgentSpec};
use gate4agent_types::{
    AgentInstanceId, ControlEffect, ControlObservation, EffectEnvelope, ObservationEnvelope,
    OperationId, PreparedInputKind, SessionGeneration, TerminalFrame, TerminalSize, TransportKind,
    CONTROL_PROTOCOL_VERSION, WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NativeSessionKey {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

struct OwnedPtySession {
    session: PtySession,
    spawn_operation_id: OperationId,
    last_terminal_sequence: u64,
    terminal_stale_published: bool,
}

/// Executes native effects and returns exactly one completion observation for
/// each accepted effect. Logical lifecycle state remains owned by the engine.
pub struct NativeEffectShell {
    catalog: AgentRegistry,
    pty_sessions: BTreeMap<NativeSessionKey, OwnedPtySession>,
}

impl NativeEffectShell {
    pub fn new(catalog: AgentRegistry) -> Self {
        Self {
            catalog,
            pty_sessions: BTreeMap::new(),
        }
    }

    pub fn active_session_count(&self) -> usize {
        self.pty_sessions.len()
    }

    pub fn spawn_operation_id(&self, key: NativeSessionKey) -> Option<OperationId> {
        self.pty_sessions
            .get(&key)
            .map(|owned| owned.spawn_operation_id)
    }

    pub fn terminal_snapshot(
        &self,
        key: NativeSessionKey,
    ) -> Result<PtyTerminalSnapshot, String> {
        self.pty_sessions
            .get(&key)
            .ok_or_else(|| missing_session_message(key))?
            .session
            .terminal_snapshot()
            .map_err(|error| error.to_string())
    }

    pub async fn execute(&mut self, envelope: EffectEnvelope) -> ObservationEnvelope {
        let EffectEnvelope {
            protocol_version,
            operation_id,
            instance_id,
            generation,
            effect,
        } = envelope;
        let key = NativeSessionKey {
            instance_id,
            generation,
        };

        let observation = if protocol_version != CONTROL_PROTOCOL_VERSION {
            effect_failure(
                &effect,
                format!(
                    "effect protocol version {protocol_version} is unsupported; expected {CONTROL_PROTOCOL_VERSION}"
                ),
            )
        } else {
            match effect {
                ControlEffect::Spawn {
                    agent_id,
                    transport,
                    request,
                } => {
                    if transport != TransportKind::Pty {
                        ControlObservation::SpawnFailed {
                            message: format!(
                                "native shell PTY provider cannot execute {transport:?} transport"
                            ),
                        }
                    } else if self.pty_sessions.contains_key(&key) {
                        ControlObservation::SpawnFailed {
                            message: format!(
                                "native session {instance_id:?}/{generation:?} already exists"
                            ),
                        }
                    } else if !request.terminal_size.is_valid() {
                        ControlObservation::SpawnFailed {
                            message: "terminal size is outside the supported range".to_owned(),
                        }
                    } else if request.working_directory.is_empty()
                        || request.working_directory.len() > WORKING_DIRECTORY_MAX_BYTES
                        || request.working_directory.contains('\0')
                    {
                        ControlObservation::SpawnFailed {
                            message: "working directory is invalid".to_owned(),
                        }
                    } else if let Some(spec) = self.catalog.get(&agent_id) {
                        match PtySession::spawn_agent_with_size(
                            spec,
                            LaunchRequest {
                                working_dir: PathBuf::from(request.working_directory),
                                platform: RuntimePlatform::current(),
                                ..LaunchRequest::default()
                            },
                            request.terminal_size.rows,
                            request.terminal_size.columns,
                        )
                        .await
                        {
                            Ok(session) => {
                                let process_id = session.root_pid();
                                self.pty_sessions.insert(
                                    key,
                                    OwnedPtySession {
                                        session,
                                        spawn_operation_id: operation_id,
                                        last_terminal_sequence: 0,
                                        terminal_stale_published: false,
                                    },
                                );
                                ControlObservation::Spawned { process_id }
                            }
                            Err(error) => ControlObservation::SpawnFailed {
                                message: error.to_string(),
                            },
                        }
                    } else {
                        ControlObservation::SpawnFailed {
                            message: format!("agent '{agent_id}' is absent from native catalog"),
                        }
                    }
                }
                ControlEffect::Stop { force } => match self.pty_sessions.remove(&key) {
                    Some(owned) => match owned.session.shutdown().await {
                        Ok(outcome) => {
                            let forced = force || outcome.termination.is_some();
                            ControlObservation::StopCompleted {
                                forced,
                                exit_code: outcome.exit_code,
                                final_terminal: Some(terminal_frame(outcome.terminal)),
                            }
                        }
                        Err(error) => ControlObservation::StopFailed {
                            message: error.to_string(),
                        },
                    },
                    None => ControlObservation::StopFailed {
                        message: missing_session_message(key),
                    },
                },
                ControlEffect::WriteInput { input } => match self.pty_sessions.get(&key) {
                    Some(owned)
                        if matches!(
                            input.kind(),
                            PreparedInputKind::TerminalText | PreparedInputKind::TerminalControl
                        ) => match owned.session.send_terminal_input(input).await {
                            Ok(()) => ControlObservation::InputCompleted,
                            Err(error) => ControlObservation::InputFailed {
                                message: error.to_string(),
                            },
                        },
                    Some(owned) => {
                        let intent = match input.kind() {
                            PreparedInputKind::InsertDraft | PreparedInputKind::AgentCommand => {
                                ReadinessIntent::DraftPaste
                            }
                            PreparedInputKind::SubmitPrompt => ReadinessIntent::FollowupPrompt,
                            PreparedInputKind::TerminalText
                            | PreparedInputKind::TerminalControl => unreachable!(),
                        };
                        let Some(spec) = self.catalog.get(owned.session.agent_id()).cloned() else {
                            return completion_observation(
                                operation_id,
                                instance_id,
                                generation,
                                ControlObservation::InputFailed {
                                    message: "session agent disappeared from native catalog"
                                        .to_owned(),
                                },
                            );
                        };
                        match wait_for_readiness(&owned.session, &spec, intent).await {
                            Ok(permit) => {
                                match owned.session.send_prepared_input(input, permit).await {
                                    Ok(()) => ControlObservation::InputCompleted,
                                    Err(error) => ControlObservation::InputFailed {
                                        message: error.to_string(),
                                    },
                                }
                            }
                            Err(message) => ControlObservation::InputFailed { message },
                        }
                    }
                    None => ControlObservation::InputFailed {
                        message: missing_session_message(key),
                    },
                },
                ControlEffect::Resize { size } if !size.is_valid() => {
                    ControlObservation::ResizeFailed {
                        message: "terminal size is outside the supported range".to_owned(),
                    }
                }
                ControlEffect::Resize { size } => match self.pty_sessions.get(&key) {
                    Some(owned) => match owned.session.resize(size.rows, size.columns).await {
                        Ok(()) => ControlObservation::ResizeCompleted { size },
                        Err(error) => ControlObservation::ResizeFailed {
                            message: error.to_string(),
                        },
                    },
                    None => ControlObservation::ResizeFailed {
                        message: missing_session_message(key),
                    },
                },
            }
        };

        completion_observation(operation_id, instance_id, generation, observation)
    }

    /// Convert naturally exited PTY children into generation-bound lifecycle
    /// observations. Runtime ticks call this before accepting new commands.
    pub async fn collect_exits(&mut self) -> Vec<ObservationEnvelope> {
        let completed: Vec<_> = self
            .pty_sessions
            .iter()
            .filter_map(|(key, owned)| owned.session.reader_finished().then_some(*key))
            .collect();
        let mut observations = Vec::with_capacity(completed.len());
        for key in completed {
            let owned = self
                .pty_sessions
                .remove(&key)
                .expect("completed key came from the owned session map");
            let (exit_code, final_terminal) = match owned.session.shutdown().await {
                Ok(outcome) => (
                    outcome.exit_code,
                    Some(terminal_frame(outcome.terminal)),
                ),
                Err(_) => (None, None),
            };
            observations.push(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: key.instance_id,
                generation: key.generation,
                observation: ControlObservation::ProcessExited {
                    exit_code,
                    final_terminal,
                },
            });
        }
        observations
    }

    /// Capture only changed terminal frames. Snapshot failures become an
    /// explicit stale observation once, until a later successful frame heals it.
    pub fn collect_terminal_frames(&mut self) -> Vec<ObservationEnvelope> {
        let mut observations = Vec::new();
        for (key, owned) in &mut self.pty_sessions {
            match owned.session.terminal_state() {
                Ok(snapshot) if snapshot.sequence > owned.last_terminal_sequence => {
                    owned.last_terminal_sequence = snapshot.sequence;
                    owned.terminal_stale_published = false;
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::TerminalFrame {
                            frame: terminal_frame(snapshot),
                        },
                    });
                }
                Ok(_) => {}
                Err(error) if !owned.terminal_stale_published => {
                    owned.terminal_stale_published = true;
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::TerminalStale {
                            message: error.to_string(),
                        },
                    });
                }
                Err(_) => {}
            }
        }
        observations
    }
}

fn missing_session_message(key: NativeSessionKey) -> String {
    format!(
        "native session {:?}/{:?} does not exist",
        key.instance_id, key.generation
    )
}

fn terminal_frame(snapshot: PtyTerminalSnapshot) -> TerminalFrame {
    TerminalFrame {
        sequence: snapshot.sequence,
        size: TerminalSize {
            rows: snapshot.size.rows,
            columns: snapshot.size.cols,
        },
        cursor_row: snapshot.cursor.0,
        cursor_column: snapshot.cursor.1,
        contents: snapshot.contents,
        formatted: snapshot.formatted,
    }
}

fn effect_failure(effect: &ControlEffect, message: String) -> ControlObservation {
    match effect {
        ControlEffect::Spawn { .. } => ControlObservation::SpawnFailed { message },
        ControlEffect::Stop { .. } => ControlObservation::StopFailed { message },
        ControlEffect::WriteInput { .. } => ControlObservation::InputFailed { message },
        ControlEffect::Resize { .. } => ControlObservation::ResizeFailed { message },
    }
}

fn completion_observation(
    operation_id: OperationId,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    observation: ControlObservation,
) -> ObservationEnvelope {
    ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: Some(operation_id),
        instance_id,
        generation,
        observation,
    }
}

async fn wait_for_readiness(
    session: &PtySession,
    spec: &AgentSpec,
    intent: ReadinessIntent,
) -> Result<ReadinessPermit, String> {
    let started = Instant::now();
    let attachment = session
        .attach_events(session.beginning_cursor())
        .map_err(|error| error.to_string())?;
    let mut receiver = attachment.receiver;
    let mut tracker = ReadinessTracker::new(spec, RuntimePlatform::current(), intent);
    let interval = Duration::from_millis(spec.readiness.poll_interval_ms.max(1));
    let mut next_probe = Instant::now();

    for event in attachment.replay {
        observe_readiness_event(&mut tracker, event.event, elapsed_ms(started))?;
    }

    loop {
        if Instant::now() >= next_probe {
            let foreground = session
                .observe_foreground()
                .await
                .map_err(|error| error.to_string())?;
            tracker.observe_foreground(&foreground.readiness, elapsed_ms(started));
            next_probe = Instant::now() + interval;
        }
        tracker.poll(elapsed_ms(started));
        if readiness_complete(tracker.status())? {
            return tracker
                .into_permit()
                .ok_or_else(|| "ready tracker did not issue a permit".to_owned());
        }

        let wait = next_probe.saturating_duration_since(Instant::now());
        match tokio::time::timeout(wait, receiver.recv()).await {
            Ok(Ok(event)) => {
                observe_readiness_event(&mut tracker, event.event, elapsed_ms(started))?;
            }
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => {
                tracker.poll(elapsed_ms(started));
            }
        }
        if readiness_complete(tracker.status())? {
            return tracker
                .into_permit()
                .ok_or_else(|| "ready tracker did not issue a permit".to_owned());
        }
    }
}

fn observe_readiness_event(
    tracker: &mut ReadinessTracker<'_>,
    event: PtyEvent,
    elapsed_ms: u64,
) -> Result<(), String> {
    match event {
        PtyEvent::Output(data) => {
            tracker.observe_output(&data, elapsed_ms);
        }
        PtyEvent::ForegroundProcess(observation) => {
            tracker.observe_foreground(&observation.readiness, elapsed_ms);
        }
        PtyEvent::DataGap { .. } => {
            return Err("PTY replay gap prevents positive readiness proof".to_owned());
        }
        PtyEvent::ReaderError { message } | PtyEvent::OperatorActionRequired { message } => {
            return Err(message);
        }
        PtyEvent::Exited { code } => {
            return Err(format!("PTY exited with code {code} before readiness"));
        }
        PtyEvent::Started | PtyEvent::Resized(_) | PtyEvent::SnapshotAvailable { .. } => {}
    }
    Ok(())
}

fn readiness_complete(status: ReadinessStatus) -> Result<bool, String> {
    match status {
        ReadinessStatus::Waiting => Ok(false),
        ReadinessStatus::Ready(_) => Ok(true),
        ReadinessStatus::TimedOut => Err("PTY readiness timed out".to_owned()),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
