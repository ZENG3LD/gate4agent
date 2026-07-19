use crate::{normalize_hook_event, HookAdapterError};
use gate4agent_types::{AdapterId, ProviderEvent};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

pub const HOOK_EVENT_ID_MAX_BYTES: usize = 256;
pub const HOOK_SEEN_EVENT_IDS_MAX: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookEventEnvelope {
    pub source_sequence: u64,
    pub event_id: Option<String>,
    pub event_name: String,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookEventDisposition {
    Applied,
    Duplicate,
    StaleSequence,
    IgnoredUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookReduction {
    pub source_sequence: u64,
    pub missed_before: u64,
    pub disposition: HookEventDisposition,
    pub events: Vec<ProviderEvent>,
}

/// Stateful, pure hook event reducer.
///
/// It owns only protocol state: source ordering, bounded event-ID replay
/// protection, and tool start/completion correlation. It does not own hook
/// installation, authentication, transport, persistence, or presentation.
#[derive(Clone, Debug)]
pub struct HookSessionReducer {
    adapter_id: AdapterId,
    last_source_sequence: u64,
    seen_event_ids: BTreeSet<String>,
    seen_event_order: VecDeque<String>,
    tool_correlations: BTreeMap<String, VecDeque<String>>,
    next_tool_id: u64,
}

impl HookSessionReducer {
    pub fn new(adapter_id: AdapterId) -> Self {
        Self {
            adapter_id,
            last_source_sequence: 0,
            seen_event_ids: BTreeSet::new(),
            seen_event_order: VecDeque::new(),
            tool_correlations: BTreeMap::new(),
            next_tool_id: 1,
        }
    }

    pub fn adapter_id(&self) -> &AdapterId {
        &self.adapter_id
    }

    pub fn last_source_sequence(&self) -> u64 {
        self.last_source_sequence
    }

    pub fn reduce(
        &mut self,
        envelope: HookEventEnvelope,
    ) -> Result<HookReduction, HookSessionReducerError> {
        if envelope.source_sequence == 0 {
            return Err(HookSessionReducerError::InvalidSourceSequence);
        }
        validate_event_id(envelope.event_id.as_deref())?;

        if envelope.source_sequence <= self.last_source_sequence {
            return Ok(HookReduction {
                source_sequence: envelope.source_sequence,
                missed_before: 0,
                disposition: HookEventDisposition::StaleSequence,
                events: Vec::new(),
            });
        }

        let missed_before = envelope
            .source_sequence
            .saturating_sub(self.last_source_sequence)
            .saturating_sub(1);
        if missed_before > 0 {
            self.tool_correlations.clear();
        }
        self.last_source_sequence = envelope.source_sequence;

        if let Some(event_id) = envelope.event_id {
            if self.seen_event_ids.contains(&event_id) {
                return Ok(HookReduction {
                    source_sequence: envelope.source_sequence,
                    missed_before,
                    disposition: HookEventDisposition::Duplicate,
                    events: Vec::new(),
                });
            }
            self.remember_event_id(event_id);
        }

        let mut events =
            normalize_hook_event(&self.adapter_id, &envelope.event_name, &envelope.payload)?;
        self.correlate_tools(&mut events);
        let disposition = if events.is_empty() {
            HookEventDisposition::IgnoredUnknown
        } else {
            HookEventDisposition::Applied
        };
        Ok(HookReduction {
            source_sequence: envelope.source_sequence,
            missed_before,
            disposition,
            events,
        })
    }

    pub fn clear_protocol_state(&mut self) {
        self.last_source_sequence = 0;
        self.seen_event_ids.clear();
        self.seen_event_order.clear();
        self.tool_correlations.clear();
        self.next_tool_id = 1;
    }

    fn remember_event_id(&mut self, event_id: String) {
        self.seen_event_ids.insert(event_id.clone());
        self.seen_event_order.push_back(event_id);
        while self.seen_event_order.len() > HOOK_SEEN_EVENT_IDS_MAX {
            if let Some(expired) = self.seen_event_order.pop_front() {
                self.seen_event_ids.remove(&expired);
            }
        }
    }

    fn correlate_tools(&mut self, events: &mut [ProviderEvent]) {
        for event in events {
            match event {
                ProviderEvent::SessionStarted { .. } | ProviderEvent::TurnStarted { .. } => {
                    self.tool_correlations.clear();
                }
                ProviderEvent::ToolStarted { id, name, .. } => {
                    let raw_id = id.clone();
                    let correlated_id = if raw_id.is_empty() || raw_id == *name {
                        let value = format!("hook-tool-{}", self.next_tool_id);
                        self.next_tool_id = self.next_tool_id.saturating_add(1);
                        value
                    } else {
                        raw_id.clone()
                    };
                    self.tool_correlations
                        .entry(raw_id)
                        .or_default()
                        .push_back(correlated_id.clone());
                    *id = correlated_id;
                }
                ProviderEvent::ToolCompleted { id, .. } => {
                    let raw_id = id.clone();
                    if let Some(correlations) = self.tool_correlations.get_mut(&raw_id) {
                        if let Some(correlated_id) = correlations.pop_front() {
                            *id = correlated_id;
                        }
                        if correlations.is_empty() {
                            self.tool_correlations.remove(&raw_id);
                        }
                    }
                }
                ProviderEvent::TurnCompleted { .. } | ProviderEvent::SessionEnded { .. } => {
                    self.tool_correlations.clear();
                }
                ProviderEvent::Text { .. }
                | ProviderEvent::Thinking { .. }
                | ProviderEvent::Error { .. }
                | ProviderEvent::Ready
                | ProviderEvent::InteractionRequested { .. }
                | ProviderEvent::SubagentStarted { .. }
                | ProviderEvent::SubagentStopped { .. }
                | ProviderEvent::RateLimited { .. } => {}
            }
        }
    }
}

fn validate_event_id(value: Option<&str>) -> Result<(), HookSessionReducerError> {
    if value.is_some_and(|value| {
        value.trim().is_empty()
            || value.len() > HOOK_EVENT_ID_MAX_BYTES
            || value.chars().any(char::is_control)
    }) {
        return Err(HookSessionReducerError::InvalidEventId);
    }
    Ok(())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HookSessionReducerError {
    #[error("hook source sequence must be greater than zero")]
    InvalidSourceSequence,
    #[error("hook event ID is empty, unsafe, or too large")]
    InvalidEventId,
    #[error(transparent)]
    Normalize(#[from] HookAdapterError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reducer() -> HookSessionReducer {
        HookSessionReducer::new(AdapterId::new("grok").unwrap())
    }

    fn envelope(
        source_sequence: u64,
        event_id: &str,
        event_name: &str,
        payload: Value,
    ) -> HookEventEnvelope {
        HookEventEnvelope {
            source_sequence,
            event_id: Some(event_id.to_owned()),
            event_name: event_name.to_owned(),
            payload,
        }
    }

    #[test]
    fn correlates_provider_tools_without_explicit_ids() {
        let mut reducer = reducer();
        let started = reducer
            .reduce(envelope(
                1,
                "e1",
                "PreToolUse",
                json!({"toolName": "shell", "toolInput": {"command": "pwd"}}),
            ))
            .unwrap();
        let [ProviderEvent::ToolStarted { id, .. }] = started.events.as_slice() else {
            panic!("expected tool start");
        };
        assert_eq!(id, "hook-tool-1");

        let completed = reducer
            .reduce(envelope(
                2,
                "e2",
                "PostToolUse",
                json!({"toolName": "shell", "toolResponse": "ok"}),
            ))
            .unwrap();
        let [ProviderEvent::ToolCompleted { id, .. }] = completed.events.as_slice() else {
            panic!("expected tool completion");
        };
        assert_eq!(id, "hook-tool-1");
    }

    #[test]
    fn suppresses_replayed_ids_and_stale_sequences() {
        let mut reducer = reducer();
        reducer
            .reduce(envelope(
                1,
                "e1",
                "UserPromptSubmit",
                json!({"prompt": "one"}),
            ))
            .unwrap();
        let duplicate = reducer
            .reduce(envelope(
                2,
                "e1",
                "UserPromptSubmit",
                json!({"prompt": "one"}),
            ))
            .unwrap();
        assert_eq!(duplicate.disposition, HookEventDisposition::Duplicate);
        assert!(duplicate.events.is_empty());

        let stale = reducer
            .reduce(envelope(1, "e2", "Stop", json!({})))
            .unwrap();
        assert_eq!(stale.disposition, HookEventDisposition::StaleSequence);
        assert!(stale.events.is_empty());
    }

    #[test]
    fn reports_gaps_and_drops_unsafe_tool_correlation() {
        let mut reducer = reducer();
        reducer
            .reduce(envelope(
                1,
                "e1",
                "PreToolUse",
                json!({"toolName": "shell"}),
            ))
            .unwrap();
        let after_gap = reducer
            .reduce(envelope(
                4,
                "e4",
                "PostToolUse",
                json!({"toolName": "shell", "toolResponse": "unknown start"}),
            ))
            .unwrap();
        assert_eq!(after_gap.missed_before, 2);
        let [ProviderEvent::ToolCompleted { id, .. }] = after_gap.events.as_slice() else {
            panic!("expected partial completion");
        };
        assert_eq!(id, "shell");
    }

    #[test]
    fn emits_canonical_turn_start_and_completion() {
        let mut reducer = reducer();
        let started = reducer
            .reduce(envelope(
                1,
                "e1",
                "userPromptSubmit",
                json!({"prompt": "fix tests"}),
            ))
            .unwrap();
        assert!(matches!(
            started.events.as_slice(),
            [ProviderEvent::TurnStarted { prompt }] if prompt.as_deref() == Some("fix tests")
        ));
        let completed = reducer
            .reduce(envelope(2, "e2", "stop", json!({})))
            .unwrap();
        assert!(matches!(
            completed.events.last(),
            Some(ProviderEvent::TurnCompleted { .. })
        ));
    }
}
