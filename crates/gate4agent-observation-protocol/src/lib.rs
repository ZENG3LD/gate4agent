//! Dependency-light, versioned observations for provider-session monitoring.
//!
//! Observations carry bounded, allowlisted workflow facts. They never carry
//! prompts, transcript text, raw tool input/output, credentials, or provider
//! configuration. Consumers must still treat the evidence source as part of
//! the fact: terminal hints cannot authoritatively claim semantic workflow
//! completion.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const OBSERVATION_EVENT_MAX_BYTES: usize = 4_096;
pub const OBSERVATION_LABEL_MAX_BYTES: usize = 64;
pub const OBSERVATION_DETAIL_MAX_BYTES: usize = 1_024;
pub const OBSERVATION_TODO_ITEMS_MAX: usize = 64;
pub const OBSERVATION_TODO_TEXT_MAX_BYTES: usize = 256;
pub const OBSERVATION_PATH_MAX_BYTES: usize = 1_024;
pub const OBSERVATION_COLLECTION_MAX: usize = 128;
/// Max bytes for `ObservationKindV1::ActionBlocked::reason` -- the
/// authority's own refusal sentence (e.g. "blocked by dangerous-command
/// gate: rule=…, argument=…", or a provider's "Reason: Blocked by
/// classifier."). Verbatim and bounded, never summarised -- see
/// [`truncate_observation_text`] for the producer-side cut this bound backs.
pub const OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES: usize = 1_024;
/// Max bytes for `ObservationKindV1::ActionBlocked::help` -- the guidance
/// tail a CLI attaches after naming the block (e.g. "add a Bash permission
/// rule"). Larger than `reason` because guidance text runs longer than a
/// one-line refusal.
pub const OBSERVATION_ACTION_BLOCKED_HELP_MAX_BYTES: usize = 2_048;

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

/// WHO decided a host request the agent sent to the ACP host -- see
/// [`ObservationKindV1::HostRequestObserved`]. Mirrors `gate4agent`'s own
/// `HostDecisionAuthority` one-for-one; this crate is dependency-light and
/// keeps its own copy rather than depending on `gate4agent`, the same
/// convention every other `*V1` wire type here follows.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostDecisionAuthorityV1 {
    /// The dangerous-command gate forced this outcome ahead of the host's
    /// policy -- `terminal/create` and `execute`-kind `session/request_
    /// permission` only. Always a denial.
    Gate,
    /// The host's policy decided the request the instant it arrived -- the
    /// default path for every request that is neither gate-blocked nor
    /// deferred.
    Policy,
    /// An operator answered a `session/request_permission` call that had
    /// been left `HostRequestDecisionV1::Deferred`.
    Operator,
    /// A `session/request_permission` call left `HostRequestDecisionV1::
    /// Deferred` reached its deadline with no operator answer, so the
    /// host's policy -- the SAME policy that would have answered it
    /// immediately had deferral never been enabled -- decided it instead.
    /// Deliberately its own variant rather than `Policy`: folding it in
    /// would erase the fact that an operator was asked first and nobody
    /// answered in time. Equally deliberately not `Operator`: no human
    /// made this choice.
    DeadlinePolicy,
}

