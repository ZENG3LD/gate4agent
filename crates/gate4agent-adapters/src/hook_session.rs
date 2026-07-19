use crate::{normalize_hook_event, HookAdapterError};
use gate4agent_types::{
    AdapterId, ProviderEvent, ProviderEventValidationError, PROVIDER_SUBAGENTS_MAX,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

pub const HOOK_EVENT_ID_MAX_BYTES: usize = 256;
pub const HOOK_SEEN_EVENT_IDS_MAX: usize = 256;
const CLAUDE_SUBAGENT_ID_MAX_BYTES: usize = 64;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ClaudeTrackedSubagent {
    agent_type: Option<String>,
    description: Option<String>,
    background_tasks_authoritative: bool,
    listed_as_subagent_task: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClaudeBackgroundTask {
    id: String,
    agent_type: Option<String>,
    description: Option<String>,
    running: bool,
    teammate: bool,
}

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
pub struct HookSubagentSeed {
    pub provider_agent_id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
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
    claude_subagents: BTreeMap<String, ClaudeTrackedSubagent>,
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
            claude_subagents: BTreeMap::new(),
        }
    }

    pub fn adapter_id(&self) -> &AdapterId {
        &self.adapter_id
    }

    pub fn last_source_sequence(&self) -> u64 {
        self.last_source_sequence
    }

    /// Seeds a persisted working-child snapshot before live Hook traffic.
    ///
    /// Only Claude currently consumes these seeds. They remain provisional:
    /// a later complete `background_tasks` inventory may reap a child whose
    /// finish hook arrived while the listener was offline.
    pub fn seed_live_subagents(&mut self, seeds: &[HookSubagentSeed]) -> usize {
        if self.adapter_id.as_str() != "claude-code" || !self.claude_subagents.is_empty() {
            return 0;
        }
        for seed in seeds.iter().take(PROVIDER_SUBAGENTS_MAX) {
            self.upsert_claude_subagent(
                &seed.provider_agent_id,
                seed.agent_type.clone(),
                seed.description.clone(),
            );
            if let Some(tracked) = self.claude_subagents.get_mut(&seed.provider_agent_id) {
                tracked.background_tasks_authoritative = true;
            }
        }
        self.claude_subagents.len()
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
        if self.adapter_id.as_str() == "claude-code" {
            self.reconcile_claude_subagents(&envelope.event_name, &envelope.payload, &mut events);
        }
        self.correlate_tools(&envelope.event_name, &mut events);
        for event in &events {
            event.validate_ingress()?;
        }
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
        self.claude_subagents.clear();
    }

    fn reconcile_claude_subagents(
        &mut self,
        event_name: &str,
        payload: &Value,
        events: &mut Vec<ProviderEvent>,
    ) {
        let before = self.claude_subagents.clone();
        let record = payload.as_object();
        let agent_id = record.and_then(|record| read_nonempty_string(record.get("agent_id")));
        let child_turn_boundary = matches!(event_name, "Stop" | "StopFailure")
            && agent_id
                .as_deref()
                .is_some_and(|agent_id| before.contains_key(agent_id));
        match event_name {
            "SubagentStart" => {
                if let Some(agent_id) = agent_id.as_deref() {
                    self.upsert_claude_subagent(
                        agent_id,
                        record.and_then(|record| read_nonempty_string(record.get("agent_type"))),
                        record.and_then(|record| read_nonempty_string(record.get("description"))),
                    );
                }
            }
            "SubagentStop" => {
                if let Some(agent_id) = agent_id.as_deref() {
                    self.claude_subagents.remove(agent_id);
                }
            }
            "TeammateIdle" => {
                if let Some(name) =
                    record.and_then(|record| read_nonempty_string(record.get("teammate_name")))
                {
                    self.claude_subagents
                        .retain(|id, _| !claude_teammate_id_matches_name(id, &name));
                }
            }
            "Stop" | "StopFailure" if !child_turn_boundary => {
                if let Some(record) = record {
                    if let Some((tasks, complete)) = read_claude_background_tasks(record) {
                        self.fold_claude_background_tasks(tasks, complete);
                    }
                }
            }
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest" => {
                if let Some(agent_id) = agent_id.as_deref() {
                    self.upsert_claude_subagent(
                        agent_id,
                        record.and_then(|record| read_nonempty_string(record.get("agent_type"))),
                        None,
                    );
                }
            }
            _ => {}
        }

        events.retain(|event| {
            !matches!(
                event,
                ProviderEvent::SubagentStarted { .. } | ProviderEvent::SubagentStopped { .. }
            )
        });
        if child_turn_boundary {
            events.clear();
        }
        let mut lifecycle = Vec::new();
        for id in before.keys() {
            if !self.claude_subagents.contains_key(id) {
                lifecycle.push(ProviderEvent::SubagentStopped {
                    agent_id: id.clone(),
                });
            }
        }
        for (id, tracked) in &self.claude_subagents {
            let changed = before.get(id).is_none_or(|previous| {
                previous.agent_type != tracked.agent_type
                    || previous.description != tracked.description
            });
            if changed {
                lifecycle.push(ProviderEvent::SubagentStarted {
                    agent_id: id.clone(),
                    agent_type: tracked.agent_type.clone(),
                    description: tracked.description.clone(),
                });
            }
        }
        lifecycle.append(events);
        *events = lifecycle;
    }

    fn upsert_claude_subagent(
        &mut self,
        id: &str,
        agent_type: Option<String>,
        description: Option<String>,
    ) {
        if id.is_empty() || id.len() > CLAUDE_SUBAGENT_ID_MAX_BYTES {
            return;
        }
        if let Some(existing) = self.claude_subagents.get_mut(id) {
            existing.agent_type = agent_type.or(existing.agent_type.take());
            existing.description = description.or(existing.description.take());
            existing.background_tasks_authoritative = false;
            return;
        }
        if self.claude_subagents.len() >= PROVIDER_SUBAGENTS_MAX {
            return;
        }
        self.claude_subagents.insert(
            id.to_owned(),
            ClaudeTrackedSubagent {
                agent_type,
                description,
                ..ClaudeTrackedSubagent::default()
            },
        );
    }

    fn fold_claude_background_tasks(
        &mut self,
        tasks: Vec<ClaudeBackgroundTask>,
        inventory_complete: bool,
    ) {
        if tasks.is_empty() {
            if inventory_complete {
                self.claude_subagents.clear();
            }
            return;
        }
        let has_teammate_task = tasks.iter().any(|task| task.teammate);
        let mut listed_ids = BTreeSet::new();
        let mut pending_running_tasks = Vec::new();
        for task in tasks.iter().filter(|task| !task.teammate) {
            listed_ids.insert(task.id.clone());
            if !task.running {
                self.claude_subagents.remove(&task.id);
                continue;
            }
            self.upsert_claude_subagent(
                &task.id,
                task.agent_type.clone(),
                task.description.clone(),
            );
            if let Some(tracked) = self.claude_subagents.get_mut(&task.id) {
                tracked.background_tasks_authoritative = true;
                tracked.listed_as_subagent_task = true;
            } else {
                pending_running_tasks.push(task.clone());
            }
        }
        if inventory_complete {
            self.claude_subagents.retain(|id, tracked| {
                listed_ids.contains(id)
                    || (has_teammate_task
                        && !tracked.background_tasks_authoritative
                        && !tracked.listed_as_subagent_task
                        && is_claude_teammate_lifecycle_id(id))
            });
        }
        for task in pending_running_tasks {
            if self.claude_subagents.len() >= PROVIDER_SUBAGENTS_MAX {
                break;
            }
            self.upsert_claude_subagent(&task.id, task.agent_type, task.description);
            if let Some(tracked) = self.claude_subagents.get_mut(&task.id) {
                tracked.background_tasks_authoritative = true;
                tracked.listed_as_subagent_task = true;
            }
        }
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

    fn correlate_tools(&mut self, event_name: &str, events: &mut [ProviderEvent]) {
        for event in events {
            match event {
                ProviderEvent::SessionStarted { .. } | ProviderEvent::TurnStarted { .. } => {
                    self.tool_correlations.clear();
                }
                ProviderEvent::ToolStarted {
                    id, name, agent_id, ..
                } => {
                    let raw_id = id.clone();
                    let correlation_key = tool_correlation_key(agent_id.as_deref(), &raw_id);
                    let coalesced_id = (matches!(self.adapter_id.as_str(), "pi" | "omp")
                        && event_name == "tool_execution_start")
                        .then(|| {
                            self.tool_correlations
                                .get(&correlation_key)
                                .and_then(|correlations| correlations.front())
                                .cloned()
                        })
                        .flatten();
                    if let Some(coalesced_id) = coalesced_id {
                        *id = coalesced_id;
                        continue;
                    }
                    let correlated_id = if raw_id.is_empty() || raw_id == *name {
                        let value = format!("hook-tool-{}", self.next_tool_id);
                        self.next_tool_id = self.next_tool_id.saturating_add(1);
                        value
                    } else {
                        raw_id.clone()
                    };
                    self.tool_correlations
                        .entry(correlation_key)
                        .or_default()
                        .push_back(correlated_id.clone());
                    *id = correlated_id;
                }
                ProviderEvent::ToolCompleted { id, agent_id, .. } => {
                    let raw_id = id.clone();
                    let correlation_key = tool_correlation_key(agent_id.as_deref(), &raw_id);
                    if let Some(correlations) = self.tool_correlations.get_mut(&correlation_key) {
                        if let Some(correlated_id) = correlations.pop_front() {
                            *id = correlated_id;
                        }
                        if correlations.is_empty() {
                            self.tool_correlations.remove(&correlation_key);
                        }
                    }
                }
                ProviderEvent::TurnCompleted { .. } | ProviderEvent::SessionEnded { .. } => {
                    self.tool_correlations.clear();
                }
                ProviderEvent::Text { .. }
                | ProviderEvent::SessionIdentityObserved { .. }
                | ProviderEvent::WorkingObserved
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

fn read_nonempty_string(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn tool_correlation_key(agent_id: Option<&str>, raw_id: &str) -> String {
    format!("{}\0{raw_id}", agent_id.unwrap_or("lead"))
}

fn read_claude_background_tasks(
    record: &serde_json::Map<String, Value>,
) -> Option<(Vec<ClaudeBackgroundTask>, bool)> {
    let raw = record.get("background_tasks")?.as_array()?;
    let mut tasks = Vec::new();
    let mut complete = true;
    for value in raw {
        let Some(task) = value.as_object() else {
            continue;
        };
        let Some(task_type) = task.get("type").and_then(Value::as_str) else {
            continue;
        };
        if task_type != "subagent" && task_type != "teammate" {
            continue;
        }
        let Some(id) = read_nonempty_string(task.get("id")) else {
            continue;
        };
        if id.len() > CLAUDE_SUBAGENT_ID_MAX_BYTES {
            continue;
        }
        if tasks.len() >= PROVIDER_SUBAGENTS_MAX {
            complete = false;
            break;
        }
        tasks.push(ClaudeBackgroundTask {
            id,
            agent_type: read_nonempty_string(task.get("agent_type")),
            description: read_nonempty_string(task.get("description")),
            running: task.get("status").and_then(Value::as_str) == Some("running"),
            teammate: task_type == "teammate",
        });
    }
    Some((tasks, complete))
}

fn is_claude_teammate_lifecycle_id(id: &str) -> bool {
    let Some(separator) = id.rfind('-') else {
        return false;
    };
    separator > 1
        && id.starts_with('a')
        && id[separator + 1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

fn claude_teammate_id_matches_name(id: &str, name: &str) -> bool {
    let prefix = format!("a{name}-");
    id.strip_prefix(&prefix)
        .is_some_and(|suffix| !suffix.is_empty() && !suffix.contains('-'))
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
    #[error(transparent)]
    InvalidCanonicalEvent(#[from] ProviderEventValidationError),
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
    fn pi_coalesces_call_and_execution_start_before_exact_completion() {
        for adapter_id in ["pi", "omp"] {
            let mut reducer = HookSessionReducer::new(AdapterId::new(adapter_id).unwrap());
            let called = reducer
                .reduce(envelope(
                    1,
                    "e1",
                    "tool_call",
                    json!({"tool_name": "bash", "tool_input": {"command": "cargo check"}}),
                ))
                .unwrap();
            let [ProviderEvent::ToolStarted { id: called_id, .. }] = called.events.as_slice()
            else {
                panic!("expected tool call");
            };

            let executing = reducer
                .reduce(envelope(
                    2,
                    "e2",
                    "tool_execution_start",
                    json!({"tool_name": "bash", "tool_input": {"command": "cargo check"}}),
                ))
                .unwrap();
            let [ProviderEvent::ToolStarted {
                id: executing_id, ..
            }] = executing.events.as_slice()
            else {
                panic!("expected execution start");
            };
            assert_eq!(executing_id, called_id);

            let completed = reducer
                .reduce(envelope(
                    3,
                    "e3",
                    "tool_execution_end",
                    json!({"tool_name": "bash"}),
                ))
                .unwrap();
            let [ProviderEvent::ToolCompleted {
                id: completed_id, ..
            }] = completed.events.as_slice()
            else {
                panic!("expected execution completion");
            };
            assert_eq!(completed_id, called_id);
        }
    }

    #[test]
    fn correlates_idless_tools_independently_per_claude_child() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        for (sequence, agent_id) in ["a1", "a2"].into_iter().enumerate() {
            reducer
                .reduce(envelope(
                    u64::try_from(sequence + 1).unwrap(),
                    &format!("start-{agent_id}"),
                    "PreToolUse",
                    json!({"agent_id": agent_id, "tool_name": "shell"}),
                ))
                .unwrap();
        }
        let child_two = reducer
            .reduce(envelope(
                3,
                "done-a2",
                "PostToolUse",
                json!({"agent_id": "a2", "tool_name": "shell", "tool_response": "ok"}),
            ))
            .unwrap();
        assert!(child_two.events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCompleted { id, agent_id: Some(agent_id), .. }
                if id == "hook-tool-2" && agent_id == "a2"
        )));

        let child_one = reducer
            .reduce(envelope(
                4,
                "done-a1",
                "PostToolUse",
                json!({"agent_id": "a1", "tool_name": "shell", "tool_response": "ok"}),
            ))
            .unwrap();
        assert!(child_one.events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCompleted { id, agent_id: Some(agent_id), .. }
                if id == "hook-tool-1" && agent_id == "a1"
        )));
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

    #[test]
    fn claude_stop_reconciles_one_shot_inventory_before_turn_completion() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        let started = reducer
            .reduce(envelope(
                1,
                "c1",
                "SubagentStart",
                json!({"agent_id": "a1", "agent_type": "reviewer"}),
            ))
            .unwrap();
        assert!(matches!(
            started.events.as_slice(),
            [ProviderEvent::SubagentStarted { agent_id, .. }] if agent_id == "a1"
        ));

        let completed = reducer
            .reduce(envelope(2, "c2", "Stop", json!({"background_tasks": []})))
            .unwrap();
        assert!(matches!(
            completed.events.as_slice(),
            [ProviderEvent::SubagentStopped { agent_id }, ProviderEvent::TurnCompleted { .. }]
                if agent_id == "a1"
        ));
    }

    #[test]
    fn claude_inventory_recovers_running_child_after_listener_restart() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        let recovered = reducer
            .reduce(envelope(
                1,
                "c1",
                "Stop",
                json!({
                    "background_tasks": [{
                        "id": "a77",
                        "type": "subagent",
                        "status": "running",
                        "agent_type": "probe",
                        "description": "verify restart"
                    }]
                }),
            ))
            .unwrap();
        assert!(matches!(
            recovered.events.as_slice(),
            [
                ProviderEvent::SubagentStarted {
                    agent_id,
                    agent_type: Some(agent_type),
                    ..
                },
                ProviderEvent::TurnCompleted { .. }
            ] if agent_id == "a77" && agent_type == "probe"
        ));
    }

    #[test]
    fn claude_persisted_seed_is_reaped_by_complete_inventory() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        assert_eq!(
            reducer.seed_live_subagents(&[HookSubagentSeed {
                provider_agent_id: "areviewer-6d3cb5b5".to_owned(),
                agent_type: Some("reviewer".to_owned()),
                description: None,
            }]),
            1
        );
        let reconciled = reducer
            .reduce(envelope(
                1,
                "c1",
                "Stop",
                json!({
                    "background_tasks": [{
                        "id": "team-reviewer",
                        "type": "teammate",
                        "status": "running"
                    }]
                }),
            ))
            .unwrap();
        assert!(matches!(
            reconciled.events.as_slice(),
            [ProviderEvent::SubagentStopped { agent_id }, ProviderEvent::TurnCompleted { .. }]
                if agent_id == "areviewer-6d3cb5b5"
        ));
    }

    #[test]
    fn claude_child_stop_is_not_misclassified_as_lead_completion() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        reducer
            .reduce(envelope(
                1,
                "c1",
                "SubagentStart",
                json!({"agent_id": "a1"}),
            ))
            .unwrap();
        let child_stop = reducer
            .reduce(envelope(2, "c2", "Stop", json!({"agent_id": "a1"})))
            .unwrap();
        assert_eq!(child_stop.disposition, HookEventDisposition::IgnoredUnknown);
        assert!(child_stop.events.is_empty());

        let stopped = reducer
            .reduce(envelope(3, "c3", "SubagentStop", json!({"agent_id": "a1"})))
            .unwrap();
        assert!(matches!(
            stopped.events.as_slice(),
            [ProviderEvent::SubagentStopped { agent_id }] if agent_id == "a1"
        ));
    }

    #[test]
    fn claude_teammate_idle_removes_only_exact_named_lifecycle_rows() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        for (sequence, id) in ["arev-6d3cb5b5", "arev-two-6d3cb5b5"]
            .into_iter()
            .enumerate()
        {
            reducer
                .reduce(envelope(
                    u64::try_from(sequence + 1).unwrap(),
                    &format!("c{}", sequence + 1),
                    "SubagentStart",
                    json!({"agent_id": id}),
                ))
                .unwrap();
        }
        let idled = reducer
            .reduce(envelope(
                3,
                "c3",
                "TeammateIdle",
                json!({"teammate_name": "rev"}),
            ))
            .unwrap();
        assert!(matches!(
            idled.events.as_slice(),
            [ProviderEvent::SubagentStopped { agent_id }] if agent_id == "arev-6d3cb5b5"
        ));
    }

    #[test]
    fn claude_complete_inventory_retries_replacement_after_stale_cleanup() {
        let mut reducer = HookSessionReducer::new(AdapterId::new("claude-code").unwrap());
        for index in 0..PROVIDER_SUBAGENTS_MAX {
            reducer
                .reduce(envelope(
                    u64::try_from(index + 1).unwrap(),
                    &format!("start-{index}"),
                    "SubagentStart",
                    json!({"agent_id": format!("stale{index}")}),
                ))
                .unwrap();
        }
        let reconciled = reducer
            .reduce(envelope(
                u64::try_from(PROVIDER_SUBAGENTS_MAX + 1).unwrap(),
                "inventory",
                "Stop",
                json!({
                    "background_tasks": [{
                        "id": "replacement",
                        "type": "subagent",
                        "status": "running"
                    }]
                }),
            ))
            .unwrap();
        assert!(reconciled.events.iter().any(|event| matches!(
            event,
            ProviderEvent::SubagentStarted { agent_id, .. } if agent_id == "replacement"
        )));
        assert_eq!(
            reconciled
                .events
                .iter()
                .filter(|event| matches!(event, ProviderEvent::SubagentStopped { .. }))
                .count(),
            PROVIDER_SUBAGENTS_MAX
        );
    }
}
