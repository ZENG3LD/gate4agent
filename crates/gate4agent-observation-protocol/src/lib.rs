//! Dependency-light, versioned observations for provider-session monitoring.
//!
//! Observations carry bounded, allowlisted workflow facts. They never carry
//! prompts, transcript text, raw tool input/output, credentials, or provider
//! configuration. Consumers must still treat the evidence source as part of
//! the fact: terminal hints cannot authoritatively claim semantic workflow
//! completion.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const OBSERVATION_PROTOCOL_VERSION_V1: u16 = 1;
pub const OBSERVATION_EVENT_MAX_BYTES: usize = 4_096;
pub const OBSERVATION_LABEL_MAX_BYTES: usize = 64;
pub const OBSERVATION_DETAIL_MAX_BYTES: usize = 1_024;
pub const OBSERVATION_TODO_ITEMS_MAX: usize = 64;
pub const OBSERVATION_TODO_TEXT_MAX_BYTES: usize = 256;
pub const OBSERVATION_PATH_MAX_BYTES: usize = 1_024;
pub const OBSERVATION_COLLECTION_MAX: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationEvidenceV1 {
    StructuredProvider,
    ManagedHook,
    NodeLifecycle,
    WorkspaceObservation,
    HistoryProjection,
    PtyHint,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationTodoStateV1 {
    Pending,
    InProgress,
    Completed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationInteractionOutcomeV1 {
    Approved,
    Answered,
    Denied,
    Interrupted,
    TurnEnded,
    Superseded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationTodoItemV1 {
    pub id: Option<String>,
    pub text: String,
    pub state: ObservationTodoStateV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationSourceFamilyV1 {
    PtySemantic,
    Pipe,
    OneShot,
    Acp,
    Hook,
    ManagedHook,
    NodeLifecycle,
    History,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCapabilitiesV1 {
    pub tools: bool,
    pub attention: bool,
    pub subagents: bool,
    pub todo: bool,
    pub usage: bool,
    pub owned_processes: bool,
    pub file_changes: bool,
    #[serde(default)]
    pub history_summary: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ObservationKindV1 {
    SourceCapabilities {
        source_family: ObservationSourceFamilyV1,
        source_adapter: String,
        capabilities: ObservationCapabilitiesV1,
    },
    SessionStarted,
    Ready,
    Stopped,
    Exited {
        success: Option<bool>,
    },
    TurnStarted,
    Working,
    TurnCompleted,
    TurnInterrupted,
    ToolStarted {
        correlation_id: String,
        class: String,
    },
    ToolCompleted {
        correlation_id: String,
        class: String,
        success: bool,
        duration_ms: Option<u64>,
    },
    ApprovalRequested {
        correlation_id: String,
        tool_class: String,
    },
    QuestionRequested {
        correlation_id: String,
        tool_class: String,
    },
    ApprovalResolved {
        correlation_id: String,
        outcome: ObservationInteractionOutcomeV1,
    },
    QuestionResolved {
        correlation_id: String,
        outcome: ObservationInteractionOutcomeV1,
    },
    InteractionResolved {
        correlation_id: String,
        outcome: ObservationInteractionOutcomeV1,
    },
    SubagentStarted {
        correlation_id: String,
        class: String,
    },
    SubagentProgress {
        correlation_id: String,
    },
    SubagentCompleted {
        correlation_id: String,
        success: Option<bool>,
    },
    TodoSnapshot {
        revision: u64,
        items: Vec<ObservationTodoItemV1>,
        complete: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        reasoning_tokens: u64,
        context_window: Option<u64>,
        is_cumulative: bool,
    },
    ContextWindowUsage {
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        unattributed_tokens: u64,
        used_tokens: u64,
        capacity_tokens: u64,
    },
    RateLimited,
    OwnedProcessStarted {
        correlation_id: String,
        class: String,
    },
    OwnedProcessExited {
        correlation_id: String,
        success: Option<bool>,
        exit_code: Option<i32>,
    },
    FileChanged {
        path: Option<String>,
    },
    HistorySnapshot {
        message_count: u64,
        message_count_exact: bool,
        completed_turn_count: Option<u64>,
        total_tokens: Option<u64>,
    },
    Gap {
        missed: u64,
    },
    SourceReset,
    Stale,
    Error {
        detail: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationV1 {
    pub source_sequence: u64,
    pub observed_at_unix_ms: Option<u64>,
    pub evidence: ObservationEvidenceV1,
    pub kind: ObservationKindV1,
    pub truncated: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationWireV1 {
    source_sequence: u64,
    observed_at_unix_ms: Option<u64>,
    evidence: ObservationEvidenceV1,
    kind: ObservationKindV1,
    truncated: bool,
}

impl<'de> Deserialize<'de> for ObservationV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ObservationWireV1::deserialize(deserializer)?;
        let observation = Self {
            source_sequence: wire.source_sequence,
            observed_at_unix_ms: wire.observed_at_unix_ms,
            evidence: wire.evidence,
            kind: wire.kind,
            truncated: wire.truncated,
        };
        observation.validate().map_err(serde::de::Error::custom)?;
        Ok(observation)
    }
}

impl ObservationV1 {
    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        if self.source_sequence == 0 {
            return Err(ObservationValidationError::ZeroSequence);
        }
        if self.observed_at_unix_ms == Some(0) {
            return Err(ObservationValidationError::ZeroObservedAt);
        }
        self.kind.validate()?;
        if matches!(self.kind, ObservationKindV1::ContextWindowUsage { .. })
            && self.evidence != ObservationEvidenceV1::StructuredProvider
        {
            return Err(
                ObservationValidationError::ContextWindowUsageRequiresStructuredProvider,
            );
        }
        if self.evidence == ObservationEvidenceV1::PtyHint
            && self.kind.requires_authoritative_semantic_evidence()
        {
            return Err(ObservationValidationError::PtyHintClaimsAuthoritativeFact);
        }
        if matches!(self.kind, ObservationKindV1::HistorySnapshot { .. })
            && self.evidence != ObservationEvidenceV1::HistoryProjection
        {
            return Err(ObservationValidationError::HistorySnapshotRequiresHistoryProjection);
        }
        let actual = self.json_encoded_len();
        if actual > OBSERVATION_EVENT_MAX_BYTES {
            return Err(ObservationValidationError::EventTooLarge {
                max: OBSERVATION_EVENT_MAX_BYTES,
                actual,
            });
        }
        Ok(())
    }

    fn json_encoded_len(&self) -> usize {
        "{\"source_sequence\":".len()
            + decimal_len(self.source_sequence)
            + ",\"observed_at_unix_ms\":".len()
            + option_u64_json_len(self.observed_at_unix_ms)
            + ",\"evidence\":".len()
            + json_string_len(self.evidence.wire_name())
            + ",\"kind\":".len()
            + self.kind.json_encoded_len()
            + ",\"truncated\":".len()
            + bool_json_len(self.truncated)
            + "}".len()
    }
}

impl ObservationKindV1 {
    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        match self {
            Self::SourceCapabilities { source_adapter, .. } => validate_required_text(
                "observation source adapter",
                source_adapter,
                OBSERVATION_LABEL_MAX_BYTES,
            ),
            Self::ToolStarted {
                correlation_id,
                class,
            }
            | Self::ToolCompleted {
                correlation_id,
                class,
                ..
            } => {
                validate_required_text(
                    "tool correlation id",
                    correlation_id,
                    OBSERVATION_LABEL_MAX_BYTES,
                )?;
                validate_required_text("tool class", class, OBSERVATION_LABEL_MAX_BYTES)
            }
            Self::ApprovalRequested {
                correlation_id,
                tool_class,
            }
            | Self::QuestionRequested {
                correlation_id,
                tool_class,
            } => {
                validate_required_text(
                    "interaction correlation id",
                    correlation_id,
                    OBSERVATION_LABEL_MAX_BYTES,
                )?;
                validate_required_text(
                    "interaction tool class",
                    tool_class,
                    OBSERVATION_LABEL_MAX_BYTES,
                )
            }
            Self::ApprovalResolved { correlation_id, .. }
            | Self::QuestionResolved { correlation_id, .. }
            | Self::InteractionResolved { correlation_id, .. } => validate_required_text(
                "interaction correlation id",
                correlation_id,
                OBSERVATION_LABEL_MAX_BYTES,
            ),
            Self::SubagentStarted {
                correlation_id,
                class,
            } => {
                validate_required_text(
                    "subagent correlation id",
                    correlation_id,
                    OBSERVATION_LABEL_MAX_BYTES,
                )?;
                validate_required_text("subagent class", class, OBSERVATION_LABEL_MAX_BYTES)
            }
            Self::SubagentProgress { correlation_id }
            | Self::SubagentCompleted { correlation_id, .. } => validate_required_text(
                "subagent correlation id",
                correlation_id,
                OBSERVATION_LABEL_MAX_BYTES,
            ),
            Self::OwnedProcessStarted {
                correlation_id,
                class,
            } => {
                validate_required_text(
                    "owned process correlation id",
                    correlation_id,
                    OBSERVATION_LABEL_MAX_BYTES,
                )?;
                validate_required_text("owned process class", class, OBSERVATION_LABEL_MAX_BYTES)
            }
            Self::OwnedProcessExited { correlation_id, .. } => validate_required_text(
                "owned process correlation id",
                correlation_id,
                OBSERVATION_LABEL_MAX_BYTES,
            ),
            Self::TodoSnapshot {
                revision, items, ..
            } => {
                if *revision == 0 {
                    return Err(ObservationValidationError::ZeroTodoRevision);
                }
                if items.len() > OBSERVATION_TODO_ITEMS_MAX {
                    return Err(ObservationValidationError::TooMany {
                        field: "todo items",
                        max: OBSERVATION_TODO_ITEMS_MAX,
                        actual: items.len(),
                    });
                }
                for item in items {
                    item.validate()?;
                }
                Ok(())
            }
            Self::ContextWindowUsage {
                uncached_input_tokens,
                cache_read_tokens,
                cache_write_tokens,
                output_tokens,
                unattributed_tokens,
                used_tokens,
                capacity_tokens,
            } => validate_context_window_usage(
                *uncached_input_tokens,
                *cache_read_tokens,
                *cache_write_tokens,
                *output_tokens,
                *unattributed_tokens,
                *used_tokens,
                *capacity_tokens,
            ),
            Self::FileChanged { path: Some(path) } => validate_relative_path(path),
            Self::Gap { missed: 0 } => Err(ObservationValidationError::ZeroGap),
            Self::Error { detail } => {
                validate_required_text("error detail", detail, OBSERVATION_DETAIL_MAX_BYTES)
            }
            _ => Ok(()),
        }
    }

    fn requires_authoritative_semantic_evidence(&self) -> bool {
        matches!(
            self,
            Self::TurnCompleted
                | Self::ToolCompleted { .. }
                | Self::SubagentCompleted { .. }
                | Self::TodoSnapshot { .. }
                | Self::FileChanged { .. }
                | Self::HistorySnapshot { .. }
                | Self::ContextWindowUsage { .. }
        )
    }

    pub fn requires_workflow_detail_capability(&self) -> bool {
        matches!(
            self,
            Self::TodoSnapshot { .. }
                | Self::FileChanged { .. }
                | Self::Error { .. }
        )
    }

    fn json_encoded_len(&self) -> usize {
        let kind = self.wire_name();
        let mut len = "{\"kind\":".len() + json_string_len(kind);
        match self {
            Self::SourceCapabilities {
                source_family,
                source_adapter,
                capabilities,
            } => {
                len += ",\"source_family\":".len() + json_string_len(source_family.wire_name());
                len += ",\"source_adapter\":".len() + json_string_len(source_adapter);
                len += ",\"capabilities\":".len() + capabilities.json_encoded_len();
            }
            Self::Exited { success } => {
                len += ",\"success\":".len() + option_bool_json_len(*success);
            }
            Self::ToolStarted {
                correlation_id,
                class,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"class\":".len() + json_string_len(class);
            }
            Self::ToolCompleted {
                correlation_id,
                class,
                success,
                duration_ms,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"class\":".len() + json_string_len(class);
                len += ",\"success\":".len() + bool_json_len(*success);
                len += ",\"duration_ms\":".len() + option_u64_json_len(*duration_ms);
            }
            Self::ApprovalRequested {
                correlation_id,
                tool_class,
            }
            | Self::QuestionRequested {
                correlation_id,
                tool_class,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"tool_class\":".len() + json_string_len(tool_class);
            }
            Self::ApprovalResolved {
                correlation_id,
                outcome,
            }
            | Self::QuestionResolved {
                correlation_id,
                outcome,
            }
            | Self::InteractionResolved {
                correlation_id,
                outcome,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"outcome\":".len() + json_string_len(outcome.wire_name());
            }
            Self::SubagentStarted {
                correlation_id,
                class,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"class\":".len() + json_string_len(class);
            }
            Self::SubagentProgress { correlation_id } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
            }
            Self::SubagentCompleted {
                correlation_id,
                success,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"success\":".len() + option_bool_json_len(*success);
            }
            Self::OwnedProcessStarted {
                correlation_id,
                class,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"class\":".len() + json_string_len(class);
            }
            Self::OwnedProcessExited {
                correlation_id,
                success,
                exit_code,
            } => {
                len += ",\"correlation_id\":".len() + json_string_len(correlation_id);
                len += ",\"success\":".len() + option_bool_json_len(*success);
                len += ",\"exit_code\":".len() + option_i32_json_len(*exit_code);
            }
            Self::TodoSnapshot {
                revision,
                items,
                complete,
            } => {
                len += ",\"revision\":".len() + decimal_len(*revision);
                len += ",\"items\":[".len();
                len += items
                    .iter()
                    .map(ObservationTodoItemV1::json_encoded_len)
                    .sum::<usize>();
                len += items.len().saturating_sub(1);
                len += "]".len();
                len += ",\"complete\":".len() + bool_json_len(*complete);
            }
            Self::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                context_window,
                is_cumulative,
            } => {
                len += ",\"input_tokens\":".len() + decimal_len(*input_tokens);
                len += ",\"output_tokens\":".len() + decimal_len(*output_tokens);
                len += ",\"cache_read_tokens\":".len() + decimal_len(*cache_read_tokens);
                len += ",\"cache_write_tokens\":".len() + decimal_len(*cache_write_tokens);
                len += ",\"reasoning_tokens\":".len() + decimal_len(*reasoning_tokens);
                len += ",\"context_window\":".len() + option_u64_json_len(*context_window);
                len += ",\"is_cumulative\":".len() + bool_json_len(*is_cumulative);
            }
            Self::ContextWindowUsage {
                uncached_input_tokens,
                cache_read_tokens,
                cache_write_tokens,
                output_tokens,
                unattributed_tokens,
                used_tokens,
                capacity_tokens,
            } => {
                len += ",\"uncached_input_tokens\":".len() + decimal_len(*uncached_input_tokens);
                len += ",\"cache_read_tokens\":".len() + decimal_len(*cache_read_tokens);
                len += ",\"cache_write_tokens\":".len() + decimal_len(*cache_write_tokens);
                len += ",\"output_tokens\":".len() + decimal_len(*output_tokens);
                len += ",\"unattributed_tokens\":".len() + decimal_len(*unattributed_tokens);
                len += ",\"used_tokens\":".len() + decimal_len(*used_tokens);
                len += ",\"capacity_tokens\":".len() + decimal_len(*capacity_tokens);
            }
            Self::FileChanged { path } => {
                len += ",\"path\":".len() + option_string_json_len(path.as_deref());
            }
            Self::HistorySnapshot {
                message_count,
                message_count_exact,
                completed_turn_count,
                total_tokens,
            } => {
                len += ",\"message_count\":".len() + decimal_len(*message_count);
                len += ",\"message_count_exact\":".len() + bool_json_len(*message_count_exact);
                len += ",\"completed_turn_count\":".len()
                    + option_u64_json_len(*completed_turn_count);
                len += ",\"total_tokens\":".len() + option_u64_json_len(*total_tokens);
            }
            Self::Gap { missed } => {
                len += ",\"missed\":".len() + decimal_len(*missed);
            }
            Self::Error { detail } => {
                len += ",\"detail\":".len() + json_string_len(detail);
            }
            _ => {}
        }
        len + "}".len()
    }

    fn wire_name(&self) -> &'static str {
        match self {
            Self::SourceCapabilities { .. } => "source-capabilities",
            Self::SessionStarted => "session-started",
            Self::Ready => "ready",
            Self::Stopped => "stopped",
            Self::Exited { .. } => "exited",
            Self::TurnStarted => "turn-started",
            Self::Working => "working",
            Self::TurnCompleted => "turn-completed",
            Self::TurnInterrupted => "turn-interrupted",
            Self::ToolStarted { .. } => "tool-started",
            Self::ToolCompleted { .. } => "tool-completed",
            Self::ApprovalRequested { .. } => "approval-requested",
            Self::QuestionRequested { .. } => "question-requested",
            Self::ApprovalResolved { .. } => "approval-resolved",
            Self::QuestionResolved { .. } => "question-resolved",
            Self::InteractionResolved { .. } => "interaction-resolved",
            Self::SubagentStarted { .. } => "subagent-started",
            Self::SubagentProgress { .. } => "subagent-progress",
            Self::SubagentCompleted { .. } => "subagent-completed",
            Self::TodoSnapshot { .. } => "todo-snapshot",
            Self::Usage { .. } => "usage",
            Self::ContextWindowUsage { .. } => "context-window-usage",
            Self::RateLimited => "rate-limited",
            Self::OwnedProcessStarted { .. } => "owned-process-started",
            Self::OwnedProcessExited { .. } => "owned-process-exited",
            Self::FileChanged { .. } => "file-changed",
            Self::HistorySnapshot { .. } => "history-snapshot",
            Self::Gap { .. } => "gap",
            Self::SourceReset => "source-reset",
            Self::Stale => "stale",
            Self::Error { .. } => "error",
        }
    }
}

impl ObservationSourceFamilyV1 {
    fn wire_name(self) -> &'static str {
        match self {
            Self::PtySemantic => "pty-semantic",
            Self::Pipe => "pipe",
            Self::OneShot => "one-shot",
            Self::Acp => "acp",
            Self::Hook => "hook",
            Self::ManagedHook => "managed-hook",
            Self::NodeLifecycle => "node-lifecycle",
            Self::History => "history",
        }
    }
}

impl ObservationCapabilitiesV1 {
    fn json_encoded_len(self) -> usize {
        "{\"tools\":".len()
            + bool_json_len(self.tools)
            + ",\"attention\":".len()
            + bool_json_len(self.attention)
            + ",\"subagents\":".len()
            + bool_json_len(self.subagents)
            + ",\"todo\":".len()
            + bool_json_len(self.todo)
            + ",\"usage\":".len()
            + bool_json_len(self.usage)
            + ",\"owned_processes\":".len()
            + bool_json_len(self.owned_processes)
            + ",\"file_changes\":".len()
            + bool_json_len(self.file_changes)
            + ",\"history_summary\":".len()
            + bool_json_len(self.history_summary)
            + "}".len()
    }
}

impl ObservationInteractionOutcomeV1 {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Answered => "answered",
            Self::Denied => "denied",
            Self::Interrupted => "interrupted",
            Self::TurnEnded => "turn-ended",
            Self::Superseded => "superseded",
        }
    }
}