/// WHO or WHAT blocked an action -- see [`ObservationKindV1::ActionBlocked`].
/// A strictly wider vocabulary than [`HostDecisionAuthorityV1`]: the harness
/// itself only ever produces `HarnessGate`/`HarnessPolicy`/`HarnessDeadline`/
/// `Operator` (see that type's own variants, which this one mirrors
/// one-for-one for the harness's own four), but a block can also come from
/// the PROVIDER side of the wire -- a classifier, a permission rule, a
/// sandbox, a plain refusal, a hook, or the user declining inside the
/// provider's own UI -- none of which `HostDecisionAuthorityV1` has any
/// vocabulary for at all. `Unknown` is the evidence-gated fallback: see
/// `ObservationV1::validate`'s pty-hint rule, the only evidence class this
/// crate lets claim it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockAuthorityV1 {
    /// The dangerous-command gate -- mirrors [`HostDecisionAuthorityV1::
    /// Gate`].
    HarnessGate,
    /// The harness's own host policy -- mirrors [`HostDecisionAuthorityV1::
    /// Policy`].
    HarnessPolicy,
    /// A deferred request the harness's policy decided after no operator
    /// answered in time -- mirrors [`HostDecisionAuthorityV1::
    /// DeadlinePolicy`].
    HarnessDeadline,
    /// An operator explicitly declined -- mirrors [`HostDecisionAuthorityV1::
    /// Operator`]. The one variant this type shares by name, not just by
    /// meaning, with `HostDecisionAuthorityV1`.
    Operator,
    /// The provider's own auto-mode classifier (e.g. Claude Code's auto
    /// mode).
    ProviderClassifier,
    /// A provider-side permission rule (an allow/deny list entry) refused
    /// the call, distinct from a classifier's runtime judgement.
    ProviderPermissionRule,
    /// The provider's sandbox refused the call (a filesystem or network
    /// boundary the sandbox itself enforces, not a policy decision).
    ProviderSandbox,
    /// The provider declined with a `stopReason: "refusal"` or equivalent,
    /// carrying no more specific typed reason than "the provider itself said
    /// no".
    ProviderRefusal,
    /// A provider-managed hook (e.g. Claude Code's `PermissionDenied` hook)
    /// reported the block.
    ProviderHook,
    /// A human declined inside the provider's OWN UI/CLI (not this
    /// harness's operator surface) -- the provider's own `user-rejected`-
    /// shaped outcome.
    UserRejected,
    /// The provider's own account/plan quota, rate limit, or usage cap was
    /// exhausted -- e.g. codex-acp's `session/prompt` RPC error carrying
    /// `data.codexErrorInfo: "usageLimitExceeded"` (measured live
    /// 2026-09-05). `reason_kind` on the sibling `ActionBlocked` carries
    /// the provider's own vendor code for this (`"usageLimitExceeded"`,
    /// `"rate_limit"`, ...) -- this variant only says WHO/WHAT the
    /// authority is, never a second copy of the code itself. Distinct from
    /// `ProviderRefusal`: a quota exhaustion is an account/plan state, not
    /// the model declining the specific request. A new unit variant on a
    /// closed, exact-version-negotiated wire enum needs no `#[serde(default)]`
    /// fallback -- there is no older payload that could ever carry it, and
    /// `NODE_PROTOCOL_VERSION`'s exact-match negotiation means an older
    /// binary that does not know this variant never receives one in the
    /// first place.
    ProviderQuota,
    /// No typed field named who blocked it -- text matching may still have
    /// filled `reason`/`help`, but never this field; see the module doc
    /// comment's rule that authority is never guessed.
    Unknown,
}

/// A typed answer to "what happened to this host request" -- see
/// [`ObservationKindV1::HostRequestObserved`]. Mirrors `gate4agent`'s own
/// `HostRequestDecision` one-for-one; see [`HostDecisionAuthorityV1`]'s doc
/// comment for why this crate keeps its own copy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HostRequestDecisionV1 {
    /// The request was allowed. `by` is who made that call.
    Granted { by: HostDecisionAuthorityV1 },
    /// The request was refused. `by` is who made that call.
    Denied { by: HostDecisionAuthorityV1 },
    /// The request has arrived and been recorded, but nothing has decided
    /// it yet. A later `ObservationKindV1::HostRequestObserved` reports the
    /// eventual `Granted`/`Denied` outcome once one exists.
    Deferred,
}

/// Whether a `Granted` host request's underlying operation actually ran
/// without an I/O or execution problem -- see [`ObservationKindV1::
/// HostRequestObserved`]. Mirrors `gate4agent`'s own `HostRequestOutcome`
/// one-for-one; see [`HostDecisionAuthorityV1`]'s doc comment for why this
/// crate keeps its own copy rather than importing it.
///
/// Only meaningful paired with `HostRequestDecisionV1::Granted`: a `Denied`
/// or `Deferred` request never attempted its underlying operation, so it is
/// always `Executed` there for lack of anything to have failed running.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HostRequestOutcomeV1 {
    /// No execution problem -- either the request ran cleanly, or (for
    /// `Denied`/`Deferred`) no execution was ever attempted to fail.
    Executed,
    /// The request was authorized but failed while running it. `error` is
    /// the bounded underlying I/O/execution failure text (e.g. an OS error
    /// from a spawn call) -- never a policy/gate refusal message, which
    /// stays on `ObservationKindV1::HostRequestObserved`'s own audit trail
    /// via the sibling `ActionBlocked` observation instead. This is NEVER a
    /// `Denied` decision in disguise: a gate/policy/deadline refusal is
    /// `HostRequestDecisionV1::Denied`, never `Granted` paired with
    /// `Failed` -- the two answer different questions ("was this allowed"
    /// versus "did doing it succeed") and must never be folded into one.
    Failed { error: String },
}

