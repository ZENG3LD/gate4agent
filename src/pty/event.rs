//! Sequenced PTY lifecycle events, bounded replay, and explicit subscriber gaps.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

use super::os_process::PtyForegroundObservation;

pub const DEFAULT_PTY_REPLAY_BYTES: usize = 64 * 1024;
pub const PTY_PROVIDER_PROTOCOL_REVISION: &str = "gate4agent-pty-r1";
const DEFAULT_PTY_REPLAY_EVENTS: usize = 4_096;
const DEFAULT_PTY_SNAPSHOT_SCROLLBACK_ROWS: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PtySignal {
    InterruptKey,
    EndOfFileKey,
    TerminateProcess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PtySignalOutcome {
    ControlWritten,
    TerminationRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PtyGapReason {
    ReplayEvicted,
    SubscriberLagged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum PtyEvent {
    Started,
    Output(Vec<u8>),
    DataGap {
        from_sequence: u64,
        to_sequence: u64,
        reason: PtyGapReason,
    },
    Resized(PtySize),
    ForegroundProcess(PtyForegroundObservation),
    SnapshotAvailable {
        snapshot_sequence: u64,
    },
    ReaderError {
        message: String,
    },
    OperatorActionRequired {
        message: String,
    },
    Exited {
        code: i32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyEventEnvelope {
    pub pty_id: String,
    pub provider_revision: String,
    pub generation: u64,
    pub sequence: u64,
    pub event: PtyEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyReplayCursor {
    pub provider_revision: String,
    pub generation: u64,
    pub next_sequence: u64,
}

impl PtyReplayCursor {
    pub fn beginning(provider_revision: impl Into<String>, generation: u64) -> Self {
        Self {
            provider_revision: provider_revision.into(),
            generation,
            next_sequence: 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyTerminalSnapshot {
    pub pty_id: String,
    pub provider_revision: String,
    pub generation: u64,
    /// Last event incorporated into this terminal state.
    pub sequence: u64,
    pub size: PtySize,
    pub cursor: (u16, u16),
    pub contents: String,
    /// ANSI-formatted visible screen suitable for reconstructing decoration.
    pub formatted: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum PtyAttachError {
    #[error("requested PTY generation {requested}, but the live generation is {actual}")]
    StaleGeneration { requested: u64, actual: u64 },
    #[error("requested PTY provider revision '{requested}', but the live revision is '{actual}'")]
    ProviderRevisionMismatch { requested: String, actual: String },
    #[error("PTY replay cursor sequence starts at 1")]
    InvalidCursor,
    #[error("PTY event journal mutex poisoned")]
    JournalPoisoned,
}

#[derive(Debug, Error)]
pub enum PtyEventRecvError {
    #[error("PTY event stream closed")]
    Closed,
}

/// Atomic replay snapshot plus the future event stream from the same boundary.
pub struct PtyAttachment {
    pub replay: Vec<PtyEventEnvelope>,
    pub receiver: PtyEventReceiver,
}

/// Receiver that converts broadcast lag into an explicit sequenced `DataGap`.
pub struct PtyEventReceiver {
    rx: broadcast::Receiver<PtyEventEnvelope>,
    pty_id: String,
    provider_revision: String,
    generation: u64,
    expected_sequence: u64,
    pending: Option<PtyEventEnvelope>,
}

impl PtyEventReceiver {
    pub fn try_recv(&mut self) -> Result<Option<PtyEventEnvelope>, PtyEventRecvError> {
        if let Some(event) = self.pending.take() {
            self.expected_sequence = event.sequence.saturating_add(1);
            return Ok(Some(event));
        }

        loop {
            match self.rx.try_recv() {
                Ok(event) if event.generation != self.generation => continue,
                Ok(event) if event.sequence < self.expected_sequence => continue,
                Ok(event) if event.sequence > self.expected_sequence => {
                    let gap = PtyEventEnvelope {
                        pty_id: self.pty_id.clone(),
                        provider_revision: self.provider_revision.clone(),
                        generation: self.generation,
                        sequence: event.sequence - 1,
                        event: PtyEvent::DataGap {
                            from_sequence: self.expected_sequence,
                            to_sequence: event.sequence - 1,
                            reason: PtyGapReason::SubscriberLagged,
                        },
                    };
                    self.pending = Some(event);
                    return Ok(Some(gap));
                }
                Ok(event) => {
                    self.expected_sequence = event.sequence.saturating_add(1);
                    return Ok(Some(event));
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Empty) => return Ok(None),
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(PtyEventRecvError::Closed);
                }
            }
        }
    }

    pub async fn recv(&mut self) -> Result<PtyEventEnvelope, PtyEventRecvError> {
        if let Some(event) = self.pending.take() {
            self.expected_sequence = event.sequence.saturating_add(1);
            return Ok(event);
        }

        loop {
            match self.rx.recv().await {
                Ok(event) if event.generation != self.generation => continue,
                Ok(event) if event.sequence < self.expected_sequence => continue,
                Ok(event) if event.sequence > self.expected_sequence => {
                    let gap = PtyEventEnvelope {
                        pty_id: self.pty_id.clone(),
                        provider_revision: self.provider_revision.clone(),
                        generation: self.generation,
                        sequence: event.sequence - 1,
                        event: PtyEvent::DataGap {
                            from_sequence: self.expected_sequence,
                            to_sequence: event.sequence - 1,
                            reason: PtyGapReason::SubscriberLagged,
                        },
                    };
                    self.pending = Some(event);
                    return Ok(gap);
                }
                Ok(event) => {
                    self.expected_sequence = event.sequence.saturating_add(1);
                    return Ok(event);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The first retained envelope gives the exact missing range.
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(PtyEventRecvError::Closed);
                }
            }
        }
    }
}

pub(crate) struct PtyEventPublisher {
    pty_id: String,
    provider_revision: String,
    generation: u64,
    tx: broadcast::Sender<PtyEventEnvelope>,
    state: Mutex<PtyEventState>,
}

struct PtyEventState {
    sequence: u64,
    replay_bytes: usize,
    replay_byte_limit: usize,
    replay_event_limit: usize,
    replay: VecDeque<PtyEventEnvelope>,
    terminal: vt100::Parser,
}

impl PtyEventPublisher {
    pub(crate) fn new(
        pty_id: String,
        provider_revision: String,
        generation: u64,
        channel_capacity: usize,
        replay_byte_limit: usize,
        rows: u16,
        cols: u16,
    ) -> Arc<Self> {
        let (tx, _) = broadcast::channel(channel_capacity);
        Arc::new(Self {
            pty_id,
            provider_revision,
            generation,
            tx,
            state: Mutex::new(PtyEventState {
                sequence: 0,
                replay_bytes: 0,
                replay_byte_limit,
                replay_event_limit: DEFAULT_PTY_REPLAY_EVENTS,
                replay: VecDeque::new(),
                terminal: vt100::Parser::new(rows, cols, DEFAULT_PTY_SNAPSHOT_SCROLLBACK_ROWS),
            }),
        })
    }

    pub(crate) fn publish(&self, event: PtyEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.observe_terminal(&event);
        state.sequence = state.sequence.saturating_add(1);
        let envelope = PtyEventEnvelope {
            pty_id: self.pty_id.clone(),
            provider_revision: self.provider_revision.clone(),
            generation: self.generation,
            sequence: state.sequence,
            event,
        };
        state.push(envelope.clone());
        let _ = self.tx.send(envelope);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn provider_revision(&self) -> &str {
        &self.provider_revision
    }

    pub(crate) fn subscribe(&self) -> Result<PtyEventReceiver, PtyAttachError> {
        let rx = self.tx.subscribe();
        let state = self
            .state
            .lock()
            .map_err(|_| PtyAttachError::JournalPoisoned)?;
        Ok(PtyEventReceiver {
            rx,
            pty_id: self.pty_id.clone(),
            provider_revision: self.provider_revision.clone(),
            generation: self.generation,
            expected_sequence: state.sequence.saturating_add(1),
            pending: None,
        })
    }

    pub(crate) fn attach(&self, cursor: PtyReplayCursor) -> Result<PtyAttachment, PtyAttachError> {
        if cursor.next_sequence == 0 {
            return Err(PtyAttachError::InvalidCursor);
        }
        if cursor.provider_revision != self.provider_revision {
            return Err(PtyAttachError::ProviderRevisionMismatch {
                requested: cursor.provider_revision,
                actual: self.provider_revision.clone(),
            });
        }
        if cursor.generation != self.generation {
            return Err(PtyAttachError::StaleGeneration {
                requested: cursor.generation,
                actual: self.generation,
            });
        }

        // Subscribe before locking. Events queued before the snapshot boundary
        // are ignored by the receiver and represented exactly once in replay.
        let rx = self.tx.subscribe();
        let state = self
            .state
            .lock()
            .map_err(|_| PtyAttachError::JournalPoisoned)?;
        let snapshot_sequence = state.sequence;
        let mut replay = Vec::new();
        let first_retained = state.replay.front().map(|event| event.sequence);

        if cursor.next_sequence <= snapshot_sequence {
            match first_retained {
                Some(first) if cursor.next_sequence < first => {
                    replay.push(PtyEventEnvelope {
                        pty_id: self.pty_id.clone(),
                        provider_revision: self.provider_revision.clone(),
                        generation: self.generation,
                        sequence: first - 1,
                        event: PtyEvent::DataGap {
                            from_sequence: cursor.next_sequence,
                            to_sequence: first - 1,
                            reason: PtyGapReason::ReplayEvicted,
                        },
                    });
                }
                None => {
                    replay.push(PtyEventEnvelope {
                        pty_id: self.pty_id.clone(),
                        provider_revision: self.provider_revision.clone(),
                        generation: self.generation,
                        sequence: snapshot_sequence,
                        event: PtyEvent::DataGap {
                            from_sequence: cursor.next_sequence,
                            to_sequence: snapshot_sequence,
                            reason: PtyGapReason::ReplayEvicted,
                        },
                    });
                }
                _ => {}
            }
            replay.extend(
                state
                    .replay
                    .iter()
                    .filter(|event| event.sequence >= cursor.next_sequence)
                    .cloned(),
            );
        }

        Ok(PtyAttachment {
            replay,
            receiver: PtyEventReceiver {
                rx,
                pty_id: self.pty_id.clone(),
                provider_revision: self.provider_revision.clone(),
                generation: self.generation,
                expected_sequence: snapshot_sequence.saturating_add(1),
                pending: None,
            },
        })
    }

    pub(crate) fn snapshot(&self) -> Result<PtyTerminalSnapshot, PtyAttachError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PtyAttachError::JournalPoisoned)?;
        let screen = state.terminal.screen();
        let (rows, cols) = screen.size();
        Ok(PtyTerminalSnapshot {
            pty_id: self.pty_id.clone(),
            provider_revision: self.provider_revision.clone(),
            generation: self.generation,
            sequence: state.sequence,
            size: PtySize { rows, cols },
            cursor: screen.cursor_position(),
            contents: screen.contents(),
            formatted: screen.contents_formatted(),
        })
    }
}

impl PtyEventState {
    fn observe_terminal(&mut self, event: &PtyEvent) {
        match event {
            PtyEvent::Output(data) => self.terminal.process(data),
            PtyEvent::Resized(size) => self.terminal.set_size(size.rows, size.cols),
            PtyEvent::Started
            | PtyEvent::DataGap { .. }
            | PtyEvent::ReaderError { .. }
            | PtyEvent::OperatorActionRequired { .. }
            | PtyEvent::ForegroundProcess(_)
            | PtyEvent::SnapshotAvailable { .. }
            | PtyEvent::Exited { .. } => {}
        }
    }

    fn push(&mut self, event: PtyEventEnvelope) {
        self.replay_bytes = self.replay_bytes.saturating_add(event_size(&event));
        self.replay.push_back(event);
        while self.replay.len() > self.replay_event_limit
            || self.replay_bytes > self.replay_byte_limit
        {
            let Some(evicted) = self.replay.pop_front() else {
                break;
            };
            self.replay_bytes = self.replay_bytes.saturating_sub(event_size(&evicted));
        }
    }
}

fn event_size(event: &PtyEventEnvelope) -> usize {
    match &event.event {
        PtyEvent::Output(data) => data.len(),
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "test-provider:1";

    #[test]
    fn replay_reports_evicted_sequences() {
        let publisher =
            PtyEventPublisher::new("pty-test".to_owned(), REVISION.to_owned(), 1, 8, 3, 24, 80);
        publisher.publish(PtyEvent::Output(vec![1, 2]));
        publisher.publish(PtyEvent::Output(vec![3, 4]));

        let attachment = publisher
            .attach(PtyReplayCursor::beginning(REVISION, 1))
            .expect("attach");
        assert!(matches!(
            attachment.replay.first().map(|event| &event.event),
            Some(PtyEvent::DataGap {
                from_sequence: 1,
                to_sequence: 1,
                reason: PtyGapReason::ReplayEvicted,
            })
        ));
        assert_eq!(attachment.replay.last().unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn receiver_turns_broadcast_lag_into_an_exact_gap() {
        let publisher =
            PtyEventPublisher::new("pty-test".to_owned(), REVISION.to_owned(), 1, 2, 64, 24, 80);
        let mut receiver = publisher.subscribe().expect("subscribe");
        for value in 0..4 {
            publisher.publish(PtyEvent::Output(vec![value]));
        }

        let gap = receiver.recv().await.expect("gap");
        assert!(matches!(
            gap.event,
            PtyEvent::DataGap {
                from_sequence: 1,
                to_sequence: 2,
                reason: PtyGapReason::SubscriberLagged,
            }
        ));
        assert_eq!(receiver.recv().await.expect("first retained").sequence, 3);
    }

    #[test]
    fn nonblocking_receiver_preserves_exact_gap_and_pending_event() {
        let publisher =
            PtyEventPublisher::new("pty-test".to_owned(), REVISION.to_owned(), 1, 2, 64, 24, 80);
        let mut receiver = publisher.subscribe().expect("subscribe");
        for value in 0..4 {
            publisher.publish(PtyEvent::Output(vec![value]));
        }
        let gap = receiver.try_recv().unwrap().unwrap();
        assert!(matches!(
            gap.event,
            PtyEvent::DataGap {
                from_sequence: 1,
                to_sequence: 2,
                reason: PtyGapReason::SubscriberLagged,
            }
        ));
        assert_eq!(receiver.try_recv().unwrap().unwrap().sequence, 3);
        assert_eq!(receiver.try_recv().unwrap().unwrap().sequence, 4);
        assert!(receiver.try_recv().unwrap().is_none());
    }

    #[test]
    fn snapshot_is_pinned_to_the_last_incorporated_sequence() {
        let publisher =
            PtyEventPublisher::new("pty-test".to_owned(), REVISION.to_owned(), 1, 8, 64, 2, 10);
        publisher.publish(PtyEvent::Output(b"hello".to_vec()));
        let snapshot = publisher.snapshot().expect("snapshot");
        assert_eq!(snapshot.sequence, 1);
        assert_eq!(snapshot.contents, "hello");
        assert_eq!(snapshot.size, PtySize { rows: 2, cols: 10 });
        assert_eq!(snapshot.provider_revision, REVISION);
    }

    #[test]
    fn attach_rejects_a_different_provider_revision() {
        let publisher =
            PtyEventPublisher::new("pty-test".to_owned(), REVISION.to_owned(), 1, 8, 64, 2, 10);
        let error = match publisher.attach(PtyReplayCursor::beginning("other-provider:1", 1)) {
            Err(error) => error,
            Ok(_) => panic!("revision mismatch must reject attach"),
        };
        assert!(matches!(
            error,
            PtyAttachError::ProviderRevisionMismatch { .. }
        ));
    }
}