impl ObservationTodoItemV1 {
    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        if let Some(id) = self.id.as_deref() {
            validate_required_text("todo id", id, OBSERVATION_LABEL_MAX_BYTES)?;
        }
        validate_required_text("todo text", &self.text, OBSERVATION_TODO_TEXT_MAX_BYTES)
    }

    fn json_encoded_len(&self) -> usize {
        "{\"id\":".len()
            + option_string_json_len(self.id.as_deref())
            + ",\"text\":".len()
            + json_string_len(&self.text)
            + ",\"state\":".len()
            + json_string_len(self.state.wire_name())
            + "}".len()
    }
}

impl ObservationEvidenceV1 {
    fn wire_name(self) -> &'static str {
        match self {
            Self::StructuredProvider => "structured-provider",
            Self::ManagedHook => "managed-hook",
            Self::NodeLifecycle => "node-lifecycle",
            Self::WorkspaceObservation => "workspace-observation",
            Self::HistoryProjection => "history-projection",
            Self::PtyHint => "pty-hint",
        }
    }
}

impl ObservationTodoStateV1 {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in-progress",
            Self::Completed => "completed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ObservationValidationError {
    #[error("observation source sequence must be non-zero")]
    ZeroSequence,
    #[error("observation timestamp must be non-zero when present")]
    ZeroObservedAt,
    #[error("observation gap must report at least one missed event")]
    ZeroGap,
    #[error("todo snapshot revision must be non-zero")]
    ZeroTodoRevision,
    #[error("observation field '{field}' is empty, contains controls, or exceeds {max} bytes")]
    InvalidText { field: &'static str, max: usize },
    #[error("observation contains too many {field}: {actual}; maximum is {max}")]
    TooMany {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    #[error("observation path must be a safe relative slash-separated path")]
    InvalidPath,
    #[error("PTY evidence cannot claim authoritative semantic workflow facts")]
    PtyHintClaimsAuthoritativeFact,
    #[error("history snapshots require history projection evidence")]
    HistorySnapshotRequiresHistoryProjection,
    #[error("context-window usage requires structured provider evidence")]
    ContextWindowUsageRequiresStructuredProvider,
    #[error("context-window capacity must be non-zero")]
    ZeroContextWindowCapacity,
    #[error("context-window token segments overflow u64")]
    ContextWindowSegmentsOverflow,
    #[error("context-window token segments sum to {segment_sum}, not used_tokens {used_tokens}")]
    ContextWindowSegmentsMismatch { segment_sum: u64, used_tokens: u64 },
    #[error("serialized observation is {actual} bytes; maximum is {max}")]
    EventTooLarge { max: usize, actual: usize },
}

fn validate_context_window_usage(
    uncached_input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    unattributed_tokens: u64,
    used_tokens: u64,
    capacity_tokens: u64,
) -> Result<(), ObservationValidationError> {
    if capacity_tokens == 0 {
        return Err(ObservationValidationError::ZeroContextWindowCapacity);
    }
    let segment_sum = uncached_input_tokens
        .checked_add(cache_read_tokens)
        .and_then(|sum| sum.checked_add(cache_write_tokens))
        .and_then(|sum| sum.checked_add(output_tokens))
        .and_then(|sum| sum.checked_add(unattributed_tokens))
        .ok_or(ObservationValidationError::ContextWindowSegmentsOverflow)?;
    if segment_sum != used_tokens {
        return Err(ObservationValidationError::ContextWindowSegmentsMismatch {
            segment_sum,
            used_tokens,
        });
    }
    Ok(())
}

fn validate_required_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ObservationValidationError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(ObservationValidationError::InvalidText { field, max });
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), ObservationValidationError> {
    if path.is_empty()
        || path.len() > OBSERVATION_PATH_MAX_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(ObservationValidationError::InvalidPath);
    }
    Ok(())
}