impl Default for HostRequestOutcomeV1 {
    /// Backs `ObservationKindV1::HostRequestObserved::outcome`'s
    /// `#[serde(default)]` -- a durable record written before this field
    /// existed (pre-K1d) never recorded an execution failure at all, so an
    /// observed `HostRequestObserved` from that era always meant the
    /// request ran; see that field's own doc comment.
    fn default() -> Self {
        Self::Executed
    }
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
        /// Cache/segment breakdown of `used_tokens`. `None` when the
        /// source only reported `used_tokens`/`capacity_tokens` and never
        /// observed how the total decomposes -- see `ProviderEvent::
        /// UsageUpdated`. Never defaulted to zero: a zero would assert a
        /// segment was observed to be empty when it was never observed at
        /// all.
        uncached_input_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        output_tokens: Option<u64>,
        unattributed_tokens: Option<u64>,
        used_tokens: u64,
        capacity_tokens: u64,
    },
    RateLimited,
    /// The agent asked the ACP host for something (`session/request_
    /// permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/
    /// create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/
    /// kill`, `terminal/release`). `class` is a coarse bucket of the ACP
    /// method requested -- the same scale as `ToolStarted::class` -- never
    /// the raw request parameters: what path was read or what command ran
    /// is never carried on this wire. `decision` is `HostRequestDecisionV1::
    /// Deferred` when the request has arrived but nothing has decided it
    /// yet (only reachable for `session/request_permission`); a later
    /// observation for the same underlying request reports the eventual
    /// `Granted`/`Denied` outcome once one exists.
    HostRequestObserved {
        class: String,
        decision: HostRequestDecisionV1,
        /// Whether an authorized (`Granted`) request actually ran cleanly
        /// or failed doing so -- see [`HostRequestOutcomeV1`]'s own doc
        /// comment for why this is a separate field from `decision` rather
        /// than a third flavor of `Denied`. Always `Executed` for `Denied`/
        /// `Deferred` (nothing ran to fail). `#[serde(default)]` reads a
        /// durable record written before this field existed (K1d) as
        /// `Executed` -- honest, since before this field existed an
        /// observed `HostRequestObserved` implied a `Granted` request had
        /// run, and there was nothing else it could have meant.
        #[serde(default)]
        outcome: HostRequestOutcomeV1,
    },
    /// Something the agent tried was blocked mid-turn -- by this harness's
    /// own gate/policy/deadline, by an operator, or by the provider itself
    /// (a classifier, a permission rule, a sandbox, a plain refusal, a hook,
    /// or the provider's own user-rejected outcome). `correlation_id` is the
    /// tool call this block belongs to, when there is one, using the SAME
    /// opaque id scheme `ToolStarted::correlation_id` does -- `None` when the
    /// block has no tool call to correlate against yet (an operator/policy
    /// denial today never does; a provider-side block wired up later may).
    /// `tool_class` is the same coarse bucket `ToolStarted::class`/
    /// `HostRequestObserved::class` use, never the raw command. `authority`
    /// is read off a typed field or this harness's own decision, NEVER
    /// guessed from text -- see [`BlockAuthorityV1`]'s doc comment.
    /// `reason_kind` is the source's own machine code for why (ACP's
    /// `nonExecutionKind`, a provider's `decision_reason_type`, a
    /// `stopReason`), when one exists. `reason` and `help` are bounded,
    /// verbatim free text -- never summarised or rewritten -- and are a
    /// SEPARATE contract from `Error::detail`, which stays a categorical
    /// slug; see this crate's module doc comment for why a sentence must
    /// never travel through that field.
    ActionBlocked {
        correlation_id: Option<String>,
        tool_class: String,
        authority: BlockAuthorityV1,
        reason_kind: Option<String>,
        reason: String,
        help: Option<String>,
    },
    /// A JSON-RPC notification the reader received but could not classify
    /// into any other kind here -- the protocol said something this build
    /// does not parse. `method` is the bare JSON-RPC method string (a
    /// small, protocol-defined vocabulary), never the notification's own
    /// payload.
    UnrecognizedNotification {
        method: String,
    },
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
        if let ObservationKindV1::ActionBlocked { authority, .. } = &self.kind {
            if self.evidence == ObservationEvidenceV1::PtyHint
                && *authority != BlockAuthorityV1::Unknown
            {
                return Err(
                    ObservationValidationError::PtyHintActionBlockedRequiresUnknownAuthority,
                );
            }
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
            Self::HostRequestObserved { class, outcome, .. } => {
                validate_required_text("host request class", class, OBSERVATION_LABEL_MAX_BYTES)?;
                if let HostRequestOutcomeV1::Failed { error } = outcome {
                    validate_required_text(
                        "host request outcome error",
                        error,
                        OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES,
                    )?;
                }
                Ok(())
            }
            Self::ActionBlocked {
                correlation_id,
                tool_class,
                reason_kind,
                reason,
                help,
                ..
            } => {
                if let Some(correlation_id) = correlation_id {
                    validate_required_text(
                        "action blocked correlation id",
                        correlation_id,
                        OBSERVATION_LABEL_MAX_BYTES,
                    )?;
                }
                validate_required_text(
                    "action blocked tool class",
                    tool_class,
                    OBSERVATION_LABEL_MAX_BYTES,
                )?;
                if let Some(reason_kind) = reason_kind {
                    validate_required_text(
                        "action blocked reason kind",
                        reason_kind,
                        OBSERVATION_LABEL_MAX_BYTES,
                    )?;
                }
                validate_required_text(
                    "action blocked reason",
                    reason,
                    OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES,
                )?;
                if let Some(help) = help {
                    validate_required_text(
                        "action blocked help",
                        help,
                        OBSERVATION_ACTION_BLOCKED_HELP_MAX_BYTES,
                    )?;
                }
                Ok(())
            }
            Self::UnrecognizedNotification { method } => validate_required_text(
                "unrecognized notification method",
                method,
                OBSERVATION_LABEL_MAX_BYTES,
            ),
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
                len +=
                    ",\"uncached_input_tokens\":".len() + option_u64_json_len(*uncached_input_tokens);
                len += ",\"cache_read_tokens\":".len() + option_u64_json_len(*cache_read_tokens);
                len += ",\"cache_write_tokens\":".len() + option_u64_json_len(*cache_write_tokens);
                len += ",\"output_tokens\":".len() + option_u64_json_len(*output_tokens);
                len += ",\"unattributed_tokens\":".len() + option_u64_json_len(*unattributed_tokens);
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
            Self::HostRequestObserved { class, decision, outcome } => {
                len += ",\"class\":".len() + json_string_len(class);
                len += ",\"decision\":".len() + decision.json_encoded_len();
                len += ",\"outcome\":".len() + outcome.json_encoded_len();
            }
            Self::ActionBlocked {
                correlation_id,
                tool_class,
                authority,
                reason_kind,
                reason,
                help,
            } => {
                len += ",\"correlation_id\":".len() + option_string_json_len(correlation_id.as_deref());
                len += ",\"tool_class\":".len() + json_string_len(tool_class);
                len += ",\"authority\":".len() + json_string_len(authority.wire_name());
                len += ",\"reason_kind\":".len() + option_string_json_len(reason_kind.as_deref());
                len += ",\"reason\":".len() + json_string_len(reason);
                len += ",\"help\":".len() + option_string_json_len(help.as_deref());
            }
            Self::UnrecognizedNotification { method } => {
                len += ",\"method\":".len() + json_string_len(method);
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
            Self::HostRequestObserved { .. } => "host-request-observed",
            Self::ActionBlocked { .. } => "action-blocked",
            Self::UnrecognizedNotification { .. } => "unrecognized-notification",
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

impl HostDecisionAuthorityV1 {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Policy => "policy",
            Self::Operator => "operator",
            Self::DeadlinePolicy => "deadline-policy",
        }
    }
}

