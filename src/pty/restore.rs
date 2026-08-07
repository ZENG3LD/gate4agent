//! Bounded, storage-agnostic PTY cold-restore checkpoints.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::event::{
    recent_scrollback_formatted, PtyEvent, PtyEventEnvelope, PtyGapReason,
    PtyMouseProtocolEncoding, PtySize, PtyTerminalSnapshot, PTY_TERMINAL_SCROLLBACK_ROWS_MAX,
};

pub const PTY_COLD_RESTORE_FORMAT_REVISION: u16 = 1;
pub const PTY_COLD_RESTORE_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PTY_COLD_RESTORE_MAX_TAIL_EVENTS: usize = 65_536;
const PTY_COLD_RESTORE_SCROLLBACK_ROWS: usize = 1_000;

/// Stable terminal checkpoint for persistence by an owning shell.
///
/// gate4agent does not choose a filesystem, database, or restart policy. A
/// shell may serialize this value and a contiguous tail of `PtyEventEnvelope`
/// values, then reconstruct the last confirmed terminal state after a crash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyColdRestoreCheckpoint {
    pub format_revision: u16,
    pub terminal: PtyTerminalSnapshot,
}

impl PtyColdRestoreCheckpoint {
    pub fn new(terminal: PtyTerminalSnapshot) -> Result<Self, PtyColdRestoreError> {
        let checkpoint = Self {
            format_revision: PTY_COLD_RESTORE_FORMAT_REVISION,
            terminal,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn validate(&self) -> Result<(), PtyColdRestoreError> {
        if self.format_revision != PTY_COLD_RESTORE_FORMAT_REVISION {
            return Err(PtyColdRestoreError::UnsupportedFormat {
                actual: self.format_revision,
                supported: PTY_COLD_RESTORE_FORMAT_REVISION,
            });
        }
        if self.terminal.pty_id.is_empty() {
            return Err(PtyColdRestoreError::InvalidCheckpoint("empty PTY ID"));
        }
        if self.terminal.provider_revision.is_empty() {
            return Err(PtyColdRestoreError::InvalidCheckpoint(
                "empty provider revision",
            ));
        }
        if self.terminal.generation == 0 {
            return Err(PtyColdRestoreError::InvalidCheckpoint(
                "generation starts at 1",
            ));
        }
        if self.terminal.size.rows == 0 || self.terminal.size.cols == 0 {
            return Err(PtyColdRestoreError::InvalidCheckpoint(
                "terminal dimensions must be non-zero",
            ));
        }
        let bytes = terminal_snapshot_bytes(&self.terminal);
        if bytes > PTY_COLD_RESTORE_MAX_BYTES {
            return Err(PtyColdRestoreError::TooLarge {
                bytes,
                max: PTY_COLD_RESTORE_MAX_BYTES,
            });
        }
        Ok(())
    }

    /// Apply a contiguous post-checkpoint tail and reconstruct terminal state.
    /// A gap or identity mismatch is rejected instead of producing a plausible
    /// but corrupted screen.
    pub fn restore_terminal(
        &self,
        tail: &[PtyEventEnvelope],
    ) -> Result<PtyTerminalSnapshot, PtyColdRestoreError> {
        self.validate()?;
        if tail.len() > PTY_COLD_RESTORE_MAX_TAIL_EVENTS {
            return Err(PtyColdRestoreError::TooManyTailEvents {
                events: tail.len(),
                max: PTY_COLD_RESTORE_MAX_TAIL_EVENTS,
            });
        }
        if tail.is_empty() {
            return Ok(self.terminal.clone());
        }

        let tail_bytes = tail.iter().try_fold(0usize, |bytes, event| {
            bytes
                .checked_add(event_restore_bytes(event))
                .ok_or(PtyColdRestoreError::TooLarge {
                    bytes: usize::MAX,
                    max: PTY_COLD_RESTORE_MAX_BYTES,
                })
        })?;
        let total_bytes = terminal_snapshot_bytes(&self.terminal)
            .checked_add(tail_bytes)
            .ok_or(PtyColdRestoreError::TooLarge {
                bytes: usize::MAX,
                max: PTY_COLD_RESTORE_MAX_BYTES,
            })?;
        if total_bytes > PTY_COLD_RESTORE_MAX_BYTES {
            return Err(PtyColdRestoreError::TooLarge {
                bytes: total_bytes,
                max: PTY_COLD_RESTORE_MAX_BYTES,
            });
        }

        let mut parser = vt100::Parser::new(
            self.terminal.size.rows,
            self.terminal.size.cols,
            PTY_COLD_RESTORE_SCROLLBACK_ROWS,
        );
        restore_terminal_modes(&mut parser, &self.terminal);
        parser.process(&self.terminal.formatted);
        let mut expected_sequence = self
            .terminal
            .sequence
            .checked_add(1)
            .ok_or(PtyColdRestoreError::SequenceExhausted)?;

        for envelope in tail {
            self.validate_envelope(envelope, expected_sequence)?;
            match &envelope.event {
                PtyEvent::Output(data) => parser.process(data),
                PtyEvent::Resized(size) => {
                    if size.rows == 0 || size.cols == 0 {
                        return Err(PtyColdRestoreError::InvalidResize {
                            rows: size.rows,
                            cols: size.cols,
                        });
                    }
                    parser.screen_mut().set_size(size.rows, size.cols);
                }
                PtyEvent::DataGap { .. } => unreachable!("gaps are rejected during validation"),
                PtyEvent::Started
                | PtyEvent::ForegroundProcess(_)
                | PtyEvent::SnapshotAvailable { .. }
                | PtyEvent::ReaderError { .. }
                | PtyEvent::OperatorActionRequired { .. }
                | PtyEvent::Exited { .. } => {}
            }
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or(PtyColdRestoreError::SequenceExhausted)?;
        }

        let reconstructed_scrollback = recent_scrollback_formatted(parser.screen());
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let scrollback_formatted = if screen.alternate_screen() {
            Vec::new()
        } else {
            merge_recent_scrollback(
                &self.terminal.scrollback_formatted,
                reconstructed_scrollback,
            )
        };
        Ok(PtyTerminalSnapshot {
            pty_id: self.terminal.pty_id.clone(),
            provider_revision: self.terminal.provider_revision.clone(),
            generation: self.terminal.generation,
            sequence: tail.last().expect("non-empty tail").sequence,
            size: PtySize { rows, cols },
            cursor: screen.cursor_position(),
            bracketed_paste: screen.bracketed_paste(),
            contents: screen.contents(),
            formatted: screen.contents_formatted(),
            scrollback_formatted,
            alternate_screen: screen.alternate_screen(),
            mouse_protocol_enabled: screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
            mouse_protocol_encoding: match screen.mouse_protocol_encoding() {
                vt100::MouseProtocolEncoding::Default => PtyMouseProtocolEncoding::Default,
                vt100::MouseProtocolEncoding::Utf8 => PtyMouseProtocolEncoding::Utf8,
                vt100::MouseProtocolEncoding::Sgr => PtyMouseProtocolEncoding::Sgr,
            },
        })
    }

    fn validate_envelope(
        &self,
        envelope: &PtyEventEnvelope,
        expected_sequence: u64,
    ) -> Result<(), PtyColdRestoreError> {
        if envelope.pty_id != self.terminal.pty_id {
            return Err(PtyColdRestoreError::PtyMismatch {
                checkpoint: self.terminal.pty_id.clone(),
                tail: envelope.pty_id.clone(),
            });
        }
        if envelope.provider_revision != self.terminal.provider_revision {
            return Err(PtyColdRestoreError::ProviderRevisionMismatch {
                checkpoint: self.terminal.provider_revision.clone(),
                tail: envelope.provider_revision.clone(),
            });
        }
        if envelope.generation != self.terminal.generation {
            return Err(PtyColdRestoreError::GenerationMismatch {
                checkpoint: self.terminal.generation,
                tail: envelope.generation,
            });
        }
        if let PtyEvent::DataGap {
            from_sequence,
            to_sequence,
            reason,
        } = &envelope.event
        {
            return Err(PtyColdRestoreError::DataGap {
                from_sequence: *from_sequence,
                to_sequence: *to_sequence,
                reason: *reason,
            });
        }
        if envelope.sequence != expected_sequence {
            return Err(PtyColdRestoreError::SequenceMismatch {
                expected: expected_sequence,
                actual: envelope.sequence,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PtyColdRestoreError {
    #[error("unsupported PTY cold-restore format {actual}; supported format is {supported}")]
    UnsupportedFormat { actual: u16, supported: u16 },
    #[error("invalid PTY cold-restore checkpoint: {0}")]
    InvalidCheckpoint(&'static str),
    #[error("PTY cold-restore payload is {bytes} bytes; limit is {max}")]
    TooLarge { bytes: usize, max: usize },
    #[error("PTY cold-restore tail has {events} events; limit is {max}")]
    TooManyTailEvents { events: usize, max: usize },
    #[error("PTY cold-restore checkpoint belongs to '{checkpoint}', tail belongs to '{tail}'")]
    PtyMismatch { checkpoint: String, tail: String },
    #[error("PTY cold-restore checkpoint provider is '{checkpoint}', tail provider is '{tail}'")]
    ProviderRevisionMismatch { checkpoint: String, tail: String },
    #[error("PTY cold-restore checkpoint generation is {checkpoint}, tail generation is {tail}")]
    GenerationMismatch { checkpoint: u64, tail: u64 },
    #[error("PTY cold-restore expected sequence {expected}, got {actual}")]
    SequenceMismatch { expected: u64, actual: u64 },
    #[error("PTY cold-restore sequence space is exhausted")]
    SequenceExhausted,
    #[error("PTY cold-restore tail contains invalid resize {rows}x{cols}")]
    InvalidResize { rows: u16, cols: u16 },
    #[error("PTY cold-restore tail contains {reason:?} gap {from_sequence}..={to_sequence}")]
    DataGap {
        from_sequence: u64,
        to_sequence: u64,
        reason: PtyGapReason,
    },
}

fn terminal_snapshot_bytes(snapshot: &PtyTerminalSnapshot) -> usize {
    snapshot
        .pty_id
        .len()
        .saturating_add(snapshot.provider_revision.len())
        .saturating_add(snapshot.contents.len())
        .saturating_add(snapshot.formatted.len())
        .saturating_add(
            snapshot
                .scrollback_formatted
                .iter()
                .fold(0usize, |bytes, row| bytes.saturating_add(row.len())),
        )
}

fn restore_terminal_modes(parser: &mut vt100::Parser, terminal: &PtyTerminalSnapshot) {
    if terminal.alternate_screen {
        parser.process(b"\x1b[?1049h");
    }
    if terminal.bracketed_paste {
        parser.process(b"\x1b[?2004h");
    }
    if terminal.mouse_protocol_enabled {
        parser.process(b"\x1b[?1000h");
    }
    match terminal.mouse_protocol_encoding {
        PtyMouseProtocolEncoding::Default => {}
        PtyMouseProtocolEncoding::Utf8 => parser.process(b"\x1b[?1005h"),
        PtyMouseProtocolEncoding::Sgr => parser.process(b"\x1b[?1006h"),
    }
}

fn merge_recent_scrollback(
    checkpoint: &[Vec<u8>],
    reconstructed: Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    let total = checkpoint.len().saturating_add(reconstructed.len());
    let skip = total.saturating_sub(PTY_TERMINAL_SCROLLBACK_ROWS_MAX);
    checkpoint
        .iter()
        .cloned()
        .chain(reconstructed)
        .skip(skip)
        .collect()
}

fn event_restore_bytes(event: &PtyEventEnvelope) -> usize {
    let payload = match &event.event {
        PtyEvent::Output(data) => data.len(),
        PtyEvent::ForegroundProcess(observation) => {
            observation.observed_process.len().saturating_add(
                observation
                    .readiness
                    .process_name
                    .as_ref()
                    .map_or(0, String::len),
            )
        }
        PtyEvent::ReaderError { message } | PtyEvent::OperatorActionRequired { message } => {
            message.len()
        }
        _ => 1,
    };
    event
        .pty_id
        .len()
        .saturating_add(event.provider_revision.len())
        .saturating_add(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint() -> PtyColdRestoreCheckpoint {
        PtyColdRestoreCheckpoint::new(PtyTerminalSnapshot {
            pty_id: "pty-restore".to_owned(),
            provider_revision: "provider:1".to_owned(),
            generation: 4,
            sequence: 7,
            size: PtySize { rows: 4, cols: 40 },
            cursor: (0, 4),
            bracketed_paste: false,
            contents: "base".to_owned(),
            formatted: b"base".to_vec(),
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: Default::default(),
        })
        .expect("checkpoint")
    }

    fn tail(sequence: u64, event: PtyEvent) -> PtyEventEnvelope {
        PtyEventEnvelope {
            pty_id: "pty-restore".to_owned(),
            provider_revision: "provider:1".to_owned(),
            generation: 4,
            sequence,
            event,
        }
    }

    #[test]
    fn checkpoint_round_trips_and_applies_contiguous_tail() {
        let mut checkpoint = checkpoint();
        checkpoint.terminal.scrollback_formatted = vec![b"older".to_vec()];
        checkpoint.terminal.mouse_protocol_enabled = true;
        checkpoint.terminal.mouse_protocol_encoding = PtyMouseProtocolEncoding::Sgr;
        let encoded = serde_json::to_vec(&checkpoint).expect("serialize checkpoint");
        let decoded: PtyColdRestoreCheckpoint =
            serde_json::from_slice(&encoded).expect("deserialize checkpoint");
        let restored = decoded
            .restore_terminal(&[
                tail(8, PtyEvent::Output(b" tail".to_vec())),
                tail(9, PtyEvent::Resized(PtySize { rows: 5, cols: 50 })),
                tail(10, PtyEvent::Exited { code: 0 }),
            ])
            .expect("restore terminal");
        assert!(restored.contents.contains("base tail"));
        assert_eq!(restored.sequence, 10);
        assert_eq!(restored.size, PtySize { rows: 5, cols: 50 });
        assert_eq!(restored.scrollback_formatted, vec![b"older".to_vec()]);
        assert!(restored.mouse_protocol_enabled);
        assert_eq!(restored.mouse_protocol_encoding, PtyMouseProtocolEncoding::Sgr);
    }

    #[test]
    fn restore_derives_alternate_screen_from_checkpoint_and_tail() {
        let mut checkpoint = checkpoint();
        checkpoint.terminal.alternate_screen = true;

        let retained = checkpoint
            .restore_terminal(&[tail(8, PtyEvent::Started)])
            .expect("retain alternate screen");
        assert!(retained.alternate_screen);
        assert!(retained.scrollback_formatted.is_empty());

        let exited = checkpoint
            .restore_terminal(&[tail(8, PtyEvent::Output(b"\x1b[?1049l".to_vec()))])
            .expect("exit alternate screen");
        assert!(!exited.alternate_screen);
    }

    #[test]
    fn restore_rejects_sequence_holes_and_explicit_gaps() {
        assert!(matches!(
            checkpoint().restore_terminal(&[tail(9, PtyEvent::Started)]),
            Err(PtyColdRestoreError::SequenceMismatch {
                expected: 8,
                actual: 9
            })
        ));
        assert!(matches!(
            checkpoint().restore_terminal(&[tail(
                8,
                PtyEvent::DataGap {
                    from_sequence: 8,
                    to_sequence: 9,
                    reason: PtyGapReason::ReplayEvicted,
                }
            )]),
            Err(PtyColdRestoreError::DataGap { .. })
        ));
    }

    #[test]
    fn restore_rejects_a_different_provider_revision() {
        let mut event = tail(8, PtyEvent::Started);
        event.provider_revision = "provider:2".to_owned();
        assert!(matches!(
            checkpoint().restore_terminal(&[event]),
            Err(PtyColdRestoreError::ProviderRevisionMismatch { .. })
        ));
    }
}