fn decimal_len(value: u64) -> usize {
    if value == 0 {
        1
    } else {
        value.ilog10() as usize + 1
    }
}

fn bool_json_len(value: bool) -> usize {
    if value { 4 } else { 5 }
}

fn option_bool_json_len(value: Option<bool>) -> usize {
    value.map(bool_json_len).unwrap_or(4)
}

fn option_u64_json_len(value: Option<u64>) -> usize {
    value.map(decimal_len).unwrap_or(4)
}

fn option_i32_json_len(value: Option<i32>) -> usize {
    value.map(|value| value.to_string().len()).unwrap_or(4)
}

fn option_string_json_len(value: Option<&str>) -> usize {
    value.map(json_string_len).unwrap_or(4)
}

fn json_string_len(value: &str) -> usize {
    2 + value.len()
        + value
            .bytes()
            .filter(|byte| matches!(byte, b'"' | b'\\'))
            .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(evidence: ObservationEvidenceV1, kind: ObservationKindV1) -> ObservationV1 {
        ObservationV1 {
            source_sequence: 1,
            observed_at_unix_ms: Some(1_786_671_234_567),
            evidence,
            kind,
            truncated: false,
        }
    }

    #[test]
    fn observation_v1_is_bounded_private_and_versioned() {
        assert_eq!(OBSERVATION_PROTOCOL_VERSION_V1, 1);
        let value = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::Error {
                detail: "bounded failure".to_string(),
            },
        );
        value.validate().expect("bounded observation");

        let encoded = serde_json::to_vec(&value).expect("serialize observation");
        assert!(encoded.len() <= OBSERVATION_EVENT_MAX_BYTES);
        assert_eq!(value.json_encoded_len(), encoded.len());
        let encoded_text = String::from_utf8(encoded.clone()).expect("JSON is UTF-8");
        assert!(!encoded_text.contains("prompt"));
        assert!(!encoded_text.contains("transcript"));
        let decoded: ObservationV1 =
            serde_json::from_slice(&encoded).expect("validated deserialize");
        assert_eq!(decoded, value);

        let invalid = serde_json::json!({
            "source_sequence": 0,
            "observed_at_unix_ms": 1,
            "evidence": "node-lifecycle",
            "kind": { "kind": "ready" },
            "truncated": false
        });
        assert!(serde_json::from_value::<ObservationV1>(invalid).is_err());

        let process = observation(
            ObservationEvidenceV1::NodeLifecycle,
            ObservationKindV1::OwnedProcessExited {
                correlation_id: "proc-0123456789abcdef".to_owned(),
                success: Some(false),
                exit_code: Some(-1),
            },
        );
        process.validate().expect("bounded owned process lifecycle");
        let encoded = serde_json::to_vec(&process).expect("serialize owned process lifecycle");
        assert_eq!(process.json_encoded_len(), encoded.len());

        assert!(ObservationKindV1::TodoSnapshot {
            revision: 1,
            items: Vec::new(),
            complete: true,
        }
        .requires_workflow_detail_capability());
        assert!(!ObservationKindV1::Working.requires_workflow_detail_capability());
    }

    #[test]
    fn source_capabilities_are_base_bounded_categorical_metadata() {
        let value = observation(
            ObservationEvidenceV1::ManagedHook,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::ManagedHook,
                source_adapter: "claude-code".to_owned(),
                capabilities: ObservationCapabilitiesV1 {
                    tools: true,
                    attention: true,
                    subagents: true,
                    ..ObservationCapabilitiesV1::default()
                },
            },
        );
        value.validate().expect("categorical source capabilities");
        assert!(!value.kind.requires_workflow_detail_capability());
        let encoded = serde_json::to_vec(&value).expect("serialize source capabilities");
        assert_eq!(value.json_encoded_len(), encoded.len());
        assert_eq!(serde_json::from_slice::<ObservationV1>(&encoded).unwrap(), value);

        let invalid = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::Pipe,
                source_adapter: "bad\nsource".to_owned(),
                capabilities: ObservationCapabilitiesV1::default(),
            },
        );
        assert!(matches!(
            invalid.validate(),
            Err(ObservationValidationError::InvalidText {
                field: "observation source adapter",
                ..
            })
        ));
    }

    #[test]
    fn context_window_usage_is_exact_bounded_private_and_serde_stable() {
        let valid = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: 70,
                cache_read_tokens: 20,
                cache_write_tokens: 0,
                output_tokens: 10,
                unattributed_tokens: 5,
                used_tokens: 105,
                capacity_tokens: 100,
            },
        );
        valid.validate().expect("over-capacity usage remains a truthful fact");
        let encoded = serde_json::to_vec(&valid).expect("serialize context usage");
        assert_eq!(valid.json_encoded_len(), encoded.len());
        assert!(encoded.len() <= OBSERVATION_EVENT_MAX_BYTES);
        assert_eq!(serde_json::from_slice::<ObservationV1>(&encoded).unwrap(), valid);
        let text = String::from_utf8(encoded).unwrap();
        for forbidden in ["prompt", "transcript", "session_id", "provider_id", "tool_input"] {
            assert!(!text.contains(forbidden));
        }

        let invalid = |kind| observation(ObservationEvidenceV1::StructuredProvider, kind);
        assert_eq!(
            invalid(ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                unattributed_tokens: 0,
                used_tokens: 1,
                capacity_tokens: 0,
            })
            .validate(),
            Err(ObservationValidationError::ZeroContextWindowCapacity)
        );
        assert_eq!(
            invalid(ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: 1,
                cache_read_tokens: 1,
                cache_write_tokens: 1,
                output_tokens: 1,
                unattributed_tokens: 1,
                used_tokens: 4,
                capacity_tokens: 1,
            })
            .validate(),
            Err(ObservationValidationError::ContextWindowSegmentsMismatch {
                segment_sum: 5,
                used_tokens: 4,
            })
        );
        assert_eq!(
            invalid(ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: u64::MAX,
                cache_read_tokens: 1,
                cache_write_tokens: 0,
                output_tokens: 0,
                unattributed_tokens: 0,
                used_tokens: u64::MAX,
                capacity_tokens: 1,
            })
            .validate(),
            Err(ObservationValidationError::ContextWindowSegmentsOverflow)
        );
    }

    #[test]
    fn context_window_usage_requires_structured_provider_for_validate_and_serde() {
        let kind = ObservationKindV1::ContextWindowUsage {
            uncached_input_tokens: 70,
            cache_read_tokens: 20,
            cache_write_tokens: 0,
            output_tokens: 10,
            unattributed_tokens: 5,
            used_tokens: 105,
            capacity_tokens: 100,
        };
        let structured = observation(
            ObservationEvidenceV1::StructuredProvider,
            kind.clone(),
        );
        structured.validate().expect("structured provider is authoritative");
        let structured_json = serde_json::to_vec(&structured).unwrap();
        assert_eq!(
            serde_json::from_slice::<ObservationV1>(&structured_json).unwrap(),
            structured
        );

        for evidence in [
            ObservationEvidenceV1::ManagedHook,
            ObservationEvidenceV1::NodeLifecycle,
            ObservationEvidenceV1::WorkspaceObservation,
            ObservationEvidenceV1::HistoryProjection,
            ObservationEvidenceV1::PtyHint,
        ] {
            let rejected = observation(evidence, kind.clone());
            assert_eq!(
                rejected.validate(),
                Err(
                    ObservationValidationError::ContextWindowUsageRequiresStructuredProvider
                ),
                "{evidence:?} must not claim exact current context"
            );
            let encoded = serde_json::to_vec(&rejected).unwrap();
            let error = serde_json::from_slice::<ObservationV1>(&encoded).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("context-window usage requires structured provider evidence"),
                "unexpected serde error for {evidence:?}: {error}"
            );
        }
    }

    #[test]
    fn history_snapshot_is_bounded_and_contains_no_content_fields() {
        let unknown_tokens = observation(
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 12,
                message_count_exact: true,
                completed_turn_count: Some(6),
                total_tokens: None,
            },
        );
        unknown_tokens.validate().expect("bounded history snapshot");
        let encoded = serde_json::to_vec(&unknown_tokens).expect("serialize history snapshot");
        assert_eq!(unknown_tokens.json_encoded_len(), encoded.len());
        assert!(encoded.len() <= OBSERVATION_EVENT_MAX_BYTES);

        let value = serde_json::from_slice::<serde_json::Value>(&encoded).unwrap();
        let history = value["kind"].as_object().unwrap();
        let keys = history.keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "completed_turn_count",
                "kind",
                "message_count",
                "message_count_exact",
                "total_tokens",
            ]
        );
        for forbidden in [
            "text",
            "prompt",
            "transcript",
            "path",
            "cwd",
            "session_id",
            "provider_id",
            "tool_input",
            "tool_output",
        ] {
            assert!(history.get(forbidden).is_none());
        }

        let observed_zero = observation(
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 0,
                message_count_exact: false,
                completed_turn_count: None,
                total_tokens: Some(0),
            },
        );
        observed_zero.validate().expect("observed zero is factual");
        assert_ne!(
            serde_json::to_value(&unknown_tokens).unwrap()["kind"]["total_tokens"],
            serde_json::to_value(&observed_zero).unwrap()["kind"]["total_tokens"]
        );

        assert_eq!(
            observation(
                ObservationEvidenceV1::StructuredProvider,
                ObservationKindV1::HistorySnapshot {
                    message_count: 1,
                    message_count_exact: true,
                    completed_turn_count: None,
                    total_tokens: None,
                },
            )
            .validate(),
            Err(ObservationValidationError::HistorySnapshotRequiresHistoryProjection)
        );
    }

    #[test]
    fn pty_hint_cannot_claim_authoritative_workflow_facts() {
        let rejected = [
            ObservationKindV1::TurnCompleted,
            ObservationKindV1::ToolCompleted {
                correlation_id: "tool-0123456789abcdef".to_string(),
                class: "command".to_string(),
                success: true,
                duration_ms: Some(5),
            },
            ObservationKindV1::SubagentCompleted {
                correlation_id: "child-1".to_string(),
                success: Some(true),
            },
            ObservationKindV1::TodoSnapshot {
                revision: 1,
                items: Vec::new(),
                complete: true,
            },
            ObservationKindV1::FileChanged {
                path: Some("src/lib.rs".to_string()),
            },
            ObservationKindV1::HistorySnapshot {
                message_count: 1,
                message_count_exact: true,
                completed_turn_count: None,
                total_tokens: None,
            },
            ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                unattributed_tokens: 0,
                used_tokens: 1,
                capacity_tokens: 1,
            },
        ];

        for kind in rejected {
            assert_eq!(
                observation(ObservationEvidenceV1::PtyHint, kind).validate(),
                Err(ObservationValidationError::PtyHintClaimsAuthoritativeFact)
            );
        }
        observation(ObservationEvidenceV1::PtyHint, ObservationKindV1::Working)
            .validate()
            .expect("activity hint is non-authoritative");
    }

    #[test]
    fn todo_snapshot_rejects_unsafe_or_oversize_content() {
        let zero_revision = observation(
            ObservationEvidenceV1::ManagedHook,
            ObservationKindV1::TodoSnapshot {
                revision: 0,
                items: Vec::new(),
                complete: true,
            },
        );
        assert_eq!(
            zero_revision.validate(),
            Err(ObservationValidationError::ZeroTodoRevision)
        );

        let unsafe_text = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::TodoSnapshot {
                revision: 1,
                items: vec![ObservationTodoItemV1 {
                    id: Some("todo-1".to_string()),
                    text: "unsafe\u{1b}text".to_string(),
                    state: ObservationTodoStateV1::Pending,
                }],
                complete: true,
            },
        );
        assert!(matches!(
            unsafe_text.validate(),
            Err(ObservationValidationError::InvalidText {
                field: "todo text",
                ..
            })
        ));

        let too_many = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::TodoSnapshot {
                revision: 2,
                items: (0..=OBSERVATION_TODO_ITEMS_MAX)
                    .map(|index| ObservationTodoItemV1 {
                        id: Some(format!("todo-{index}")),
                        text: "bounded".to_string(),
                        state: ObservationTodoStateV1::Unknown,
                    })
                    .collect(),
                complete: true,
            },
        );
        assert!(matches!(
            too_many.validate(),
            Err(ObservationValidationError::TooMany {
                field: "todo items",
                ..
            })
        ));

        let oversized_event = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::TodoSnapshot {
                revision: 3,
                items: (0..20)
                    .map(|index| ObservationTodoItemV1 {
                        id: Some(format!("todo-{index}")),
                        text: "x".repeat(OBSERVATION_TODO_TEXT_MAX_BYTES),
                        state: ObservationTodoStateV1::InProgress,
                    })
                    .collect(),
                complete: false,
            },
        );
        assert!(matches!(
            oversized_event.validate(),
            Err(ObservationValidationError::EventTooLarge { .. })
        ));

        let unsafe_path = observation(
            ObservationEvidenceV1::WorkspaceObservation,
            ObservationKindV1::FileChanged {
                path: Some("src/../secret".to_string()),
            },
        );
        assert_eq!(
            unsafe_path.validate(),
            Err(ObservationValidationError::InvalidPath)
        );
    }
}