impl BlockAuthorityV1 {
    fn wire_name(self) -> &'static str {
        match self {
            Self::HarnessGate => "harness-gate",
            Self::HarnessPolicy => "harness-policy",
            Self::HarnessDeadline => "harness-deadline",
            Self::Operator => "operator",
            Self::ProviderClassifier => "provider-classifier",
            Self::ProviderPermissionRule => "provider-permission-rule",
            Self::ProviderSandbox => "provider-sandbox",
            Self::ProviderRefusal => "provider-refusal",
            Self::ProviderHook => "provider-hook",
            Self::UserRejected => "user-rejected",
            Self::ProviderQuota => "provider-quota",
            Self::Unknown => "unknown",
        }
    }
}

impl HostRequestDecisionV1 {
    fn wire_name(&self) -> &'static str {
        match self {
            Self::Granted { .. } => "granted",
            Self::Denied { .. } => "denied",
            Self::Deferred => "deferred",
        }
    }

    fn json_encoded_len(&self) -> usize {
        let mut len = "{\"kind\":".len() + json_string_len(self.wire_name());
        if let Self::Granted { by } | Self::Denied { by } = self {
            len += ",\"by\":".len() + json_string_len(by.wire_name());
        }
        len + "}".len()
    }
}

impl HostRequestOutcomeV1 {
    fn wire_name(&self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::Failed { .. } => "failed",
        }
    }

    fn json_encoded_len(&self) -> usize {
        let mut len = "{\"kind\":".len() + json_string_len(self.wire_name());
        if let Self::Failed { error } = self {
            len += ",\"error\":".len() + json_string_len(error);
        }
        len + "}".len()
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
    #[error("PTY-hint action-blocked observations may only assert authority 'unknown'")]
    PtyHintActionBlockedRequiresUnknownAuthority,
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

/// The five breakdown segments are `Option<u64>` -- a source that only
/// observed `used_tokens`/`capacity_tokens` (see `ObservationKindV1::
/// ContextWindowUsage`'s own doc) reports them as `None`. The segment-sum
/// check below only runs when EVERY segment was observed: a partially
/// unknown breakdown has nothing sound to compare against `used_tokens`,
/// and treating a missing segment as zero would assert it was observed
/// to be empty rather than never measured.
fn validate_context_window_usage(
    uncached_input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    output_tokens: Option<u64>,
    unattributed_tokens: Option<u64>,
    used_tokens: u64,
    capacity_tokens: u64,
) -> Result<(), ObservationValidationError> {
    if capacity_tokens == 0 {
        return Err(ObservationValidationError::ZeroContextWindowCapacity);
    }
    if let (
        Some(uncached_input_tokens),
        Some(cache_read_tokens),
        Some(cache_write_tokens),
        Some(output_tokens),
        Some(unattributed_tokens),
    ) = (
        uncached_input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        output_tokens,
        unattributed_tokens,
    ) {
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

/// Cuts `value` to at most `max_bytes`, backing off to the nearest UTF-8
/// character boundary at or before that limit -- never inside a multi-byte
/// sequence. Returns `(text, true)` when a cut was made, `(text, false)`
/// when `value` already fit. This is the one honest way to bound
/// `ActionBlocked::reason`/`help` without rewriting them: the byte limit is
/// enforced here, at the producer, BEFORE `ObservationV1::validate` ever
/// sees the text -- that validator rejects an oversize value outright (see
/// `validate_required_text`), it does not truncate. A caller that cuts text
/// with this function is responsible for setting the sibling
/// `ObservationV1::truncated` from the returned bool; this function only
/// makes the cut safe, it does not know how to report having made it.
pub fn truncate_observation_text(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
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
                uncached_input_tokens: Some(70),
                cache_read_tokens: Some(20),
                cache_write_tokens: Some(0),
                output_tokens: Some(10),
                unattributed_tokens: Some(5),
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
                uncached_input_tokens: Some(1),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                output_tokens: Some(0),
                unattributed_tokens: Some(0),
                used_tokens: 1,
                capacity_tokens: 0,
            })
            .validate(),
            Err(ObservationValidationError::ZeroContextWindowCapacity)
        );
        assert_eq!(
            invalid(ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: Some(1),
                cache_read_tokens: Some(1),
                cache_write_tokens: Some(1),
                output_tokens: Some(1),
                unattributed_tokens: Some(1),
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
                uncached_input_tokens: Some(u64::MAX),
                cache_read_tokens: Some(1),
                cache_write_tokens: Some(0),
                output_tokens: Some(0),
                unattributed_tokens: Some(0),
                used_tokens: u64::MAX,
                capacity_tokens: 1,
            })
            .validate(),
            Err(ObservationValidationError::ContextWindowSegmentsOverflow)
        );
    }

    #[test]
    fn context_window_usage_with_unobserved_breakdown_skips_the_segment_check() {
        // `ProviderEvent::UsageUpdated` only ever carries `used_tokens`/
        // `context_window` -- the five breakdown segments arrive as `None`
        // and the segment-sum check does not run against them: there is
        // nothing sound to compare, and defaulting a missing segment to
        // zero would assert it was observed to be empty rather than never
        // measured at all.
        let unobserved = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::ContextWindowUsage {
                uncached_input_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                output_tokens: None,
                unattributed_tokens: None,
                used_tokens: 12_345,
                capacity_tokens: 200_000,
            },
        );
        unobserved
            .validate()
            .expect("used/capacity alone is still a truthful fact");
        let encoded = serde_json::to_vec(&unobserved).expect("serialize unobserved usage");
        assert_eq!(unobserved.json_encoded_len(), encoded.len());
        assert_eq!(
            serde_json::from_slice::<ObservationV1>(&encoded).unwrap(),
            unobserved
        );

        assert_eq!(
            observation(
                ObservationEvidenceV1::StructuredProvider,
                ObservationKindV1::ContextWindowUsage {
                    uncached_input_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    output_tokens: None,
                    unattributed_tokens: None,
                    used_tokens: 1,
                    capacity_tokens: 0,
                },
            )
            .validate(),
            Err(ObservationValidationError::ZeroContextWindowCapacity),
            "capacity is still required even with an unobserved breakdown"
        );
    }

    #[test]
    fn context_window_usage_requires_structured_provider_for_validate_and_serde() {
        let kind = ObservationKindV1::ContextWindowUsage {
            uncached_input_tokens: Some(70),
            cache_read_tokens: Some(20),
            cache_write_tokens: Some(0),
            output_tokens: Some(10),
            unattributed_tokens: Some(5),
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

    /// A `Granted` decision paired with a `Failed` outcome (an authorized
    /// host request that failed WHILE EXECUTING, e.g. a `terminal/create`
    /// spawn error) is a distinct, bounded, serde-stable fact -- never
    /// collapsed into `Denied`, and its `error` text is bounded the same
    /// way `ActionBlocked::reason` is.
    #[test]
    fn host_request_observed_carries_a_bounded_failed_outcome() {
        let value = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::HostRequestObserved {
                class: "Terminal".to_owned(),
                decision: HostRequestDecisionV1::Granted { by: HostDecisionAuthorityV1::Policy },
                outcome: HostRequestOutcomeV1::Failed {
                    error: "terminal/create spawn failed: os error 3".to_owned(),
                },
            },
        );
        value.validate().expect("bounded granted-but-failed host request");
        let encoded = serde_json::to_vec(&value).expect("serialize host request observed");
        assert_eq!(value.json_encoded_len(), encoded.len());
        assert_eq!(serde_json::from_slice::<ObservationV1>(&encoded).unwrap(), value);

        let executed = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::HostRequestObserved {
                class: "Terminal".to_owned(),
                decision: HostRequestDecisionV1::Granted { by: HostDecisionAuthorityV1::Policy },
                outcome: HostRequestOutcomeV1::Executed,
            },
        );
        executed.validate().expect("bounded granted-and-executed host request");
        let encoded_executed = serde_json::to_vec(&executed).expect("serialize executed outcome");
        assert_eq!(executed.json_encoded_len(), encoded_executed.len());

        let oversized_error = "e".repeat(OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES + 1);
        let rejected = observation(
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::HostRequestObserved {
                class: "Terminal".to_owned(),
                decision: HostRequestDecisionV1::Granted { by: HostDecisionAuthorityV1::Policy },
                outcome: HostRequestOutcomeV1::Failed { error: oversized_error },
            },
        );
        assert_eq!(
            rejected.validate(),
            Err(ObservationValidationError::InvalidText {
                field: "host request outcome error",
                max: OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES,
            })
        );
    }

    /// A `host-request-observed` record written before `outcome` existed
    /// (pre-K1d) -- the exact shape the durable observation checkpoint
    /// holds for every such record minted before this field shipped --
    /// carries no `outcome` field at all. `#[serde(default)]` must still
    /// read it back, as `HostRequestOutcomeV1::Executed`: before this field
    /// existed, an observed `HostRequestObserved` always meant a `Granted`
    /// request ran, so that is the only honest default for the old shape.
    /// This is the forward migration a new required field on a persisted
    /// type owes its old data -- see the crate's own contract for the rule.
    #[test]
    fn host_request_observed_outcome_defaults_to_executed_for_pre_k1d_json() {
        let pre_k1d = serde_json::json!({
            "source_sequence": 1,
            "observed_at_unix_ms": 1_786_671_234_567u64,
            "evidence": "structured-provider",
            "kind": {
                "kind": "host-request-observed",
                "class": "Terminal",
                "decision": { "kind": "granted", "by": "policy" }
            },
            "truncated": false
        });
        let decoded: ObservationV1 = serde_json::from_value(pre_k1d)
            .expect("pre-K1d shape without outcome must still deserialize");
        assert!(matches!(
            decoded.kind,
            ObservationKindV1::HostRequestObserved {
                decision: HostRequestDecisionV1::Granted {
                    by: HostDecisionAuthorityV1::Policy
                },
                outcome: HostRequestOutcomeV1::Executed,
                ..
            }
        ));
        decoded.validate().expect("defaulted outcome remains a valid observation");
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

    /// A `PtyHint` may report activity and nothing else: every workflow
    /// fact below is refused for it.
    ///
    /// Paired with the error each one actually produces, rather than
    /// asserting a single blanket verdict over the whole list. The blanket
    /// form was wrong and had been failing: `ContextWindowUsage` is checked
    /// by a NARROWER rule that runs first and demands
    /// `StructuredProvider` specifically, so it never reaches the
    /// pty-hint rule at all. Rejection was never in doubt -- the fixture
    /// was refused the whole time -- but a test that cannot say WHICH rule
    /// refused it is not testing that rule, and it went red rather than
    /// telling anyone the ordering had changed.
    ///
    /// Written as pairs, the ordering itself is now the thing under test:
    /// move the context-window check after the pty-hint one and this fails,
    /// which is the correct outcome, because the more specific diagnosis is
    /// the one an operator should get.
    #[test]
    fn pty_hint_cannot_claim_authoritative_workflow_facts() {
        let rejected = [
            (
                ObservationKindV1::TurnCompleted,
                ObservationValidationError::PtyHintClaimsAuthoritativeFact,
            ),
            (
                ObservationKindV1::ToolCompleted {
                    correlation_id: "tool-0123456789abcdef".to_string(),
                    class: "command".to_string(),
                    success: true,
                    duration_ms: Some(5),
                },
                ObservationValidationError::PtyHintClaimsAuthoritativeFact,
            ),
            (
                ObservationKindV1::SubagentCompleted {
                    correlation_id: "child-1".to_string(),
                    success: Some(true),
                },
                ObservationValidationError::PtyHintClaimsAuthoritativeFact,
            ),
            (
                ObservationKindV1::TodoSnapshot {
                    revision: 1,
                    items: Vec::new(),
                    complete: true,
                },
                ObservationValidationError::PtyHintClaimsAuthoritativeFact,
            ),
            (
                ObservationKindV1::FileChanged {
                    path: Some("src/lib.rs".to_string()),
                },
                ObservationValidationError::PtyHintClaimsAuthoritativeFact,
            ),
            (
                ObservationKindV1::HistorySnapshot {
                    message_count: 1,
                    message_count_exact: true,
                    completed_turn_count: None,
                    total_tokens: None,
                },
                ObservationValidationError::PtyHintClaimsAuthoritativeFact,
            ),
            (
                // The one that is NOT refused by the pty-hint rule: its own
                // check sits earlier and admits only `StructuredProvider`,
                // so a pty hint is turned away before evidence class is
                // even considered as a general question.
                ObservationKindV1::ContextWindowUsage {
                    uncached_input_tokens: Some(1),
                    cache_read_tokens: Some(0),
                    cache_write_tokens: Some(0),
                    output_tokens: Some(0),
                    unattributed_tokens: Some(0),
                    used_tokens: 1,
                    capacity_tokens: 1,
                },
                ObservationValidationError::ContextWindowUsageRequiresStructuredProvider,
            ),
        ];

        for (kind, expected) in rejected {
            assert_eq!(
                observation(ObservationEvidenceV1::PtyHint, kind).validate(),
                Err(expected),
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

    fn action_blocked(authority: BlockAuthorityV1, reason: &str) -> ObservationKindV1 {
        ObservationKindV1::ActionBlocked {
            correlation_id: Some("tool-0123456789abcdef".to_string()),
            tool_class: "Bash".to_string(),
            authority,
            reason_kind: Some("nonExecutionKind".to_string()),
            reason: reason.to_string(),
            help: Some("add a Bash permission rule".to_string()),
        }
    }

    /// A `StructuredProvider` block may assert ANY authority, including the
    /// provider-side ones `HostDecisionAuthorityV1` has no vocabulary for at
    /// all -- the fixture from the owner's sample: a classifier block naming
    /// its own reason and help text.
    #[test]
    fn action_blocked_structured_provider_accepts_any_authority() {
        for authority in [
            BlockAuthorityV1::HarnessGate,
            BlockAuthorityV1::HarnessPolicy,
            BlockAuthorityV1::HarnessDeadline,
            BlockAuthorityV1::Operator,
            BlockAuthorityV1::ProviderClassifier,
            BlockAuthorityV1::ProviderPermissionRule,
            BlockAuthorityV1::ProviderSandbox,
            BlockAuthorityV1::ProviderRefusal,
            BlockAuthorityV1::ProviderHook,
            BlockAuthorityV1::UserRejected,
            BlockAuthorityV1::ProviderQuota,
            BlockAuthorityV1::Unknown,
        ] {
            let value = observation(
                ObservationEvidenceV1::StructuredProvider,
                action_blocked(authority, "denied by the Claude Code auto mode classifier"),
            );
            value
                .validate()
                .unwrap_or_else(|error| panic!("{authority:?} must be accepted: {error}"));
            let encoded = serde_json::to_vec(&value).expect("serialize action-blocked");
            assert_eq!(value.json_encoded_len(), encoded.len());
            assert!(encoded.len() <= OBSERVATION_EVENT_MAX_BYTES);
            assert_eq!(serde_json::from_slice::<ObservationV1>(&encoded).unwrap(), value);
        }
    }

    /// A `PtyHint` block may only ever assert `authority: Unknown` -- every
    /// other authority is refused, by the SAME rule the module doc comment
    /// describes: text matching may fill `reason`/`help`, never `authority`.
    #[test]
    fn action_blocked_pty_hint_requires_unknown_authority() {
        let refused = [
            BlockAuthorityV1::HarnessGate,
            BlockAuthorityV1::HarnessPolicy,
            BlockAuthorityV1::HarnessDeadline,
            BlockAuthorityV1::Operator,
            BlockAuthorityV1::ProviderClassifier,
            BlockAuthorityV1::ProviderPermissionRule,
            BlockAuthorityV1::ProviderSandbox,
            BlockAuthorityV1::ProviderRefusal,
            BlockAuthorityV1::ProviderHook,
            BlockAuthorityV1::UserRejected,
            BlockAuthorityV1::ProviderQuota,
        ];
        for authority in refused {
            let rejected = observation(
                ObservationEvidenceV1::PtyHint,
                action_blocked(authority, "bash denied by auto mode"),
            );
            assert_eq!(
                rejected.validate(),
                Err(ObservationValidationError::PtyHintActionBlockedRequiresUnknownAuthority),
                "{authority:?} must be refused under pty-hint evidence"
            );
        }

        let accepted = observation(
            ObservationEvidenceV1::PtyHint,
            action_blocked(BlockAuthorityV1::Unknown, "bash denied by auto mode"),
        );
        accepted
            .validate()
            .expect("pty-hint may assert Unknown authority");
    }

    /// `reason`/`help` are bounded and verbatim -- `validate` REJECTS an
    /// over-long value outright rather than silently cutting it (the same
    /// convention every other bounded text field in this crate follows), so
    /// the producer must cut with `truncate_observation_text` before
    /// minting. That helper cuts at a safe UTF-8 boundary and reports the
    /// cut so the producer can mark `ObservationV1::truncated`.
    #[test]
    fn action_blocked_reason_is_bounded_and_truncation_is_producer_side() {
        let oversized_reason = "r".repeat(OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES + 1);
        let rejected = observation(
            ObservationEvidenceV1::StructuredProvider,
            action_blocked(BlockAuthorityV1::ProviderClassifier, &oversized_reason),
        );
        assert!(matches!(
            rejected.validate(),
            Err(ObservationValidationError::InvalidText {
                field: "action blocked reason",
                ..
            })
        ));

        // A multi-byte character sits exactly on the cut boundary -- the
        // truncation must back off to the last whole character, never split
        // it, and must still report that a cut happened.
        let mut boundary_straddling =
            "a".repeat(OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES - 1);
        boundary_straddling.push('€'); // 3 bytes: straddles the 1024-byte cut
        boundary_straddling.push_str(" trailing text past the limit");
        let (truncated, was_truncated) = truncate_observation_text(
            &boundary_straddling,
            OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES,
        );
        assert!(was_truncated);
        assert!(truncated.len() <= OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());

        let fits_after_cut = observation(
            ObservationEvidenceV1::StructuredProvider,
            action_blocked(BlockAuthorityV1::ProviderClassifier, &truncated),
        );
        fits_after_cut
            .validate()
            .expect("truncated reason fits the bound");

        let (untouched, was_truncated) =
            truncate_observation_text("short reason", OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES);
        assert!(!was_truncated);
        assert_eq!(untouched, "short reason");
    }
}
