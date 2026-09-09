//! Privacy-minimized read wire contract for the harness localhost host.
//!
//! Each loopback TCP connection carries exactly one JSON request frame followed
//! by `\n`. The caller must then half-close its write side; EOF is the request
//! boundary and the host sends exactly one newline-terminated reply. A newline
//! without the write half-close is incomplete and expires categorically as
//! `HarnessReadHostErrorV1::Deadline`.

use std::{collections::BTreeMap, fmt};

pub use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessApprovalLevelV1,
    HarnessArtifactRef, HarnessEntityReadScopeV1, HarnessExecutionModeV1,
    HarnessFailureCategoryV1, HarnessIdempotencyRef, HarnessMonitoringVisibilityV1,
    HarnessOperationId,
    HarnessOperationKindV1, HarnessOperationStateV1, HarnessOutcomeUnknownReasonV1,
    HarnessCancelTaskRequestV1, HarnessCreateTaskRequestV1, HarnessDispatchIntentV1,
    HarnessExpectedExecutionSpecRevisionV1, HarnessExecutionSpecId,
    HarnessContinuationOutcomeUnknownReasonV1, HarnessContinuationRef,
    HarnessContinuationStateV1, HarnessDeliveryBundleDigestV1,
    HarnessDeliveryBundleIdV1, HarnessDeliveryBundleRevisionV1, HarnessDeliveryBundleV1,
    HarnessDeliveryBundleSelectionV1, HarnessDeliveryComponentCountV1,
    HarnessDeliveryComponentKindV1, HarnessContextSourceSelectionV1,
    HarnessContextSourceAvailabilityV1,
    HarnessDeliveryManifestDigestV2, HarnessDeliveryRef, HarnessDeliveryStateV1,
    HarnessGrantTargetV1,
    HarnessLaunchAuthorityRefV1, HarnessLaunchPlanRefV1, HarnessMoveTaskRequestV1,
    HarnessOperatorAuthorityV1, HarnessReplaceTaskExecutionSpecRequestV1,
    HarnessReplaceTaskRequestV1, HarnessRequestDigest, HarnessRetryTaskRequestV1,
    HarnessScheduledLaunchRefV2, HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1,
    HarnessStartTaskRequestV1, HarnessTaskExecutionSpecInputV1,
    HarnessTaskExecutionSpecV1, HarnessTaskExecutionSpecV2,
    HarnessTaskLaunchIssuanceId, HarnessTaskLaunchIssuanceRefV1,
    HarnessTaskReviewPolicyV1,
    HarnessTaskStartOutcomeV1,
    HarnessContextPackLineageV1,
    HarnessReadPermissionsV1, HarnessReconciliationOutcomeV1, HarnessResolvedContextPackReceiptV1,
    HarnessResultDispositionV1,
    HarnessInlineRef, HarnessReceiptRef, HarnessResultRef, HarnessRevision, HarnessRunId, HarnessRunIntentV1,
    HarnessRunFinishOutcomeV1, HarnessRunFinishResultV1,
    HarnessRunGitFactsV1, HarnessRunLifecycleV1,
    HarnessRuntimeIdentityV1, HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
    HarnessTaskCreateResultV1, HarnessTaskMoveResultV1,
    HarnessValidationError, HarnessWorktreeIntentV1, SessionGrantId,
    HarnessMailAckV1, HarnessMailAddressV1, HarnessMailMessageId, HarnessMailMessageV1,
    HarnessMailRefV1,
    HarnessRecordRef,
    HARNESS_ARTIFACTS_MAX, HARNESS_BODY_MAX_BYTES,
    HARNESS_CHILD_COUNT_MAX, HARNESS_CHILD_DEPTH_MAX, HARNESS_DEPENDENCIES_MAX,
    HARNESS_CONTEXT_PACK_MAX_BYTES, HARNESS_CONTEXT_PACK_RETAINED_MESSAGES_MAX,
    HARNESS_LINKS_MAX, HARNESS_MAIL_REFS_MAX, HARNESS_RESULTS_MAX, HARNESS_TITLE_MAX_BYTES,
    HARNESS_RUN_GIT_PATH_MAX_BYTES,
};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const HARNESS_READ_REQUEST_MAX_BYTES: usize = 64 * 1024;
pub const HARNESS_READ_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
pub const HARNESS_READ_LIMIT_MAX: u16 = 256;
pub const HARNESS_ENTITY_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_TIMELINE_PAGE_LIMIT_MAX: u16 = 128;
pub const HARNESS_MONITOR_FACTS_MAX: usize = 128;
pub const HARNESS_OBSERVATION_LABEL_MAX_BYTES: usize = 64;
pub const HARNESS_OBSERVATION_TODO_TEXT_MAX_BYTES: usize = 256;
pub const HARNESS_OBSERVATION_PATH_MAX_BYTES: usize = 1_024;
/// Max bytes for `BlockFactV1::label` (`<authority slug>: <reason>`,
/// producer-truncated to fit) -- sized like
/// `HARNESS_OBSERVATION_TODO_TEXT_MAX_BYTES` rather than
/// `HARNESS_OBSERVATION_LABEL_MAX_BYTES`: a block's `reason` is verbatim
/// provider/gate prose, bounded at 1024 bytes where it is minted (see
/// `gate4agent_observation_protocol::OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES`),
/// not a short class slug like "bash"/"editor" the 64-byte activity-label
/// bound was sized for.
pub const HARNESS_OBSERVATION_BLOCK_LABEL_MAX_BYTES: usize = 256;
pub const HARNESS_READ_CREDENTIAL_MAX_BYTES: usize = 8 * 1024;
pub const HARNESS_MCP_AUDIENCE: &str = "gate4agent-harness-mcp-read-v1";
/// The harness operator wire is a loopback protocol between processes built
/// from the same tree and rolled together (`gate4agent-harness` and its two
/// clients, `gate4agent-tui` / `gate4agent-tui-light`, via
/// `gate4agent-harness-client`). There is exactly one peer shape at a time --
/// no peer that cannot be rebuilt exists -- so the envelope now checks
/// [`BUILD_STAMP`] for exact equality instead of a hand-typed version
/// counter: `BUILD_STAMP` is a content hash of the working tree computed at
/// compile time (see `gate4agent-build-stamp`), so any wire-shape change
/// anywhere in the co-built tree moves it automatically, and a peer built
/// from a byte-identical checkout always matches. An additive read-wire
/// field still just needs `#[serde(default)]`, same as before -- nothing
/// about that discipline changes. A persisted format version (a checkpoint,
/// database schema, or any other record that must survive forward migration
/// across releases) is a different thing entirely and keeps its own
/// explicit, hand-bumped integer.
pub use gate4agent_build_stamp::BUILD_STAMP;
// Realistic multi-pane ceiling with headroom; bounds the harness-side
// per-subscriber HashSet<RuntimeSessionKey> and the connect-time seed burst
// `SubscribeTerminal`'s handler sends immediately after registering (see
// `gate4agent-harness-service::runtime`'s `HostCommand::SubscribeTerminal`
// arm).
pub const HARNESS_TERMINAL_SUBSCRIPTION_SESSIONS_MAX: usize = 32;
// Same rationale as immediately above, for `SubscribeAgentStream` -- bounds
// the harness-side per-subscriber HashSet<RuntimeSessionKey> and the
// connect-time seed burst its handler sends immediately after registering.
// A distinct constant from the terminal one even though the value matches:
// the two subscription kinds track separate per-subscriber sets and could
// diverge later without forcing a shared bound.
pub const HARNESS_AGENT_STREAM_SUBSCRIPTION_SESSIONS_MAX: usize = 32;
pub const HARNESS_OPERATOR_REQUEST_MAX_BYTES: usize = 64 * 1024;
pub const HARNESS_OPERATOR_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
pub const HARNESS_OPERATOR_CREDENTIAL_MAX_BYTES: usize = 256;
pub const HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX: u16 = 64;
pub const HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX: u16 = 24;
pub const HARNESS_NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES: usize = 4_096;
pub const HARNESS_SESSION_RECORD_DISPLAY_NAME_MAX_BYTES: usize = 256;
pub const HARNESS_SESSION_RECORD_TASK_ID_MAX_BYTES: usize = 128;
pub const HARNESS_PROVIDER_SESSION_ID_MAX_BYTES: usize = 512;
pub const HARNESS_PROVIDER_SESSION_TRANSCRIPT_PATH_MAX_BYTES: usize = 32_768;
pub const HARNESS_LAUNCH_PLAN_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_TASK_LAUNCH_OPTIONS_MAX: usize = 64;
/// Bounds `HarnessTaskLaunchOptionsV1::context_source_exclusions` -- a
/// diagnostic dump of every run `context_source_option` declined to surface,
/// not an operator-facing selection page, so it gets its own smaller ceiling
/// rather than reusing [`HARNESS_TASK_LAUNCH_OPTIONS_MAX`].
pub const HARNESS_CONTEXT_SOURCE_EXCLUSIONS_MAX: usize = 32;
/// Bounds `ContextSourceExclusionV1::NodeIncarnationUnknown`'s own
/// `known_incarnations` list -- the incarnation(s) the runtime inventory
/// currently holds for the excluded run's `node_id`.
pub const CONTEXT_SOURCE_EXCLUSION_KNOWN_INCARNATIONS_MAX: usize = 8;
pub const HARNESS_RUNTIME_SPAWN_PROFILES_MAX: usize = 64;
pub const HARNESS_RUNTIME_LAUNCH_BUNDLES_MAX: usize = 128;
pub const HARNESS_REPOSITORY_PATH_MAX_BYTES: usize = 1_024;
pub const HARNESS_WORKSPACE_FILE_MAX_BYTES: usize = 256 * 1_024;
pub const HARNESS_WORKSPACE_TREE_ENTRIES_MAX: usize = 512;
pub const HARNESS_GIT_STATUS_ENTRIES_MAX: usize = 128;
pub const HARNESS_GIT_RECENT_COMMITS_MAX: usize = 12;
pub const HARNESS_GIT_HISTORY_LIMIT_MAX: u16 = 50;
pub const HARNESS_GIT_DIFF_MAX_BYTES: usize = 512 * 1_024;
pub const HARNESS_GIT_COMMIT_PARENTS_MAX: usize = 32;
pub const HARNESS_GIT_SUMMARY_MAX_BYTES: usize = 1_024;
pub const HARNESS_GIT_IDENTITY_MAX_BYTES: usize = 512;
pub const HARNESS_GIT_TIMESTAMP_MAX_BYTES: usize = 128;
pub const HARNESS_GIT_SIGNER_MAX_BYTES: usize = 1_024;
pub const HARNESS_REVERSE_ATTRIBUTION_LINKS_MAX: usize = 64;
pub const HARNESS_TERMINAL_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_TERMINAL_SCROLLBACK_LINES_MAX: usize = 512;
// Ceiling for one wire terminal frame. Must stay above the node's real maximum
// live frame: PTY_TERMINAL_SCROLLBACK_ROWS_MAX (256) styled rows plus the screen,
// which on a wide, heavily styled terminal can exceed 512 KiB. 2 MiB gives
// headroom so validate() never rejects a legitimate frame while still bounding a
// malformed one.
pub const HARNESS_TERMINAL_FRAME_MAX_BYTES: usize = 2 * 1_024 * 1_024;
// Matches the node's own `MAX_NODE_TEXT_BYTES` (gate4agent-node-protocol):
// this crate has no dependency on that crate, so the bound is mirrored here
// as the wire-level ceiling `WriteSessionInput` is rejected above; the node
// re-checks the same limit authoritatively on its own side regardless.
// `PromptSession`/`PasteSession` reuse this exact constant: the node's own
// `validate_node_text` checks "prompt"/"paste"/"terminal input" against the
// same single `MAX_NODE_TEXT_BYTES` ceiling, not a per-verb one.
pub const HARNESS_SESSION_INPUT_MAX_BYTES: usize = 32 * 1_024;
// Matches the node's own `MAX_NODE_TERMINAL_BYTES` (gate4agent-node-protocol)
// / `gate4agent_types::TERMINAL_BYTES_MAX_BYTES`, mirrored here for the same
// reason as `HARNESS_SESSION_INPUT_MAX_BYTES` above: this is the wire-level
// ceiling `WriteSessionBytes` is rejected above, not a substitute for the
// node's own authoritative re-check.
pub const HARNESS_SESSION_BYTES_MAX_BYTES: usize = 64;
// Matches the node's own `MAX_WORKSPACE_ROOT_BYTES` (gate4agent-node-protocol,
// itself `gate4agent_types::WORKING_DIRECTORY_MAX_BYTES`): the wire-level
// ceiling `HarnessHostPathV1` (a host filesystem path -- workspace/worktree
// root, host-directory-browse cursor) is rejected above. Not shared with
// `HARNESS_REPOSITORY_PATH_MAX_BYTES`: that bound is for repo-relative paths,
// a categorically smaller and differently-shaped value.
pub const HARNESS_HOST_PATH_MAX_BYTES: usize = 32 * 1_024;
// Matches the node's own `MAX_HOST_DIRECTORY_ENTRIES`/
// `MAX_HOST_DIRECTORY_DISPLAY_NAME_BYTES` (gate4agent-node-protocol).
pub const HARNESS_HOST_DIRECTORY_ENTRIES_MAX: usize = 256;
pub const HARNESS_HOST_DIRECTORY_DISPLAY_NAME_MAX_BYTES: usize = 1_024;
// Mirrors `gate4agent-node-protocol::MAX_ACP_CONTROL_ID_BYTES`
// (`gate4agent_types::PROVIDER_EVENT_ID_MAX_BYTES`): this crate has no
// dependency on either (see the doc comment on `HarnessTerminalControlV1`),
// so the ACP setter verbs' `mode_id`/`option_id`/`model_id` and the agent
// stream's `tool_name`/catalog id fields are bounded to the same ceiling the
// provider event stream already validates them against.
pub const HARNESS_AGENT_STREAM_ID_MAX_BYTES: usize = 512;
// Mirrors `gate4agent-node-protocol::MAX_ACP_CONTROL_TEXT_BYTES`
// (`gate4agent_types::PROVIDER_EVENT_TEXT_MAX_BYTES`): free text carried on
// the agent content stream (`Text`, `Thinking`, interaction `prompt`/
// `title`, catalog entry descriptions) and `SetSessionConfigOption`'s
// `value_json`.
pub const HARNESS_AGENT_STREAM_TEXT_MAX_BYTES: usize = 262_144;
// Mirrors `gate4agent-node-protocol::MAX_ACP_INTERACTION_OPTIONS`
// (`gate4agent_types::PROVIDER_CONFIG_OPTION_CHOICES_MAX`): the option list
// on one `InteractionPrompt` chunk, and (the same source constant reused for
// the same reason on the node side) the `choices` list on one
// `ConfigOptions` entry.
pub const HARNESS_AGENT_STREAM_INTERACTION_OPTIONS_MAX: usize = 256;
// Mirrors `gate4agent-node-protocol::MAX_ACP_CATALOG_ENTRIES`
// (`gate4agent_types::PROVIDER_CONFIG_OPTIONS_MAX`): the catalog list on a
// `ModeCatalog`/`ConfigOptions`/`ModelCatalog` chunk.
pub const HARNESS_AGENT_STREAM_CATALOG_ENTRIES_MAX: usize = 256;
// Mirrors `gate4agent_types::PROVIDER_INTERACTION_RESPONSE_MAX_BYTES`: the
// bound on `ResolveInteraction`'s `Answer { text }` payload. This wire does
// not also mirror that source type's `EmptyAnswer` rule (an `Answer` must be
// non-empty only when the live interaction is a `Question`) -- see
// `HarnessProviderInteractionResponseV1`'s own doc comment for why that
// check stays the node's job.
pub const HARNESS_ACP_INTERACTION_RESPONSE_MAX_BYTES: usize = 32_768;

pub const HARNESS_READ_TOOL_IDS: [&str; 8] = [
    "g4a_context_get",
    "g4a_monitor_get",
    "g4a_timeline_read",
    "g4a_tasks_list",
    "g4a_tasks_get",
    "g4a_runs_list",
    "g4a_runs_get",
    "g4a_operation_get",
];

/// The mailbox arc's Slice B write allow-list (D1 + D7), joined by Slice C's
/// `g4a_mail_fetch`, Slice D's `g4a_task_create`/`g4a_task_move` (D5), and
/// S10's `g4a_run_finish`. Kept separate from `HARNESS_READ_TOOL_IDS` rather
/// than folded into it -- that array's own test
/// (`all_eight_tool_schemas_are_stable_and_closed`) pins it as exactly the
/// eight read-only tools zipped in `tool_definitions()`'s declared order, and
/// every mail tool here is gated by `grant.mail`, not by any of the three
/// `read_permissions`/`monitoring_visibility` scopes the eight reads use.
/// `g4a_mail_inbox` and `g4a_mail_fetch` perform no mutation -- they are
/// reads in effect, `g4a_mail_fetch`'s the C1 read whose authorization is
/// message-membership ("it was mailed to me") rather than lineage scope --
/// but both belong here with their mutating siblings because the gate they
/// share is the same one, not the eight reads' gate. `g4a_task_create`/
/// `g4a_task_move` are gated by `grant.task_permissions.create`/`.mutate`
/// respectively, the same wiring `mail` already establishes for its own four
/// siblings here. `g4a_run_finish` is the one member of this array gated by
/// no grant permission at all (`expected_allowed_tool_ids` carries it
/// unconditionally, the same way it carries `g4a_context_get` out of
/// `HARNESS_READ_TOOL_IDS`) -- every session needs the ability to report its
/// own work finished regardless of what else its grant allows. D7: this
/// array, and `HARNESS_READ_TOOL_IDS`, are the only two names `tools/call`
/// ever admits; `ResolveInteraction` (or any other name) is refused by name
/// regardless of what a grant's `allowed_tool_ids` claims.
pub const HARNESS_WRITE_TOOL_IDS: [&str; 7] = [
    "g4a_mail_ack",
    "g4a_mail_fetch",
    "g4a_mail_inbox",
    "g4a_mail_send",
    "g4a_run_finish",
    "g4a_task_create",
    "g4a_task_move",
];
/// Bound on a mail send/ack named-refusal reason string
/// (`HarnessMailSendResultV1::Refused`). The longest reason
/// `HarnessEngineError` actually produces for `SendMail` (`MailRecipientUnknown`/
/// `MailRecipientEnded`/`MailPermissionDenied`, formatted via `to_string()`)
/// runs well under 128 bytes even with a maximal opaque id embedded; this
/// stays generous headroom rather than a tight fit.
pub const HARNESS_MAIL_REFUSAL_REASON_MAX_BYTES: usize = 256;
/// Bound on the `thread` id list `g4a_mail_inbox` attaches to one page entry
/// -- the wire-level cap on what is otherwise an unbounded transitive
/// closure (`HarnessEngine::mail_inbox`'s own doc comment), sized like the
/// other page-shaped limits in this crate (`HARNESS_TIMELINE_PAGE_LIMIT_MAX`).
pub const HARNESS_MAIL_THREAD_IDS_MAX: u16 = 128;

const TOKEN_PREFIX: &str = "g4ah2_";
const OPERATOR_TOKEN_PREFIX: &str = "g4aho_";
const OPERATOR_REQUEST_REF_PREFIX: &str = "hireq_";

#[derive(Clone, Eq, PartialEq)]
pub struct HarnessReadCredential(String);

impl HarnessReadCredential {
    pub fn parse(value: impl Into<String>) -> Result<Self, HarnessReadApiError> {
        let value = value.into();
        validate_credential(&value)?;
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str { &self.0 }
}

impl fmt::Debug for HarnessReadCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HarnessReadCredential([REDACTED])")
    }
}

impl Serialize for HarnessReadCredential {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessReadCredential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HarnessOperatorCredential(String);

impl HarnessOperatorCredential {
    pub fn parse(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_operator_credential(&value)?;
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str { &self.0 }
}

impl fmt::Debug for HarnessOperatorCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HarnessOperatorCredential([REDACTED])")
    }
}

impl Serialize for HarnessOperatorCredential {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessOperatorCredential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperatorEnvelopeV1 {
    pub build_stamp: String,
    pub credential: HarnessOperatorCredential,
    pub request: HarnessOperatorRequestV1,
}

impl HarnessOperatorEnvelopeV1 {
    /// Builds an envelope carrying this binary's own [`BUILD_STAMP`] -- the
    /// one place every caller in this tree (this crate's own tests,
    /// `gate4agent-harness-service`, `gate4agent-harness-client`) should
    /// build one from, rather than hand-filling `build_stamp` at each site.
    pub fn new(credential: HarnessOperatorCredential, request: HarnessOperatorRequestV1) -> Self {
        Self {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }
    }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.build_stamp != BUILD_STAMP {
            return Err(HarnessOperatorApiError::BuildStampMismatch {
                expected: BUILD_STAMP.to_string(),
                received: self.build_stamp.clone(),
            });
        }
        validate_operator_credential(self.credential.expose())?;
        self.request.validate()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessOperatorRequestRefV1(String);

impl HarnessOperatorRequestRefV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_operator_request_ref(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        validate_operator_request_ref(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessOperatorRequestRefV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperatorIntentV1 {
    pub request_ref: HarnessOperatorRequestRefV1,
    pub submitted_at_unix_ms: u64,
    pub action: HarnessOperatorActionV1,
}

impl HarnessOperatorIntentV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.request_ref.validate()?;
        if self.submitted_at_unix_ms == 0 {
            return Err(HarnessOperatorApiError::InvalidSubmittedAt);
        }
        let authority = HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "0".repeat(24)))
                .map_err(HarnessOperatorApiError::Protocol)?,
            idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{}", "0".repeat(24)))
                .map_err(HarnessOperatorApiError::Protocol)?,
            actor_id: HarnessSelectorV1::new("harness-operator")
                .map_err(HarnessOperatorApiError::Protocol)?,
            now_unix_ms: self.submitted_at_unix_ms,
        };
        let task_id = HarnessTaskId::new(format!("htask_{}", "0".repeat(24)))
            .map_err(HarnessOperatorApiError::Protocol)?;
        self.action.clone().authorize(authority, task_id).validate()
    }

    pub fn authorize(
        self,
        authority: HarnessOperatorAuthorityV1,
        create_task_id: HarnessTaskId,
    ) -> HarnessOperatorRequestV1 {
        self.action.authorize(authority, create_task_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorActionV1 {
    CreateTask {
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
        dependencies: Vec<HarnessTaskId>,
        initial_state: HarnessTaskStateV1,
    },
    ReplaceTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
        dependencies: Vec<HarnessTaskId>,
    },
    MoveTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        state: HarnessTaskStateV1,
    },
    CancelTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
    },
    RetryTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
    },
    ScheduleNext {
        plan_id: Option<HarnessSelectorV1>,
    },
    ReplaceTaskExecutionSpec {
        task_id: HarnessTaskId,
        expected_task_revision: HarnessRevision,
        expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1,
        spec: HarnessTaskExecutionSpecInputV1,
    },
    StartTask {
        task_id: HarnessTaskId,
        expected_task_revision: HarnessRevision,
        expected_execution_spec_revision: HarnessRevision,
        expected_scheduled_launch_digest: HarnessRequestDigest,
    },
    ReplaceTaskExecutionSpecV2 {
        task_id: HarnessTaskId,
        expected_task_revision: HarnessRevision,
        expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1,
        selection: HarnessReviewedTaskLaunchSelectionV1,
    },
    StartTaskV2 {
        task_id: HarnessTaskId,
        expected_task_revision: HarnessRevision,
        expected_execution_spec_revision: HarnessRevision,
        expected_launch_issuance: HarnessTaskLaunchIssuanceRefV1,
    },
}

impl HarnessOperatorActionV1 {
    pub fn authorize(
        self,
        authority: HarnessOperatorAuthorityV1,
        create_task_id: HarnessTaskId,
    ) -> HarnessOperatorRequestV1 {
        match self {
            Self::CreateTask {
                title,
                body,
                parent_task_id,
                dependencies,
                initial_state,
            } => HarnessOperatorRequestV1::CreateTask {
                request: HarnessCreateTaskRequestV1 {
                    authority,
                    task_id: create_task_id,
                    title,
                    body,
                    parent_task_id,
                    dependencies,
                    initial_state,
                },
            },
            Self::ReplaceTask {
                task_id,
                expected_revision,
                title,
                body,
                parent_task_id,
                dependencies,
            } => HarnessOperatorRequestV1::ReplaceTask {
                request: HarnessReplaceTaskRequestV1 {
                    authority,
                    task_id,
                    expected_revision,
                    title,
                    body,
                    parent_task_id,
                    dependencies,
                },
            },
            Self::MoveTask { task_id, expected_revision, state } => {
                HarnessOperatorRequestV1::MoveTask {
                    request: HarnessMoveTaskRequestV1 {
                        authority,
                        task_id,
                        expected_revision,
                        state,
                    },
                }
            }
            Self::CancelTask { task_id, expected_revision } => {
                HarnessOperatorRequestV1::CancelTask {
                    request: HarnessCancelTaskRequestV1 {
                        authority,
                        task_id,
                        expected_revision,
                    },
                }
            }
            Self::RetryTask { task_id, expected_revision } => {
                HarnessOperatorRequestV1::RetryTask {
                    request: HarnessRetryTaskRequestV1 {
                        authority,
                        task_id,
                        expected_revision,
                    },
                }
            }
            Self::ScheduleNext { plan_id } => HarnessOperatorRequestV1::ScheduleNext {
                request: HarnessScheduleNextRequestV1 { authority, plan_id },
            },
            Self::ReplaceTaskExecutionSpec {
                task_id,
                expected_task_revision,
                expected_execution_spec_revision,
                spec,
            } => HarnessOperatorRequestV1::ReplaceTaskExecutionSpec {
                request: HarnessReplaceTaskExecutionSpecRequestV1 {
                    authority,
                    task_id,
                    expected_task_revision,
                    expected_execution_spec_revision,
                    spec,
                },
            },
            Self::StartTask {
                task_id,
                expected_task_revision,
                expected_execution_spec_revision,
                expected_scheduled_launch_digest,
            } => HarnessOperatorRequestV1::StartTask {
                request: HarnessStartTaskRequestV1 {
                    authority,
                    task_id,
                    expected_task_revision,
                    expected_execution_spec_revision,
                    expected_scheduled_launch_digest,
                },
            },
            Self::ReplaceTaskExecutionSpecV2 {
                task_id,
                expected_task_revision,
                expected_execution_spec_revision,
                selection,
            } => HarnessOperatorRequestV1::ReplaceTaskExecutionSpecV2 {
                request: HarnessReplaceTaskExecutionSpecRequestV2 {
                    authority,
                    task_id,
                    expected_task_revision,
                    expected_execution_spec_revision,
                    selection,
                },
            },
            Self::StartTaskV2 {
                task_id,
                expected_task_revision,
                expected_execution_spec_revision,
                expected_launch_issuance,
            } => HarnessOperatorRequestV1::StartTaskV2 {
                request: HarnessStartTaskRequestV2 {
                    authority,
                    task_id,
                    expected_task_revision,
                    expected_execution_spec_revision,
                    expected_launch_issuance,
                },
            },
        }
    }

}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessLaunchPlanSummaryV1 {
    pub scheduled_launch: HarnessScheduledLaunchRefV2,
    pub node_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub worktree: HarnessWorktreeIntentV1,
    pub provider_profile: HarnessSelectorV1,
    pub provider_id: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
}

impl HarnessLaunchPlanSummaryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.scheduled_launch.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.worktree.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.provider_profile.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.provider_id.validate().map_err(HarnessOperatorApiError::Protocol)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessLaunchPlanPageV1 {
    pub plans: Vec<HarnessLaunchPlanSummaryV1>,
    pub next_plan_id: Option<HarnessSelectorV1>,
}

impl HarnessLaunchPlanPageV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.plans.len() > usize::from(HARNESS_LAUNCH_PLAN_PAGE_LIMIT_MAX) {
            return Err(HarnessOperatorApiError::InvalidLaunchPlans);
        }
        for plan in &self.plans { plan.validate()?; }
        if self.plans.windows(2).any(|plans| {
            plans[0].scheduled_launch.plan.plan_id.as_str()
                >= plans[1].scheduled_launch.plan.plan_id.as_str()
        }) {
            return Err(HarnessOperatorApiError::InvalidLaunchPlans);
        }
        if let Some(next_plan_id) = &self.next_plan_id {
            next_plan_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
            if self.plans.last().map(|plan| &plan.scheduled_launch.plan.plan_id)
                != Some(next_plan_id)
            {
                return Err(HarnessOperatorApiError::InvalidLaunchPlans);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOrdinaryLaunchPlanOptionV1 {
    pub plan: HarnessLaunchPlanRefV1,
    pub node_id: HarnessSelectorV1,
    pub source_workspace_id: HarnessSelectorV1,
    pub provider_profile: HarnessSelectorV1,
    pub provider_id: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
}

impl HarnessOrdinaryLaunchPlanOptionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.plan.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.source_workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.provider_profile.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.provider_id.validate().map_err(HarnessOperatorApiError::Protocol)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessManagedWorktreeRetentionV1 {
    RemoveWhenReleased,
    Retain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManagedWorktreeProfileOptionV1 {
    pub node_id: HarnessSelectorV1,
    pub node_incarnation: HarnessSelectorV1,
    pub source_workspace_id: HarnessSelectorV1,
    pub profile_id: HarnessSelectorV1,
    pub profile_revision: HarnessSelectorV1,
    pub retention: HarnessManagedWorktreeRetentionV1,
    pub observed_at_unix_ms: u64,
}

impl HarnessManagedWorktreeProfileOptionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_incarnation.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.source_workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.profile_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.profile_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if self.observed_at_unix_ms == 0 {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReviewedWorktreeSelectionV1 {
    Existing,
    Managed { profile: HarnessManagedWorktreeProfileOptionV1 },
}

impl HarnessReviewedWorktreeSelectionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Existing => Ok(()),
            Self::Managed { profile } => profile.validate(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReviewedTaskLaunchSelectionV1 {
    pub plan: HarnessOrdinaryLaunchPlanOptionV1,
    pub worktree: HarnessReviewedWorktreeSelectionV1,
    pub context_source: Option<HarnessContextSourceSelectionV1>,
    pub delivery: Option<HarnessDeliveryBundleSelectionV1>,
    pub review_policy: HarnessTaskReviewPolicyV1,
}

impl HarnessReviewedTaskLaunchSelectionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.plan.validate()?;
        self.worktree.validate()?;
        if let HarnessReviewedWorktreeSelectionV1::Managed { profile } = &self.worktree {
            if profile.node_id != self.plan.node_id
                || profile.source_workspace_id != self.plan.source_workspace_id
            {
                return Err(HarnessOperatorApiError::InvalidTaskLaunchSelection);
            }
        }
        if let Some(context_source) = &self.context_source {
            context_source.validate().map_err(HarnessOperatorApiError::Protocol)?;
        }
        if let Some(delivery) = &self.delivery {
            delivery.validate().map_err(HarnessOperatorApiError::Protocol)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessIssuedExecutionSpecSummaryV1 {
    pub task_id: HarnessTaskId,
    pub execution_spec_id: HarnessExecutionSpecId,
    pub revision: HarnessRevision,
    pub launch_issuance: HarnessTaskLaunchIssuanceRefV1,
    pub review_policy: HarnessTaskReviewPolicyV1,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl HarnessIssuedExecutionSpecSummaryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.launch_issuance.revision != self.revision {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
        }
        HarnessTaskExecutionSpecV2 {
            execution_spec_id: self.execution_spec_id.clone(),
            revision: self.revision,
            task_id: self.task_id.clone(),
            launch_issuance: self.launch_issuance.clone(),
            review_policy: self.review_policy,
            created_at_unix_ms: self.created_at_unix_ms,
            updated_at_unix_ms: self.updated_at_unix_ms,
        }.validate().map_err(HarnessOperatorApiError::Protocol)
    }
}

/// Names, with its compared inputs, the exact reason `context_source_option`
/// (`gate4agent-harness-service::runtime`) declined to surface a run as a
/// `context_sources` candidate. Instrumentation only, never a policy
/// decision -- the admission logic itself is unchanged by this type's
/// existence; it only makes an already-silent exclusion observable from
/// `launch-options` and the harness's own logs (`context source excluded`).
/// One variant per early exit in that function, Live and Durable branches
/// alike; a Live-branch-only or Durable-branch-only condition says so in its
/// own doc comment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ContextSourceExclusionV1 {
    /// The run's `lifecycle` is neither `Running` nor `Waiting`, and it
    /// carries no durable context pack (which would have taken the Durable
    /// branch instead of this lifecycle gate).
    LifecycleNotLive { lifecycle: HarnessRunLifecycleV1 },
    /// The run carries no `binding` at all -- Live or Durable branch alike.
    NoBinding,
    /// The bound session identity is not `Managed` at all (Durable branch),
    /// or (Live branch) it is `Managed` with no `active_session`.
    NoActiveManagedSession,
    /// Live branch only: no entry in the runtime inventory carries this
    /// run's exact `node_id` + `node_incarnation` pair (the Durable branch
    /// re-resolves the node's CURRENT incarnation by `node_id` alone
    /// instead; see `DurableNodeUnknown`). `known_incarnations` names the
    /// incarnation(s) the inventory DOES hold for that `node_id`, bounded at
    /// [`CONTEXT_SOURCE_EXCLUSION_KNOWN_INCARNATIONS_MAX`].
    NodeIncarnationUnknown {
        node_id: HarnessSelectorV1,
        node_incarnation: HarnessSelectorV1,
        known_incarnations: Vec<HarnessSelectorV1>,
    },
    /// The matched node's `managed_sessions` page carries no record whose
    /// `record_id`/`workspace_id`/`active_binding` (workspace, instance,
    /// generation) match the run's binding, and that page was NOT cut for
    /// size -- the record genuinely does not exist on this node right now.
    /// `node_has_records` is the page's own length, for scale.
    ManagedSessionRecordMismatch {
        record_id: HarnessSelectorV1,
        workspace_id: HarnessSelectorV1,
        instance_id: u64,
        generation: u64,
        node_has_records: u32,
    },
    /// Same missing-record shape as `ManagedSessionRecordMismatch`, but the
    /// node's `managed_sessions_truncated` flag was set: the page the
    /// harness received is a PREFIX of the node's full record set, so the
    /// matching record may simply sit past that cut rather than genuinely be
    /// absent. `page_len` is the page's own length, `total` the node's full
    /// `managed_session_count`.
    ManagedSessionsPageTruncated {
        record_id: HarnessSelectorV1,
        page_len: u32,
        total: u32,
    },
    /// Live branch only: `execute_operator_monitor` returned an error for
    /// this run.
    MonitorUnavailable,
    /// Live branch only: the monitor projection resolved but is not live
    /// enough to route an export through right now.
    ProjectionNotLive {
        availability: ProjectionAvailabilityV1,
        freshness: ProjectionFreshnessV1,
        transport_incomplete: bool,
    },
    /// Durable branch only: the pack's `node_id` is not currently known by
    /// the runtime inventory at all (any incarnation), so there is no route
    /// to resolve the pack through right now.
    DurableNodeUnknown { node_id: HarnessSelectorV1 },
}

impl ContextSourceExclusionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::LifecycleNotLive { .. }
            | Self::NoBinding
            | Self::NoActiveManagedSession
            | Self::MonitorUnavailable
            | Self::ProjectionNotLive { .. } => Ok(()),
            Self::NodeIncarnationUnknown { node_id, node_incarnation, known_incarnations } => {
                node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                node_incarnation.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if known_incarnations.len() > CONTEXT_SOURCE_EXCLUSION_KNOWN_INCARNATIONS_MAX {
                    return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
                }
                for incarnation in known_incarnations {
                    incarnation.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                Ok(())
            }
            Self::ManagedSessionRecordMismatch {
                record_id, workspace_id, instance_id, generation, ..
            } => {
                record_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if *instance_id == 0 || *generation == 0 {
                    return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
                }
                Ok(())
            }
            Self::ManagedSessionsPageTruncated { record_id, page_len, total } => {
                record_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if total < page_len {
                    return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
                }
                Ok(())
            }
            Self::DurableNodeUnknown { node_id } => {
                node_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
        }
    }
}

/// One [`ContextSourceExclusionV1`] pinned to the run it was computed for --
/// `HarnessTaskLaunchOptionsV1::context_source_exclusions`'s element type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSourceExclusionEntryV1 {
    pub run_id: HarnessRunId,
    pub exclusion: ContextSourceExclusionV1,
}

impl ContextSourceExclusionEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.exclusion.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTaskLaunchOptionsV1 {
    pub task_id: HarnessTaskId,
    pub task_revision: HarnessRevision,
    pub policy_digest: HarnessRequestDigest,
    pub plans: Vec<HarnessOrdinaryLaunchPlanOptionV1>,
    pub managed_worktree_profiles: Vec<HarnessManagedWorktreeProfileOptionV1>,
    pub context_sources: Vec<HarnessContextSourceSelectionV1>,
    pub delivery_bundles: Vec<HarnessDeliveryBundleSelectionV1>,
    pub current_issued_spec: Option<HarnessIssuedExecutionSpecSummaryV1>,
    pub truncated: bool,
    /// Resumes specifically the `plans` list's own page (mirrors
    /// `HarnessLaunchPlanPageV1::next_plan_id`) -- `Some` only when `plans`
    /// itself was cut for paging, `None` whenever that list's page is
    /// complete even if `truncated` is `true` for one of the other three
    /// lists. `#[serde(default)]`: additive over the original response
    /// shape, an older caller that never asked for it still deserializes.
    #[serde(default)]
    pub next_after: Option<HarnessSelectorV1>,
    /// Every early exit `context_source_option` took while building
    /// `context_sources` above, one entry per excluded run --
    /// instrumentation only, see [`ContextSourceExclusionV1`]. Bounded
    /// independently of the other four lists at
    /// [`HARNESS_CONTEXT_SOURCE_EXCLUSIONS_MAX`]: their own
    /// `HARNESS_TASK_LAUNCH_OPTIONS_MAX` sizes an operator-facing selection
    /// page, this one sizes a diagnostic dump. `#[serde(default)]`:
    /// additive over the pre-instrumentation wire shape, an older caller
    /// that never asked for it still deserializes.
    #[serde(default)]
    pub context_source_exclusions: Vec<ContextSourceExclusionEntryV1>,
}

impl HarnessTaskLaunchOptionsV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.task_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.policy_digest.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if self.plans.len() > HARNESS_TASK_LAUNCH_OPTIONS_MAX
            || self.managed_worktree_profiles.len() > HARNESS_TASK_LAUNCH_OPTIONS_MAX
            || self.context_sources.len() > HARNESS_TASK_LAUNCH_OPTIONS_MAX
            || self.delivery_bundles.len() > HARNESS_TASK_LAUNCH_OPTIONS_MAX
            || self.context_source_exclusions.len() > HARNESS_CONTEXT_SOURCE_EXCLUSIONS_MAX
        {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
        }
        for plan in &self.plans { plan.validate()?; }
        for profile in &self.managed_worktree_profiles { profile.validate()?; }
        for source in &self.context_sources {
            source.validate().map_err(HarnessOperatorApiError::Protocol)?;
        }
        for delivery in &self.delivery_bundles {
            delivery.validate().map_err(HarnessOperatorApiError::Protocol)?;
        }
        for exclusion in &self.context_source_exclusions {
            exclusion.validate()?;
        }
        if self.context_source_exclusions.windows(2).any(|items| {
            items[0].run_id >= items[1].run_id
        }) {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
        }
        if self.plans.windows(2).any(|items| {
            (&items[0].plan.plan_id, items[0].plan.revision)
                >= (&items[1].plan.plan_id, items[1].plan.revision)
        }) || self.managed_worktree_profiles.windows(2).any(|items| {
            (
                &items[0].node_id,
                &items[0].source_workspace_id,
                &items[0].profile_id,
                &items[0].profile_revision,
            ) >= (
                &items[1].node_id,
                &items[1].source_workspace_id,
                &items[1].profile_id,
                &items[1].profile_revision,
            )
        }) || self.context_sources.windows(2).any(|items| {
            (&items[0].source_run_id, items[0].source_run_revision)
                >= (&items[1].source_run_id, items[1].source_run_revision)
        }) || self.delivery_bundles.windows(2).any(|items| {
            (&items[0].bundle.bundle_id, &items[0].bundle.revision)
                >= (&items[1].bundle.bundle_id, &items[1].bundle.revision)
        }) {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
        }
        if let Some(current) = &self.current_issued_spec {
            current.validate()?;
            if current.task_id != self.task_id {
                return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
            }
        }
        if let Some(next_after) = &self.next_after {
            next_after.validate().map_err(HarnessOperatorApiError::Protocol)?;
            if !self.truncated
                || self.plans.last().map(|plan| &plan.plan.plan_id) != Some(next_after)
            {
                return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
            }
        }
        Ok(())
    }

    pub fn validate_for(&self, task_id: &HarnessTaskId) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if &self.task_id != task_id {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchOptions);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReplaceTaskExecutionSpecRequestV2 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub expected_task_revision: HarnessRevision,
    pub expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1,
    pub selection: HarnessReviewedTaskLaunchSelectionV1,
}

impl HarnessReplaceTaskExecutionSpecRequestV2 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.authority.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.expected_task_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.expected_execution_spec_revision.validate()
            .map_err(HarnessOperatorApiError::Protocol)?;
        self.selection.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessStartTaskRequestV2 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub expected_task_revision: HarnessRevision,
    pub expected_execution_spec_revision: HarnessRevision,
    pub expected_launch_issuance: HarnessTaskLaunchIssuanceRefV1,
}

impl HarnessStartTaskRequestV2 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.authority.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.expected_task_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.expected_execution_spec_revision.validate()
            .map_err(HarnessOperatorApiError::Protocol)?;
        self.expected_launch_issuance.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if self.expected_launch_issuance.revision != self.expected_execution_spec_revision {
            return Err(HarnessOperatorApiError::InvalidTaskLaunchSelection);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessRepositoryPathV1(String);

impl HarnessRepositoryPathV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_repository_path(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        validate_repository_path(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessRepositoryPathV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A bounded, UTF-8 host filesystem path: a workspace/worktree root, a
/// worktree target, or a host-directory-browse cursor (`directory`/`after`/
/// `parent`/`next_after`). Mirrors the node's own `OpaqueHostPath` narrowed
/// to its UTF-8 representation only, the same way `HarnessRepositoryPathV1`
/// narrows the node's dual-representation `RepositoryPath` -- see
/// `repository_path_from_api`/`project_repository_path` in
/// `gate4agent-harness-service` for that precedent: a genuinely non-UTF-8
/// host path from the node is a projection failure, never silently
/// lossy-displayed. Validation intentionally stays as minimal as the node's
/// own `validate_opaque_host_path` (non-empty, bounded, no NUL byte): unlike
/// a repository-relative path, an absolute host path legitimately contains
/// backslashes, drive letters, and colons (Windows), so this type imposes no
/// format assumptions beyond that.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessHostPathV1(String);

impl HarnessHostPathV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_host_path(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        validate_host_path(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessHostPathV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

fn validate_host_path(value: &str) -> Result<(), HarnessOperatorApiError> {
    if value.is_empty() || value.len() > HARNESS_HOST_PATH_MAX_BYTES || value.contains('\0') {
        return Err(HarnessOperatorApiError::InvalidHostPath);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessGitObjectIdV1(String);

impl HarnessGitObjectIdV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_git_object_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        validate_git_object_id(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessGitObjectIdV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessWorkspaceFileRevisionV1(String);

impl HarnessWorkspaceFileRevisionV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        if value.len() != 64 || !value.bytes().all(is_lower_hex) {
            return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.0.len() != 64 || !self.0.bytes().all(is_lower_hex) {
            return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for HarnessWorkspaceFileRevisionV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunWorkspaceOriginV1 {
    pub run_id: HarnessRunId,
    pub run_revision: HarnessRevision,
    pub node_id: HarnessSelectorV1,
    pub node_incarnation_id: HarnessNodeIncarnationV1,
    pub workspace_id: HarnessSelectorV1,
}

impl HarnessRunWorkspaceOriginV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.run_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_incarnation_id.validate()?;
        self.workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)
    }

    pub fn validate_for(&self, run_id: &HarnessRunId) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if &self.run_id != run_id {
            return Err(HarnessOperatorApiError::InvalidWorkspaceOrigin);
        }
        Ok(())
    }
}

/// Node-scoped sibling of `HarnessRunWorkspaceOriginV1`: identifies a
/// workspace read routed straight from a node/workspace pair, with no run
/// binding to descend from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNodeWorkspaceOriginV1 {
    pub node_id: String,
    pub node_incarnation_id: String,
    pub workspace_id: String,
}

impl HarnessNodeWorkspaceOriginV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.node_id, 128)
            || self.node_incarnation_id.len() != 32
            || !self.node_incarnation_id.bytes().all(is_lower_hex)
            || !valid_runtime_id(&self.workspace_id, 128)
        {
            return Err(HarnessOperatorApiError::InvalidWorkspaceOrigin);
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        node_id: &str,
        workspace_id: &str,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if self.node_id != node_id || self.workspace_id != workspace_id {
            return Err(HarnessOperatorApiError::InvalidWorkspaceOrigin);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessWorkspaceEntryKindV1 {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessWorkspaceTreeEntryV1 {
    pub relative_path: HarnessRepositoryPathV1,
    pub kind: HarnessWorkspaceEntryKindV1,
}

impl HarnessWorkspaceTreeEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.relative_path.validate()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessGitStatusCodeV1 {
    Unmodified,
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
    Ignored,
    TypeChanged,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessGitStatusEntryV1 {
    pub index_status: HarnessGitStatusCodeV1,
    pub worktree_status: HarnessGitStatusCodeV1,
    pub path: HarnessRepositoryPathV1,
    pub previous_path: Option<HarnessRepositoryPathV1>,
}

impl HarnessGitStatusEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.path.validate()?;
        if let Some(previous_path) = &self.previous_path {
            previous_path.validate()?;
            if previous_path == &self.path {
                return Err(HarnessOperatorApiError::InvalidGitStatus);
            }
        }
        let renamed_or_copied = matches!(self.index_status, HarnessGitStatusCodeV1::Renamed | HarnessGitStatusCodeV1::Copied)
            || matches!(self.worktree_status, HarnessGitStatusCodeV1::Renamed | HarnessGitStatusCodeV1::Copied);
        if self.previous_path.is_some() != renamed_or_copied {
            return Err(HarnessOperatorApiError::InvalidGitStatus);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessGitCommitSummaryV1 {
    pub id: HarnessGitObjectIdV1,
    pub summary: String,
}

impl HarnessGitCommitSummaryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.id.validate()?;
        validate_git_single_line(&self.summary, HARNESS_GIT_SUMMARY_MAX_BYTES, false)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessGitSummaryV1 {
    pub is_repository: bool,
    pub branch: Option<String>,
    pub status: Vec<HarnessGitStatusEntryV1>,
    pub recent_commits: Vec<HarnessGitCommitSummaryV1>,
    pub truncated: bool,
}

impl HarnessGitSummaryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.status.len() > HARNESS_GIT_STATUS_ENTRIES_MAX
            || self.recent_commits.len() > HARNESS_GIT_RECENT_COMMITS_MAX
            || !self.is_repository && (self.branch.is_some() || !self.status.is_empty() || !self.recent_commits.is_empty() || self.truncated)
        {
            return Err(HarnessOperatorApiError::InvalidGitSummary);
        }
        if let Some(branch) = &self.branch {
            validate_git_single_line(branch, HARNESS_REPOSITORY_PATH_MAX_BYTES, true)?;
        }
        for status in &self.status { status.validate()?; }
        if self.status.windows(2).any(|entries| entries[0].path >= entries[1].path) {
            return Err(HarnessOperatorApiError::InvalidGitStatus);
        }
        for commit in &self.recent_commits { commit.validate()?; }
        if self.recent_commits.iter().enumerate().any(|(index, commit)| {
            self.recent_commits[..index].iter().any(|existing| existing.id == commit.id)
        }) {
            return Err(HarnessOperatorApiError::InvalidGitSummary);
        }
        Ok(())
    }
}

/// Additive mirror of `gate4agent_node_protocol::WorkspaceInspectionTruncationV1`
/// carried onto both `HarnessRunWorkspaceInspectionV1` and
/// `HarnessNodeWorkspaceInspectionV1` — see that type for field meaning.
/// Kept as this crate's own type (rather than reused directly) to match
/// every other leaf field on these two structs, which project into
/// `Harness*V1` types rather than exposing node-protocol/c2-protocol types
/// on the operator-facing API surface.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessWorkspaceInspectionTruncationV1 {
    pub walk_time_budget_exceeded: bool,
    pub walk_entry_cap_exceeded: bool,
    pub git_time_budget_exceeded: bool,
    pub entries_visited: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunWorkspaceInspectionV1 {
    pub origin: HarnessRunWorkspaceOriginV1,
    pub entries: Vec<HarnessWorkspaceTreeEntryV1>,
    pub tree_truncated: bool,
    pub git: HarnessGitSummaryV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<HarnessWorkspaceInspectionTruncationV1>,
}

impl HarnessRunWorkspaceInspectionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        if self.entries.len() > HARNESS_WORKSPACE_TREE_ENTRIES_MAX {
            return Err(HarnessOperatorApiError::InvalidWorkspaceTree);
        }
        for entry in &self.entries { entry.validate()?; }
        if self.entries.windows(2).any(|entries| entries[0].relative_path >= entries[1].relative_path) {
            return Err(HarnessOperatorApiError::InvalidWorkspaceTree);
        }
        self.git.validate()
    }

    pub fn validate_for(&self, run_id: &HarnessRunId) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(run_id)
    }
}

/// Node-scoped sibling of `HarnessRunWorkspaceInspectionV1`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNodeWorkspaceInspectionV1 {
    pub origin: HarnessNodeWorkspaceOriginV1,
    pub entries: Vec<HarnessWorkspaceTreeEntryV1>,
    pub tree_truncated: bool,
    pub git: HarnessGitSummaryV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<HarnessWorkspaceInspectionTruncationV1>,
}

impl HarnessNodeWorkspaceInspectionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        if self.entries.len() > HARNESS_WORKSPACE_TREE_ENTRIES_MAX {
            return Err(HarnessOperatorApiError::InvalidWorkspaceTree);
        }
        for entry in &self.entries { entry.validate()?; }
        if self.entries.windows(2).any(|entries| entries[0].relative_path >= entries[1].relative_path) {
            return Err(HarnessOperatorApiError::InvalidWorkspaceTree);
        }
        self.git.validate()
    }

    pub fn validate_for(
        &self,
        node_id: &str,
        workspace_id: &str,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(node_id, workspace_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessWorkspaceFileContentV1 {
    Utf8 { text: String, byte_len: u32 },
    NonUtf8 { byte_len: u32 },
    TooLarge { limit_bytes: u32 },
}

impl HarnessWorkspaceFileContentV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Utf8 { text, byte_len } => {
                if text.len() > HARNESS_WORKSPACE_FILE_MAX_BYTES || usize::try_from(*byte_len).ok() != Some(text.len()) {
                    return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
                }
            }
            Self::NonUtf8 { byte_len } => {
                if usize::try_from(*byte_len).map_or(true, |length| length > HARNESS_WORKSPACE_FILE_MAX_BYTES) {
                    return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
                }
            }
            Self::TooLarge { limit_bytes } => {
                if usize::try_from(*limit_bytes).ok() != Some(HARNESS_WORKSPACE_FILE_MAX_BYTES) {
                    return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunWorkspaceFileV1 {
    pub origin: HarnessRunWorkspaceOriginV1,
    pub path: HarnessRepositoryPathV1,
    pub content: HarnessWorkspaceFileContentV1,
    pub revision: Option<HarnessWorkspaceFileRevisionV1>,
}

impl HarnessRunWorkspaceFileV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        self.path.validate()?;
        self.content.validate()?;
        if let Some(revision) = &self.revision { revision.validate()?; }
        if !matches!(&self.content, HarnessWorkspaceFileContentV1::Utf8 { .. }) && self.revision.is_some() {
            return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
        }
        Ok(())
    }


    pub fn validate_for(
        &self,
        run_id: &HarnessRunId,
        path: &HarnessRepositoryPathV1,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(run_id)?;
        if &self.path != path {
            return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
        }
        Ok(())
    }
}

/// Node-scoped sibling of `HarnessRunWorkspaceFileV1`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNodeWorkspaceFileV1 {
    pub origin: HarnessNodeWorkspaceOriginV1,
    pub path: HarnessRepositoryPathV1,
    pub content: HarnessWorkspaceFileContentV1,
    pub revision: Option<HarnessWorkspaceFileRevisionV1>,
}

impl HarnessNodeWorkspaceFileV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        self.path.validate()?;
        self.content.validate()?;
        if let Some(revision) = &self.revision { revision.validate()?; }
        if !matches!(&self.content, HarnessWorkspaceFileContentV1::Utf8 { .. }) && self.revision.is_some() {
            return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        node_id: &str,
        workspace_id: &str,
        path: &HarnessRepositoryPathV1,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(node_id, workspace_id)?;
        if &self.path != path {
            return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
        }
        Ok(())
    }
}

/// Response payload for `CreateNodeWorkspaceDirectory`: the one node-scoped
/// creation response (`WorkspaceDirectoryCreated`) that carries a
/// `WorkspaceEntry` instead of a `WorkspaceFileRead` -- `HarnessNodeWorkspace
/// FileV1` above covers `WriteNodeWorkspaceFile`/`CreateNodeWorkspaceFile`,
/// both of which echo a file. Reuses `HarnessWorkspaceTreeEntryV1`, already
/// the entry shape inside `HarnessNodeWorkspaceInspectionV1`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNodeWorkspaceDirectoryV1 {
    pub origin: HarnessNodeWorkspaceOriginV1,
    pub entry: HarnessWorkspaceTreeEntryV1,
}

impl HarnessNodeWorkspaceDirectoryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        self.entry.validate()?;
        if self.entry.kind != HarnessWorkspaceEntryKindV1::Directory {
            return Err(HarnessOperatorApiError::InvalidWorkspaceTree);
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        node_id: &str,
        workspace_id: &str,
        path: &HarnessRepositoryPathV1,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(node_id, workspace_id)?;
        if &self.entry.relative_path != path {
            return Err(HarnessOperatorApiError::InvalidWorkspaceTree);
        }
        Ok(())
    }
}

/// One entry in a `BrowseHostDirectories` page. Mirrors the node's own
/// `HostDirectoryEntry` field-for-field; that type's own constructor already
/// rejects a non-UTF-8 `path`, so the node never actually produces the case
/// `HarnessHostPathV1` would reject here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessHostDirectoryEntryV1 {
    pub path: HarnessHostPathV1,
    pub display_name: String,
    pub is_link: bool,
}

impl HarnessHostDirectoryEntryV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.path.validate()?;
        if self.display_name.is_empty()
            || self.display_name.len() > HARNESS_HOST_DIRECTORY_DISPLAY_NAME_MAX_BYTES
            || self.display_name.chars().any(char::is_control)
        {
            return Err(HarnessOperatorApiError::InvalidHostDirectoryBrowseRequest);
        }
        Ok(())
    }
}

/// `BrowseHostDirectories`'s reply: mirrors the node's own
/// `HostDirectoryListing` field-for-field. Entries are deliberately not
/// asserted to be sorted (unlike most other bounded collections on this
/// wire, e.g. `HarnessWorkspaceTreeEntryV1`): a live directory listing has no
/// such guarantee from the underlying filesystem, so imposing one here would
/// reject a legitimate page.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessHostDirectoryListingV1 {
    pub directory: Option<HarnessHostPathV1>,
    pub parent: Option<HarnessHostPathV1>,
    pub entries: Vec<HarnessHostDirectoryEntryV1>,
    pub next_after: Option<HarnessHostPathV1>,
    /// True only when another page of supported directory entries is
    /// available.
    pub incomplete: bool,
}

impl HarnessHostDirectoryListingV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if let Some(directory) = &self.directory { directory.validate()?; }
        if let Some(parent) = &self.parent { parent.validate()?; }
        if self.entries.len() > HARNESS_HOST_DIRECTORY_ENTRIES_MAX {
            return Err(HarnessOperatorApiError::InvalidHostDirectoryBrowseRequest);
        }
        for entry in &self.entries { entry.validate()?; }
        if let Some(next_after) = &self.next_after { next_after.validate()?; }
        Ok(())
    }
}

/// Mirrors the node's own `WorktreeServiceMode`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessWorktreeServiceModeV1 {
    Manual,
    Managed,
    Off,
}

/// `RegisterWorkspace`/`CreateStandaloneWorkspace`'s reply, and half of
/// `CreateWorktree`'s. A deliberately narrowed projection of the node's own
/// `WorkspaceSnapshot`: `sessions` is dropped (redundant with the runtime
/// inventory, which every roster-affecting resource mutation already
/// invalidates on success -- see `HarnessOperatorRequestV1::RegisterWorkspace`'s
/// doc comment) and `managed_worktree_profiles` is dropped (`SpawnManagedWorktree`
/// has no typed operator verb -- see that request variant's own doc comment
/// for why -- so a profile-catalog listing here would serve no reachable
/// caller). `workspace_id` stays a bounded opaque string rather than a typed
/// selector, matching every other node-scoped workspace id on this wire
/// (`HarnessNodeWorkspaceOriginV1.workspace_id`, etc).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessWorkspaceSnapshotV1 {
    pub workspace_id: String,
    pub canonical_root: HarnessHostPathV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_service_mode: Option<HarnessWorktreeServiceModeV1>,
}

impl HarnessWorkspaceSnapshotV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.workspace_id, 128) {
            return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
        }
        self.canonical_root.validate()
    }
}

/// The other half of `CreateWorktree`'s reply. Mirrors the node's own
/// `GitWorktreeSnapshot`, minus `lock_reason`/`prunable_reason` (free-text
/// reason strings the node's own C2-facing mirror, `C2GitWorktreeSnapshot`,
/// already drops -- this wire never receives them to begin with).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessGitWorktreeSnapshotV1 {
    pub path: HarnessHostPathV1,
    pub head: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_main: bool,
    pub locked: bool,
    pub prunable: bool,
    pub workspace_id: Option<String>,
}

impl HarnessGitWorktreeSnapshotV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.path.validate()?;
        if !valid_native_single_line(&self.head, HARNESS_GIT_SUMMARY_MAX_BYTES, true) {
            return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
        }
        if let Some(branch) = &self.branch {
            if !valid_native_single_line(branch, HARNESS_REPOSITORY_PATH_MAX_BYTES, false) {
                return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
            }
        }
        if let Some(workspace_id) = &self.workspace_id {
            if !valid_runtime_id(workspace_id, 128) {
                return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessGitSignatureStatusV1 {
    Good,
    Bad,
    UnknownValidity,
    ExpiredSignature,
    ExpiredKey,
    RevokedKey,
    CannotCheck,
    NoSignature,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessGitCommitV1 {
    pub id: HarnessGitObjectIdV1,
    pub parents: Vec<HarnessGitObjectIdV1>,
    pub subject: String,
    pub author_name: String,
    pub authored_at: String,
    pub committer_name: String,
    pub committed_at: String,
    pub signature_status: HarnessGitSignatureStatusV1,
    pub signer: Option<String>,
}

impl HarnessGitCommitV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.id.validate()?;
        if self.parents.len() > HARNESS_GIT_COMMIT_PARENTS_MAX {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        for parent in &self.parents { parent.validate()?; }
        if self.parents.iter().any(|parent| parent == &self.id) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        validate_git_single_line(&self.subject, HARNESS_GIT_SUMMARY_MAX_BYTES, false)?;
        validate_git_single_line(&self.author_name, HARNESS_GIT_IDENTITY_MAX_BYTES, false)?;
        validate_git_single_line(&self.authored_at, HARNESS_GIT_TIMESTAMP_MAX_BYTES, true)?;
        validate_git_single_line(&self.committer_name, HARNESS_GIT_IDENTITY_MAX_BYTES, false)?;
        validate_git_single_line(&self.committed_at, HARNESS_GIT_TIMESTAMP_MAX_BYTES, true)?;
        if let Some(signer) = &self.signer {
            validate_git_single_line(signer, HARNESS_GIT_SIGNER_MAX_BYTES, true)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunGitHistoryPageV1 {
    pub origin: HarnessRunWorkspaceOriginV1,
    pub path: Option<HarnessRepositoryPathV1>,
    pub commits: Vec<HarnessGitCommitV1>,
    pub next_before: Option<HarnessGitObjectIdV1>,
    pub truncated: bool,
}

impl HarnessRunGitHistoryPageV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        if let Some(path) = &self.path { path.validate()?; }
        if self.commits.len() > usize::from(HARNESS_GIT_HISTORY_LIMIT_MAX) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        for commit in &self.commits { commit.validate()?; }
        if self.commits.iter().enumerate().any(|(index, commit)| {
            self.commits[..index].iter().any(|existing| existing.id == commit.id)
        }) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        if let Some(next_before) = &self.next_before {
            next_before.validate()?;
            if self.commits.last().map(|commit| &commit.id) != Some(next_before) {
                return Err(HarnessOperatorApiError::InvalidGitHistory);
            }
        }
        Ok(())
    }


    pub fn validate_for(
        &self,
        run_id: &HarnessRunId,
        path: Option<&HarnessRepositoryPathV1>,
        limit: u16,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(run_id)?;
        if self.path.as_ref() != path || self.commits.len() > usize::from(limit) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        Ok(())
    }
}

/// Node-scoped sibling of `HarnessRunGitHistoryPageV1`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNodeGitHistoryPageV1 {
    pub origin: HarnessNodeWorkspaceOriginV1,
    pub path: Option<HarnessRepositoryPathV1>,
    pub commits: Vec<HarnessGitCommitV1>,
    pub next_before: Option<HarnessGitObjectIdV1>,
    pub truncated: bool,
}

impl HarnessNodeGitHistoryPageV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        if let Some(path) = &self.path { path.validate()?; }
        if self.commits.len() > usize::from(HARNESS_GIT_HISTORY_LIMIT_MAX) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        for commit in &self.commits { commit.validate()?; }
        if self.commits.iter().enumerate().any(|(index, commit)| {
            self.commits[..index].iter().any(|existing| existing.id == commit.id)
        }) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        if let Some(next_before) = &self.next_before {
            next_before.validate()?;
            if self.commits.last().map(|commit| &commit.id) != Some(next_before) {
                return Err(HarnessOperatorApiError::InvalidGitHistory);
            }
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        node_id: &str,
        workspace_id: &str,
        path: Option<&HarnessRepositoryPathV1>,
        limit: u16,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(node_id, workspace_id)?;
        if self.path.as_ref() != path || self.commits.len() > usize::from(limit) {
            return Err(HarnessOperatorApiError::InvalidGitHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessGitDiffModeV1 {
    Working,
    Staged,
    Commit { revision: HarnessGitObjectIdV1 },
}

impl HarnessGitDiffModeV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Working | Self::Staged => Ok(()),
            Self::Commit { revision } => revision.validate(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunGitDiffV1 {
    pub origin: HarnessRunWorkspaceOriginV1,
    pub mode: HarnessGitDiffModeV1,
    pub path: Option<HarnessRepositoryPathV1>,
    pub text: String,
    pub truncated: bool,
}

impl HarnessRunGitDiffV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        self.mode.validate()?;
        if let Some(path) = &self.path { path.validate()?; }
        if self.text.len() > HARNESS_GIT_DIFF_MAX_BYTES {
            return Err(HarnessOperatorApiError::InvalidGitDiff);
        }
        Ok(())
    }


    pub fn validate_for(
        &self,
        run_id: &HarnessRunId,
        mode: &HarnessGitDiffModeV1,
        path: Option<&HarnessRepositoryPathV1>,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(run_id)?;
        if &self.mode != mode || self.path.as_ref() != path {
            return Err(HarnessOperatorApiError::InvalidGitDiff);
        }
        Ok(())
    }
}

/// Node-scoped sibling of `HarnessRunGitDiffV1`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNodeGitDiffV1 {
    pub origin: HarnessNodeWorkspaceOriginV1,
    pub mode: HarnessGitDiffModeV1,
    pub path: Option<HarnessRepositoryPathV1>,
    pub text: String,
    pub truncated: bool,
}

impl HarnessNodeGitDiffV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.origin.validate()?;
        self.mode.validate()?;
        if let Some(path) = &self.path { path.validate()?; }
        if self.text.len() > HARNESS_GIT_DIFF_MAX_BYTES {
            return Err(HarnessOperatorApiError::InvalidGitDiff);
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        node_id: &str,
        workspace_id: &str,
        mode: &HarnessGitDiffModeV1,
        path: Option<&HarnessRepositoryPathV1>,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        self.origin.validate_for(node_id, workspace_id)?;
        if &self.mode != mode || self.path.as_ref() != path {
            return Err(HarnessOperatorApiError::InvalidGitDiff);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReverseAttributionWorkspaceV1 {
    pub node_id: HarnessSelectorV1,
    pub node_incarnation_id: HarnessNodeIncarnationV1,
    pub workspace_id: HarnessSelectorV1,
}

impl HarnessReverseAttributionWorkspaceV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_incarnation_id.validate()?;
        self.workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReverseAttributionSubjectV1 {
    ManagedRecord {
        workspace: HarnessReverseAttributionWorkspaceV1,
        record_id: HarnessSelectorV1,
    },
    RuntimeSession {
        workspace: HarnessReverseAttributionWorkspaceV1,
        instance_id: u64,
        generation: u64,
    },
    Workspace {
        workspace: HarnessReverseAttributionWorkspaceV1,
    },
    FileScope {
        workspace: HarnessReverseAttributionWorkspaceV1,
        relative_path: HarnessRepositoryPathV1,
    },
    CommitScope {
        workspace: HarnessReverseAttributionWorkspaceV1,
        object_id: HarnessGitObjectIdV1,
    },
}

impl HarnessReverseAttributionSubjectV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let workspace = match self {
            Self::ManagedRecord { workspace, record_id } => {
                record_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                workspace
            }
            Self::RuntimeSession { workspace, instance_id, generation } => {
                if *instance_id == 0 || *generation == 0 {
                    return Err(HarnessOperatorApiError::InvalidReverseAttribution);
                }
                workspace
            }
            Self::Workspace { workspace } => workspace,
            Self::FileScope { workspace, relative_path } => {
                relative_path.validate()?;
                workspace
            }
            Self::CommitScope { workspace, object_id } => {
                object_id.validate()?;
                workspace
            }
        };
        workspace.validate()
    }

    fn workspace(&self) -> &HarnessReverseAttributionWorkspaceV1 {
        match self {
            Self::ManagedRecord { workspace, .. }
            | Self::RuntimeSession { workspace, .. }
            | Self::Workspace { workspace }
            | Self::FileScope { workspace, .. }
            | Self::CommitScope { workspace, .. } => workspace,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessReverseAttributionRelationV1 {
    ManagedRecordBinding,
    RuntimeSessionBinding,
    WorkspaceBinding,
    WorkspaceScope,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReverseAttributionBindingV1 {
    ManagedRecord {
        workspace: HarnessReverseAttributionWorkspaceV1,
        record_id: HarnessSelectorV1,
        active_instance_id: Option<u64>,
        active_generation: Option<u64>,
    },
    RuntimeSession {
        workspace: HarnessReverseAttributionWorkspaceV1,
        instance_id: u64,
        generation: u64,
    },
    Workspace {
        workspace: HarnessReverseAttributionWorkspaceV1,
    },
}

impl HarnessReverseAttributionBindingV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let workspace = match self {
            Self::ManagedRecord {
                workspace,
                record_id,
                active_instance_id,
                active_generation,
            } => {
                record_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if active_instance_id.is_some() != active_generation.is_some()
                    || active_instance_id == &Some(0)
                    || active_generation == &Some(0)
                {
                    return Err(HarnessOperatorApiError::InvalidReverseAttribution);
                }
                workspace
            }
            Self::RuntimeSession { workspace, instance_id, generation } => {
                if *instance_id == 0 || *generation == 0 {
                    return Err(HarnessOperatorApiError::InvalidReverseAttribution);
                }
                workspace
            }
            Self::Workspace { workspace } => workspace,
        };
        workspace.validate()
    }

    fn workspace(&self) -> &HarnessReverseAttributionWorkspaceV1 {
        match self {
            Self::ManagedRecord { workspace, .. }
            | Self::RuntimeSession { workspace, .. }
            | Self::Workspace { workspace } => workspace,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReverseAttributionLinkV1 {
    pub task_id: HarnessTaskId,
    pub run_id: HarnessRunId,
    pub run_revision: HarnessRevision,
    pub binding: HarnessReverseAttributionBindingV1,
    pub relation: HarnessReverseAttributionRelationV1,
}

impl HarnessReverseAttributionLinkV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.run_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.binding.validate()?;
        let relation_matches = matches!(
            (&self.relation, &self.binding),
            (
                HarnessReverseAttributionRelationV1::ManagedRecordBinding,
                HarnessReverseAttributionBindingV1::ManagedRecord { .. },
            ) | (
                HarnessReverseAttributionRelationV1::RuntimeSessionBinding,
                HarnessReverseAttributionBindingV1::RuntimeSession { .. },
            ) | (
                HarnessReverseAttributionRelationV1::WorkspaceBinding
                    | HarnessReverseAttributionRelationV1::WorkspaceScope,
                HarnessReverseAttributionBindingV1::Workspace { .. },
            )
        );
        if !relation_matches {
            return Err(HarnessOperatorApiError::InvalidReverseAttribution);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessReverseAttributionOutcomeV1 {
    Attributed,
    Unattributed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReverseAttributionV1 {
    pub subject: HarnessReverseAttributionSubjectV1,
    pub outcome: HarnessReverseAttributionOutcomeV1,
    pub links: Vec<HarnessReverseAttributionLinkV1>,
}

impl HarnessReverseAttributionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.subject.validate()?;
        if self.links.len() > HARNESS_REVERSE_ATTRIBUTION_LINKS_MAX
            || matches!(self.outcome, HarnessReverseAttributionOutcomeV1::Attributed)
                != !self.links.is_empty()
            || self.links.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(HarnessOperatorApiError::InvalidReverseAttribution);
        }
        for link in &self.links {
            link.validate()?;
            if link.binding.workspace() != self.subject.workspace()
                || !self.link_matches_subject(link)
            {
                return Err(HarnessOperatorApiError::InvalidReverseAttribution);
            }
        }
        Ok(())
    }

    pub fn validate_for(
        &self,
        subject: &HarnessReverseAttributionSubjectV1,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if &self.subject != subject {
            return Err(HarnessOperatorApiError::InvalidReverseAttribution);
        }
        Ok(())
    }

    fn link_matches_subject(&self, link: &HarnessReverseAttributionLinkV1) -> bool {
        match (&self.subject, &link.relation, &link.binding) {
            (
                HarnessReverseAttributionSubjectV1::ManagedRecord { record_id, .. },
                HarnessReverseAttributionRelationV1::ManagedRecordBinding,
                HarnessReverseAttributionBindingV1::ManagedRecord {
                    record_id: binding_record_id,
                    ..
                },
            ) => record_id == binding_record_id,
            (
                HarnessReverseAttributionSubjectV1::RuntimeSession {
                    instance_id,
                    generation,
                    ..
                },
                HarnessReverseAttributionRelationV1::RuntimeSessionBinding,
                HarnessReverseAttributionBindingV1::RuntimeSession {
                    instance_id: binding_instance_id,
                    generation: binding_generation,
                    ..
                },
            ) => instance_id == binding_instance_id && generation == binding_generation,
            (
                HarnessReverseAttributionSubjectV1::Workspace { .. },
                HarnessReverseAttributionRelationV1::WorkspaceBinding,
                HarnessReverseAttributionBindingV1::Workspace { .. },
            )
            | (
                HarnessReverseAttributionSubjectV1::FileScope { .. }
                    | HarnessReverseAttributionSubjectV1::CommitScope { .. },
                HarnessReverseAttributionRelationV1::WorkspaceScope,
                HarnessReverseAttributionBindingV1::Workspace { .. },
            ) => true,
            _ => false,
        }
    }
}

/// Mirrors `gate4agent_types::ProviderSessionKey` exactly, for the same
/// reason `HarnessTerminalControlV1` duplicates `TerminalControl` rather than
/// importing it -- see that type's own doc comment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessProviderSessionKeyV1 {
    SessionId,
    ConversationId,
}

/// Mirrors `gate4agent_types::ProviderSessionIdentity`: the caller-supplied
/// reference `IndexProviderSession` binds a durable managed-session record
/// to. Bounds match the node's own `PROVIDER_EVENT_ID_MAX_BYTES`/
/// `PROVIDER_SESSION_LOCATOR_MAX_BYTES` (the node re-validates on its own
/// terms regardless; this bound exists to reject an obviously-oversized
/// request before it ever reaches C2).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProviderSessionIdentityV1 {
    pub key: HarnessProviderSessionKeyV1,
    pub id: String,
    pub transcript_path: Option<String>,
}

impl HarnessProviderSessionIdentityV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.id.is_empty()
            || self.id.len() > HARNESS_PROVIDER_SESSION_ID_MAX_BYTES
            || self.id.starts_with('-')
            || self.id.chars().any(char::is_control)
            || self.transcript_path.as_ref().is_some_and(|path| {
                path.is_empty()
                    || path.len() > HARNESS_PROVIDER_SESSION_TRANSCRIPT_PATH_MAX_BYTES
                    || path.chars().any(char::is_control)
            })
        {
            return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
        }
        Ok(())
    }
}

/// Mirrors the node's own `SessionTaskTargetV1` shape exactly (`New` starts
/// a fresh correlation, `Existing` binds to a caller-named task, `Clear`
/// removes the binding). The node's `task_id` there is its own local
/// identifier space (format `task-<24 lowercase hex>`, unrelated to this
/// crate's `HarnessTaskId` despite the similar name -- the node layer has no
/// dependency on `gate4agent-harness-protocol` and never mints one from a
/// `HarnessTaskId`), so `task_id` here stays a bounded opaque string rather
/// than the typed `HarnessTaskId`: the harness-service boundary parses and
/// validates it against the node's own format when building the C2 request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessSessionTaskTargetV1 {
    New,
    Existing { task_id: String },
    Clear,
}

impl HarnessSessionTaskTargetV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if let Self::Existing { task_id } = self {
            if !valid_runtime_id(task_id, HARNESS_SESSION_RECORD_TASK_ID_MAX_BYTES) {
                return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
            }
        }
        Ok(())
    }
}

/// Exact mirror of `gate4agent_types::ProviderInteractionKind`: this crate
/// has no dependency on `gate4agent-types` (see the doc comment on
/// `HarnessTerminalControlV1`), so the ACP interaction classification the
/// node resolves a `correlation_id` against is duplicated here as its own
/// closed wire enum, the same way `HarnessAgentStreamChunkKindV1::
/// InteractionPrompt` carries it on the outbound side.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessProviderInteractionKindV1 {
    Approval,
    Question,
}

/// Exact mirror of `gate4agent_types::ProviderInteractionResponse`: this
/// crate has no dependency on `gate4agent-types` (see the doc comment on
/// `HarnessTerminalControlV1`), so `ResolveInteraction`'s answer payload is
/// duplicated here as its own closed wire enum rather than imported.
/// Unlike the source type's own `validate_for`, `validate()` below does not
/// check `Answer`/`ApproveOnce`/`Deny` against the live interaction's own
/// `HarnessProviderInteractionKindV1` (`Approval` vs `Question`) -- that
/// check needs the interaction the node is holding under `correlation_id`
/// and stays the node's job, not this wire's, the same way
/// `WriteSessionInput` never checks that a PTY exists before sending. It
/// bounds `Answer { text }` to `HARNESS_ACP_INTERACTION_RESPONSE_MAX_BYTES`
/// and rejects unsafe control bytes, but not emptiness: the source type's
/// `EmptyAnswer` rule applies only when the interaction turns out to be a
/// `Question`, which this wire does not know either.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessProviderInteractionResponseV1 {
    ApproveOnce,
    Deny,
    Answer { text: String },
}

impl HarnessProviderInteractionResponseV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::ApproveOnce | Self::Deny => Ok(()),
            Self::Answer { text } => {
                if text.len() > HARNESS_ACP_INTERACTION_RESPONSE_MAX_BYTES
                    || contains_unsafe_control_bytes(text)
                {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
        }
    }
}

/// Authority for `HarnessOperatorRequestV1::AckMail`: the operator's own ack
/// of one message on behalf of `record_id`, applied through
/// `HarnessMutationV1::AckMail` with the operator as the operation's actor
/// (`gate4agent-harness-service::operator_ack_mail` resolves the mail-
/// permitted grant currently bound to `record_id` to satisfy that mutation's
/// own `grant_id` authority -- see that function's doc comment).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMailAckOperatorRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub record_id: HarnessSelectorV1,
    pub message_id: HarnessMailMessageId,
}

impl HarnessMailAckOperatorRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        self.record_id.validate()?;
        self.message_id.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorRequestV1 {
    MonitorGet { run_id: HarnessRunId },
    TimelineRead {
        run_id: HarnessRunId,
        after_sequence: Option<u64>,
        limit: u16,
    },
    TasksList {
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        /// "What did this task spawn": restricts the page to tasks whose
        /// own `parent_task_id` equals this one -- direct children only,
        /// never deeper descendants. Additive over the original bare
        /// `{ after_task_id, state, limit }` shape (`#[serde(default)]`
        /// keeps an older caller decoding), mirrors `HarnessReadRequestV1::
        /// TasksList`'s own addition of the same filter.
        #[serde(default)]
        parent_task_id: Option<HarnessTaskId>,
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    /// D5, Slice D: the operator's own per-task operations ledger --
    /// `HarnessEngine::operations_for_task`'s bounded, newest-first page,
    /// with the raw `HarnessActorV1` (the operator is already trusted with
    /// every other raw identity this wire exposes; the agent side never
    /// sees this, only the category -- see `HarnessOperationLedgerEntryV1`'s
    /// own doc comment).
    TaskOperations { task_id: HarnessTaskId, limit: u16 },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        /// "What did this run spawn": restricts the page to runs whose own
        /// `parent_run_id` equals this one. Additive (`#[serde(default)]`),
        /// mirrors `HarnessReadRequestV1::RunsList`'s own addition.
        #[serde(default)]
        parent_run_id: Option<HarnessRunId>,
        limit: u16,
    },
    RunGet { run_id: HarnessRunId },
    RunCorrelationGet { run_id: HarnessRunId },
    RunTransferGet { run_id: HarnessRunId },
    ReverseAttributionGet { subject: HarnessReverseAttributionSubjectV1 },
    ObserveRunContextSource { run_id: HarnessRunId },
    InspectRunWorkspace { run_id: HarnessRunId },
    ReadRunWorkspaceFile {
        run_id: HarnessRunId,
        path: HarnessRepositoryPathV1,
    },
    ReadRunGitHistory {
        run_id: HarnessRunId,
        path: Option<HarnessRepositoryPathV1>,
        before: Option<HarnessGitObjectIdV1>,
        limit: u16,
    },
    ReadRunGitDiff {
        run_id: HarnessRunId,
        mode: HarnessGitDiffModeV1,
        path: Option<HarnessRepositoryPathV1>,
    },
    // Node-scoped siblings of the `*RunWorkspace*`/`*RunGit*` family above:
    // routed straight from a node/workspace pair with no run binding, for
    // sidebar Files/Git reads that have no run in flight (harness-mode
    // parity with the direct-C2 TUI's `InspectWorkspace`).
    InspectNodeWorkspace { node_id: String, workspace_id: String },
    ReadNodeWorkspaceFile {
        node_id: String,
        workspace_id: String,
        path: HarnessRepositoryPathV1,
    },
    ReadNodeGitHistory {
        node_id: String,
        workspace_id: String,
        path: Option<HarnessRepositoryPathV1>,
        before: Option<HarnessGitObjectIdV1>,
        limit: u16,
    },
    ReadNodeGitDiff {
        node_id: String,
        workspace_id: String,
        mode: HarnessGitDiffModeV1,
        path: Option<HarnessRepositoryPathV1>,
    },
    // V11 write/create siblings of the four node-workspace read verbs above:
    // same direct node/workspace-pair routing (`exact_route`, no run
    // binding involved), just a write instead of a read -- the harness-mode
    // twin of the light TUI editor's save/create-file/create-directory
    // affordances. `WriteNodeWorkspaceFile` relays `NodeRequest::
    // WriteWorkspaceFile`: the node's own CAS write, gated on
    // `expected_revision` matching the file's current on-disk SHA-256. A
    // stale value comes back as the node's `RepositoryFileRevisionConflict`
    // failure code, which this wire surfaces as the typed `Conflict` host
    // error (see `map_node_workspace_write_error` in
    // `gate4agent-harness-service`), never collapsed into `Internal`.
    // `CreateNodeWorkspaceFile`/`CreateNodeWorkspaceDirectory` relay the
    // node's own two distinct creation verbs (`NodeRequest::
    // CreateWorkspaceFile`/`CreateWorkspaceDirectory`) -- the node models
    // file and directory creation as two separate requests, not one verb
    // with a kind flag, so this wire mirrors that shape exactly rather than
    // inventing a combined one.
    WriteNodeWorkspaceFile {
        node_id: String,
        workspace_id: String,
        path: HarnessRepositoryPathV1,
        content: String,
        expected_revision: HarnessWorkspaceFileRevisionV1,
    },
    CreateNodeWorkspaceFile {
        node_id: String,
        workspace_id: String,
        path: HarnessRepositoryPathV1,
    },
    CreateNodeWorkspaceDirectory {
        node_id: String,
        workspace_id: String,
        path: HarnessRepositoryPathV1,
    },
    LaunchPlansList {
        after_plan_id: Option<HarnessSelectorV1>,
        limit: u16,
    },
    TaskExecutionSpecGet { task_id: HarnessTaskId },
    /// `provider`/`workspace`/`plan_id` filter the derived plan catalogue,
    /// `after` pages it (same idiom as `LaunchPlansList`'s `after_plan_id`):
    /// the catalogue itself has no cap (see `HarnessTaskLaunchOptionsV1`'s
    /// own doc comment), only a page of it does. All four are additive over
    /// the original bare `{ task_id }` shape -- `#[serde(default)]` so an
    /// older caller that only ever sent `task_id` keeps deserializing.
    TaskLaunchOptionsGet {
        task_id: HarnessTaskId,
        #[serde(default)]
        provider: Option<HarnessSelectorV1>,
        #[serde(default)]
        workspace: Option<HarnessSelectorV1>,
        #[serde(default)]
        plan_id: Option<HarnessSelectorV1>,
        #[serde(default)]
        after: Option<HarnessSelectorV1>,
    },
    RuntimeInventoryList {
        after_node_id: Option<String>,
        limit: u16,
    },
    TerminalRead {
        session: HarnessRuntimeSessionAddressV1,
        after_sequence: Option<u64>,
        limit: u16,
    },
    /// D1/D8, Slice B: the operator's own bounded, redacted mailbox read for
    /// one session record -- the same `HarnessMailInboxPageV1` shape
    /// `g4a_mail_inbox` returns.
    MailInbox {
        record_id: HarnessSelectorV1,
        since_unix_ms: Option<u64>,
        limit: u16,
    },
    // Direct operator session verbs: unlike the CAS/task-mutation family
    // below (`CreateTask`..`StartTaskV2`, closed-set `HarnessOperatorActionV1`
    // shape), a session spawn/write/resize/stop has no task to CAS against --
    // it relays straight to C2 the same way `TerminalRead` reads straight
    // from C2, just as a write instead of a read. See the harness-service
    // host's async select loop (not `execute_operator_request`, which is
    // synchronous with no C2 handle in scope) for the dispatch.
    SpawnSession {
        node_id: String,
        workspace_id: String,
        provider: String,
        provider_profile: String,
        mode: HarnessExecutionModeV1,
        terminal_size: HarnessRuntimeTerminalSizeV1,
        // `None` means "the axis default" (`ApprovalLevel::FullAuto` --
        // not asking a human is the owner's stated norm and this field does
        // not change that default), so every caller that predates this
        // field, and every caller that simply does not care, is unchanged.
        // `Some(_)` is the one way `Moderate`/`ReadOnly`/`Unmanaged` becomes
        // reachable on a session spawned through this wire at all -- see
        // `gate4agent-node`'s `require_session_runtime_policy` doc and
        // `HostPolicy::Yolo`'s own doc for why `FullAuto` alone left the
        // whole permission-deferral path dead code in production.
        #[serde(default)]
        approval_level: Option<HarnessApprovalLevelV1>,
    },
    WriteSessionInput {
        session: HarnessRuntimeSessionAddressV1,
        text: String,
    },
    // `PromptSession` relays to `NodeRequest::Prompt`, the node's semantic-
    // prompt verb -- a DIFFERENT node request from the one immediately
    // above. `WriteSessionInput` relays to `NodeRequest::Input` (typed/raw
    // PTY text) and keeps meaning exactly that; this is an additional verb,
    // not a reinterpretation of it, the same way `PasteSession` relaying
    // `NodeRequest::Paste` is additional to it rather than a replacement.
    // Bounded by the same `HARNESS_SESSION_INPUT_MAX_BYTES` ceiling
    // `WriteSessionInput`/`PasteSession` already use (see that constant's
    // doc comment) -- the node checks all three against the same single
    // `MAX_NODE_TEXT_BYTES`, so there is no separate per-verb bound to pick.
    //
    // Admissible only against an ACP- or inline-transport session, and
    // refused BY NAME for a PTY-transport one -- unconditionally, even for
    // the rare PTY session whose provider profile currently admits a node-
    // level `SemanticPrompt` runtime policy (reachable today only through
    // `ResumeSessionRecord`'s own `initial_prompt`, a narrow capability for
    // typing exactly one prompt into a raw terminal at resume time, for a
    // provider CLI a human is watching). This is a deliberate, permanent
    // boundary the owner drew, not a limitation waiting to be lifted: a PTY
    // session has a human sitting at it, choosing what runs and watching
    // what appears, so what lands on that screen is the operator's
    // business, never this wire's. ACP and inline are the two transports
    // this project opens with nobody at a terminal, and prompting is their
    // ordinary interface -- `PromptSession` and `WriteSessionInput` must
    // never silently fall back to each other across that line.
    //
    // Enforced in `gate4agent-harness-service`, not here: this wire type
    // carries only a bare routing address (`HarnessRuntimeSessionAddressV1`
    // has no transport field of its own), and tightening the node's shared
    // `NodeRequest::Prompt` handler itself would also narrow the
    // `ResumeSessionRecord` capability above, which this change does not
    // touch. `gate4agent-harness-service` checks its own cached runtime
    // inventory before ever dispatching to C2/the node and refuses by name
    // (`HarnessOperatorHostErrorV1::UnsupportedTransport`) when that cache
    // already knows the target is PTY; the node's existing
    // `require_session_runtime_policy(SemanticPrompt)` gate remains the
    // backstop for the narrow window right after a spawn/resume where the
    // cache cannot yet prove the transport either way.
    PromptSession {
        session: HarnessRuntimeSessionAddressV1,
        text: String,
    },
    ResizeSession {
        session: HarnessRuntimeSessionAddressV1,
        terminal_size: HarnessRuntimeTerminalSizeV1,
    },
    StopSession {
        session: HarnessRuntimeSessionAddressV1,
        force: bool,
    },
    // V11 siblings of the four V10 session verbs above -- the same direct,
    // no-CAS C2 relay, just five more `NodeRequest` shapes the light TUI's
    // terminal already sends. `ControlSession` relays `NodeRequest::
    // TerminalControl` (special keys: Enter, Ctrl-C, arrows, ...).
    // `WriteSessionBytes` relays `NodeRequest::TerminalBytes` (a short raw
    // byte sequence, bounded well below `WriteSessionInput`'s text ceiling --
    // see `HARNESS_SESSION_BYTES_MAX_BYTES`). `PasteSession` relays
    // `NodeRequest::Paste`: a distinct node-level verb from `WriteSessionInput`
    // (the node frames it as a semantic bracketed paste, not plain typed
    // text), so it gets its own wire verb rather than folding into
    // `WriteSessionInput`. `RemoveSession` relays `NodeRequest::Remove`,
    // clearing an exited/failed session's binding from the node -- the same
    // node-side call a successful `StopSession` already fires as a best-effort
    // follow-up reap (see `HarnessC2Adapter::remove_stopped_session`'s doc
    // comment in `gate4agent-harness-service`), now exposed as its own
    // first-class verb for a session that reached that state on its own.
    // `ResumeSession` relays `NodeRequest::Resume` and, unlike `SpawnSession`,
    // acks dispatch only: the node never returns a new address for it (same
    // `instance_id`, generation bumped only once the resume actually settles,
    // reported through the runtime inventory rather than this reply).
    ControlSession {
        session: HarnessRuntimeSessionAddressV1,
        control: HarnessTerminalControlV1,
    },
    WriteSessionBytes {
        session: HarnessRuntimeSessionAddressV1,
        bytes: Vec<u8>,
    },
    PasteSession {
        session: HarnessRuntimeSessionAddressV1,
        text: String,
    },
    RemoveSession {
        session: HarnessRuntimeSessionAddressV1,
    },
    ResumeSession {
        session: HarnessRuntimeSessionAddressV1,
        terminal_size: HarnessRuntimeTerminalSizeV1,
    },
    // The four ACP control verbs, session-address-scoped exactly like the
    // eight session-control verbs above and relayed the same direct,
    // no-CAS way to C2/node -- gated node-side behind its own
    // `acp-control-v1` capability (a peer that predates it never negotiates
    // it and never sees these sent), so asking one of a PTY or inline
    // session is refused by name rather than silently accepted. They answer
    // what the outbound `SubscribeAgentStream` content stream reports (see
    // `HarnessOperatorAgentEventV1`), not what `SubscribeEvents`'s
    // task/run/node telemetry or `TimelineRead`'s `ObservationV1` stream
    // can: neither carries the question text, the option list, or the
    // mode/config/model catalogs these verbs act on
    // (`docs/gate4agent/plans/gate4agent-acp-control-plane-on-the-wire-2026-09-02.md`
    // §3-5). All four ack with a bare unit reply
    // (`InteractionResolved`/`SessionModeSet`/`SessionConfigOptionSet`/
    // `SessionModelSet`), the same shape `WriteSessionInput` acks with,
    // because the node itself acks all four with its own bare
    // `NodeResponse::Accepted`.
    //
    // `ResolveInteraction` answers the exact `correlation_id` the
    // observation stream already minted onto `ObservationKindV1::
    // ApprovalRequested`/`QuestionRequested`. `response` must additionally
    // match the interaction's own kind (`Approval` vs `Question`) -- that
    // check needs the live interaction the node holds and stays the node's
    // job, not this wire's, the same way `WriteSessionInput` never checks
    // that a PTY exists before sending.
    ResolveInteraction {
        session: HarnessRuntimeSessionAddressV1,
        correlation_id: String,
        response: HarnessProviderInteractionResponseV1,
    },
    SetSessionMode {
        session: HarnessRuntimeSessionAddressV1,
        mode_id: String,
    },
    SetSessionConfigOption {
        session: HarnessRuntimeSessionAddressV1,
        option_id: String,
        value_json: String,
    },
    SetSessionModel {
        session: HarnessRuntimeSessionAddressV1,
        model_id: String,
    },
    CatalogNativeSessions {
        route: HarnessNativeSessionRouteV1,
        limit: u16,
    },
    PageNativeSessions {
        route: HarnessNativeSessionRouteV1,
        window: HarnessNativeSessionCatalogWindowV1,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        after_selection_id: Option<String>,
        limit: u16,
    },
    PreviewNativeSession {
        selection: HarnessNativeSessionSelectionV1,
        message_limit: u16,
    },
    // Session-record family: node-scoped like `InspectNodeWorkspace`
    // (`node_id` resolved live via `exact_route`, no caller-pinned
    // incarnation), not session-address-scoped like the eight session-
    // control verbs above -- a managed session record has its own
    // `record_id` identity independent of whether it currently has a live
    // PTY bound to it. `PreviewSessionRecord` relays `NodeRequest::
    // PreviewSessionRecord` and rides the same read-worker pool as
    // `CatalogNativeSessions`/`PageNativeSessions`/`PreviewNativeSession`
    // above (`is_native_history_request` in `gate4agent-harness-service`) --
    // the light TUI sends this exact node request for both an initial
    // preview open and a background history refresh of an already-open
    // preview tab, so this wire stays a single verb the same way.
    PreviewSessionRecord {
        node_id: String,
        record_id: String,
        message_limit: u16,
    },
    // `ResumeSessionRecord`/`RenameSessionRecord`/`SetSessionTask`/
    // `ForgetSessionRecord`/`IndexProviderSession`/`IndexNativeSession` are
    // mutations, dispatched through the bounded session-record-mutation
    // worker pool (`is_session_record_mutation_request`) the same way the
    // eight session-control verbs use their own pool -- unlike those eight,
    // each of these six carries a distinct, non-`Accepted` node reply (a
    // managed-session record, or for `ResumeSessionRecord` a record plus the
    // freshly spawned session address), so the reply is relayed back
    // correlated rather than collapsed into one shared ack shape. Unlike
    // `ResumeSession` (existing live session, ack-only, generation bump
    // reported through the runtime inventory), `ResumeSessionRecord` spawns
    // a *new* session from a dormant record and the node returns that
    // session's address synchronously -- this wire mirrors that exactly.
    ResumeSessionRecord {
        node_id: String,
        record_id: String,
        terminal_size: HarnessRuntimeTerminalSizeV1,
        initial_prompt: Option<String>,
    },
    RenameSessionRecord {
        node_id: String,
        record_id: String,
        display_name: String,
    },
    SetSessionTask {
        node_id: String,
        record_id: String,
        expected_revision: u64,
        target: HarnessSessionTaskTargetV1,
    },
    ForgetSessionRecord {
        node_id: String,
        record_id: String,
    },
    IndexProviderSession {
        node_id: String,
        workspace_id: String,
        provider: String,
        identity: HarnessProviderSessionIdentityV1,
        display_name: String,
    },
    // Unlike the five siblings above, routed via `selection.route` (same
    // caller-pinned-incarnation shape `CatalogNativeSessions`/
    // `PageNativeSessions`/`PreviewNativeSession` already use) rather than a
    // bare `node_id`: the selection being committed into a durable record
    // was chosen from a previously fetched native-session catalog page, so
    // it carries its own route already. Still dispatched through the
    // session-record-mutation pool, not the native-history read pool --
    // it mutates the managed-session store, it does not read a catalog.
    IndexNativeSession {
        selection: HarnessNativeSessionSelectionV1,
        display_name: String,
    },
    CreateTask { request: HarnessCreateTaskRequestV1 },
    ReplaceTask { request: HarnessReplaceTaskRequestV1 },
    MoveTask { request: HarnessMoveTaskRequestV1 },
    CancelTask { request: HarnessCancelTaskRequestV1 },
    RetryTask { request: HarnessRetryTaskRequestV1 },
    ScheduleNext { request: HarnessScheduleNextRequestV1 },
    ReplaceTaskExecutionSpec { request: HarnessReplaceTaskExecutionSpecRequestV1 },
    StartTask { request: HarnessStartTaskRequestV1 },
    ReplaceTaskExecutionSpecV2 { request: HarnessReplaceTaskExecutionSpecRequestV2 },
    StartTaskV2 { request: HarnessStartTaskRequestV2 },
    /// The operator's own ack (D1, Slice B): an operator act, applied
    /// through the same `HarnessMutationV1::AckMail` mutation an agent's own
    /// `g4a_mail_ack` uses -- not a distinct authority channel, so it is not
    /// routed through `HarnessOperatorActionV1`/`SubmitIntent` the way the
    /// task-mutation family above is; there is no task to CAS against, the
    /// same reason the direct session-control verbs below skip that layer
    /// too.
    AckMail { request: HarnessMailAckOperatorRequestV1 },
    SubmitIntent { intent: HarnessOperatorIntentV1 },
    // Node-scoped, paged host-directory listing behind the folder-browser
    // dialog: the harness-mode sibling of the light TUI's own
    // `AppAction::BrowseHostDirectories`, relaying `NodeRequest::
    // BrowseHostDirectories` directly. No `workspace_id`: unlike every read
    // above, a host-directory browse targets the node's host filesystem, not
    // a registered workspace, so it does not fit `InspectNodeWorkspace`'s
    // family (see `PreparedHostDirectoryBrowse`'s doc comment in
    // `gate4agent-harness-service` for why this rides its own bounded read
    // lane rather than that family's). `directory: None` opens the node's
    // default root; `after` pages a previously returned `next_after` cursor.
    BrowseHostDirectories {
        node_id: String,
        directory: Option<HarnessHostPathV1>,
        after: Option<HarnessHostPathV1>,
    },
    // `RegisterWorkspace`/`UnregisterWorkspace`/`CreateStandaloneWorkspace`/
    // `CreateWorktree`/`RemoveWorktree`/`ExportContextPack`/
    // `ForgetContextPack` are the resource-mutation family: workspace/
    // worktree lifecycle plus context-pack export/forget, dispatched through
    // the bounded resource-mutation worker pool (`is_resource_mutation_
    // request` in `gate4agent-harness-service`) the same way the session-
    // record family uses its own pool -- see `ResourceMutationKind`'s doc
    // comment there for why these seven heterogeneous verbs share one lane.
    // `RegisterWorkspace`'s success (like every workspace/worktree lifecycle
    // verb in this family) adds an entry to the node's `workspaces` map,
    // which the runtime inventory's own roster caches
    // (`HarnessRuntimeInventoryV1::workspaces`) -- the host invalidates that
    // route's cached entry unconditionally on success, so a harness-mode
    // sidebar converges the same way it does after a session-record
    // mutation.
    RegisterWorkspace {
        node_id: String,
        workspace_id: String,
        root: HarnessHostPathV1,
    },
    UnregisterWorkspace {
        node_id: String,
        workspace_id: String,
    },
    CreateStandaloneWorkspace {
        node_id: String,
        workspace_id: String,
        root: HarnessHostPathV1,
        initial_branch: Option<String>,
    },
    CreateWorktree {
        node_id: String,
        source_workspace_id: String,
        workspace_id: String,
        target_root: HarnessHostPathV1,
        branch: String,
        base: Option<String>,
    },
    RemoveWorktree {
        node_id: String,
        source_workspace_id: String,
        target_root: HarnessHostPathV1,
    },
    // Session-address-scoped, not node-id-scoped like the five workspace/
    // worktree verbs above: relays `NodeRequest::ExportContextPack` exactly
    // the way the eight session-control verbs relay their own node requests
    // from a `HarnessRuntimeSessionAddressV1` -- the node/incarnation this
    // targets comes from `session.node_id`/`session.incarnation_id`, so
    // there is no separate `node_id` field. Reuses
    // `HarnessResolvedContextPackReceiptV1` (already defined for the A2 run-
    // continuation arc) as its reply verbatim rather than inventing a
    // duplicate: the node's own `ResolvedContextPackReceipt` is the exact
    // same payload shape either way `ExportContextPack`/
    // `ExportContextPackForSessionRecord` produces it.
    ExportContextPack {
        session: HarnessRuntimeSessionAddressV1,
    },
    // Node-scoped like the five workspace/worktree verbs above, not session-
    // address-scoped like `ExportContextPack`: forgetting an exported
    // context pack from the node's `ContextPackStore` needs only the node
    // and the pack's own id, no session or incarnation binding.
    ForgetContextPack {
        node_id: String,
        context_id: HarnessSelectorV1,
    },
    // Opens a long-lived, server-push subscription instead of the usual
    // one-shot request/reply: see the framing note on `HarnessOperatorEventV1`
    // for the wire shape this switches the connection into.
    SubscribeEvents {},
    // Same "opens a long-lived, server-push subscription" framing as
    // SubscribeEvents immediately above, on its own connection: see
    // `HarnessOperatorTerminalEventV1`'s doc comment for why terminal
    // frames do not ride SubscribeEvents's own queue. `sessions` is
    // declared once, at subscribe time -- this wire has no representable
    // second client-to-host message (see the module doc), so a client
    // that opens or closes a pane reconnects with the updated list rather
    // than patching an existing subscription.
    SubscribeTerminal { sessions: Vec<HarnessRuntimeSessionAddressV1> },
    // Same "opens a long-lived, server-push subscription" framing as
    // SubscribeTerminal immediately above, on its own connection, for the
    // ACP agent-content stream (node-side `agent-stream-events-v1`) rather
    // than terminal frames -- see `HarnessOperatorAgentEventV1`'s doc
    // comment for why that push channel needs its own event type distinct
    // from both `HarnessOperatorEventV1` and `HarnessOperatorTerminalEventV1`.
    // `sessions` is declared once, at subscribe time, for the same reason
    // SubscribeTerminal's is (see the module doc): a client that opens or
    // closes a pane reconnects with the updated list rather than patching
    // an existing subscription.
    SubscribeAgentStream { sessions: Vec<HarnessRuntimeSessionAddressV1> },
}

impl HarnessOperatorRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::MonitorGet { run_id } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::TimelineRead { run_id, after_sequence, limit } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if after_sequence == &Some(0) {
                    return Err(HarnessOperatorApiError::InvalidCursor);
                }
                validate_operator_timeline_limit(*limit)
            }
            Self::TasksList { after_task_id, parent_task_id, limit, .. } => {
                if let Some(task_id) = after_task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(task_id) = parent_task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                validate_operator_limit(*limit)
            }
            Self::TaskGet { task_id } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::TaskOperations { task_id, limit } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                validate_operator_limit(*limit)
            }
            Self::RunsList { task_id, after_run_id, parent_run_id, limit, .. } => {
                if let Some(task_id) = task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(run_id) = after_run_id {
                    run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(run_id) = parent_run_id {
                    run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                validate_operator_limit(*limit)
            }
            Self::RunGet { run_id }
            | Self::RunCorrelationGet { run_id }
            | Self::RunTransferGet { run_id }
            | Self::ObserveRunContextSource { run_id }
            | Self::InspectRunWorkspace { run_id } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ReverseAttributionGet { subject } => subject.validate(),
            Self::ReadRunWorkspaceFile { run_id, path } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                path.validate()
            }
            Self::ReadRunGitHistory { run_id, path, before, limit } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if let Some(path) = path { path.validate()?; }
                if let Some(before) = before { before.validate()?; }
                if !(1..=HARNESS_GIT_HISTORY_LIMIT_MAX).contains(limit) {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                Ok(())
            }
            Self::ReadRunGitDiff { run_id, mode, path } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                mode.validate()?;
                if let Some(path) = path { path.validate()?; }
                Ok(())
            }
            Self::InspectNodeWorkspace { node_id, workspace_id } => {
                validate_node_workspace_route(node_id, workspace_id)
            }
            Self::ReadNodeWorkspaceFile { node_id, workspace_id, path } => {
                validate_node_workspace_route(node_id, workspace_id)?;
                path.validate()
            }
            Self::ReadNodeGitHistory { node_id, workspace_id, path, before, limit } => {
                validate_node_workspace_route(node_id, workspace_id)?;
                if let Some(path) = path { path.validate()?; }
                if let Some(before) = before { before.validate()?; }
                if !(1..=HARNESS_GIT_HISTORY_LIMIT_MAX).contains(limit) {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                Ok(())
            }
            Self::ReadNodeGitDiff { node_id, workspace_id, mode, path } => {
                validate_node_workspace_route(node_id, workspace_id)?;
                mode.validate()?;
                if let Some(path) = path { path.validate()?; }
                Ok(())
            }
            Self::WriteNodeWorkspaceFile {
                node_id, workspace_id, path, content, expected_revision,
            } => {
                validate_node_workspace_route(node_id, workspace_id)?;
                path.validate()?;
                expected_revision.validate()?;
                if content.len() > HARNESS_WORKSPACE_FILE_MAX_BYTES {
                    return Err(HarnessOperatorApiError::InvalidWorkspaceFile);
                }
                Ok(())
            }
            Self::CreateNodeWorkspaceFile { node_id, workspace_id, path }
            | Self::CreateNodeWorkspaceDirectory { node_id, workspace_id, path } => {
                validate_node_workspace_route(node_id, workspace_id)?;
                path.validate()
            }
            Self::LaunchPlansList { after_plan_id, limit } => {
                if let Some(plan_id) = after_plan_id {
                    plan_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if !(1..=HARNESS_LAUNCH_PLAN_PAGE_LIMIT_MAX).contains(limit) {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                Ok(())
            }
            Self::TaskExecutionSpecGet { task_id } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::TaskLaunchOptionsGet { task_id, provider, workspace, plan_id, after } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if let Some(provider) = provider {
                    provider.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(workspace) = workspace {
                    workspace.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(plan_id) = plan_id {
                    plan_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(after) = after {
                    after.validate().map_err(|_| HarnessOperatorApiError::InvalidCursor)?;
                }
                Ok(())
            }
            Self::RuntimeInventoryList { after_node_id, limit } => {
                if !(1..=HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX).contains(limit) {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                if after_node_id.as_deref().is_some_and(|node_id| {
                    !valid_runtime_id(node_id, 128)
                }) {
                    return Err(HarnessOperatorApiError::InvalidCursor);
                }
                Ok(())
            }
            Self::TerminalRead { session, limit, .. } => {
                // after_sequence is unconstrained: node terminal sequences are
                // 0-based, so Some(0) legitimately means "after frame 0".
                session.validate()?;
                validate_operator_terminal_limit(*limit)
            }
            Self::MailInbox { record_id, limit, .. } => {
                record_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                validate_operator_limit(*limit)
            }
            Self::AckMail { request } => request.validate().map_err(HarnessOperatorApiError::Protocol),
            Self::SpawnSession { node_id, workspace_id, provider, provider_profile, terminal_size, .. } => {
                validate_node_workspace_route(node_id, workspace_id)?;
                if !valid_runtime_id(provider, 128) || !valid_runtime_id(provider_profile, 128) {
                    return Err(HarnessOperatorApiError::InvalidSessionSpawn);
                }
                if terminal_size.rows == 0 || terminal_size.columns == 0 {
                    return Err(HarnessOperatorApiError::InvalidSessionSpawn);
                }
                Ok(())
            }
            Self::WriteSessionInput { session, text } => {
                session.validate()?;
                if text.len() > HARNESS_SESSION_INPUT_MAX_BYTES {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            // Same bound as `WriteSessionInput` immediately above -- see
            // `PromptSession`'s own doc comment for why this is a separate
            // verb rather than a flag on that one, and why the PTY-transport
            // refusal it also carries lives in `gate4agent-harness-service`
            // rather than here: this type alone cannot see the target
            // session's transport.
            Self::PromptSession { session, text } => {
                session.validate()?;
                if text.len() > HARNESS_SESSION_INPUT_MAX_BYTES {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::ResizeSession { session, terminal_size } => {
                session.validate()?;
                if terminal_size.rows == 0 || terminal_size.columns == 0 {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::StopSession { session, .. } => session.validate(),
            Self::ControlSession { session, .. } => session.validate(),
            Self::WriteSessionBytes { session, bytes } => {
                session.validate()?;
                if bytes.is_empty() || bytes.len() > HARNESS_SESSION_BYTES_MAX_BYTES {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::PasteSession { session, text } => {
                session.validate()?;
                if text.len() > HARNESS_SESSION_INPUT_MAX_BYTES {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::RemoveSession { session } => session.validate(),
            Self::ResumeSession { session, terminal_size } => {
                session.validate()?;
                if terminal_size.rows == 0 || terminal_size.columns == 0 {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::ResolveInteraction { session, correlation_id, response } => {
                session.validate()?;
                if !valid_agent_stream_correlation_id(correlation_id) {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                response.validate()
            }
            Self::SetSessionMode { session, mode_id } => {
                session.validate()?;
                if !valid_agent_stream_id(mode_id) {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::SetSessionConfigOption { session, option_id, value_json } => {
                session.validate()?;
                if !valid_agent_stream_id(option_id)
                    || value_json.is_empty()
                    || value_json.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                    || contains_unsafe_control_bytes(value_json)
                {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::SetSessionModel { session, model_id } => {
                session.validate()?;
                if !valid_agent_stream_id(model_id) {
                    return Err(HarnessOperatorApiError::InvalidSessionControl);
                }
                Ok(())
            }
            Self::CatalogNativeSessions { route, limit } => {
                route.validate()?;
                validate_native_session_catalog_limit(*limit)
            }
            Self::PageNativeSessions {
                route,
                catalog_revision,
                after_selection_id,
                limit,
                ..
            } => {
                route.validate()?;
                if *catalog_revision == 0
                    || after_selection_id.as_deref().is_some_and(|value| {
                        !valid_native_selection_id(value)
                    })
                {
                    return Err(HarnessOperatorApiError::InvalidNativeHistory);
                }
                validate_native_session_catalog_limit(*limit)
            }
            Self::PreviewNativeSession { selection, message_limit } => {
                selection.validate()?;
                if !(1..=HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
                    .contains(message_limit)
                {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                Ok(())
            }
            Self::PreviewSessionRecord { node_id, record_id, message_limit } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(record_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                if !(1..=HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
                    .contains(message_limit)
                {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                Ok(())
            }
            Self::ResumeSessionRecord { node_id, record_id, terminal_size, initial_prompt } => {
                if !valid_runtime_id(node_id, 128)
                    || !valid_runtime_id(record_id, 128)
                    || terminal_size.rows == 0
                    || terminal_size.columns == 0
                {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                if initial_prompt.as_deref().is_some_and(|prompt| {
                    prompt.is_empty() || prompt.len() > HARNESS_BODY_MAX_BYTES
                }) {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                Ok(())
            }
            Self::RenameSessionRecord { node_id, record_id, display_name } => {
                if !valid_runtime_id(node_id, 128)
                    || !valid_runtime_id(record_id, 128)
                    || display_name.is_empty()
                    || display_name.len() > HARNESS_SESSION_RECORD_DISPLAY_NAME_MAX_BYTES
                    || display_name.chars().any(char::is_control)
                {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                Ok(())
            }
            Self::SetSessionTask { node_id, record_id, target, .. } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(record_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                target.validate()
            }
            Self::ForgetSessionRecord { node_id, record_id } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(record_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                Ok(())
            }
            Self::IndexProviderSession { node_id, workspace_id, provider, identity, display_name } => {
                if !valid_runtime_id(node_id, 128)
                    || !valid_runtime_id(workspace_id, 128)
                    || !valid_runtime_id(provider, 128)
                    || display_name.is_empty()
                    || display_name.len() > HARNESS_SESSION_RECORD_DISPLAY_NAME_MAX_BYTES
                    || display_name.chars().any(char::is_control)
                {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                identity.validate()
            }
            Self::IndexNativeSession { selection, display_name } => {
                selection.validate()?;
                if display_name.is_empty()
                    || display_name.len() > HARNESS_SESSION_RECORD_DISPLAY_NAME_MAX_BYTES
                    || display_name.chars().any(char::is_control)
                {
                    return Err(HarnessOperatorApiError::InvalidSessionRecordRequest);
                }
                Ok(())
            }
            Self::CreateTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ReplaceTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::MoveTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::CancelTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::RetryTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ScheduleNext { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ReplaceTaskExecutionSpec { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::StartTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ReplaceTaskExecutionSpecV2 { request } => request.validate(),
            Self::StartTaskV2 { request } => request.validate(),
            Self::SubmitIntent { intent } => intent.validate(),
            Self::BrowseHostDirectories { node_id, directory, after } => {
                if !valid_runtime_id(node_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidHostDirectoryBrowseRequest);
                }
                if let Some(directory) = directory { directory.validate()?; }
                if let Some(after) = after { after.validate()?; }
                Ok(())
            }
            Self::RegisterWorkspace { node_id, workspace_id, root } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(workspace_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                root.validate()
            }
            Self::UnregisterWorkspace { node_id, workspace_id } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(workspace_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                Ok(())
            }
            Self::CreateStandaloneWorkspace { node_id, workspace_id, root, initial_branch } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(workspace_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                root.validate()?;
                if let Some(branch) = initial_branch {
                    if !valid_native_single_line(branch, HARNESS_REPOSITORY_PATH_MAX_BYTES, true) {
                        return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                    }
                }
                Ok(())
            }
            Self::CreateWorktree {
                node_id, source_workspace_id, workspace_id, target_root, branch, base,
            } => {
                if !valid_runtime_id(node_id, 128)
                    || !valid_runtime_id(source_workspace_id, 128)
                    || !valid_runtime_id(workspace_id, 128)
                    || !valid_native_single_line(branch, HARNESS_REPOSITORY_PATH_MAX_BYTES, true)
                {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                if let Some(base) = base {
                    if !valid_native_single_line(base, HARNESS_REPOSITORY_PATH_MAX_BYTES, true) {
                        return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                    }
                }
                target_root.validate()
            }
            Self::RemoveWorktree { node_id, source_workspace_id, target_root } => {
                if !valid_runtime_id(node_id, 128) || !valid_runtime_id(source_workspace_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                target_root.validate()
            }
            Self::ExportContextPack { session } => session.validate(),
            Self::ForgetContextPack { node_id, context_id } => {
                if !valid_runtime_id(node_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                context_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::SubscribeEvents {} => Ok(()),
            Self::SubscribeTerminal { sessions } => {
                if sessions.is_empty()
                    || sessions.len() > HARNESS_TERMINAL_SUBSCRIPTION_SESSIONS_MAX
                {
                    return Err(HarnessOperatorApiError::InvalidTerminalPage);
                }
                for session in sessions { session.validate()?; }
                let mut seen = std::collections::HashSet::with_capacity(sessions.len());
                if !sessions.iter().all(|session| seen.insert(session)) {
                    return Err(HarnessOperatorApiError::InvalidTerminalPage);
                }
                Ok(())
            }
            Self::SubscribeAgentStream { sessions } => {
                if sessions.is_empty()
                    || sessions.len() > HARNESS_AGENT_STREAM_SUBSCRIPTION_SESSIONS_MAX
                {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                for session in sessions { session.validate()?; }
                let mut seen = std::collections::HashSet::with_capacity(sessions.len());
                if !sessions.iter().all(|session| seen.insert(session)) {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                Ok(())
            }
        }
    }

}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorReplyV1 {
    Ok { response: HarnessOperatorResponseV1 },
    Error { error: HarnessOperatorHostErrorV1 },
}

impl HarnessOperatorReplyV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Ok { response } => response.validate(),
            Self::Error { .. } => Ok(()),
        }
    }
}

/// Long-lived, server-push counterpart to `HarnessOperatorReplyV1`. A
/// `SubscribeEvents` connection stops replying with a single frame and
/// instead writes one of these, newline-terminated, per change -- see the
/// module doc for the framing this switches the connection into.
/// `sequence` is a per-subscription monotonic counter the host mints for
/// that one connection alone; it never resets, including across a
/// `Lagged`/`SnapshotBaseline` pair, so a client can detect a gap without
/// cross-referencing wall time. An auth failure or an over-limit subscribe
/// never reaches this type at all -- both still use the ordinary
/// single-frame `HarnessOperatorReplyV1::Error` before the connection
/// closes, since neither one ever admits a subscription in the first place.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorEventV1 {
    /// Always the first frame after a successful subscribe, and re-sent as
    /// the recovery frame immediately after a `Lagged` -- the client must
    /// treat this as a full replacement of its task/run/node state, never a
    /// delta against whatever it held before.
    SnapshotBaseline {
        sequence: u64,
        tasks: Vec<RedactedTaskV1>,
        runs: Vec<RedactedRunV1>,
        nodes: Vec<HarnessRuntimeNodeInventoryV1>,
    },
    TaskChanged { sequence: u64, task: RedactedTaskV1 },
    RunChanged { sequence: u64, run: RedactedRunV1 },
    RuntimeInventoryChanged { sequence: u64, node: HarnessRuntimeNodeInventoryV1 },
    RuntimeInventoryRemoved { sequence: u64, node_id: String },
    /// This subscriber's outbound queue overflowed and one or more events
    /// were dropped for it specifically -- other subscribers are unaffected.
    /// The host always follows this with a fresh `SnapshotBaseline` once it
    /// can build one; until that frame arrives the client must treat every
    /// task/run/node it is holding as stale rather than trying to patch
    /// around the gap.
    Lagged { sequence: u64 },
    /// Server-side keep-alive, unconditional and periodic (see
    /// `gate4agent-harness-service::runtime`'s own
    /// `HOST_SUBSCRIBER_KEEPALIVE_INTERVAL`): the registry's only way to
    /// discover a dead subscriber is a write to it actually failing, so
    /// without something to push on an otherwise-idle connection an
    /// abandoned subscriber can sit occupying its slot indefinitely (see
    /// `SubscriberRegistry`'s own doc comment). Deliberately its own
    /// variant rather than reusing an existing one: `Lagged` carries a
    /// promise (a `SnapshotBaseline` follows) that a bare keep-alive would
    /// break, and every other variant carries real task/run/node state a
    /// client applies -- faking either risks a client acting on a change
    /// that never happened. `Ping` carries nothing to act on, so it cannot
    /// be mistaken for either; every current reader
    /// (`gate4agent-harness-client`'s generic `next_event` decode,
    /// `gate4agent-tui`'s `project_harness_operator_event`) drops it on the
    /// floor by construction. `sequence` is kept only for schema symmetry
    /// with every other variant -- there is no gap to detect against a
    /// keep-alive that carries no state of its own.
    Ping { sequence: u64 },
}

impl HarnessOperatorEventV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::SnapshotBaseline { tasks, runs, nodes, .. } => {
                for task in tasks { task.validate().map_err(HarnessOperatorApiError::Read)?; }
                if tasks.windows(2).any(|pair| pair[0].task_id >= pair[1].task_id) {
                    return Err(HarnessOperatorApiError::Read(HarnessReadApiError::InvalidCollection));
                }
                for run in runs { run.validate().map_err(HarnessOperatorApiError::Read)?; }
                if runs.windows(2).any(|pair| pair[0].run_id >= pair[1].run_id) {
                    return Err(HarnessOperatorApiError::Read(HarnessReadApiError::InvalidCollection));
                }
                for node in nodes { node.validate()?; }
                if nodes.windows(2).any(|pair| pair[0].node_id >= pair[1].node_id) {
                    return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
                }
                Ok(())
            }
            Self::TaskChanged { task, .. } => task.validate().map_err(HarnessOperatorApiError::Read),
            Self::RunChanged { run, .. } => run.validate().map_err(HarnessOperatorApiError::Read),
            Self::RuntimeInventoryChanged { node, .. } => node.validate(),
            Self::RuntimeInventoryRemoved { node_id, .. } => {
                if !valid_runtime_id(node_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
                }
                Ok(())
            }
            Self::Lagged { .. } => Ok(()),
            Self::Ping { .. } => Ok(()),
        }
    }
}

/// Long-lived, server-push counterpart to a `SubscribeTerminal` connection --
/// see `HarnessOperatorEventV1`'s own doc comment for the shared framing this
/// also uses. Deliberately its own type, not a variant of
/// `HarnessOperatorEventV1`: that type's `Lagged`/`SnapshotBaseline` pair
/// promises a full task/run/node resync on overflow, which is both wrong for
/// a terminal frame (there is no "task/run/node" to resync, only a screen
/// that is already self-contained) and unaffordable at terminal-frame volume
/// (see `TerminalSubscriberRegistry`'s own doc comment for the coalescing
/// discipline this type's `coalesced_since_last` field reports on).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorTerminalEventV1 {
    /// Sent once per session immediately after a successful subscribe (the
    /// tail of whatever `TerminalBufferRegistry` already holds, or nothing
    /// if the ring is empty for that session -- a legitimate state for a
    /// pane with no output yet), and again every time that session's ring
    /// gains a strictly newer frame. Always a full screen, never a delta
    /// (see backlog item 4) -- a client that missed intermediate frames is
    /// already caught up the moment this one arrives.
    TerminalFrame {
        sequence: u64,
        session: HarnessRuntimeSessionAddressV1,
        frame: HarnessRuntimeTerminalFrameV1,
        /// How many frames for THIS session were coalesced away (replaced
        /// before ever being sent) since the last frame actually delivered
        /// on this subscription. 0 on every frame delivered without
        /// contention, including the very first. Diagnostic only.
        coalesced_since_last: u32,
    },
    /// Same keep-alive rationale as `HarnessOperatorEventV1::Ping`: this is
    /// a physically separate connection with its own dead-peer-detection
    /// problem, independent of the task/run/node subscription's.
    Ping { sequence: u64 },
}

impl HarnessOperatorTerminalEventV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::TerminalFrame { session, frame, .. } => {
                session.validate()?;
                frame.validate()
            }
            Self::Ping { .. } => Ok(()),
        }
    }
}

/// One named catalog entry -- a selectable session mode or model on the
/// `ModeCatalog`/`ModelCatalog` chunk kinds. Exact mirror of
/// `gate4agent-node-protocol`'s own `AgentStreamNamedIdV1` (not a
/// `gate4agent_types` type -- it is minted on the node's own wire contract,
/// same duplication rationale as `HarnessTerminalControlV1`). `id` is what
/// `SetSessionMode`/`SetSessionModel` take back.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAgentStreamNamedIdV1 {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// One selectable answer to an `InteractionPrompt` chunk -- an ACP
/// permission option, named by the provider rather than invented here.
/// Exact mirror of `gate4agent-node-protocol`'s own
/// `AgentStreamInteractionOptionV1`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAgentStreamInteractionOptionV1 {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

/// Exact mirror of `gate4agent_types::ProviderConfigOptionKind`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessProviderConfigOptionKindV1 {
    Select,
    Boolean,
    Unknown,
}

/// Exact mirror of `gate4agent_types::ProviderConfigChoice`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProviderConfigChoiceV1 {
    pub value_json: String,
    pub label: Option<String>,
}

/// Exact mirror of `gate4agent_types::ProviderConfigOption`: the mechanism
/// ACP uses to change model, reasoning effort, and similar settings.
/// `ConfigOptions` always carries the FULL current set, never a delta --
/// same contract as the source type's own doc comment states for
/// `ProviderEvent::ConfigOptionsUpdated`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessProviderConfigOptionV1 {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub kind: HarnessProviderConfigOptionKindV1,
    pub value_json: String,
    pub choices: Vec<HarnessProviderConfigChoiceV1>,
}

/// Exact mirror of `gate4agent_observation_protocol::BlockAuthorityV1`: this
/// crate has no dependency on `gate4agent-observation-protocol` (see the
/// doc comment on `HarnessTerminalControlV1`), so the WHO/WHAT-blocked-it
/// vocabulary `HarnessAgentStreamChunkKindV1::Blocked` carries is duplicated
/// here as its own closed wire enum rather than imported, the same way
/// `HarnessProviderInteractionKindV1` mirrors `gate4agent_types::
/// ProviderInteractionKind`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessBlockAuthorityV1 {
    HarnessGate,
    HarnessPolicy,
    HarnessDeadline,
    Operator,
    ProviderClassifier,
    ProviderPermissionRule,
    ProviderSandbox,
    ProviderRefusal,
    ProviderHook,
    UserRejected,
    /// Mirrors `gate4agent_observation_protocol::BlockAuthorityV1::
    /// ProviderQuota` -- the provider's own account/plan quota, rate
    /// limit, or usage cap was exhausted.
    ProviderQuota,
    Unknown,
}

/// The kind of a single `HarnessAgentStreamChunkV1` -- content the operator
/// needs to act on a running ACP session: what the agent is saying, a
/// pending interaction it needs answered, and the catalogs the three ACP
/// setter verbs (`SetSessionMode`, `SetSessionConfigOption`,
/// `SetSessionModel`) operate over. Exact mirror of
/// `gate4agent-node-protocol`'s own `AgentStreamChunkKindV1`, which itself
/// mirrors `gate4agent_types::ProviderEvent`'s content variants
/// deliberately, in contrast to the collapsed `ObservationKindV1` telemetry
/// stream, which carries none of it
/// (`docs/gate4agent/plans/gate4agent-acp-control-plane-on-the-wire-2026-09-02.md`
/// §3-4).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessAgentStreamChunkKindV1 {
    Text { text: String, is_delta: bool },
    Thinking { text: String },
    /// The same `correlation_id` `ObservationKindV1::ApprovalRequested`/
    /// `QuestionRequested` already minted, plus the question and option
    /// list `ResolveInteraction` answers blind without.
    InteractionPrompt {
        correlation_id: String,
        interaction_kind: HarnessProviderInteractionKindV1,
        tool_name: String,
        title: Option<String>,
        prompt: String,
        options: Vec<HarnessAgentStreamInteractionOptionV1>,
    },
    ModeCatalog {
        current: Option<String>,
        available: Vec<HarnessAgentStreamNamedIdV1>,
    },
    ConfigOptions { options: Vec<HarnessProviderConfigOptionV1> },
    ModelCatalog {
        current: Option<String>,
        available: Vec<HarnessAgentStreamNamedIdV1>,
    },
    /// The chunk twin of `ObservationKindV1::ActionBlocked`, delivered on
    /// the agent content stream instead of (never in place of) the
    /// observation channel -- see `gate4agent-node-protocol`'s own
    /// `AgentStreamChunkKindV1::Blocked` doc comment for why. An EVENT, not
    /// state: never seeded to a fresh subscriber, exactly like `Text`/
    /// `Thinking`.
    Blocked {
        correlation_id: Option<String>,
        tool_class: String,
        authority: HarnessBlockAuthorityV1,
        reason_kind: Option<String>,
        reason: String,
        help: Option<String>,
    },
}

impl HarnessAgentStreamChunkKindV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Text { text, .. } | Self::Thinking { text } => {
                if text.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                    || contains_unsafe_control_bytes(text)
                {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                Ok(())
            }
            Self::InteractionPrompt { correlation_id, tool_name, title, prompt, options, .. } => {
                if !valid_agent_stream_correlation_id(correlation_id) {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                if !valid_agent_stream_id(tool_name) {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                if let Some(title) = title {
                    if title.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                        || contains_unsafe_control_bytes(title)
                    {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                }
                if prompt.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                    || contains_unsafe_control_bytes(prompt)
                {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                if options.len() > HARNESS_AGENT_STREAM_INTERACTION_OPTIONS_MAX {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                for option in options {
                    if !valid_agent_stream_id(&option.option_id)
                        || !valid_agent_stream_id(&option.name)
                        || !valid_agent_stream_id(&option.kind)
                    {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                }
                Ok(())
            }
            Self::ModeCatalog { current, available } | Self::ModelCatalog { current, available } => {
                if let Some(current) = current {
                    if !valid_agent_stream_id(current) {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                }
                validate_agent_stream_catalog_entries(available)
            }
            Self::ConfigOptions { options } => {
                if options.len() > HARNESS_AGENT_STREAM_CATALOG_ENTRIES_MAX {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                for option in options {
                    if !valid_agent_stream_id(&option.id) || !valid_agent_stream_id(&option.name) {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                    if let Some(description) = &option.description {
                        if description.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                            || contains_unsafe_control_bytes(description)
                        {
                            return Err(HarnessOperatorApiError::InvalidAgentStream);
                        }
                    }
                    if let Some(category) = &option.category {
                        if !valid_agent_stream_id(category) {
                            return Err(HarnessOperatorApiError::InvalidAgentStream);
                        }
                    }
                    if option.value_json.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                        || contains_unsafe_control_bytes(&option.value_json)
                    {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                    if option.choices.len() > HARNESS_AGENT_STREAM_INTERACTION_OPTIONS_MAX {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                    for choice in &option.choices {
                        if choice.value_json.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                            || contains_unsafe_control_bytes(&choice.value_json)
                        {
                            return Err(HarnessOperatorApiError::InvalidAgentStream);
                        }
                        if let Some(label) = &choice.label {
                            if label.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                                || contains_unsafe_control_bytes(label)
                            {
                                return Err(HarnessOperatorApiError::InvalidAgentStream);
                            }
                        }
                    }
                }
                Ok(())
            }
            Self::Blocked { correlation_id, tool_class, reason_kind, reason, help, .. } => {
                if let Some(correlation_id) = correlation_id {
                    if !valid_agent_stream_correlation_id(correlation_id) {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                }
                if !valid_agent_stream_id(tool_class) {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                if let Some(reason_kind) = reason_kind {
                    if !valid_agent_stream_id(reason_kind) {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                }
                if reason.is_empty()
                    || reason.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                    || contains_unsafe_control_bytes(reason)
                {
                    return Err(HarnessOperatorApiError::InvalidAgentStream);
                }
                if let Some(help) = help {
                    if help.is_empty()
                        || help.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                        || contains_unsafe_control_bytes(help)
                    {
                        return Err(HarnessOperatorApiError::InvalidAgentStream);
                    }
                }
                Ok(())
            }
        }
    }
}

fn validate_agent_stream_catalog_entries(
    entries: &[HarnessAgentStreamNamedIdV1],
) -> Result<(), HarnessOperatorApiError> {
    if entries.len() > HARNESS_AGENT_STREAM_CATALOG_ENTRIES_MAX {
        return Err(HarnessOperatorApiError::InvalidAgentStream);
    }
    for entry in entries {
        if !valid_agent_stream_id(&entry.id) || !valid_agent_stream_id(&entry.name) {
            return Err(HarnessOperatorApiError::InvalidAgentStream);
        }
        if let Some(description) = &entry.description {
            if description.len() > HARNESS_AGENT_STREAM_TEXT_MAX_BYTES
                || contains_unsafe_control_bytes(description)
            {
                return Err(HarnessOperatorApiError::InvalidAgentStream);
            }
        }
    }
    Ok(())
}

/// One chunk of the outbound agent content stream -- the operator-wire
/// counterpart of `gate4agent-node-protocol`'s `AgentStreamChunkV1`, itself
/// mirroring `NodeEvent::TerminalFrame`'s own precedent: its own
/// subscription, its own type, no `ObservationV1` resync promise (see
/// `HarnessOperatorAgentEventV1`'s doc comment below). `source_sequence`
/// orders chunks within one provider source the way
/// `ObservationV1::source_sequence` orders observations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAgentStreamChunkV1 {
    pub source_sequence: u64,
    pub kind: HarnessAgentStreamChunkKindV1,
}

impl HarnessAgentStreamChunkV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.kind.validate()
    }
}

/// Long-lived, server-push counterpart to a `SubscribeAgentStream`
/// connection -- see `HarnessOperatorEventV1`'s own doc comment for the
/// shared framing this also uses. Deliberately its own type, not a variant
/// of `HarnessOperatorEventV1` and not a variant of
/// `HarnessOperatorTerminalEventV1` either, for two separate reasons this
/// stream's overflow story below has to hold at once:
///
/// - Like `HarnessOperatorTerminalEventV1`, `HarnessOperatorEventV1`'s
///   `Lagged`/`SnapshotBaseline` pair is wrong here: there is no
///   task/run/node to resync, only content that is already self-contained.
/// - Unlike `HarnessOperatorTerminalEventV1`, a dropped chunk here cannot be
///   waved off with a diagnostic counter either. A terminal frame is always
///   a full screen, so `TerminalFrame`'s own `coalesced_since_last` can
///   silently supersede a stale frame with a fresher one and lose nothing
///   real. An agent-stream chunk has no such property: a `Text`/`Thinking`
///   delta or an `InteractionPrompt` is a fact about one instant, not a
///   snapshot of the whole session, so a chunk this stream drops is gone --
///   there is no later chunk that already contains it, and dropping an
///   `InteractionPrompt` in particular would strand the operator unable to
///   ever answer a correlation id they never saw.
///
/// So overflow here gets its own honest shape: `Lagged` reports a real,
/// unrecoverable loss for one subscribed session -- no snapshot follows it,
/// because none could repair what was lost -- and the client's only correct
/// response is to surface that loss to the operator, not to wait for a
/// recovery frame that is never coming.
///
/// `ReplayBoundary` is a separate, narrower concession to the same
/// "instants are gone once missed" rule: a subscriber routinely registers
/// seconds after a session's first turn already started (an operator can
/// only address a session that has been spawned), and every `Text`/
/// `Thinking`/`Blocked` chunk published into that gap used to be gone for
/// good. The harness now keeps a small bounded ring of the most recent ones
/// per session and replays it once at subscribe time, immediately marked by
/// this event so it is never mistaken for live content -- see this field's
/// own doc comment on `AgentChunk::published_at_ms` for how a replayed
/// chunk keeps proving it is history, not now.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorAgentEventV1 {
    AgentChunk {
        sequence: u64,
        session: HarnessRuntimeSessionAddressV1,
        chunk: HarnessAgentStreamChunkV1,
        /// The harness's own wall-clock reading of the moment this chunk
        /// was first published to `AgentStreamSubscriberRegistry`
        /// (`gate4agent-harness-service::agent_stream`), carried unchanged
        /// through every delivery path -- live, state seed, and the bounded
        /// replay `ReplayBoundary` marks the end of. A replayed chunk keeps
        /// the timestamp it was born with here; nothing ever restamps it to
        /// the moment it happened to be replayed.
        published_at_ms: u64,
    },
    /// This subscriber's outbound queue overflowed for the named session and
    /// `dropped` chunks were lost for it specifically -- other subscribers,
    /// and this subscriber's other subscribed sessions, are unaffected. See
    /// this type's own doc comment for why there is no recovery frame to
    /// follow it.
    Lagged {
        sequence: u64,
        session: HarnessRuntimeSessionAddressV1,
        dropped: u64,
    },
    /// Sent at most once per `SubscribeAgentStream` subscription per
    /// session, right after that session's replayed instant chunks
    /// (`Text`/`Thinking`/`Blocked` pulled from the harness's own bounded
    /// replay ring -- see `gate4agent-harness-service::agent_stream`'s own
    /// doc comment) and before any live chunk for that session: the marker
    /// that lets the operator tell "recent history it missed" apart from
    /// "happening right now". Sent for every session the harness has
    /// recorded ANYTHING for at all (state, replay, or both), even if the
    /// ring itself turned out empty for it (`replayed: 0`) -- but never for
    /// a session the harness has not seen even once, since there is nothing
    /// to mark a boundary against there and the first thing that subscriber
    /// sees for it will just be live content, unambiguously so.
    /// `dropped_before_replay` is the ring's own running eviction count --
    /// nonzero here means the replay itself is truncated, and it names that
    /// gap rather than silently handing back a partial history as if it
    /// were the whole one.
    ReplayBoundary {
        session: HarnessRuntimeSessionAddressV1,
        replayed: u32,
        dropped_before_replay: u64,
    },
    /// Same keep-alive rationale as `HarnessOperatorTerminalEventV1::Ping`:
    /// this is a physically separate connection with its own dead-peer-
    /// detection problem, independent of the terminal and task/run/node
    /// subscriptions' own.
    Ping { sequence: u64 },
}

impl HarnessOperatorAgentEventV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::AgentChunk { session, chunk, .. } => {
                session.validate()?;
                chunk.validate()
            }
            Self::Lagged { session, .. } => session.validate(),
            Self::ReplayBoundary { session, .. } => session.validate(),
            Self::Ping { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorResponseV1 {
    Monitor(SessionMonitorV1),
    Timeline(TimelinePageV1),
    Tasks(TaskPageV1),
    Task(RedactedTaskV1),
    TaskOperations(Vec<HarnessOperationLedgerEntryV1>),
    Runs(RunPageV1),
    Run(RedactedRunV1),
    RunCorrelation(HarnessRunCorrelationV1),
    RunTransfer(HarnessRunTransferSummaryV1),
    ReverseAttribution(HarnessReverseAttributionV1),
    RunContextSourceObserved(HarnessRunContextSourceObservationV1),
    RunWorkspaceInspected(HarnessRunWorkspaceInspectionV1),
    RunWorkspaceFileRead(HarnessRunWorkspaceFileV1),
    RunGitHistoryRead(HarnessRunGitHistoryPageV1),
    RunGitDiffRead(HarnessRunGitDiffV1),
    NodeWorkspaceInspected(HarnessNodeWorkspaceInspectionV1),
    NodeWorkspaceFileRead(HarnessNodeWorkspaceFileV1),
    NodeGitHistoryRead(HarnessNodeGitHistoryPageV1),
    NodeGitDiffRead(HarnessNodeGitDiffV1),
    // V11 write/create siblings of the three node-workspace read responses
    // above: same payload shapes the node's own `WorkspaceFileWritten`/
    // `WorkspaceFileCreated`/`WorkspaceDirectoryCreated` replies carry,
    // projected the same way `NodeWorkspaceFileRead` already is.
    NodeWorkspaceFileWritten(HarnessNodeWorkspaceFileV1),
    NodeWorkspaceFileCreated(HarnessNodeWorkspaceFileV1),
    NodeWorkspaceDirectoryCreated(HarnessNodeWorkspaceDirectoryV1),
    LaunchPlans(HarnessLaunchPlanPageV1),
    TaskExecutionSpec(Option<HarnessTaskExecutionSpecV1>),
    TaskLaunchOptions(HarnessTaskLaunchOptionsV1),
    RuntimeInventory(HarnessRuntimeInventoryPageV1),
    TerminalRead(HarnessRuntimeTerminalPageV1),
    MailInbox(HarnessMailInboxPageV1),
    SessionSpawned(HarnessRuntimeSessionAddressV1),
    SessionInputWritten,
    SessionResized,
    SessionStopped,
    SessionControlled,
    SessionBytesWritten,
    SessionPasted,
    SessionRemoved,
    SessionResumed,
    // `PromptSession`'s ack -- same bare shape as `SessionInputWritten`
    // above, because the node's own `NodeRequest::Prompt` handler acks with
    // a bare `NodeResponse::Accepted` too. See `HarnessOperatorRequestV1::
    // PromptSession`'s own doc comment.
    SessionPrompted,
    // Unit acks for the four ACP control verbs above -- same bare-ack shape
    // as the eight session-control acks immediately above, because the node
    // itself acks all four with its own bare `NodeResponse::Accepted`.
    InteractionResolved,
    SessionModeSet,
    SessionConfigOptionSet,
    SessionModelSet,
    NativeSessionsCataloged(HarnessNativeSessionsCatalogedV1),
    NativeSessionsPaged(HarnessNativeSessionsPagedV1),
    NativeSessionPreviewed(HarnessNativeSessionPreviewedV1),
    SessionRecordPreviewed(HarnessSessionRecordPreviewedV1),
    SessionRecordResumed(HarnessSessionRecordResumedV1),
    SessionRecordUpdated(HarnessRuntimeManagedSessionV1),
    SessionRecordForgotten { record_id: String },
    ProviderSessionIndexed(HarnessRuntimeManagedSessionV1),
    NativeSessionIndexed(HarnessNativeSessionIndexedV1),
    HostDirectoriesBrowsed(HarnessHostDirectoryListingV1),
    WorkspaceRegistered(HarnessWorkspaceSnapshotV1),
    StandaloneWorkspaceCreated(HarnessWorkspaceSnapshotV1),
    WorkspaceUnregistered { workspace_id: String },
    WorktreeCreated {
        worktree: HarnessGitWorktreeSnapshotV1,
        workspace: HarnessWorkspaceSnapshotV1,
    },
    WorktreeRemoved {
        target_root: HarnessHostPathV1,
        workspace_id: Option<String>,
    },
    ContextPackExported(HarnessResolvedContextPackReceiptV1),
    ContextPackForgotten { context_id: String },
    Mutation(HarnessOperatorMutationOutcomeV1),
    ExecutionSpecMutation(HarnessOperatorMutationOutcomeV1),
    Schedule(HarnessScheduleOutcomeV1),
    TaskStarted(HarnessTaskStartOutcomeV1),
}

impl HarnessOperatorResponseV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Monitor(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Timeline(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Tasks(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Task(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::TaskOperations(entries) => {
                if entries.len() > usize::from(HARNESS_ENTITY_PAGE_LIMIT_MAX) {
                    return Err(HarnessOperatorApiError::InvalidOperationLedger);
                }
                for entry in entries {
                    entry.validate()?;
                }
                Ok(())
            }
            Self::Runs(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Run(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::RunCorrelation(value) => value.validate(),
            Self::RunTransfer(value) => value.validate(),
            Self::ReverseAttribution(value) => value.validate(),
            Self::RunContextSourceObserved(value) => value.validate(),
            Self::RunWorkspaceInspected(value) => value.validate(),
            Self::RunWorkspaceFileRead(value) => value.validate(),
            Self::RunGitHistoryRead(value) => value.validate(),
            Self::RunGitDiffRead(value) => value.validate(),
            Self::NodeWorkspaceInspected(value) => value.validate(),
            Self::NodeWorkspaceFileRead(value) => value.validate(),
            Self::NodeGitHistoryRead(value) => value.validate(),
            Self::NodeGitDiffRead(value) => value.validate(),
            Self::NodeWorkspaceFileWritten(value) | Self::NodeWorkspaceFileCreated(value) => {
                value.validate()
            }
            Self::NodeWorkspaceDirectoryCreated(value) => value.validate(),
            Self::LaunchPlans(value) => value.validate(),
            Self::TaskExecutionSpec(value) => {
                if let Some(value) = value {
                    value.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                Ok(())
            }
            Self::TaskLaunchOptions(value) => value.validate(),
            Self::RuntimeInventory(value) => value.validate(),
            Self::TerminalRead(value) => value.validate(),
            Self::MailInbox(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::SessionSpawned(value) => value.validate(),
            Self::SessionInputWritten
            | Self::SessionResized
            | Self::SessionStopped
            | Self::SessionControlled
            | Self::SessionBytesWritten
            | Self::SessionPasted
            | Self::SessionRemoved
            | Self::SessionResumed
            | Self::SessionPrompted
            | Self::InteractionResolved
            | Self::SessionModeSet
            | Self::SessionConfigOptionSet
            | Self::SessionModelSet => Ok(()),
            Self::NativeSessionsCataloged(value) => value.validate(),
            Self::NativeSessionsPaged(value) => value.validate(),
            Self::NativeSessionPreviewed(value) => value.validate(),
            Self::SessionRecordPreviewed(value) => value.validate(),
            Self::SessionRecordResumed(value) => value.validate(),
            Self::SessionRecordUpdated(value) | Self::ProviderSessionIndexed(value) => {
                value.validate()
            }
            Self::SessionRecordForgotten { record_id } => {
                if !valid_runtime_id(record_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
                }
                Ok(())
            }
            Self::NativeSessionIndexed(value) => value.validate(),
            Self::HostDirectoriesBrowsed(value) => value.validate(),
            Self::WorkspaceRegistered(value) | Self::StandaloneWorkspaceCreated(value) => {
                value.validate()
            }
            Self::WorkspaceUnregistered { workspace_id } => {
                if !valid_runtime_id(workspace_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                Ok(())
            }
            Self::WorktreeCreated { worktree, workspace } => {
                worktree.validate()?;
                workspace.validate()
            }
            Self::WorktreeRemoved { target_root, workspace_id } => {
                target_root.validate()?;
                if let Some(workspace_id) = workspace_id {
                    if !valid_runtime_id(workspace_id, 128) {
                        return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                    }
                }
                Ok(())
            }
            Self::ContextPackExported(value) => {
                value.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ContextPackForgotten { context_id } => {
                if !valid_runtime_id(context_id, 128) {
                    return Err(HarnessOperatorApiError::InvalidResourceMutationRequest);
                }
                Ok(())
            }
            Self::Mutation(_) | Self::ExecutionSpecMutation(_) => Ok(()),
            Self::Schedule(HarnessScheduleOutcomeV1::Idle) => Ok(()),
            Self::Schedule(HarnessScheduleOutcomeV1::Dispatch(value)) => {
                value.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::TaskStarted(value) => {
                value.validate().map_err(HarnessOperatorApiError::Protocol)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessNodeIncarnationV1(HarnessSelectorV1);

impl HarnessNodeIncarnationV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        if value.len() != 32 || !value.bytes().all(is_lower_hex) {
            return Err(HarnessOperatorApiError::InvalidRunCorrelation);
        }
        HarnessSelectorV1::new(value)
            .map(Self)
            .map_err(HarnessOperatorApiError::Protocol)
    }

    pub fn as_str(&self) -> &str { self.0.as_str() }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.0.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if self.0.as_str().len() != 32 || !self.0.as_str().bytes().all(is_lower_hex) {
            return Err(HarnessOperatorApiError::InvalidRunCorrelation);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for HarnessNodeIncarnationV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessRunWorktreeViewV1 {
    Existing,
    Managed { worktree_ref: HarnessSelectorV1 },
}

impl HarnessRunWorktreeViewV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Existing => Ok(()),
            Self::Managed { worktree_ref } => {
                worktree_ref.validate().map_err(HarnessOperatorApiError::Protocol)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManagedRunSessionV1 {
    pub record_id: HarnessSelectorV1,
    pub active_session: Option<HarnessRuntimeIdentityV1>,
}

impl HarnessManagedRunSessionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.record_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if let Some(active_session) = &self.active_session {
            active_session.validate().map_err(HarnessOperatorApiError::Protocol)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessInlineRunSessionV1 {
    pub inline_ref: HarnessInlineRef,
}

impl HarnessInlineRunSessionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.inline_ref.validate().map_err(HarnessOperatorApiError::Protocol)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessRunSessionViewV1 {
    Managed(HarnessManagedRunSessionV1),
    Inline(HarnessInlineRunSessionV1),
}

impl HarnessRunSessionViewV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Managed(value) => value.validate(),
            Self::Inline(value) => value.validate(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRunCorrelationAvailabilityV1 {
    Available,
    Dormant,
    Unavailable,
    NotObserved,
    StaleIncarnation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunCorrelationV1 {
    pub run_id: HarnessRunId,
    pub run_revision: HarnessRevision,
    pub task_id: HarnessTaskId,
    pub node_id: HarnessSelectorV1,
    pub node_incarnation_id: HarnessNodeIncarnationV1,
    pub workspace_id: HarnessSelectorV1,
    pub provider_profile: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
    pub worktree: HarnessRunWorktreeViewV1,
    pub session: HarnessRunSessionViewV1,
    pub availability: HarnessRunCorrelationAvailabilityV1,
    pub observed_at_unix_ms: Option<u64>,
}

impl HarnessRunCorrelationV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.run_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.node_incarnation_id.validate()?;
        self.workspace_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.provider_profile.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.worktree.validate()?;
        self.session.validate()?;
        if matches!(&self.session, HarnessRunSessionViewV1::Inline(_))
            && self.mode != HarnessExecutionModeV1::Inline
        {
            return Err(HarnessOperatorApiError::InvalidRunCorrelation);
        }
        if matches!(self.availability, HarnessRunCorrelationAvailabilityV1::NotObserved)
            != self.observed_at_unix_ms.is_none()
            || self.observed_at_unix_ms == Some(0)
        {
            return Err(HarnessOperatorApiError::InvalidRunCorrelation);
        }
        match (&self.session, self.availability) {
            (
                HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                    active_session: Some(_),
                    ..
                }),
                HarnessRunCorrelationAvailabilityV1::Dormant,
            )
            | (
                HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                    active_session: None,
                    ..
                }),
                HarnessRunCorrelationAvailabilityV1::Available,
            )
            | (
                HarnessRunSessionViewV1::Inline(_),
                HarnessRunCorrelationAvailabilityV1::Available
                    | HarnessRunCorrelationAvailabilityV1::Dormant,
            ) => Err(HarnessOperatorApiError::InvalidRunCorrelation),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunDeliveryTransferV1 {
    pub delivery_ref: HarnessDeliveryRef,
    pub revision: HarnessRevision,
    pub state: HarnessDeliveryStateV1,
    pub selector: HarnessSelectorV1,
    pub bundle_id: HarnessDeliveryBundleIdV1,
    pub bundle_revision: HarnessDeliveryBundleRevisionV1,
    pub bundle_digest: HarnessDeliveryBundleDigestV1,
    pub manifest_digest: HarnessDeliveryManifestDigestV2,
    pub receipt_ref: Option<HarnessReceiptRef>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub staged_at_unix_ms: Option<u64>,
    pub committed_at_unix_ms: Option<u64>,
}

impl HarnessRunDeliveryTransferV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.delivery_ref.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.selector.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.bundle_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.bundle_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.bundle_digest.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.manifest_digest.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if let Some(receipt_ref) = &self.receipt_ref {
            receipt_ref.validate().map_err(HarnessOperatorApiError::Protocol)?;
        }
        if !valid_transfer_timestamps(
            self.created_at_unix_ms,
            self.updated_at_unix_ms,
            [self.staged_at_unix_ms, self.committed_at_unix_ms],
        ) {
            return Err(HarnessOperatorApiError::InvalidRunTransfer);
        }
        let state_fields = match self.state {
            HarnessDeliveryStateV1::Prepared => {
                self.staged_at_unix_ms.is_none()
                    && self.committed_at_unix_ms.is_none()
                    && self.receipt_ref.is_none()
            }
            HarnessDeliveryStateV1::Staged => {
                self.staged_at_unix_ms.is_some()
                    && self.committed_at_unix_ms.is_none()
                    && self.receipt_ref.is_none()
            }
            HarnessDeliveryStateV1::Committed => {
                self.staged_at_unix_ms.is_some()
                    && self.committed_at_unix_ms.is_some()
                    && self.receipt_ref.is_some()
            }
        };
        if !state_fields
            || matches!(
                (self.staged_at_unix_ms, self.committed_at_unix_ms),
                (Some(staged), Some(committed)) if committed < staged
            )
        {
            return Err(HarnessOperatorApiError::InvalidRunTransfer);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunContextTransferV1 {
    pub context_ref: HarnessSelectorV1,
    pub digest: String,
    pub source_message_count: u64,
    pub retained_message_count: u64,
    pub byte_len: u32,
    pub truncated: bool,
}

impl HarnessRunContextTransferV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.context_ref.validate().map_err(HarnessOperatorApiError::Protocol)?;
        let valid_digest = self.digest.strip_prefix("sha256:").is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(is_lower_hex)
        });
        if !valid_digest
            || self.source_message_count == 0
            || self.retained_message_count == 0
            || self.retained_message_count > self.source_message_count
            || self.retained_message_count > HARNESS_CONTEXT_PACK_RETAINED_MESSAGES_MAX
            || self.byte_len == 0
            || self.byte_len > HARNESS_CONTEXT_PACK_MAX_BYTES
            || self.truncated != (self.source_message_count > self.retained_message_count)
        {
            return Err(HarnessOperatorApiError::InvalidRunTransfer);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunContinuationTransferV1 {
    pub continuation_ref: HarnessContinuationRef,
    pub receipt_ref: HarnessReceiptRef,
    pub revision: HarnessRevision,
    pub state: HarnessContinuationStateV1,
    pub source_run_id: HarnessRunId,
    pub target_run_id: HarnessRunId,
    pub source_provider: HarnessSelectorV1,
    pub context: Option<HarnessRunContextTransferV1>,
    pub prepared_at_unix_ms: u64,
    pub exporting_at_unix_ms: Option<u64>,
    pub exported_at_unix_ms: Option<u64>,
    pub bound_at_unix_ms: Option<u64>,
    pub expired_at_unix_ms: Option<u64>,
    pub outcome_unknown_at_unix_ms: Option<u64>,
    pub outcome_unknown_reason: Option<HarnessContinuationOutcomeUnknownReasonV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl HarnessRunContinuationTransferV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.continuation_ref.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.receipt_ref.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.source_run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.target_run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.source_provider.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if let Some(context) = &self.context { context.validate()?; }
        if self.source_run_id == self.target_run_id
            || self.prepared_at_unix_ms != self.created_at_unix_ms
            || !valid_transfer_timestamps(
                self.created_at_unix_ms,
                self.updated_at_unix_ms,
                [
                    Some(self.prepared_at_unix_ms),
                    self.exporting_at_unix_ms,
                    self.exported_at_unix_ms,
                    self.bound_at_unix_ms,
                    self.expired_at_unix_ms,
                    self.outcome_unknown_at_unix_ms,
                ],
            )
        {
            return Err(HarnessOperatorApiError::InvalidRunTransfer);
        }
        let ordered = self.exporting_at_unix_ms
            .is_none_or(|value| value >= self.prepared_at_unix_ms)
            && self.exported_at_unix_ms.is_none_or(|value| {
                self.exporting_at_unix_ms.is_some_and(|started| value >= started)
            })
            && self.bound_at_unix_ms.is_none_or(|value| {
                self.exported_at_unix_ms.is_some_and(|exported| value >= exported)
            })
            && self.outcome_unknown_at_unix_ms.is_none_or(|value| {
                self.exporting_at_unix_ms.is_some_and(|started| value >= started)
            })
            && self.expired_at_unix_ms
                .is_none_or(|value| value >= self.prepared_at_unix_ms);
        let state_fields = match self.state {
            HarnessContinuationStateV1::Prepared => self.context.is_none()
                && self.exporting_at_unix_ms.is_none()
                && self.exported_at_unix_ms.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::Exporting => self.context.is_none()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::Exported => self.context.is_some()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_some()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::Bound => self.context.is_some()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_some()
                && self.bound_at_unix_ms.is_some()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::OutcomeUnknown => self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_some()
                && self.outcome_unknown_reason.is_some(),
            HarnessContinuationStateV1::Expired => self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_some()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
        };
        if !ordered || !state_fields {
            return Err(HarnessOperatorApiError::InvalidRunTransfer);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunTransferSummaryV1 {
    pub run_id: HarnessRunId,
    pub run_revision: HarnessRevision,
    pub delivery: Option<HarnessRunDeliveryTransferV1>,
    pub continuation: Option<HarnessRunContinuationTransferV1>,
}

impl HarnessRunTransferSummaryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.run_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if let Some(delivery) = &self.delivery { delivery.validate()?; }
        if let Some(continuation) = &self.continuation {
            continuation.validate()?;
            if continuation.target_run_id != self.run_id {
                return Err(HarnessOperatorApiError::InvalidRunTransfer);
            }
        }
        Ok(())
    }

    pub fn validate_for(&self, run_id: &HarnessRunId) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if &self.run_id != run_id {
            return Err(HarnessOperatorApiError::InvalidRunTransfer);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunContextSourceObservationV1 {
    pub run_id: HarnessRunId,
    pub run_revision: HarnessRevision,
    pub feature_state: FeatureObservationStateV1,
    pub message_count: u64,
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
    pub observed_at_unix_ms: Option<u64>,
}

impl HarnessRunContextSourceObservationV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.run_revision.validate().map_err(HarnessOperatorApiError::Protocol)?;
        let observed = self.feature_state == FeatureObservationStateV1::Observed;
        let observed_fields_valid = self.message_count > 0
            && self.message_count_exact
            && self.observed_at_unix_ms.is_some_and(|timestamp| timestamp > 0)
            && self.completed_turn_count.map_or(true, |count| count <= self.message_count);
        let unobserved_fields_empty = self.message_count == 0
            && !self.message_count_exact
            && self.completed_turn_count.is_none()
            && self.total_tokens.is_none()
            && self.observed_at_unix_ms.is_none();
        if observed && !observed_fields_valid || !observed && !unobserved_fields_empty {
            return Err(HarnessOperatorApiError::InvalidRunContextSourceObservation);
        }
        Ok(())
    }

    pub fn validate_for(&self, run_id: &HarnessRunId) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if &self.run_id != run_id {
            return Err(HarnessOperatorApiError::InvalidRunContextSourceObservation);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeNodeInventoryV1 {
    pub node_id: String,
    pub incarnation_id: String,
    pub observed_at_unix_ms: u64,
    pub event_sequence: u64,
    pub inventory: HarnessRuntimeInventoryV1,
}

impl HarnessRuntimeNodeInventoryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.node_id, 128)
            || self.incarnation_id.len() != 32
            || !self.incarnation_id.bytes().all(is_lower_hex)
            || self.observed_at_unix_ms == 0
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        self.inventory.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeInventoryV1 {
    pub enabled_providers: Vec<String>,
    pub workspaces: BTreeMap<String, HarnessRuntimeWorkspaceV1>,
    pub workspace_count: usize,
    pub workspaces_truncated: bool,
    pub session_count: usize,
    pub sessions_truncated: bool,
    pub managed_sessions: Vec<HarnessRuntimeManagedSessionV1>,
    pub managed_session_count: usize,
    pub managed_sessions_truncated: bool,
    /// Lifetime count of managed session records the node's own retention
    /// sweep has retired (mirrors `gate4agent_c2_protocol::SlimNodeInventory::
    /// retired_count`). Additive and informational only, `#[serde(default)]`
    /// so a payload from before this field existed still deserializes.
    #[serde(default)]
    pub retired_count: usize,
    // Operator-visible surface: no redaction beyond what the direct-C2 TUI
    // already shows for the same node (`NodeView::launch_inventory`). Mirrors
    // `gate4agent_node_protocol::LaunchInventory` field-for-field rather than
    // reusing it: `gate4agent-node-protocol` already depends on this crate
    // (for the shared Harness MCP wire types), so the reverse edge would be
    // a cyclic package dependency. The TUI reconstructs the real node-protocol
    // type from this mirror on receipt (`client.rs::project_harness_inventory_node`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_inventory: Option<HarnessRuntimeLaunchInventoryV1>,
}

/// Same liveness-first rationale and exact rank table as c2-protocol's own
/// `managed_session_liveness_rank` (`gate4agent_c2_protocol::SlimNodeInventory`
/// page-cut ordering): `Live` ranks first, `IdentityPending` next, `Dormant`
/// next, terminal `Unavailable` last. The producer orders a page by this
/// rank before the `MAX_C2_MANAGED_SESSIONS_PER_NODE` cut, so a page is
/// never sorted by bare `record_id` alone -- validating strict ascending
/// `record_id` order regardless of state rejected every real page (fixed
/// after `ed4fa61` regressed this: see `managed_session_liveness_rank`'s
/// own doc comment in c2-protocol for why the producer's order changed).
fn managed_session_liveness_rank(state: &HarnessRuntimeManagedStateV1) -> u8 {
    match state {
        HarnessRuntimeManagedStateV1::Live => 0,
        HarnessRuntimeManagedStateV1::IdentityPending => 1,
        HarnessRuntimeManagedStateV1::Dormant => 2,
        HarnessRuntimeManagedStateV1::Unavailable => 3,
    }
}

impl HarnessRuntimeInventoryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let included_sessions = self.workspaces.values()
            .map(|workspace| workspace.sessions.len())
            .sum::<usize>();
        if self.enabled_providers.len() > 64
            || self.enabled_providers.windows(2).any(|pair| pair[0] >= pair[1])
            || self.enabled_providers.iter().any(|provider| !valid_runtime_id(provider, 128))
            || self.workspaces.len() > 32
            || included_sessions > 128
            || self.managed_sessions.len() > 128
            || self.workspace_count < self.workspaces.len()
            || self.session_count < included_sessions
            || self.managed_session_count < self.managed_sessions.len()
            || self.workspaces_truncated != (self.workspace_count > self.workspaces.len())
            || self.sessions_truncated != (self.session_count > included_sessions)
            || self.managed_sessions_truncated
                != (self.managed_session_count > self.managed_sessions.len())
            || self.managed_sessions.windows(2).any(|pair| {
                let left = (managed_session_liveness_rank(&pair[0].state), pair[0].record_id.as_str());
                let right = (managed_session_liveness_rank(&pair[1].state), pair[1].record_id.as_str());
                left >= right
            })
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        for (workspace_id, workspace) in &self.workspaces {
            if workspace_id != &workspace.workspace_id { return Err(HarnessOperatorApiError::InvalidRuntimeInventory); }
            workspace.validate()?;
        }
        for record in &self.managed_sessions { record.validate()?; }
        if let Some(launch_inventory) = &self.launch_inventory { launch_inventory.validate()?; }
        Ok(())
    }
}

/// Mirror of `gate4agent_node_protocol::ResolvedEnvironmentProfileReceipt`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeEnvironmentProfileReceiptV1 {
    pub profile_id: String,
    pub profile_revision: String,
}

impl HarnessRuntimeEnvironmentProfileReceiptV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.profile_id, 128) || !valid_runtime_id(&self.profile_revision, 128) {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        Ok(())
    }
}

/// Mirror of `gate4agent_node_protocol::SpawnProfileSummary`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeSpawnProfileSummaryV1 {
    pub id: String,
    pub revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_profile: Option<HarnessRuntimeEnvironmentProfileReceiptV1>,
}

impl HarnessRuntimeSpawnProfileSummaryV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.id, 128) || !valid_runtime_id(&self.revision, 128) {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        if let Some(environment_profile) = &self.environment_profile {
            environment_profile.validate()?;
        }
        Ok(())
    }
}

/// Mirror of `gate4agent_node_protocol::ResolvedBundleReceipt`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeBundleReceiptV1 {
    pub id: String,
    pub revision: String,
    pub digest: String,
}

impl HarnessRuntimeBundleReceiptV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.id, 128)
            || !valid_runtime_id(&self.revision, 128)
            || !valid_sha256_digest(&self.digest)
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        Ok(())
    }
}

/// Mirror of `gate4agent_node_protocol::LaunchInventory`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeLaunchInventoryV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_profiles: Option<Vec<HarnessRuntimeSpawnProfileSummaryV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundles: Option<Vec<HarnessRuntimeBundleReceiptV1>>,
}

impl HarnessRuntimeLaunchInventoryV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.spawn_profiles.is_none() && self.bundles.is_none() {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        if let Some(profiles) = &self.spawn_profiles {
            if profiles.len() > HARNESS_RUNTIME_SPAWN_PROFILES_MAX {
                return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
            }
            for profile in profiles { profile.validate()?; }
            if profiles.iter().enumerate().any(|(index, profile)| {
                profiles[..index].iter().any(|existing| existing.id == profile.id)
            }) {
                return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
            }
        }
        if let Some(bundles) = &self.bundles {
            if bundles.len() > HARNESS_RUNTIME_LAUNCH_BUNDLES_MAX {
                return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
            }
            for bundle in bundles { bundle.validate()?; }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeWorkspaceV1 {
    pub workspace_id: String,
    pub display_root: String,
    pub display_root_truncated: bool,
    pub sessions: Vec<HarnessRuntimeSessionV1>,
    pub session_count: usize,
    pub sessions_truncated: bool,
}

impl HarnessRuntimeWorkspaceV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.workspace_id, 128)
            || self.display_root.len() > 1024
            || self.display_root.chars().any(char::is_control)
            || self.sessions.len() > 128
            || self.session_count < self.sessions.len()
            || self.sessions_truncated != (self.session_count > self.sessions.len())
            || self.sessions.windows(2).any(|pair| {
                (pair[0].instance_id, pair[0].generation)
                    >= (pair[1].instance_id, pair[1].generation)
            })
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        for session in &self.sessions { session.validate()?; }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeTransportV1 { Pty, Pipe, Acp }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeSessionStatusV1 { Registered, Starting, Running, Stopping, Exited, Failed }

/// Exact mirror of `gate4agent_types::TerminalControl`: this crate has no
/// dependency on `gate4agent-types` (see the doc comment on
/// `HARNESS_SESSION_BYTES_MAX_BYTES`), so `ControlSession`'s special-key
/// payload is duplicated here as its own closed wire enum rather than
/// imported. The variant set and names are kept in lockstep by hand --
/// `gate4agent-harness-service`'s `c2::map_terminal_control` is the single
/// place that converts one into the other, so a variant added to one side
/// without the other fails to compile there. No `validate()` method: unlike
/// a bounded string or byte vector, a closed enum's own deserialization is
/// already the bound -- an unrecognized variant name is rejected before this
/// type is ever constructed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessTerminalControlV1 {
    Interrupt,
    EndOfFile,
    ControlA,
    ControlB,
    ControlE,
    ControlF,
    ControlG,
    ControlH,
    ControlI,
    ControlJ,
    ControlK,
    ControlL,
    ControlM,
    ControlN,
    ControlO,
    ControlP,
    ControlQ,
    ControlR,
    ControlS,
    ControlT,
    ControlU,
    ControlV,
    ControlW,
    ControlX,
    ControlY,
    ControlZ,
    Enter,
    LineFeed,
    Escape,
    Backspace,
    Tab,
    BackTab,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Function1,
    Function2,
    Function3,
    Function4,
    Function5,
    Function6,
    Function7,
    Function8,
    Function9,
    Function10,
    Function11,
    Function12,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeTerminalSizeV1 {
    pub rows: u16,
    pub columns: u16,
}

/// Mirrors `gate4agent_types::FOREGROUND_PROCESS_NAME_MAX_BYTES`, the bound
/// on `PtyScreenStateV1::NotAgent`'s `observed_process`. Hand-kept in step
/// with the source constant for the same reason the enum itself is
/// hand-duplicated below.
pub const HARNESS_SCREEN_STATE_PROCESS_MAX_BYTES: usize = 512;
/// Mirrors `gate4agent_types::PTY_SCREEN_GATE_NAME_MAX_BYTES`, the bound on
/// `Failing`'s `reason`.
pub const HARNESS_SCREEN_STATE_GATE_MAX_BYTES: usize = 128;
/// Mirrors `gate4agent_types::OPERATOR_GATE_OPTIONS_MAX`.
pub const HARNESS_GATE_OPTIONS_MAX: usize = 8;
/// Mirrors `gate4agent_types::OPERATOR_GATE_OPTION_TEXT_MAX_BYTES`.
pub const HARNESS_GATE_OPTION_TEXT_MAX_BYTES: usize = 128;
/// Mirrors `gate4agent_types::OPERATOR_GATE_PATH_MAX_BYTES`.
pub const HARNESS_GATE_PATH_MAX_BYTES: usize = 32_768;

/// Exact mirror of `gate4agent_types::OperatorGateKind`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorGateKindV1 {
    WorkspaceTrust,
    HookTrust,
    Authentication,
    VendorUpdate,
    Onboarding,
    TerminalAppearance,
    ConfigurationMigration,
}

impl OperatorGateKindV1 {
    /// Mirrors `gate4agent_types::OperatorGateKind::label`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceTrust => "workspace trust",
            Self::HookTrust => "hook trust review",
            Self::Authentication => "authentication",
            Self::VendorUpdate => "vendor update",
            Self::Onboarding => "onboarding",
            Self::TerminalAppearance => "terminal appearance setup",
            Self::ConfigurationMigration => "configuration migration",
        }
    }
}

/// Exact mirror of `gate4agent_types::OperatorGateSubject`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum OperatorGateSubjectV1 {
    Directory { path: Option<String> },
    Hooks { count: Option<u32> },
    McpServers,
    Account,
    ApiKey,
    Appearance,
    Unknown,
}

/// Exact mirror of `gate4agent_types::OperatorGateInput`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorGateInputV1 {
    NumberedList,
    ArrowList,
    PressEnter,
    TextEntry,
    Unknown,
}

/// Exact mirror of `gate4agent_types::OperatorGateOptionSemantics`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorGateOptionSemanticsV1 {
    Accept,
    Decline,
    Inspect,
    Exit,
    Unknown,
}

/// Exact mirror of `gate4agent_types::OperatorGateOption`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorGateOptionV1 {
    pub text: String,
    pub semantics: OperatorGateOptionSemanticsV1,
    pub selected: bool,
}

impl OperatorGateOptionV1 {
    fn is_valid(&self) -> bool {
        valid_native_single_line(&self.text, HARNESS_GATE_OPTION_TEXT_MAX_BYTES, true)
    }
}

/// Exact mirror of `gate4agent_types::OperatorGateState`: this crate has no
/// dependency on `gate4agent-types` (see the doc comment on
/// `HarnessTerminalControlV1`), so the node's gate classification is
/// duplicated here as its own closed wire shape rather than imported.
/// `gate4agent-harness-service` is the single place that maps one into the
/// other (`map_operator_gate`), so a field added to one side without the
/// other fails to compile there.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorGateStateV1 {
    pub kind: OperatorGateKindV1,
    pub subject: OperatorGateSubjectV1,
    pub input: OperatorGateInputV1,
    pub options: Vec<OperatorGateOptionV1>,
}

impl OperatorGateStateV1 {
    fn is_valid(&self) -> bool {
        let subject_valid = match &self.subject {
            OperatorGateSubjectV1::Directory { path: Some(path) } => {
                valid_native_single_line(path, HARNESS_GATE_PATH_MAX_BYTES, true)
            }
            OperatorGateSubjectV1::Directory { path: None }
            | OperatorGateSubjectV1::Hooks { .. }
            | OperatorGateSubjectV1::McpServers
            | OperatorGateSubjectV1::Account
            | OperatorGateSubjectV1::ApiKey
            | OperatorGateSubjectV1::Appearance
            | OperatorGateSubjectV1::Unknown => true,
        };
        subject_valid
            && self.options.len() <= HARNESS_GATE_OPTIONS_MAX
            && self.options.iter().all(OperatorGateOptionV1::is_valid)
    }
}

/// Exact mirror of `gate4agent_types::PtyScreenState`: this crate has no
/// dependency on `gate4agent-types` (see the doc comment on
/// `HarnessTerminalControlV1`), so the node's screen-content classification
/// is duplicated here as its own closed wire enum rather than imported.
/// `gate4agent-harness-service` is the single place that maps one into the
/// other, so a variant added to one side without the other fails to compile
/// there.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PtyScreenStateV1 {
    #[default]
    Unknown,
    NotAgent { observed_process: String },
    OperatorGate { gate: OperatorGateStateV1 },
    Failing { reason: String },
    Ready,
}

impl PtyScreenStateV1 {
    /// Refuses only the three states that carry a finding -- mirrors
    /// `gate4agent_types::PtyScreenState::admits_blind_write` exactly, so a
    /// wire consumer gets the same answer as the node and no call site
    /// open-codes its own reading. See that method for why `Unknown` admits.
    pub fn admits_blind_write(&self) -> bool {
        !matches!(
            self,
            Self::OperatorGate { .. } | Self::NotAgent { .. } | Self::Failing { .. }
        )
    }

    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let ok = match self {
            Self::Unknown | Self::Ready => true,
            Self::NotAgent { observed_process } => {
                valid_native_single_line(observed_process, HARNESS_SCREEN_STATE_PROCESS_MAX_BYTES, true)
            }
            Self::OperatorGate { gate } => gate.is_valid(),
            Self::Failing { reason } => {
                valid_native_single_line(reason, HARNESS_SCREEN_STATE_GATE_MAX_BYTES, true)
            }
        };
        if ok { Ok(()) } else { Err(HarnessOperatorApiError::InvalidRuntimeInventory) }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeSessionV1 {
    pub instance_id: u64,
    pub generation: u64,
    pub provider: String,
    pub transport: HarnessRuntimeTransportV1,
    pub status: HarnessRuntimeSessionStatusV1,
    pub process_id: Option<u32>,
    pub terminal_size: Option<HarnessRuntimeTerminalSizeV1>,
    pub operation_pending: bool,
    pub input_pending: bool,
    /// The node's current screen classification. Always populated by every
    /// server on this wire -- `Unknown` is a real classification, not an
    /// absence marker -- so `Some` is the only value ever actually sent;
    /// `Option`-wrapped only so a decoder never has to special-case a
    /// missing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_state: Option<PtyScreenStateV1>,
}

impl HarnessRuntimeSessionV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.instance_id == 0
            || self.generation == 0
            || !valid_runtime_id(&self.provider, 128)
            || self.terminal_size.is_some_and(|size| size.rows == 0 || size.columns == 0)
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        if let Some(screen_state) = &self.screen_state {
            screen_state.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeMouseProtocolEncodingV1 { Default, Utf8, Sgr }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeTerminalFrameV1 {
    pub sequence: u64,
    pub size: HarnessRuntimeTerminalSizeV1,
    pub cursor_row: u16,
    pub cursor_column: u16,
    pub formatted: Vec<u8>,
    pub scrollback_formatted: Vec<Vec<u8>>,
    pub alternate_screen: bool,
    pub mouse_protocol_enabled: bool,
    pub mouse_protocol_encoding: HarnessRuntimeMouseProtocolEncodingV1,
    /// Unix-epoch milliseconds when the source `TerminalFrame` was
    /// materialized (`gate4agent_types::TerminalFrame::produced_at_unix_ms`,
    /// stamped once at the PTY snapshot and carried unchanged through every
    /// relay hop, including the harness's own terminal ring buffer). An
    /// operator computing an age from this must account for clock skew
    /// between the node host and its own, not just transit/queue delay.
    /// `#[serde(default)]` so an operator client built before this field
    /// existed still decodes the frame -- it just can't answer "how stale".
    #[serde(default)]
    pub produced_at_unix_ms: u64,
    /// The screen classification stamped at the instant this frame's screen
    /// was materialized. Always populated -- `Unknown` is a real
    /// classification, not an absence marker -- so `Some` is the only value
    /// ever actually sent; `Option`-wrapped only so a decoder never has to
    /// special-case a missing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_state: Option<PtyScreenStateV1>,
    /// Mirrors `gate4agent_types::TerminalFrame::bracketed_paste` onto the
    /// operator wire unconditionally. `None` here means only "the node
    /// hasn't captured a value yet" -- the source field's own genuine
    /// absence -- never a wire-version gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bracketed_paste: Option<bool>,
}
// NOTE: gate4agent_types::TerminalFrame::contents (plain-text render) is
// deliberately dropped on the wire -- gate4agent-tui's apply_terminal_frame
// never reads it. No dead field on the wire.

impl HarnessRuntimeTerminalFrameV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.size.rows == 0 || self.size.columns == 0 {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        if self.scrollback_formatted.len() > HARNESS_TERMINAL_SCROLLBACK_LINES_MAX {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        let scrollback_bytes = self.scrollback_formatted.iter()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        if self.formatted.len().saturating_add(scrollback_bytes) > HARNESS_TERMINAL_FRAME_MAX_BYTES {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        if let Some(screen_state) = &self.screen_state {
            screen_state.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeSessionAddressV1 {
    pub node_id: String,
    pub incarnation_id: String,
    pub workspace_id: String,
    pub instance_id: u64,
    pub generation: u64,
}

impl HarnessRuntimeSessionAddressV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.node_id, 128)
            || self.incarnation_id.len() != 32
            || !self.incarnation_id.bytes().all(is_lower_hex)
            || !valid_runtime_id(&self.workspace_id, 128)
            || self.instance_id == 0
            || self.generation == 0
        {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeTerminalPageV1 {
    pub session: HarnessRuntimeSessionAddressV1,
    pub frames: Vec<HarnessRuntimeTerminalFrameV1>,
    pub dropped: u64,
    pub transport_incomplete: bool,
    pub next_cursor: Option<u64>,
}

impl HarnessRuntimeTerminalPageV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.session.validate()?;
        if self.frames.len() > usize::from(HARNESS_TERMINAL_PAGE_LIMIT_MAX)
            || self.frames.windows(2).any(|pair| pair[0].sequence >= pair[1].sequence)
            || self.next_cursor == Some(0)
        {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        if self.next_cursor.is_some()
            && self.next_cursor != self.frames.last().map(|frame| frame.sequence)
        {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        for frame in &self.frames { frame.validate()?; }
        Ok(())
    }

    pub fn validate_for(
        &self,
        session: &HarnessRuntimeSessionAddressV1,
    ) -> Result<(), HarnessOperatorApiError> {
        self.validate()?;
        if &self.session != session {
            return Err(HarnessOperatorApiError::InvalidTerminalPage);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeManagedModeV1 { Pty, Inline, Acp }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeManagedStateV1 { IdentityPending, Live, Dormant, Unavailable }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeSessionBindingV1 {
    pub workspace_id: String,
    pub instance_id: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeManagedSessionV1 {
    pub record_id: String,
    pub display_name: String,
    pub display_name_truncated: bool,
    pub provider: String,
    pub mode: HarnessRuntimeManagedModeV1,
    pub state: HarnessRuntimeManagedStateV1,
    pub workspace_id: String,
    pub active_binding: Option<HarnessRuntimeSessionBindingV1>,
    pub provider_identity_present: bool,
    pub updated_at_unix_ms: u64,
    /// Count of `ObservationKindV1::ActionBlocked` observations recorded
    /// against this managed session, from the harness's own per-session
    /// observation projection -- see
    /// `gate4agent_observation_engine::SessionProjection::blocked_count`.
    /// The fleet-wide instrument this exists for: one `runtime-inventory`
    /// read answers "how many sessions are stuck behind a gate right now"
    /// across every node at once.
    pub blocked_count: u64,
    /// `received_at_ms` of the most recent such observation, or `None` when
    /// `blocked_count` is zero.
    pub last_blocked_at_ms: Option<u64>,
    /// Unacked mail addressed to this record directly, plus mail addressed
    /// to a task this record's currently bound grant can see -- the mailbox
    /// arc's counterpart of `blocked_count` immediately above, filled the
    /// same way (`gate4agent-harness-service::runtime::
    /// fill_managed_session_blocked_stats`).
    pub unread_mail: u64,
}

impl HarnessRuntimeManagedSessionV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.record_id, 128)
            || !valid_runtime_id(&self.provider, 128)
            || !valid_runtime_id(&self.workspace_id, 128)
            || self.display_name.is_empty()
            || self.display_name.len() > 256
            || self.display_name.chars().any(char::is_control)
            || self.updated_at_unix_ms == 0
            || self.active_binding.as_ref().is_some_and(|binding| {
                !valid_runtime_id(&binding.workspace_id, 128)
                    || binding.instance_id == 0
                    || binding.generation == 0
            })
            || (self.blocked_count == 0) != self.last_blocked_at_ms.is_none()
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeInventoryPageV1 {
    pub nodes: Vec<HarnessRuntimeNodeInventoryV1>,
    pub next_cursor: Option<String>,
}

impl HarnessRuntimeInventoryPageV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.nodes.len() > usize::from(HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX)
            || self.nodes.windows(2).any(|pair| pair[0].node_id >= pair[1].node_id)
            || self.next_cursor.as_ref().is_some_and(|cursor| match self.nodes.last() {
                Some(last) => cursor != &last.node_id,
                None => true,
            })
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        for node in &self.nodes { node.validate()?; }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionCatalogScopeV1 { Workspace, Unregistered }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionCatalogWindowV1 { Recent, Older }

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionRouteV1 {
    pub node_id: String,
    pub incarnation_id: String,
    pub scope: HarnessNativeSessionCatalogScopeV1,
    pub workspace_id: Option<String>,
    pub provider: String,
}

impl HarnessNativeSessionRouteV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let scope_valid = matches!(
            (self.scope, self.workspace_id.as_ref()),
            (HarnessNativeSessionCatalogScopeV1::Workspace, Some(_))
                | (HarnessNativeSessionCatalogScopeV1::Unregistered, None)
        );
        if !scope_valid
            || !valid_runtime_id(&self.node_id, 128)
            || self.incarnation_id.len() != 32
            || !self.incarnation_id.bytes().all(is_lower_hex)
            || !valid_runtime_id(&self.provider, 128)
            || self.workspace_id.as_deref().is_some_and(|value| {
                !valid_runtime_id(value, 128)
            })
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionSelectionV1 {
    pub route: HarnessNativeSessionRouteV1,
    pub catalog_revision: u64,
    pub recent_cutoff_unix_ms: u64,
    pub selection_id: String,
}

impl HarnessNativeSessionSelectionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.route.validate()?;
        if self.catalog_revision == 0 || !valid_native_selection_id(&self.selection_id) {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionExternalGroupKindV1 { Project, Global }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionExternalGroupV1 {
    pub group_id: String,
    pub kind: HarnessNativeSessionExternalGroupKindV1,
    pub display_name: String,
}

impl HarnessNativeSessionExternalGroupV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_native_group_id(&self.group_id)
            || !valid_native_single_line(&self.display_name, 256, true)
            || matches!(self.display_name.as_str(), "." | "..")
            || self.display_name.contains('/')
            || self.display_name.contains('\\')
            || self.display_name.contains(':')
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionCatalogEntryV1 {
    pub selection_id: String,
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    pub completed_turn_count: Option<u64>,
    pub external_group: Option<HarnessNativeSessionExternalGroupV1>,
    pub record_id: Option<String>,
}

impl HarnessNativeSessionCatalogEntryV1 {
    fn validate_for_route(
        &self,
        route: &HarnessNativeSessionRouteV1,
    ) -> Result<(), HarnessOperatorApiError> {
        if !valid_native_selection_id(&self.selection_id)
            || self.title.as_ref().is_some_and(|value| {
                !valid_native_single_line(value, 512, false)
            })
            || self.model.as_ref().is_some_and(|value| {
                !valid_native_single_line(value, 512, true)
            })
            || self.record_id.as_deref().is_some_and(|value| {
                !valid_runtime_id(value, 128)
            })
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        if let Some(group) = &self.external_group { group.validate()?; }
        let route_fields_valid = match route.scope {
            HarnessNativeSessionCatalogScopeV1::Workspace => self.external_group.is_none(),
            HarnessNativeSessionCatalogScopeV1::Unregistered => {
                self.external_group.is_some() && self.record_id.is_none()
            }
        };
        if !route_fields_valid {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionCatalogSummaryV1 {
    pub catalog_revision: u64,
    pub recent_cutoff_unix_ms: u64,
    pub recent_total_count: u32,
    pub older_total_count: u32,
    pub recent_next_after_selection_id: Option<String>,
    pub recent_has_more: bool,
}

impl HarnessNativeSessionCatalogSummaryV1 {
    fn validate(&self, entry_count: usize) -> Result<(), HarnessOperatorApiError> {
        if self.catalog_revision == 0
            || self.recent_next_after_selection_id.as_deref().is_some_and(|value| {
                !valid_native_selection_id(value)
            })
            || self.recent_has_more != self.recent_next_after_selection_id.is_some()
            || usize::try_from(self.recent_total_count).map_or(true, |count| count < entry_count)
            || self.recent_has_more != (usize::try_from(self.recent_total_count)
                .unwrap_or(usize::MAX) > entry_count)
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionsCatalogedV1 {
    pub route: HarnessNativeSessionRouteV1,
    pub entries: Vec<HarnessNativeSessionCatalogEntryV1>,
    pub summary: Option<HarnessNativeSessionCatalogSummaryV1>,
}

impl HarnessNativeSessionsCatalogedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.route.validate()?;
        validate_native_entries(&self.route, &self.entries)?;
        if let Some(summary) = &self.summary { summary.validate(self.entries.len())?; }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionCatalogPageV1 {
    pub window: HarnessNativeSessionCatalogWindowV1,
    pub revision: u64,
    pub entries: Vec<HarnessNativeSessionCatalogEntryV1>,
    pub next_after_selection_id: Option<String>,
    pub remaining_count: u32,
    pub has_more: bool,
}

impl HarnessNativeSessionCatalogPageV1 {
    fn validate_for_route(
        &self,
        route: &HarnessNativeSessionRouteV1,
    ) -> Result<(), HarnessOperatorApiError> {
        validate_native_entries(route, &self.entries)?;
        if self.revision == 0
            || self.next_after_selection_id.as_deref().is_some_and(|value| {
                !valid_native_selection_id(value)
            })
            || self.has_more != self.next_after_selection_id.is_some()
            || self.has_more != (self.remaining_count > 0)
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionsPagedV1 {
    pub route: HarnessNativeSessionRouteV1,
    pub page: HarnessNativeSessionCatalogPageV1,
}

impl HarnessNativeSessionsPagedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.route.validate()?;
        self.page.validate_for_route(&self.route)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionPreviewRoleV1 { User, Assistant }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionPreviewMessageV1 {
    pub role: HarnessNativeSessionPreviewRoleV1,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionPreviewV1 {
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
    pub truncated: bool,
    pub messages: Vec<HarnessNativeSessionPreviewMessageV1>,
}

impl HarnessNativeSessionPreviewV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.title.as_ref().is_some_and(|value| {
            !valid_native_single_line(value, 512, false)
        })
            || self.model.as_ref().is_some_and(|value| {
                !valid_native_single_line(value, 512, true)
            })
            || self.messages.len()
                > usize::from(HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
            || self.messages.iter().any(|message| {
                message.text.len() > HARNESS_NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES
                    || message.text.chars().any(|character| {
                        character.is_control()
                            && !matches!(character, '\n' | '\r' | '\t')
                    })
            })
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionPreviewedV1 {
    pub selection: HarnessNativeSessionSelectionV1,
    pub preview: HarnessNativeSessionPreviewV1,
}

impl HarnessNativeSessionPreviewedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.selection.validate()?;
        self.preview.validate()
    }
}

/// `PreviewSessionRecord`'s reply: reuses `HarnessNativeSessionPreviewV1`
/// verbatim rather than defining a session-record-specific preview shape --
/// the node's own `SessionRecordPreview` and `NativeSessionPreview` are the
/// same type (`pub type NativeSessionPreview = SessionRecordPreview;` in
/// `gate4agent-node-protocol`), so the wire mirrors that identity instead of
/// inventing a duplicate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSessionRecordPreviewedV1 {
    pub record_id: String,
    pub preview: HarnessNativeSessionPreviewV1,
}

impl HarnessSessionRecordPreviewedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.record_id, 128) {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        self.preview.validate()
    }
}

/// `ResumeSessionRecord`'s reply: unlike `ResumeSession` (ack-only, no
/// address -- see that request variant's doc comment), the node returns the
/// freshly spawned session's address synchronously alongside the updated
/// record, and this wire mirrors that exactly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSessionRecordResumedV1 {
    pub record: HarnessRuntimeManagedSessionV1,
    pub session: HarnessRuntimeSessionAddressV1,
}

impl HarnessSessionRecordResumedV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.record.validate()?;
        self.session.validate()
    }
}

/// `IndexNativeSession`'s reply: echoes the committed selection alongside
/// the resulting managed-session record, the same pairing
/// `HarnessNativeSessionPreviewedV1` uses for its own selection/payload
/// echo.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionIndexedV1 {
    pub selection: HarnessNativeSessionSelectionV1,
    pub record: HarnessRuntimeManagedSessionV1,
}

impl HarnessNativeSessionIndexedV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.selection.validate()?;
        self.record.validate()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOperatorMutationOutcomeV1 {
    Applied,
    Replayed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOperatorHostErrorV1 {
    InvalidRequest,
    Unauthorized,
    NotFound,
    Conflict,
    TooLarge,
    Deadline,
    Busy,
    Unavailable,
    // Distinct from `Deadline`: a read timing out has no side effect, so
    // "safe to retry" is implicit. A `SpawnSession` whose C2/Node round trip
    // is lost after being accepted carries no such guarantee -- the session
    // may or may not exist. This case exists to tell the operator "check the
    // runtime inventory before retrying", not "retry freely".
    OutcomeUnknown,
    // Distinct from the generic `Unavailable`/`Internal` buckets a bare
    // `NodeFailureCode::BackendOperationFailed` would otherwise collapse
    // into: the node rejected this `SpawnSession` specifically because the
    // requested provider profile does not declare the requested transport.
    // Names its own inputs -- mirrors the kernel's own
    // `KernelCommandError::UnsupportedTransport { agent_id, transport }`,
    // which this variant exists to carry through to the operator instead of
    // collapsing into a bare `Unavailable`/`Deadline` that names neither
    // (see the `spawn_session_with_deadline` commit-wait loop in
    // `gate4agent-node`'s `server.rs`, which used to lose exactly this
    // reason to a blind commit-deadline poll). This is why the enum gave up
    // `Copy` -- every other variant is a bare tag, but the whole point here
    // is not making the operator already know what it asked for; it is
    // proving the host actually looked at the same request the operator
    // sent.
    UnsupportedTransport {
        agent: String,
        transport: HarnessRuntimeTransportV1,
    },
    // Distinct from the generic `Unavailable` bucket a bare `NodeFailureCode
    // ::UnsupportedCapability` used to collapse into: the node did not say
    // "busy" or "disconnected", it said this exact capability does not
    // exist for the addressed session/provider. `Unavailable` reads as
    // transient to a caller ("try again shortly"); this operation can never
    // succeed, so folding it into `Unavailable` invited a retry that was
    // guaranteed to fail the same way forever. Every mapper on this wire
    // that used to fold `NodeFailureCode::UnsupportedCapability` into
    // `Unavailable` now returns this instead.
    UnsupportedCapability,
    Internal,
    // Added alongside `gate4agent-harness-light` (the P2.2 light-harness
    // extraction), riding the V11 era: distinct from `NotFound` (a request
    // scoped to an id that provably does not and never will exist under the
    // current backend, e.g. any task/run id against a backend with no task
    // kernel) and from `Internal` (an unexpected failure). `Unsupported`
    // means "this verb is recognized and well-formed, but this particular
    // operator host does not implement it" -- a light-harness host rejecting
    // the task-kernel mutation family (`SubmitIntent` and its authorized
    // siblings), the node-workspace/native-history/session-record/resource-
    // mutation families, `TerminalRead`, and `SubscribeEvents` in its first
    // slice, per the app-harness protocol contract's fail-closed principle
    // (a verb a host cannot serve gets a typed rejection, never silent
    // misbehavior or a misleading `NotFound`). Purely additive: an existing
    // host (the full harness) never returns it, and every wire version this
    // enum has ever shipped under already tolerates an unrecognized error
    // variant the same way `#[serde(rename_all = "kebab-case")]` does for
    // any other closed enum here -- see this module's round-trip tests. No
    // skew risk in practice: `gate4agent-harness-light` pairs this crate
    // in-process (same build, same binary), never across a version boundary.
    Unsupported,
    // The host decoded the envelope but its declared `build_stamp` did not
    // match this side's own `BUILD_STAMP` (see `HarnessOperatorApiError::
    // BuildStampMismatch`, which this carries verbatim onto the wire so the
    // caller sees both stamps instead of the generic `InvalidRequest` every
    // other malformed-envelope shape collapses to). Reaching this variant
    // itself proves a build skew between the two loopback sides -- this
    // protocol has exactly one accepted build stamp, so a well-formed peer
    // never triggers it against a matching one.
    BuildStampMismatch { expected: String, received: String },
    // -- Everything below names its own inputs instead of collapsing into a
    // bare `Conflict`. `map_operator_service_error` (`gate4agent-harness-
    // service`'s `runtime.rs`) used to have a catch-all `_ => Conflict` arm
    // covering every one of these -- the reason a stack whose derived
    // launch-plan catalogue exceeded its page size had every `spec save`
    // refused with nothing but the word "Conflict": `InvalidTaskLaunchSelection`
    // was one of the swallowed variants. Every `HarnessServiceError` variant
    // that arm used to swallow now has its own arm there and its own named
    // variant here; the catch-all is gone. Fields whose native type lives in
    // a crate this one does not depend on (`gate4agent-node-protocol`,
    // `gate4agent-harness-service`, or the foreign `HarnessEngineError` this
    // crate must not open) ride the wire as `String` -- the same treatment
    // `UnsupportedTransport::agent` above already gives an unreachable id.
    //
    // The one exception is `HarnessServiceError::Engine`, whose OWN inner
    // variants live in `gate4agent-harness-engine` -- a crate under active
    // edit by another worker this fix must not open. Every non-`NotFound`
    // `HarnessEngineError` still collapses into `EngineRefused { detail }`
    // (its Debug rendering), which is strictly more than the bare `Conflict`
    // it used to get, without requiring that file's variant list.
    EngineRefused { detail: String },
    /// The one variant named explicitly in this fix's own brief: a reviewed
    /// task-launch selection failed one of `validate_current_launch_options`/
    /// `validate_current_issued_launch_options`'s checks. `why` is always one
    /// of a small set of fixed literals (see `HarnessServiceError::
    /// InvalidTaskLaunchSelection`'s doc comment); `plan_id` is `Some`
    /// whenever the failing check concerns a specific plan.
    InvalidLaunchSelection {
        task_id: HarnessTaskId,
        plan_id: Option<HarnessSelectorV1>,
        why: String,
    },
    UnsupportedCheckpointVersion { version: u16 },
    InvalidDispatchContext { reason: String },
    MutationDigestMismatch,
    DispatchFingerprintUnavailable,
    NonAtomicRunOperation,
    AcceptedSpawnProofRequired,
    InvalidAcceptedSpawnProof { reason: String },
    DeliveryAuthorityWindowClosed,
    DeliveryCompilationInvalid,
    InvalidStagedDeliveryProof { reason: String },
    AtomicDeliveryCommitRequired,
    ContinuationAuthorityWindowClosed,
    InvalidContinuationProof { reason: String },
    AtomicContinuationBindRequired,
    InvalidHarnessMcpReservation { reason: String },
    HarnessMcpGrantActorRefused {
        actor_kind: String,
        parent_run_id: Option<HarnessRunId>,
        grant_actor_run_id: HarnessRunId,
    },
    HarnessMcpGrantOperationLinkRefused {
        grant_id: SessionGrantId,
        operation_id: HarnessOperationId,
        existing_grant_id: SessionGrantId,
    },
    HarnessMcpGrantRevisionRefused {
        grant_id: SessionGrantId,
        durable_revision: HarnessRevision,
        presented_revision: HarnessRevision,
    },
    HarnessMcpGrantLinkRefused {
        grant_id: SessionGrantId,
        grant_actor_run_id: HarnessRunId,
        dispatch_actor_run_id: HarnessRunId,
    },
    HarnessMcpGrantTargetRefused {
        grant_id: SessionGrantId,
        presented_target: HarnessGrantTargetV1,
        allowed_targets: Vec<HarnessGrantTargetV1>,
    },
    HarnessMcpReplayMismatch,
    HarnessMcpProofMismatch,
    HarnessMcpArmProofReservationFieldRefused {
        field: String,
        durable: String,
        proof: String,
    },
    HarnessMcpArmProofRouteRefused {
        field: String,
        durable: String,
        route: String,
    },
    HarnessMcpArmProofBindingRefused {
        field: String,
        expected: String,
        actual: String,
    },
    HarnessMcpArmDurableLookupMissing {
        missing: String,
        operation_id: Option<HarnessOperationId>,
        reservation_id: Option<String>,
    },
    HarnessMcpArmReservationNotReadyRefused {
        reservation_id: String,
        state: String,
        armed_at_unix_ms: u64,
        updated_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    },
    HarnessMcpArmRouteInvalid {
        operation_id: HarnessOperationId,
        field: String,
        value: String,
    },
    HarnessMcpLaunchPolicyRefused {
        plan_policy: String,
        reservation_present: bool,
    },
    HarnessMcpLaunchReservationNotArmedRefused {
        reservation_id: String,
        state: String,
    },
    HarnessMcpLaunchOperationRefused {
        reservation_operation_id: HarnessOperationId,
        dispatch_operation_id: HarnessOperationId,
    },
    HarnessMcpLaunchGrantRefused {
        plan_grant_id: SessionGrantId,
        plan_grant_revision: HarnessRevision,
        reservation_grant_id: SessionGrantId,
        reservation_grant_revision: HarnessRevision,
    },
    HarnessMcpSpecializedTransitionRequired,
    OperatorRequestConflict { operation_id: HarnessOperationId },
    InvalidOperatorTaskTransition {
        from: HarnessTaskStateV1,
        to: HarnessTaskStateV1,
    },
    TaskHasActiveRun,
    ExecutionSpecRevisionMismatch {
        expected: Option<HarnessRevision>,
        actual: Option<HarnessRevision>,
    },
    ExecutionSpecLaunchMismatch,
    IssuedExecutionCasMismatch {
        expected: HarnessExpectedExecutionSpecRevisionV1,
        spec: Option<HarnessRevision>,
        issuance: Option<HarnessRevision>,
    },
    TaskNotReady,
    // `TaskNotReady` answered three different questions with one word: the
    // task is not in `Ready`, a dependency of it is unfinished, or it already
    // has a live run. Only the first is something the operator can see for
    // itself with `task get`; the other two are facts the host holds and the
    // operator does not. These two carry them, for the same reason
    // `UnsupportedTransport` above carries its own inputs -- the point is not
    // making the operator already know what blocked it, it is proving the
    // host looked. `dependency_ids` lists exactly the dependencies that are
    // not `Done` yet, so an operator sees what to finish first instead of a
    // shrug.
    TaskDependenciesNotDone {
        task_id: String,
        dependency_ids: Vec<String>,
    },
    // Unreachable through today's paths -- every route back into `Ready`
    // (`operator_move_task`, `operator_retry_task`) already refuses while a
    // non-terminal run exists, and `start_task_v2` flips `Ready`->`Running`
    // atomically with run creation. Named anyway rather than folded back into
    // `TaskNotReady`, so that if a future path into `Ready` ever reopens it,
    // the refusal arrives already saying which of the three things happened.
    TaskStartBlockedByRun { task_id: String },
    SchedulerResourceExhausted,
    SchedulerInvalidGraph { reason: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReadEnvelopeV1 {
    pub build_stamp: String,
    pub credential: HarnessReadCredential,
    pub request: HarnessReadRequestV1,
}

impl HarnessReadEnvelopeV1 {
    /// Builds an envelope carrying this binary's own [`BUILD_STAMP`] -- the
    /// one place every caller in this tree (this crate's own tests,
    /// `gate4agent-harness-client`) should build one from, rather than
    /// hand-filling `build_stamp` at each site.
    pub fn new(credential: HarnessReadCredential, request: HarnessReadRequestV1) -> Self {
        Self { build_stamp: BUILD_STAMP.to_string(), credential, request }
    }

    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        if self.build_stamp != BUILD_STAMP {
            return Err(HarnessReadApiError::BuildStampMismatch {
                expected: BUILD_STAMP.to_string(),
                received: self.build_stamp.clone(),
            });
        }
        validate_credential(self.credential.expose())?;
        self.request.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReadRequestV1 {
    ContextGet,
    MonitorGet { run_id: Option<HarnessRunId> },
    TimelineRead {
        run_id: Option<HarnessRunId>,
        after_sequence: Option<u64>,
        limit: u16,
    },
    TasksList {
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        /// "What did this task spawn": restricts the page to tasks whose
        /// own `parent_task_id` equals this one -- direct children only,
        /// never deeper descendants. Additive over the original bare
        /// `{ after_task_id, state, limit }` shape, so `#[serde(default)]`
        /// keeps an older caller decoding. Composes with `state` (both
        /// filters apply together) and pages the same way via
        /// `after_task_id`.
        #[serde(default)]
        parent_task_id: Option<HarnessTaskId>,
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        /// "What did this run spawn": restricts the page to runs whose own
        /// `parent_run_id` equals this one -- direct children only. Additive
        /// over the original shape (`#[serde(default)]`), composes with
        /// `task_id`/`lifecycle`, pages the same way via `after_run_id`.
        #[serde(default)]
        parent_run_id: Option<HarnessRunId>,
        limit: u16,
    },
    RunGet { run_id: HarnessRunId },
    OperationGet { operation_id: HarnessOperationId },
    /// D1/D8: `to` addresses a session record or a task (forum); `subject`/
    /// `body` reuse the task title/body bounds (S9, unchanged). The named
    /// refusal (unknown/ended recipient, no `mail` permission) travels back
    /// as `HarnessMailSendResultV1::Refused`, never as a host error -- see
    /// that type's own doc comment. `refs` is Slice C's addition (D3): at
    /// most `HARNESS_MAIL_REFS_MAX` dereferenceable references, defaulted so
    /// a caller built before Slice C still sends a valid request.
    MailSend {
        to: HarnessMailAddressV1,
        subject: String,
        body: String,
        reply_to: Option<HarnessMailMessageId>,
        #[serde(default)]
        refs: Vec<HarnessMailRefV1>,
    },
    /// D8's forum read: mail addressed to the caller's own session record
    /// plus every message addressed to a task currently visible to the
    /// caller's grant. Unacked entries sort first.
    MailInbox {
        since_unix_ms: Option<u64>,
        limit: u16,
    },
    /// Idempotent: acking an already-acked message is a no-op success (S9's
    /// at-least-once delivery termination).
    MailAck { message_id: HarnessMailMessageId },
    /// D3/Slice C: dereference one ref on a message already in the caller's
    /// inbox ("it was mailed to me" -- `HarnessEngine::mail_fetch`'s own doc
    /// comment). Authorization here is message membership, never the
    /// caller's own lineage (`read_visibility`) -- see
    /// `HARNESS_WRITE_TOOL_IDS`'s doc comment for why this otherwise-read
    /// tool lives there rather than in `HARNESS_READ_TOOL_IDS`. A refusal
    /// (not addressed to the caller, an out-of-range index, or a ref kind
    /// C1 does not yet fetch) travels back as
    /// `HarnessMailFetchResultV1::Refused`, never as a host error -- same
    /// pattern as `MailSend`'s own named refusal.
    MailFetch {
        message_id: HarnessMailMessageId,
        ref_index: u16,
    },
    /// D5, Slice D: an agent creates a task under its own subtree --
    /// `parent_task_id: None` means "under my own task"; `Some` must name
    /// the caller's own task or a strict descendant of it (checked
    /// service-side, `HarnessEngine::task_is_strict_descendant`), never a
    /// task system-wide the way the operator's own `HarnessCreateTaskRequestV1`
    /// can. Gated by `grant.task_permissions.create`. Every refusal --
    /// unreachable parent, a terminal parent, or an invalid title/body/
    /// dependencies -- travels back as `HarnessTaskCreateResultV1`'s own
    /// named variant, never a host error (the same discipline `MailSend`
    /// established for mail). This bound is the coarse wire-level ceiling
    /// only; the authoritative title/body rule is the engine's own
    /// `HarnessTaskV1::validate()`, reached when the mutation applies.
    TaskCreate {
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
    },
    /// D5, Slice D: restricted service-side to a STRICT descendant of the
    /// caller's own task -- never the task that governs the caller's own
    /// run (`HarnessTaskMoveResultV1::TaskIsOwn`). Gated by
    /// `grant.task_permissions.mutate`. `to` names the operator's own task
    /// graph (`validate_operator_move` in `gate4agent-harness-service`); an
    /// illegal transition or a stale `expected_revision` is a named refusal,
    /// never a host error.
    TaskMove {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        to: HarnessTaskStateV1,
    },
    /// S10: a session reports its OWN run finished -- resolved from the
    /// caller's own grant binding (`actor_run_id`), the same field every
    /// other agent verb resolves its identity from; there is no run-id
    /// argument, so a session can never finish another session's run.
    /// `done` applies `HarnessLifecycleProjectionV1::CompletedReview`
    /// (`gate4agent-harness-service`), landing the task in `Review` --
    /// never `Done`, the review gate a session cannot skip. `failed`
    /// applies `HarnessLifecycleProjectionV1::Failed` and records a
    /// retryable `HarnessFailureV1`. Never gated by any grant permission
    /// (unconditional in `allowed_tool_ids`, the same way `g4a_context_get`
    /// is): every session needs the ability to report its own work
    /// finished regardless of what else its grant allows, or nothing ever
    /// closes -- the exact structural gap this verb exists to close.
    /// `summary`, when given, is posted as mail to the task's own forum
    /// (`HarnessMailAddressV1::Task`) through the existing mail-send path,
    /// never a new free-text result field; a mail failure never undoes the
    /// finish, which is already committed by the time the mail is
    /// attempted. Not idempotent: a second call against an already-terminal
    /// run is refused by name as `HarnessRunFinishResultV1::AlreadyFinished`,
    /// never silently accepted.
    RunFinish {
        outcome: HarnessRunFinishOutcomeV1,
        #[serde(default)]
        summary: Option<String>,
    },
}

impl HarnessReadRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::RunFinish { summary, .. } => {
                if let Some(summary) = summary {
                    if summary.len() > HARNESS_BODY_MAX_BYTES {
                        return Err(HarnessReadApiError::InvalidText("run finish summary"));
                    }
                }
                Ok(())
            }
            Self::TaskCreate { title, body, parent_task_id } => {
                if title.is_empty()
                    || title.len() > HARNESS_TITLE_MAX_BYTES
                    || body.len() > HARNESS_BODY_MAX_BYTES
                {
                    return Err(HarnessReadApiError::InvalidTaskCreate);
                }
                if let Some(parent_task_id) = parent_task_id {
                    parent_task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                Ok(())
            }
            Self::TaskMove { task_id, expected_revision, .. } => {
                task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                expected_revision.validate().map_err(HarnessReadApiError::Protocol)
            }
            Self::MailSend { to, subject, body, reply_to, refs } => {
                to.validate().map_err(HarnessReadApiError::Protocol)?;
                if subject.is_empty()
                    || subject.len() > HARNESS_TITLE_MAX_BYTES
                    || body.len() > HARNESS_BODY_MAX_BYTES
                    || refs.len() > HARNESS_MAIL_REFS_MAX
                {
                    return Err(HarnessReadApiError::InvalidMailMessage);
                }
                if let Some(reply_to) = reply_to {
                    reply_to.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                for reference in refs {
                    reference.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                Ok(())
            }
            Self::MailInbox { limit, .. } => validate_limit(*limit, HARNESS_ENTITY_PAGE_LIMIT_MAX),
            Self::MailAck { message_id } => {
                message_id.validate().map_err(HarnessReadApiError::Protocol)
            }
            Self::MailFetch { message_id, .. } => {
                message_id.validate().map_err(HarnessReadApiError::Protocol)
            }
            Self::TimelineRead { run_id, after_sequence, limit } => {
                if let Some(run_id) = run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if after_sequence == &Some(0) {
                    return Err(HarnessReadApiError::InvalidCursor);
                }
                validate_limit(*limit, HARNESS_TIMELINE_PAGE_LIMIT_MAX)
            }
            Self::TasksList { after_task_id, parent_task_id, limit, .. } => {
                if let Some(task_id) = after_task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if let Some(task_id) = parent_task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                validate_limit(*limit, HARNESS_ENTITY_PAGE_LIMIT_MAX)
            }
            Self::RunsList { task_id, after_run_id, parent_run_id, limit, .. } => {
                if let Some(task_id) = task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if let Some(run_id) = after_run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if let Some(run_id) = parent_run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                validate_limit(*limit, HARNESS_ENTITY_PAGE_LIMIT_MAX)
            }
            Self::TaskGet { task_id } => task_id.validate().map_err(HarnessReadApiError::Protocol),
            Self::RunGet { run_id } => run_id.validate().map_err(HarnessReadApiError::Protocol),
            Self::OperationGet { operation_id } => {
                operation_id.validate().map_err(HarnessReadApiError::Protocol)
            }
            Self::MonitorGet { run_id } => {
                if let Some(run_id) = run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                Ok(())
            }
            Self::ContextGet => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReadReplyV1 {
    Ok { response: HarnessReadResponseV1 },
    Error { error: HarnessReadHostErrorV1 },
}

impl HarnessReadReplyV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::Ok { response } => response.validate(),
            Self::Error { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReadResponseV1 {
    Context(SessionContextV1),
    Monitor(SessionMonitorV1),
    Timeline(TimelinePageV1),
    Tasks(TaskPageV1),
    Task(RedactedTaskV1),
    Runs(RunPageV1),
    Run(RedactedRunV1),
    Operation(RedactedOperationV1),
    MailSend(HarnessMailSendResultV1),
    MailInbox(HarnessMailInboxPageV1),
    MailAck(HarnessMailAckResultV1),
    MailFetch(HarnessMailFetchResultV1),
    TaskCreate(HarnessTaskCreateResultV1),
    TaskMove(HarnessTaskMoveResultV1),
    RunFinish(HarnessRunFinishResultV1),
}

impl HarnessReadResponseV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::Context(value) => value.validate(),
            Self::Monitor(value) => value.validate(),
            Self::Timeline(value) => value.validate(),
            Self::Tasks(value) => value.validate(),
            Self::Task(value) => value.validate(),
            Self::Runs(value) => value.validate(),
            Self::Run(value) => value.validate(),
            Self::Operation(value) => value.validate(),
            Self::MailSend(value) => value.validate(),
            Self::MailInbox(value) => value.validate(),
            Self::MailAck(value) => value.validate(),
            Self::MailFetch(value) => value.validate(),
            Self::TaskCreate(value) => value.validate().map_err(HarnessReadApiError::Protocol),
            Self::TaskMove(value) => value.validate().map_err(HarnessReadApiError::Protocol),
            Self::RunFinish(value) => value.validate().map_err(HarnessReadApiError::Protocol),
        }
    }
}

/// The outcome of one `g4a_mail_send` call. A refusal is a domain answer,
/// never a host error (D1's "an unknown, ended or unreachable recipient is a
/// named refusal, never a silent drop"): `reason` is
/// `HarnessEngineError`'s own `Display` text for `MailRecipientUnknown`/
/// `MailRecipientEnded`/`MailPermissionDenied`, carried verbatim so the
/// caller sees exactly what the engine refused and why.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessMailSendResultV1 {
    Accepted { message_id: HarnessMailMessageId },
    Refused { reason: String },
}

impl HarnessMailSendResultV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::Accepted { message_id } => {
                message_id.validate().map_err(HarnessReadApiError::Protocol)
            }
            Self::Refused { reason } => {
                if reason.is_empty()
                    || reason.len() > HARNESS_MAIL_REFUSAL_REASON_MAX_BYTES
                    || reason.chars().any(char::is_control)
                {
                    return Err(HarnessReadApiError::InvalidMailMessage);
                }
                Ok(())
            }
        }
    }
}

/// One `g4a_mail_inbox` page entry: the envelope, whether the caller has
/// acked it, and its `thread` -- the ids of every message in the transitive
/// `reply_to` closure (S9: "threading needs no second concept"), bounded by
/// `HARNESS_MAIL_THREAD_IDS_MAX`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMailInboxEntryV1 {
    pub message_id: HarnessMailMessageId,
    pub from: HarnessRecordRef,
    pub to: HarnessMailAddressV1,
    pub subject: String,
    pub body: String,
    pub reply_to: Option<HarnessMailMessageId>,
    pub task_id: Option<HarnessTaskId>,
    /// Slice C's addition: what this message carries, so a reader sees what
    /// is attached without dereferencing it -- bounded the same way the
    /// message itself is, `HARNESS_MAIL_REFS_MAX`.
    #[serde(default)]
    pub refs: Vec<HarnessMailRefV1>,
    pub created_at_unix_ms: u64,
    pub acked: bool,
    pub thread: Vec<HarnessMailMessageId>,
}

impl HarnessMailInboxEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.message_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.from.validate().map_err(HarnessReadApiError::Protocol)?;
        self.to.validate().map_err(HarnessReadApiError::Protocol)?;
        if self.subject.is_empty()
            || self.subject.len() > HARNESS_TITLE_MAX_BYTES
            || self.body.len() > HARNESS_BODY_MAX_BYTES
            || self.refs.len() > HARNESS_MAIL_REFS_MAX
        {
            return Err(HarnessReadApiError::InvalidMailMessage);
        }
        if let Some(reply_to) = &self.reply_to {
            reply_to.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        for reference in &self.refs {
            reference.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if self.created_at_unix_ms == 0 || self.thread.len() > usize::from(HARNESS_MAIL_THREAD_IDS_MAX) {
            return Err(HarnessReadApiError::InvalidMailInbox);
        }
        for message_id in &self.thread {
            message_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        Ok(())
    }
}

/// `g4a_mail_inbox`'s full reply, and the operator wire's `MailInbox` reply
/// -- the same shape both doors read (`docs/gate4agent/plans/
/// gate4agent-arc-mailbox-and-task-layer-2026-09-02.md` §3 Slice B).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMailInboxPageV1 {
    pub messages: Vec<HarnessMailInboxEntryV1>,
    pub unacked: u64,
}

impl HarnessMailInboxPageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        if self.messages.len() > usize::from(HARNESS_ENTITY_PAGE_LIMIT_MAX) {
            return Err(HarnessReadApiError::InvalidMailInbox);
        }
        for message in &self.messages {
            message.validate()?;
        }
        Ok(())
    }
}

/// `g4a_mail_ack`'s reply -- idempotent, so `acked` is `true` on every
/// success (an already-acked message acks again as a no-op).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMailAckResultV1 {
    pub acked: bool,
}

impl HarnessMailAckResultV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> { Ok(()) }
}

/// Bound on `HarnessMailFetchResultV1::MailRefWorkspaceAmbiguous.nodes` --
/// generous for any real deployment (a workspace name registered on this
/// many distinct connected nodes at once would itself be the anomaly worth
/// investigating), and small enough that the list never threatens this
/// wire's response-size bound the way an unbounded node roster could.
pub const HARNESS_MAIL_FETCH_AMBIGUOUS_NODES_MAX: usize = 16;

/// The outcome of one `g4a_mail_fetch` call (D3, Slice C). `Run` and
/// `Result` are the two kinds C1 dereferences: `Run` returns the named run's
/// redacted monitor projection plus a bounded timeline page -- the same
/// shapes `g4a_monitor_get`/`g4a_timeline_read` already return, just reached
/// through the mailed ref instead of the caller's own lineage; `Result`
/// returns the redacted run record the result reference names. `ContextPack`
/// (C2) is the mailed pack's own bytes, hex-encoded like every other raw
/// byte payload this wire carries (`HarnessMcpReplyChunkHexV1`'s convention):
/// `digest`/`id` mirror `HarnessResolvedContextPackReceiptV1`'s own fields,
/// `byte_len` is the RAW (pre-hex) length, and `bytes_hex.len()` is always
/// exactly `byte_len * 2`. `WorkspacePath` (C3) is the mailed file's own
/// UTF-8 text verbatim (never hex -- there is no non-UTF-8 case to encode
/// around, see below): `byte_len` is `text`'s own byte length, and `sha256`
/// is the bare (no `sha256:` prefix) lowercase hex digest of `text`'s bytes,
/// recomputed by the service against the mailed ref's own `sha256` before
/// this is ever returned -- a mismatch is `MailedWorkspacePathDigestMismatch`
/// instead. `Refused` carries `HarnessEngineError`'s own `Display` text
/// verbatim for `MailNotForReader` (message not addressed to this reader) or
/// `MailRefIndexOutOfRange` (no such ref on the message), or
/// `gate4agent-harness-service`'s own text for a malformed mailed
/// `WorkspacePath` ref (an invalid workspace or path identifier), a mailed
/// `Run`/`Result` ref naming a run that no longer exists, or a malformed
/// `Result` reference -- none of these name a bounded, reusable set of
/// inputs the way the typed variants below do, so they stay prose. The
/// remaining seven variants are named resolutions of a ref this side could
/// not complete, each carrying its own inputs directly rather than a
/// formatted string, the same pattern `HarnessMailSendResultV1` established
/// for `Refused` alone. Two are `ContextPack` (C2)'s own:
/// `MailRefContextPackUnknown` folds together the two moments nothing
/// answers for a mailed pack's `digest` any more -- no run's own
/// `context_pack` receipt carries it at the engine's scan time
/// (`HarnessEngineError::MailRefTargetUnknown`, D3/Slice C's own resolution
/// gate) and the pack having been evicted at the node by the time the fetch
/// actually reaches it (`NodeFailureCode::UnknownContextPack`) -- both mean
/// the identical thing to the caller, so both fold into this one variant;
/// `MailedContextPackDigestMismatch` is the pack's own bytes not hashing to
/// `digest` once fetched (`expected`/`recomputed` are `SpawnContextDigest`'s
/// own `sha256:<64 hex>` rendering, unlike `WorkspacePath`'s bare hex,
/// because a `ContextPack` ref's digest was always a `SpawnContextDigest` to
/// begin with). The other five are `WorkspacePath` (C3)'s own, unchanged:
/// `MailRefWorkspaceUnknown` (no connected node's runtime inventory
/// registers `workspace`, or the node no longer does by the time the read
/// reaches it), `MailRefWorkspaceAmbiguous` (more than one connected node
/// does, named in `nodes`), `MailRefWorkspacePathUnknown` (the one node that
/// does has no such file at `path`, or cannot read it),
/// `MailedWorkspacePathContentNotFetchable` (the file exists but is not
/// UTF-8, or exceeds the node's read bound -- `why` names which), and
/// `MailedWorkspacePathDigestMismatch` (the file's own bytes do not hash to
/// the digest the mailed ref named).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessMailFetchResultV1 {
    Run { monitor: SessionMonitorV1, timeline: TimelinePageV1 },
    Result { run: RedactedRunV1 },
    ContextPack { digest: String, id: String, byte_len: u32, bytes_hex: String },
    WorkspacePath { workspace: String, path: String, byte_len: u32, sha256: String, text: String },
    Refused { reason: String },
    MailRefContextPackUnknown { digest: String },
    MailedContextPackDigestMismatch { expected: String, recomputed: String },
    MailRefWorkspaceUnknown { workspace: String },
    MailRefWorkspaceAmbiguous { workspace: String, nodes: Vec<String> },
    MailRefWorkspacePathUnknown { workspace: String, path: String },
    MailedWorkspacePathContentNotFetchable { workspace: String, path: String, why: String },
    MailedWorkspacePathDigestMismatch { expected: String, recomputed: String },
}

fn valid_mail_fetch_workspace(workspace: &str) -> bool {
    valid_runtime_id(workspace, 128)
}

fn valid_mail_fetch_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= HARNESS_RUN_GIT_PATH_MAX_BYTES
        && !path.chars().any(char::is_control)
}

fn valid_mail_fetch_why(why: &str) -> bool {
    !why.is_empty()
        && why.len() <= HARNESS_MAIL_REFUSAL_REASON_MAX_BYTES
        && !why.chars().any(char::is_control)
}

fn valid_bare_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(is_lower_hex)
}

impl HarnessMailFetchResultV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::Run { monitor, timeline } => {
                monitor.validate()?;
                timeline.validate()
            }
            Self::Result { run } => run.validate(),
            Self::ContextPack { digest, id, byte_len, bytes_hex } => {
                if !valid_sha256_digest(digest)
                    || !valid_runtime_id(id, 128)
                    || *byte_len == 0
                    || *byte_len > HARNESS_CONTEXT_PACK_MAX_BYTES
                    || bytes_hex.len() != (*byte_len as usize) * 2
                    || !bytes_hex.bytes().all(is_lower_hex)
                {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::WorkspacePath { workspace, path, byte_len, sha256, text } => {
                if !valid_mail_fetch_workspace(workspace)
                    || !valid_mail_fetch_path(path)
                    || !valid_bare_sha256(sha256)
                    || text.len() as u64 != u64::from(*byte_len)
                {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::Refused { reason } => {
                if reason.is_empty()
                    || reason.len() > HARNESS_MAIL_REFUSAL_REASON_MAX_BYTES
                    || reason.chars().any(char::is_control)
                {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailRefContextPackUnknown { digest } => {
                if !valid_sha256_digest(digest) {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailedContextPackDigestMismatch { expected, recomputed } => {
                if !valid_sha256_digest(expected) || !valid_sha256_digest(recomputed) {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailRefWorkspaceUnknown { workspace } => {
                if !valid_mail_fetch_workspace(workspace) {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailRefWorkspaceAmbiguous { workspace, nodes } => {
                if !valid_mail_fetch_workspace(workspace)
                    || nodes.len() < 2
                    || nodes.len() > HARNESS_MAIL_FETCH_AMBIGUOUS_NODES_MAX
                    || !nodes.iter().all(|node| valid_runtime_id(node, 128))
                {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailRefWorkspacePathUnknown { workspace, path } => {
                if !valid_mail_fetch_workspace(workspace) || !valid_mail_fetch_path(path) {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailedWorkspacePathContentNotFetchable { workspace, path, why } => {
                if !valid_mail_fetch_workspace(workspace)
                    || !valid_mail_fetch_path(path)
                    || !valid_mail_fetch_why(why)
                {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
            Self::MailedWorkspacePathDigestMismatch { expected, recomputed } => {
                if !valid_bare_sha256(expected) || !valid_bare_sha256(recomputed) {
                    return Err(HarnessReadApiError::InvalidMailFetch);
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessReadHostErrorV1 {
    InvalidRequest,
    Unauthorized,
    NotFoundOrDenied,
    TooLarge,
    Deadline,
    Internal,
    // The host decoded the envelope but its declared `build_stamp` did not
    // match this side's own `BUILD_STAMP` (see `HarnessReadApiError::
    // BuildStampMismatch`, which this carries verbatim onto the wire so the
    // caller sees both stamps instead of the generic `InvalidRequest` every
    // other malformed-envelope shape collapses to). Reaching this variant
    // itself proves a build skew between the two loopback sides -- this
    // protocol has exactly one accepted build stamp, so a well-formed peer
    // never triggers it against a matching one.
    BuildStampMismatch { expected: String, received: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContextV1 {
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub actor_run: CallerRunV1,
    /// The calling run's own task, redacted the same way as `TaskGet` --
    /// present only when the grant's task scope makes it visible (mirrors
    /// `actor_run.task_id`).
    pub task: Option<RedactedTaskV1>,
    /// Every OTHER run recorded against `task` ("what previous sessions did
    /// on it"), redacted the same way as `RunGet`. Gated solely by `task`
    /// being visible -- independent of the grant's `runs` read scope, which
    /// governs the general run-browsing tools instead. Always empty when
    /// `task` is `None`; never includes runs of another task.
    pub sibling_runs: Vec<RedactedRunV1>,
    pub read_permissions: HarnessReadPermissionsV1,
    pub monitoring_visibility: HarnessMonitoringVisibilityV1,
    /// Observation, not a budget: the number of tasks parented directly
    /// under the calling run's own task right now
    /// (`HarnessEngine::task_child_count`), `0` if it has no children.
    /// Unconditional -- computed from `actor_run`'s own task the same way
    /// the retired `maximum_child_count` wire field it replaces was
    /// (independent of whether `task` above is redacted to `None`). That
    /// retired field stated a cap that was validated for range but compared
    /// against no actual child count anywhere in the service -- a session
    /// could read a limit here and still create children past it. The
    /// owner's ruling: do not cap a session's children, detect them. Nothing
    /// in this wire enforces an upper bound on this count.
    pub child_task_count: u64,
    /// Observation, not a budget: the depth of the deepest chain of tasks
    /// parented (transitively) under the calling run's own task right now
    /// (`HarnessEngine::task_subtree_depth`) -- `0` if it has no children,
    /// `1` if it has children but no grandchildren, and so on. Unconditional
    /// in the same sense as `child_task_count` above. Replaces the retired
    /// `maximum_child_depth` wire field for the same reason `child_task_
    /// count` replaces `maximum_child_count` (see that field's doc comment).
    /// Nothing in this wire enforces an upper bound on this depth.
    pub child_task_subtree_depth: u64,
    /// D1: whether this grant holds the mail permission -- the source
    /// `expected_allowed_tool_ids` reads to decide whether the four
    /// `g4a_mail_*` ids belong in `allowed_tool_ids`, the same way
    /// `read_permissions`/`monitoring_visibility` decide the eight reads.
    pub mail: bool,
    /// D5, Slice D: mirrors `grant.task_permissions.create` -- gates
    /// `g4a_task_create` in `allowed_tool_ids`, the same wiring `mail`
    /// already establishes for its own four siblings.
    pub task_create: bool,
    /// D5, Slice D: mirrors `grant.task_permissions.mutate` -- gates
    /// `g4a_task_move` in `allowed_tool_ids`.
    pub task_mutate: bool,
    pub allowed_tool_ids: Vec<String>,
    pub history_message_count: Option<u64>,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl SessionContextV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.grant_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.grant_revision.validate().map_err(HarnessReadApiError::Protocol)?;
        self.actor_run.validate()?;
        match &self.task {
            Some(task) => {
                task.validate()?;
                if self.actor_run.task_id.as_ref() != Some(&task.task_id) {
                    return Err(HarnessReadApiError::InvalidContextTask);
                }
            }
            None if !self.sibling_runs.is_empty() => {
                return Err(HarnessReadApiError::InvalidContextTask);
            }
            None => {}
        }
        if self.sibling_runs.len() > HARNESS_LINKS_MAX
            || self.sibling_runs.windows(2).any(|pair| pair[0].run_id >= pair[1].run_id)
            || self.sibling_runs.iter().any(|run| {
                run.run_id == self.actor_run.run_id
                    || run.task_id.as_ref() != self.actor_run.task_id.as_ref()
            })
        {
            return Err(HarnessReadApiError::InvalidContextTask);
        }
        for run in &self.sibling_runs {
            run.validate()?;
        }
        self.read_permissions.validate().map_err(HarnessReadApiError::Protocol)?;
        if self.allowed_tool_ids.len() > HARNESS_READ_TOOL_IDS.len() + HARNESS_WRITE_TOOL_IDS.len()
            || self.allowed_tool_ids.windows(2).any(|pair| pair[0] >= pair[1])
            || self.allowed_tool_ids.iter().any(|id| {
                !HARNESS_READ_TOOL_IDS.contains(&id.as_str())
                    && !HARNESS_WRITE_TOOL_IDS.contains(&id.as_str())
            })
            || self.allowed_tool_ids != expected_allowed_tool_ids(self)
        {
            return Err(HarnessReadApiError::InvalidAllowedTools);
        }
        Ok(())
    }
}

fn expected_allowed_tool_ids(context: &SessionContextV1) -> Vec<String> {
    // S10: `g4a_run_finish` is unconditional, exactly like `g4a_context_get`
    // -- see `HARNESS_WRITE_TOOL_IDS`'s own doc comment for why it carries
    // no grant-permission gate at all.
    let mut tools = vec!["g4a_context_get", "g4a_run_finish"];
    if context.monitoring_visibility != HarnessMonitoringVisibilityV1::None {
        tools.push("g4a_monitor_get");
    }
    if context.monitoring_visibility == HarnessMonitoringVisibilityV1::Timeline {
        tools.push("g4a_timeline_read");
    }
    if context.read_permissions.tasks != HarnessEntityReadScopeV1::None {
        tools.extend(["g4a_tasks_get", "g4a_tasks_list"]);
    }
    if context.read_permissions.runs != HarnessEntityReadScopeV1::None {
        tools.extend(["g4a_runs_get", "g4a_runs_list"]);
    }
    if context.read_permissions.operations != HarnessEntityReadScopeV1::None {
        tools.push("g4a_operation_get");
    }
    if context.mail {
        tools.extend(["g4a_mail_send", "g4a_mail_inbox", "g4a_mail_ack", "g4a_mail_fetch"]);
    }
    if context.task_create {
        tools.push("g4a_task_create");
    }
    if context.task_mutate {
        tools.push("g4a_task_move");
    }
    tools.sort_unstable();
    tools.into_iter().map(str::to_owned).collect()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallerRunV1 {
    pub run_id: HarnessRunId,
    pub task_id: Option<HarnessTaskId>,
    pub parent_run_id: Option<HarnessRunId>,
    pub lifecycle: HarnessRunLifecycleV1,
    pub references_redacted: bool,
}

impl CallerRunV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(parent_run_id) = &self.parent_run_id {
            parent_run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionAvailabilityV1 {
    Unknown,
    NotObserved,
    Current,
    Partial,
    Frozen,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionFreshnessV1 {
    Unavailable,
    Live,
    Stale,
    IncompleteAfterGap,
    LastKnown,
    ReplacedIncarnation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureObservationStateV1 {
    Unknown,
    NotSupportedByObservedSources,
    SupportedNotObserved,
    Observed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorFeatureStatesV1 {
    pub todo: FeatureObservationStateV1,
    pub tools: FeatureObservationStateV1,
    pub subagents: FeatureObservationStateV1,
    pub interactions: FeatureObservationStateV1,
    pub owned_processes: FeatureObservationStateV1,
    pub files: FeatureObservationStateV1,
    pub usage: FeatureObservationStateV1,
    pub history: FeatureObservationStateV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMonitorV1 {
    pub run_id: HarnessRunId,
    pub visibility: HarnessMonitoringVisibilityV1,
    pub availability: ProjectionAvailabilityV1,
    pub freshness: ProjectionFreshnessV1,
    pub transport_incomplete: bool,
    pub features: MonitorFeatureStatesV1,
    pub todo_total: u16,
    pub todo_completed: u16,
    pub active_tools: u16,
    pub active_subagents: u16,
    pub active_interactions: u16,
    /// Count of `ObservationKindV1::ActionBlocked` observations recorded
    /// against this session so far (`SessionProjection::blocked_count`,
    /// saturated to `u16`) -- NOT a "currently pending" count like its
    /// `active_*` siblings: unlike a tool call or an interaction, a block
    /// has no resolution step to wait on (see that observation kind's own
    /// doc comment), so there is no pending/resolved split to count
    /// separately. Mirrors the fleet-wide `HarnessRuntimeManagedSessionV1::
    /// blocked_count` this same tally already backs, just for this one run.
    /// `#[serde(default)]`: additive field on a read-wire response that is
    /// never persisted, only built fresh per read.
    #[serde(default)]
    pub active_blocks: u16,
    pub active_processes: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub context_window_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<SessionMonitorHistoryV1>,
    pub detail: Option<SessionMonitorDetailV1>,
}

impl SessionMonitorV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        if self.todo_completed > self.todo_total {
            return Err(HarnessReadApiError::InvalidMonitorCounts);
        }
        let detail_allowed = matches!(
            self.visibility,
            HarnessMonitoringVisibilityV1::Detail | HarnessMonitoringVisibilityV1::Timeline
        );
        if self.detail.is_some() && !detail_allowed {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        if matches!(self.availability, ProjectionAvailabilityV1::Unknown | ProjectionAvailabilityV1::NotObserved)
            && self.detail.is_some()
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        if self.freshness == ProjectionFreshnessV1::IncompleteAfterGap
            && !self.transport_incomplete
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        if let Some(detail) = &self.detail {
            detail.validate(&self.features)?;
        }
        if self.features.history != FeatureObservationStateV1::Observed
            && self.history.is_some()
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        validate_feature_counts(self)?;
        Ok(())
    }

    pub fn validate_for(&self, run_id: &HarnessRunId) -> Result<(), HarnessReadApiError> {
        self.validate()?;
        if &self.run_id != run_id {
            return Err(HarnessReadApiError::InvalidRunState);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMonitorHistoryV1 {
    pub message_count: u64,
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMonitorDetailV1 {
    pub todo_facts: Vec<TodoFactV1>,
    pub tool_facts: Vec<ActivityFactV1>,
    pub subagent_facts: Vec<ActivityFactV1>,
    pub interaction_facts: Vec<InteractionFactV1>,
    /// One fact per `ObservationKindV1::ActionBlocked` observation recorded
    /// against this session. `gate4agent-harness-service::read::monitor_detail`
    /// derives these from the session's own bounded `timeline`
    /// (`gate4agent_observation_engine::SessionProjection::timeline`), not
    /// from a dedicated correlation-tracked list: a block has no
    /// request/response pairing for that engine's projection to track, only
    /// a running `blocked_count` tally (see `SessionMonitorV1::active_blocks`).
    /// Its own list rather than folded into `interaction_facts`:
    /// `interaction_facts` is built strictly off `SessionProjection::
    /// interactions`, which `ActionBlocked` never populates, so folding it
    /// in would need widening that engine-side projection, out of this
    /// field's own scope. `#[serde(default)]` reads a monitor response
    /// captured before this field existed as empty rather than failing to
    /// deserialize -- this type is never persisted, only built fresh per
    /// read and sent over the operator/grant-bound wire.
    #[serde(default)]
    pub block_facts: Vec<BlockFactV1>,
    pub process_facts: Vec<ActivityFactV1>,
    pub file_facts: Vec<FileFactV1>,
}

impl SessionMonitorDetailV1 {
    pub fn validate(&self, features: &MonitorFeatureStatesV1) -> Result<(), HarnessReadApiError> {
        validate_monitor_facts(&self.todo_facts)?;
        validate_monitor_facts(&self.tool_facts)?;
        validate_monitor_facts(&self.subagent_facts)?;
        validate_monitor_facts(&self.interaction_facts)?;
        validate_monitor_facts(&self.block_facts)?;
        validate_monitor_facts(&self.process_facts)?;
        validate_monitor_facts(&self.file_facts)?;
        for fact in &self.todo_facts { fact.validate()?; }
        for fact in &self.tool_facts { fact.validate()?; }
        for fact in &self.subagent_facts { fact.validate()?; }
        for fact in &self.interaction_facts { fact.validate()?; }
        for fact in &self.block_facts { fact.validate()?; }
        for fact in &self.process_facts { fact.validate()?; }
        for fact in &self.file_facts { fact.validate()?; }
        if self.tool_facts.iter().any(|fact| fact.class != ActivityClassV1::Tool)
            || self.subagent_facts.iter().any(|fact| fact.class != ActivityClassV1::Subagent)
            || self.process_facts.iter().any(|fact| fact.class != ActivityClassV1::OwnedProcess)
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        Ok(())
            .and_then(|_| require_observed_or_empty(features.todo, &self.todo_facts))
            .and_then(|_| require_observed_or_empty(features.tools, &self.tool_facts))
            .and_then(|_| require_observed_or_empty(features.subagents, &self.subagent_facts))
            .and_then(|_| require_observed_or_empty(features.interactions, &self.interaction_facts))
            .and_then(|_| require_observed_or_empty(features.owned_processes, &self.process_facts))
            .and_then(|_| require_observed_or_empty(features.files, &self.file_facts))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TodoFactV1 {
    pub state: TodoStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub evidence: ObservationEvidenceV1,
}

impl TodoFactV1 {
    fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_optional_observation_text(
            "todo id",
            self.todo_id.as_deref(),
            HARNESS_OBSERVATION_LABEL_MAX_BYTES,
        )?;
        validate_optional_observation_text(
            "todo label",
            self.label.as_deref(),
            HARNESS_OBSERVATION_TODO_TEXT_MAX_BYTES,
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TodoStateV1 { Pending, InProgress, Completed, Unknown }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityFactV1 {
    pub class: ActivityClassV1,
    pub state: ActivityStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<u16>,
    pub evidence: ObservationEvidenceV1,
}

impl ActivityFactV1 {
    fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_optional_observation_text(
            "activity label",
            self.label.as_deref(),
            HARNESS_OBSERVATION_LABEL_MAX_BYTES,
        )?;
        validate_observation_correlation(self.correlation)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityClassV1 { Tool, Subagent, OwnedProcess }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityStateV1 { Started, Active, Waiting, Completed, Failed, UnknownAfterGap }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionFactV1 {
    pub class: InteractionClassV1,
    pub state: InteractionStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<u16>,
    pub evidence: ObservationEvidenceV1,
}

impl InteractionFactV1 {
    fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_optional_observation_text(
            "interaction label",
            self.label.as_deref(),
            HARNESS_OBSERVATION_LABEL_MAX_BYTES,
        )?;
        validate_observation_correlation(self.correlation)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InteractionClassV1 { Attention, Approval, UserInput }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InteractionStateV1 { Required, Responded, Dismissed, UnknownAfterGap }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileFactV1 {
    pub action: FileActionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    pub evidence: ObservationEvidenceV1,
}

impl FileFactV1 {
    fn validate(&self) -> Result<(), HarnessReadApiError> {
        if let Some(path) = self.relative_path.as_deref() {
            validate_observation_relative_path(path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileActionV1 { Changed }

/// One `ObservationKindV1::ActionBlocked` observation surfaced on the
/// monitor wire -- see `SessionMonitorDetailV1::block_facts`'s own doc
/// comment for why this is its own list rather than folded into
/// `interaction_facts`/`process_facts`. `label` is `<authority slug>:
/// <reason>`, truncated to `HARNESS_OBSERVATION_BLOCK_LABEL_MAX_BYTES` by
/// the producer -- never the raw, up-to-1024-byte `reason` verbatim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockFactV1 {
    pub state: BlockStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<u16>,
    pub evidence: ObservationEvidenceV1,
}

impl BlockFactV1 {
    fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_optional_observation_text(
            "block label",
            self.label.as_deref(),
            HARNESS_OBSERVATION_BLOCK_LABEL_MAX_BYTES,
        )?;
        validate_observation_correlation(self.correlation)
    }
}

/// A single, always-terminal variant today (paralleling `FileActionV1::
/// Changed`) -- kept as its own enum rather than a bare unit field so a
/// future, more granular block state has somewhere to land without another
/// wire shape change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockStateV1 { Blocked }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationEvidenceV1 {
    StructuredProvider,
    ManagedHook,
    WorkspaceObservation,
    NodeLifecycle,
    History,
    PtyHint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineEntryV1 {
    pub sequence: u64,
    pub received_at_ms: u64,
    pub category: TimelineCategoryV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub state: TimelineStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<u16>,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelinePageV1 {
    pub run_id: HarnessRunId,
    pub availability: ProjectionAvailabilityV1,
    pub freshness: ProjectionFreshnessV1,
    pub transport_incomplete: bool,
    pub entries: Vec<TimelineEntryV1>,
    pub next_cursor: Option<u64>,
}

impl TimelinePageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        validate_bounded(&self.entries, HARNESS_TIMELINE_PAGE_LIMIT_MAX, TimelineEntryV1::validate)?;
        if self.entries.windows(2).any(|pair| pair[0].sequence >= pair[1].sequence)
            || self.next_cursor == Some(0)
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        if self.next_cursor.is_some()
            && self.next_cursor != self.entries.last().map(|entry| entry.sequence)
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        Ok(())
    }

    pub fn validate_for(&self, run_id: &HarnessRunId) -> Result<(), HarnessReadApiError> {
        self.validate()?;
        if &self.run_id != run_id {
            return Err(HarnessReadApiError::InvalidRunState);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPageV1 {
    pub tasks: Vec<RedactedTaskV1>,
    pub next_cursor: Option<HarnessTaskId>,
}

impl TaskPageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_bounded(&self.tasks, HARNESS_ENTITY_PAGE_LIMIT_MAX, RedactedTaskV1::validate)?;
        if self.tasks.windows(2).any(|pair| pair[0].task_id >= pair[1].task_id)
            || self.next_cursor.as_ref() != self.tasks.last().map(|task| &task.task_id)
                && self.next_cursor.is_some()
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunPageV1 {
    pub runs: Vec<RedactedRunV1>,
    pub next_cursor: Option<HarnessRunId>,
}

impl RunPageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_bounded(&self.runs, HARNESS_ENTITY_PAGE_LIMIT_MAX, RedactedRunV1::validate)?;
        if self.runs.windows(2).any(|pair| pair[0].run_id >= pair[1].run_id)
            || self.next_cursor.as_ref() != self.runs.last().map(|run| &run.run_id)
                && self.next_cursor.is_some()
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        Ok(())
    }
}

impl TimelineEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        if self.sequence == 0 || self.received_at_ms == 0 {
            return Err(HarnessReadApiError::InvalidTimelineEntry);
        }
        validate_optional_observation_text(
            "timeline label",
            self.label.as_deref(),
            HARNESS_OBSERVATION_PATH_MAX_BYTES,
        )?;
        validate_observation_correlation(self.correlation)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineStateV1 {
    Started,
    Active,
    Waiting,
    Required,
    Updated,
    Changed,
    Completed,
    Failed,
    Dismissed,
    Interrupted,
    Stale,
    UnknownAfterGap,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineCategoryV1 {
    Lifecycle,
    Todo,
    Tool,
    Subagent,
    Interaction,
    Process,
    File,
    Usage,
    History,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedTaskV1 {
    pub task_id: HarnessTaskId,
    pub revision: HarnessRevision,
    pub title: String,
    pub body: String,
    pub creator: TaskCreatorCategoryV1,
    pub parent_task_id: Option<HarnessTaskId>,
    pub dependency_ids: Vec<HarnessTaskId>,
    pub state: HarnessTaskStateV1,
    pub run_ids: Vec<HarnessRunId>,
    pub references_redacted: bool,
    pub result_refs: Vec<HarnessResultRef>,
    pub artifact_refs: Vec<HarnessArtifactRef>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl RedactedTaskV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.revision.validate().map_err(HarnessReadApiError::Protocol)?;
        validate_text("title", &self.title, HARNESS_TITLE_MAX_BYTES, false)?;
        validate_text("body", &self.body, HARNESS_BODY_MAX_BYTES, true)?;
        if let Some(parent) = &self.parent_task_id {
            parent.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        validate_bounded_ids_with_max(&self.dependency_ids, HARNESS_DEPENDENCIES_MAX)?;
        validate_bounded_ids_with_max(&self.run_ids, HARNESS_LINKS_MAX)?;
        validate_bounded_ids_with_max(&self.result_refs, HARNESS_RESULTS_MAX)?;
        validate_bounded_ids_with_max(&self.artifact_refs, HARNESS_ARTIFACTS_MAX)?;
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskCreatorCategoryV1 { User, ParentRun }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactedWorktreeIntentV1 { Existing, Managed }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedRunIntentV1 {
    pub mode: HarnessExecutionModeV1,
    pub worktree: RedactedWorktreeIntentV1,
    pub has_delivery_bundle: bool,
    pub has_continuation: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedRunV1 {
    pub run_id: HarnessRunId,
    pub revision: HarnessRevision,
    pub parent_run_id: Option<HarnessRunId>,
    pub task_id: Option<HarnessTaskId>,
    pub operation_id: Option<HarnessOperationId>,
    pub intent: RedactedRunIntentV1,
    pub lifecycle: HarnessRunLifecycleV1,
    pub binding: RedactedBindingStateV1,
    pub result_disposition: Option<HarnessResultDispositionV1>,
    pub failure_category: Option<HarnessFailureCategoryV1>,
    pub context_pack: Option<HarnessResolvedContextPackReceiptV1>,
    pub git_facts: Option<HarnessRunGitFactsV1>,
    pub references_redacted: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl RedactedRunV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.revision.validate().map_err(HarnessReadApiError::Protocol)?;
        if let Some(parent) = &self.parent_run_id {
            parent.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(operation_id) = &self.operation_id {
            operation_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if matches!(self.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting)
            && self.binding == RedactedBindingStateV1::None
        {
            return Err(HarnessReadApiError::InvalidRunState);
        }
        if matches!(self.lifecycle, HarnessRunLifecycleV1::Failed) != self.failure_category.is_some() {
            return Err(HarnessReadApiError::InvalidRunState);
        }
        if let Some(pack) = &self.context_pack {
            pack.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(facts) = &self.git_facts {
            facts.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactedBindingStateV1 { None, ManagedDormant, ManagedActive, Inline }

/// D5, Slice D's operator-side per-task operations ledger entry
/// (`HarnessOperatorRequestV1::TaskOperations` /
/// `HarnessOperatorResponseV1::TaskOperations`), one row per
/// `HarnessEngine::operations_for_task` entry. Unlike `RedactedOperationV1`,
/// `actor` here is the RAW `HarnessActorV1` -- the operator is already
/// trusted with every other raw identity this wire exposes (session
/// records, node/workspace ids, ...), and this ledger is the audit trail
/// the plan promises ("the operator's ledger on T1 shows A's run as the
/// actor of both writes"). The agent side never gets this: `g4a_tasks_get`
/// keeps `RedactedTaskV1`'s existing category-only exposure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperationLedgerEntryV1 {
    pub operation_id: HarnessOperationId,
    pub created_at_unix_ms: u64,
    pub kind: HarnessOperationKindV1,
    pub actor: HarnessActorV1,
}

impl HarnessOperationLedgerEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.operation_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
        self.actor.validate().map_err(HarnessOperatorApiError::Protocol)?;
        if self.created_at_unix_ms == 0 {
            return Err(HarnessOperatorApiError::InvalidOperationLedger);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedOperationV1 {
    pub operation_id: HarnessOperationId,
    pub revision: HarnessRevision,
    pub kind: HarnessOperationKindV1,
    pub state: HarnessOperationStateV1,
    pub task_id: Option<HarnessTaskId>,
    pub run_id: Option<HarnessRunId>,
    pub reconciles_operation_id: Option<HarnessOperationId>,
    pub references_redacted: bool,
    pub failure_category: Option<HarnessFailureCategoryV1>,
    pub outcome_unknown_reason: Option<HarnessOutcomeUnknownReasonV1>,
    pub reconciliation_outcome: Option<HarnessReconciliationOutcomeV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub dispatched_at_unix_ms: Option<u64>,
    pub finished_at_unix_ms: Option<u64>,
}

impl RedactedOperationV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.operation_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.revision.validate().map_err(HarnessReadApiError::Protocol)?;
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(run_id) = &self.run_id {
            run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(operation_id) = &self.reconciles_operation_id {
            operation_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if (self.state == HarnessOperationStateV1::Failed) != self.failure_category.is_some()
            || (self.state == HarnessOperationStateV1::OutcomeUnknown)
                != self.outcome_unknown_reason.is_some()
            || (self.state == HarnessOperationStateV1::Reconciled)
                != self.reconciliation_outcome.is_some()
        {
            return Err(HarnessReadApiError::InvalidOperationState);
        }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;
        for timestamp in [self.dispatched_at_unix_ms, self.finished_at_unix_ms].into_iter().flatten() {
            if timestamp < self.created_at_unix_ms || timestamp > self.updated_at_unix_ms {
                return Err(HarnessReadApiError::InvalidTimestamp);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum HarnessReadApiError {
    #[error(
        "build stamp mismatch: this side was built from tree {expected}, the peer from \
         tree {received} -- rebuild and restart the out-of-date side (gate4agent-harness \
         and its gate4agent-harness-client-based peers are built from the same tree and \
         must be rolled together)"
    )]
    BuildStampMismatch { expected: String, received: String },
    #[error("harness read credential is malformed")]
    MalformedCredential,
    #[error("harness read limit is outside the supported range")]
    InvalidLimit,
    #[error("harness read cursor is invalid")]
    InvalidCursor,
    #[error("harness read context has invalid allowed tools")]
    InvalidAllowedTools,
    #[error("harness monitor counts are inconsistent")]
    InvalidMonitorCounts,
    #[error("harness monitor detail is inconsistent with visibility or availability")]
    InvalidMonitorDetail,
    #[error("harness timeline entry is invalid")]
    InvalidTimelineEntry,
    #[error("harness read collection is not canonical or bounded")]
    InvalidCollection,
    #[error("harness read timestamp is invalid")]
    InvalidTimestamp,
    #[error("harness redacted run state is invalid")]
    InvalidRunState,
    #[error("harness redacted operation state is invalid")]
    InvalidOperationState,
    #[error("harness context task or sibling runs are inconsistent with the actor run")]
    InvalidContextTask,
    #[error("harness read text field is invalid: {0}")]
    InvalidText(&'static str),
    #[error("harness mail message is invalid")]
    InvalidMailMessage,
    #[error("harness mail inbox page is invalid")]
    InvalidMailInbox,
    #[error("harness mail fetch result is invalid")]
    InvalidMailFetch,
    #[error("harness task create request is invalid")]
    InvalidTaskCreate,
    #[error("harness protocol value is invalid: {0}")]
    Protocol(#[from] gate4agent_harness_protocol::HarnessValidationError),
}

#[derive(Debug, Error)]
pub enum HarnessOperatorApiError {
    #[error(
        "build stamp mismatch: this side was built from tree {expected}, the peer from \
         tree {received} -- rebuild and restart the out-of-date side \
         (gate4agent-harness and its gate4agent-tui / gate4agent-tui-light client are built \
         from the same tree and must be rolled together)"
    )]
    BuildStampMismatch { expected: String, received: String },
    #[error("harness operator credential is malformed")]
    MalformedCredential,
    #[error("harness operator limit is outside the supported range")]
    InvalidLimit,
    #[error("harness operator cursor is invalid")]
    InvalidCursor,
    #[error("harness operator request reference is malformed")]
    MalformedRequestRef,
    #[error("harness operator submission timestamp is invalid")]
    InvalidSubmittedAt,
    #[error("harness runtime inventory is invalid")]
    InvalidRuntimeInventory,
    #[error("harness run correlation is invalid")]
    InvalidRunCorrelation,
    #[error("harness run transfer summary is invalid")]
    InvalidRunTransfer,
    #[error("harness reverse attribution is invalid")]
    InvalidReverseAttribution,
    #[error("harness run context source observation is invalid")]
    InvalidRunContextSourceObservation,
    #[error("harness run workspace origin is invalid")]
    InvalidWorkspaceOrigin,
    #[error("harness repository-relative path is invalid")]
    InvalidRepositoryPath,
    #[error("harness run workspace tree is invalid")]
    InvalidWorkspaceTree,
    #[error("harness run workspace file is invalid")]
    InvalidWorkspaceFile,
    #[error("harness git status is invalid")]
    InvalidGitStatus,
    #[error("harness git summary is invalid")]
    InvalidGitSummary,
    #[error("harness git object id is invalid")]
    InvalidGitObjectId,
    #[error("harness git history is invalid")]
    InvalidGitHistory,
    #[error("harness git diff is invalid")]
    InvalidGitDiff,
    #[error("harness launch plan catalog is invalid")]
    InvalidLaunchPlans,
    #[error("harness task launch options are invalid")]
    InvalidTaskLaunchOptions,
    #[error("harness reviewed task launch selection is invalid")]
    InvalidTaskLaunchSelection,
    #[error("harness native history value is invalid")]
    InvalidNativeHistory,
    #[error("harness terminal page is invalid")]
    InvalidTerminalPage,
    #[error("harness session spawn request is invalid")]
    InvalidSessionSpawn,
    #[error("harness session control request is invalid")]
    InvalidSessionControl,
    #[error("harness agent stream content is invalid")]
    InvalidAgentStream,
    #[error("harness session record request is invalid")]
    InvalidSessionRecordRequest,
    #[error("harness host path is invalid")]
    InvalidHostPath,
    #[error("harness host directory browse request is invalid")]
    InvalidHostDirectoryBrowseRequest,
    #[error("harness workspace or worktree resource mutation request is invalid")]
    InvalidResourceMutationRequest,
    #[error("harness task operations ledger entry is invalid")]
    InvalidOperationLedger,
    #[error("harness operator response is invalid")]
    Read(#[source] HarnessReadApiError),
    #[error("harness protocol value is invalid: {0}")]
    Protocol(#[source] gate4agent_harness_protocol::HarnessValidationError),
}

fn validate_credential(value: &str) -> Result<(), HarnessReadApiError> {
    if value.len() > HARNESS_READ_CREDENTIAL_MAX_BYTES || !value.starts_with(TOKEN_PREFIX) {
        return Err(HarnessReadApiError::MalformedCredential);
    }
    let (payload, proof) = value[TOKEN_PREFIX.len()..]
        .split_once('.')
        .ok_or(HarnessReadApiError::MalformedCredential)?;
    if payload.is_empty()
        || payload.len() % 2 != 0
        || proof.len() != 64
        || !payload.bytes().chain(proof.bytes()).all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HarnessReadApiError::MalformedCredential);
    }
    Ok(())
}

fn validate_operator_credential(value: &str) -> Result<(), HarnessOperatorApiError> {
    let payload = value.strip_prefix(OPERATOR_TOKEN_PREFIX)
        .ok_or(HarnessOperatorApiError::MalformedCredential)?;
    if value.len() > HARNESS_OPERATOR_CREDENTIAL_MAX_BYTES
        || payload.len() != 64
        || !payload.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HarnessOperatorApiError::MalformedCredential);
    }
    Ok(())
}

fn validate_operator_request_ref(value: &str) -> Result<(), HarnessOperatorApiError> {
    let payload = value.strip_prefix(OPERATOR_REQUEST_REF_PREFIX)
        .ok_or(HarnessOperatorApiError::MalformedRequestRef)?;
    if payload.len() != 24
        || !payload.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HarnessOperatorApiError::MalformedRequestRef);
    }
    Ok(())
}

fn valid_runtime_id(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.is_ascii()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b'+')
        })
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:")
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(is_lower_hex))
}

fn validate_node_workspace_route(
    node_id: &str,
    workspace_id: &str,
) -> Result<(), HarnessOperatorApiError> {
    if !valid_runtime_id(node_id, 128) || !valid_runtime_id(workspace_id, 128) {
        return Err(HarnessOperatorApiError::InvalidWorkspaceOrigin);
    }
    Ok(())
}

fn validate_repository_path(value: &str) -> Result<(), HarnessOperatorApiError> {
    if value.is_empty()
        || value.len() > HARNESS_REPOSITORY_PATH_MAX_BYTES
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
        || value.split('/').any(|component| {
            component.is_empty() || matches!(component, "." | "..")
        })
    {
        return Err(HarnessOperatorApiError::InvalidRepositoryPath);
    }
    Ok(())
}

fn validate_git_object_id(value: &str) -> Result<(), HarnessOperatorApiError> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(is_lower_hex) {
        return Err(HarnessOperatorApiError::InvalidGitObjectId);
    }
    Ok(())
}

fn validate_git_single_line(
    value: &str,
    maximum: usize,
    required: bool,
) -> Result<(), HarnessOperatorApiError> {
    if value.len() > maximum
        || required && value.is_empty()
        || value.chars().any(char::is_control)
    {
        return Err(HarnessOperatorApiError::InvalidGitSummary);
    }
    Ok(())
}

fn validate_native_session_catalog_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn valid_native_selection_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 1_024
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn valid_native_group_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn valid_native_single_line(value: &str, maximum: usize, required: bool) -> bool {
    (!required || !value.trim().is_empty())
        && value.len() <= maximum
        && !value.chars().any(char::is_control)
}

/// Mirrors `gate4agent-node-protocol`'s own `contains_unsafe_control_bytes`:
/// every control character is unsafe except the three ACP content is allowed
/// to carry verbatim (`\n`, `\r`, `\t`), itself mirroring
/// `gate4agent_types::control::validate_text`'s own allowance, so
/// agent-stream free text (`Text`, `Thinking`, `title`, `prompt`,
/// `description`, `value_json`, `label`) and `SetSessionConfigOption`'s
/// `value_json` validate against the same rule the provider event stream
/// already applies to it.
fn contains_unsafe_control_bytes(value: &str) -> bool {
    value.chars().any(|character| {
        character.is_control() && !matches!(character, '\n' | '\r' | '\t')
    })
}

/// Structural bound for an ACP-minted identifier (`mode_id`/`option_id`/
/// `model_id`/`tool_name`/catalog entry `id`/`name`/interaction option
/// `option_id`/`name`/`kind`) -- mirrors `gate4agent-node-protocol`'s own
/// `deserialize_acp_control_id`: non-empty, bounded by
/// `HARNESS_AGENT_STREAM_ID_MAX_BYTES`, free of every control character
/// (never just the unsafe subset `contains_unsafe_control_bytes` allows --
/// an id is not free text).
fn valid_agent_stream_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= HARNESS_AGENT_STREAM_ID_MAX_BYTES
        && !value.chars().any(char::is_control)
}

/// Structural bound for `ResolveInteraction`'s `correlation_id` and
/// `InteractionPrompt`'s own copy of it -- mirrors `gate4agent-node-
/// protocol`'s own `deserialize_acp_correlation_id`, bounded by
/// `HARNESS_OBSERVATION_LABEL_MAX_BYTES` (the same source constant,
/// `gate4agent_observation_protocol::OBSERVATION_LABEL_MAX_BYTES`, that
/// bound already mirrors) since a correlation id is exactly what
/// `ObservationKindV1::ApprovalRequested`/`QuestionRequested` minted onto
/// the timeline at that bound.
fn valid_agent_stream_correlation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= HARNESS_OBSERVATION_LABEL_MAX_BYTES
        && !value.chars().any(char::is_control)
}

fn validate_native_entries(
    route: &HarnessNativeSessionRouteV1,
    entries: &[HarnessNativeSessionCatalogEntryV1],
) -> Result<(), HarnessOperatorApiError> {
    if entries.len() > usize::from(HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX) {
        return Err(HarnessOperatorApiError::InvalidNativeHistory);
    }
    for (index, entry) in entries.iter().enumerate() {
        entry.validate_for_route(route)?;
        if entries[..index].iter().any(|existing| {
            existing.selection_id == entry.selection_id
                || entry.record_id.is_some() && existing.record_id == entry.record_id
        }) {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
    }
    Ok(())
}

fn validate_operator_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_ENTITY_PAGE_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_operator_timeline_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_TIMELINE_PAGE_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_operator_terminal_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_TERMINAL_PAGE_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_limit(limit: u16, maximum: u16) -> Result<(), HarnessReadApiError> {
    if !(1..=maximum).contains(&limit) {
        return Err(HarnessReadApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_bounded<T>(
    values: &[T],
    maximum: u16,
    validate: impl Fn(&T) -> Result<(), HarnessReadApiError>,
) -> Result<(), HarnessReadApiError> {
    if values.len() > maximum as usize {
        return Err(HarnessReadApiError::InvalidCollection);
    }
    for value in values { validate(value)?; }
    Ok(())
}

fn validate_bounded_ids_with_max<T: Ord>(
    values: &[T],
    maximum: usize,
) -> Result<(), HarnessReadApiError> {
    if values.len() > maximum
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(HarnessReadApiError::InvalidCollection);
    }
    Ok(())
}

fn validate_monitor_facts<T>(values: &[T]) -> Result<(), HarnessReadApiError> {
    if values.len() > HARNESS_MONITOR_FACTS_MAX {
        return Err(HarnessReadApiError::InvalidCollection);
    }
    Ok(())
}

fn validate_optional_observation_text(
    field: &'static str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), HarnessReadApiError> {
    let Some(value) = value else { return Ok(()); };
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(HarnessReadApiError::InvalidText(field));
    }
    Ok(())
}

fn validate_observation_correlation(value: Option<u16>) -> Result<(), HarnessReadApiError> {
    if value.is_some_and(|value| value == 0 || usize::from(value) > HARNESS_MONITOR_FACTS_MAX) {
        return Err(HarnessReadApiError::InvalidTimelineEntry);
    }
    Ok(())
}

fn validate_observation_relative_path(path: &str) -> Result<(), HarnessReadApiError> {
    if path.is_empty()
        || path.len() > HARNESS_OBSERVATION_PATH_MAX_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path.chars().any(char::is_control)
        || path.split('/').any(|component| {
            component.is_empty() || matches!(component, "." | "..")
        })
    {
        return Err(HarnessReadApiError::InvalidText("relative path"));
    }
    Ok(())
}

fn require_observed_or_empty<T>(
    state: FeatureObservationStateV1,
    values: &[T],
) -> Result<(), HarnessReadApiError> {
    if state != FeatureObservationStateV1::Observed && !values.is_empty() {
        return Err(HarnessReadApiError::InvalidMonitorDetail);
    }
    Ok(())
}

fn validate_feature_counts(monitor: &SessionMonitorV1) -> Result<(), HarnessReadApiError> {
    if monitor.features.todo != FeatureObservationStateV1::Observed
        && (monitor.todo_total != 0 || monitor.todo_completed != 0)
        || monitor.features.tools != FeatureObservationStateV1::Observed
            && monitor.active_tools != 0
        || monitor.features.subagents != FeatureObservationStateV1::Observed
            && monitor.active_subagents != 0
        || monitor.features.interactions != FeatureObservationStateV1::Observed
            && monitor.active_interactions != 0
        || monitor.features.owned_processes != FeatureObservationStateV1::Observed
            && monitor.active_processes != 0
        || monitor.features.usage != FeatureObservationStateV1::Observed
            && (monitor.input_tokens != 0
                || monitor.output_tokens != 0
                || monitor.cache_read_tokens != 0
                || monitor.cache_write_tokens != 0
                || monitor.reasoning_tokens != 0
                || monitor.context_window_tokens.is_some())
    {
        return Err(HarnessReadApiError::InvalidMonitorDetail);
    }
    if matches!(monitor.availability, ProjectionAvailabilityV1::Unknown | ProjectionAvailabilityV1::NotObserved)
        && [
            monitor.features.todo,
            monitor.features.tools,
            monitor.features.subagents,
            monitor.features.interactions,
            monitor.features.owned_processes,
            monitor.features.files,
            monitor.features.usage,
            monitor.features.history,
        ]
        .contains(&FeatureObservationStateV1::Observed)
    {
        return Err(HarnessReadApiError::InvalidMonitorDetail);
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), HarnessReadApiError> {
    if value.len() > maximum || !allow_empty && value.trim().is_empty() || value.contains('\0') {
        return Err(HarnessReadApiError::InvalidText(field));
    }
    Ok(())
}

fn validate_timestamps(created: u64, updated: u64) -> Result<(), HarnessReadApiError> {
    if created == 0 || updated < created {
        return Err(HarnessReadApiError::InvalidTimestamp);
    }
    Ok(())
}

fn valid_transfer_timestamps<const N: usize>(
    created: u64,
    updated: u64,
    optional: [Option<u64>; N],
) -> bool {
    created != 0
        && updated >= created
        && optional.into_iter().flatten().all(|timestamp| {
            timestamp >= created && timestamp <= updated
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_run_correlation() -> HarnessRunCorrelationV1 {
        HarnessRunCorrelationV1 {
            run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(7).unwrap(),
            task_id: HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            worktree: HarnessRunWorktreeViewV1::Managed {
                worktree_ref: HarnessSelectorV1::new("worktree-a").unwrap(),
            },
            session: HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 41,
                    generation: 3,
                }),
            }),
            availability: HarnessRunCorrelationAvailabilityV1::Available,
            observed_at_unix_ms: Some(100),
        }
    }

    fn run_transfer_summary() -> HarnessRunTransferSummaryV1 {
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        HarnessRunTransferSummaryV1 {
            run_id: run_id.clone(),
            run_revision: HarnessRevision::new(9).unwrap(),
            delivery: Some(HarnessRunDeliveryTransferV1 {
                delivery_ref: HarnessDeliveryRef::new(format!(
                    "hdelivery_{}",
                    "b".repeat(24),
                )).unwrap(),
                revision: HarnessRevision::new(3).unwrap(),
                state: HarnessDeliveryStateV1::Committed,
                selector: HarnessSelectorV1::new("delivery-safe").unwrap(),
                bundle_id: HarnessDeliveryBundleIdV1::new("bundle-safe").unwrap(),
                bundle_revision: HarnessDeliveryBundleRevisionV1::new("revision-4").unwrap(),
                bundle_digest: HarnessDeliveryBundleDigestV1::new(format!(
                    "sha256:{}",
                    "c".repeat(64),
                )).unwrap(),
                manifest_digest: HarnessDeliveryManifestDigestV2::new(format!(
                    "sha256:{}",
                    "d".repeat(64),
                )).unwrap(),
                receipt_ref: Some(HarnessReceiptRef::new(format!(
                    "hreceipt_{}",
                    "e".repeat(24),
                )).unwrap()),
                created_at_unix_ms: 10,
                updated_at_unix_ms: 30,
                staged_at_unix_ms: Some(20),
                committed_at_unix_ms: Some(30),
            }),
            continuation: Some(HarnessRunContinuationTransferV1 {
                continuation_ref: HarnessContinuationRef::new(format!(
                    "hcontinuation_{}",
                    "f".repeat(24),
                )).unwrap(),
                receipt_ref: HarnessReceiptRef::new(format!(
                    "hreceipt_{}",
                    "0".repeat(24),
                )).unwrap(),
                revision: HarnessRevision::new(4).unwrap(),
                state: HarnessContinuationStateV1::Bound,
                source_run_id: HarnessRunId::new(format!(
                    "hrun_{}",
                    "1".repeat(24),
                )).unwrap(),
                target_run_id: run_id,
                source_provider: HarnessSelectorV1::new("claude").unwrap(),
                context: Some(HarnessRunContextTransferV1 {
                    context_ref: HarnessSelectorV1::new("context-safe").unwrap(),
                    digest: format!("sha256:{}", "2".repeat(64)),
                    source_message_count: 12,
                    retained_message_count: 8,
                    byte_len: 4096,
                    truncated: true,
                }),
                prepared_at_unix_ms: 10,
                exporting_at_unix_ms: Some(15),
                exported_at_unix_ms: Some(20),
                bound_at_unix_ms: Some(25),
                expired_at_unix_ms: None,
                outcome_unknown_at_unix_ms: None,
                outcome_unknown_reason: None,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 25,
            }),
        }
    }

    fn run_workspace_origin() -> HarnessRunWorkspaceOriginV1 {
        HarnessRunWorkspaceOriginV1 {
            run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(7).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
        }
    }

    fn node_workspace_origin() -> HarnessNodeWorkspaceOriginV1 {
        HarnessNodeWorkspaceOriginV1 {
            node_id: "node-a".to_owned(),
            node_incarnation_id: "07".repeat(16),
            workspace_id: "workspace-a".to_owned(),
        }
    }

    fn reverse_attribution_workspace() -> HarnessReverseAttributionWorkspaceV1 {
        HarnessReverseAttributionWorkspaceV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
        }
    }

    fn reverse_attribution_link(run_byte: char) -> HarnessReverseAttributionLinkV1 {
        HarnessReverseAttributionLinkV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap(),
            run_id: HarnessRunId::new(format!("hrun_{}", run_byte.to_string().repeat(24)))
                .unwrap(),
            run_revision: HarnessRevision::new(3).unwrap(),
            binding: HarnessReverseAttributionBindingV1::Workspace {
                workspace: reverse_attribution_workspace(),
            },
            relation: HarnessReverseAttributionRelationV1::WorkspaceScope,
        }
    }

    fn git_commit(id_byte: char) -> HarnessGitCommitV1 {
        HarnessGitCommitV1 {
            id: HarnessGitObjectIdV1::new(id_byte.to_string().repeat(40)).unwrap(),
            parents: Vec::new(),
            subject: "Bounded subject".to_owned(),
            author_name: "Author".to_owned(),
            authored_at: "2026-08-16T10:00:00Z".to_owned(),
            committer_name: "Committer".to_owned(),
            committed_at: "2026-08-16T10:00:00Z".to_owned(),
            signature_status: HarnessGitSignatureStatusV1::NoSignature,
            signer: None,
        }
    }

    fn assert_json_has_no_forbidden_keys(encoded: &str, forbidden: &[&str]) {
        fn visit(value: &serde_json::Value, forbidden: &[&str]) {
            match value {
                serde_json::Value::Object(fields) => {
                    for (field, value) in fields {
                        assert!(
                            !forbidden.iter().any(|forbidden| field == forbidden),
                            "response exposed structural field {field}",
                        );
                        visit(value, forbidden);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values { visit(value, forbidden); }
                }
                _ => {}
            }
        }

        let value: serde_json::Value = serde_json::from_str(encoded).unwrap();
        visit(&value, forbidden);
    }

    #[test]
    fn credential_debug_and_errors_never_expose_secret() {
        let credential = HarnessReadCredential::parse(format!("g4ah2_aa.{}", "0".repeat(64)))
            .expect("credential");
        assert_eq!(format!("{credential:?}"), "HarnessReadCredential([REDACTED])");
        assert!(!HarnessReadApiError::MalformedCredential.to_string().contains(credential.expose()));
    }

    #[test]
    fn operator_and_agent_credentials_are_strictly_separated_and_redacted() {
        let operator = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).expect("operator credential");
        let read = format!("g4ah2_aa.{}", "0".repeat(64));
        assert!(HarnessOperatorCredential::parse(&read).is_err());
        assert!(HarnessReadCredential::parse(operator.expose()).is_err());
        assert_eq!(
            format!("{operator:?}"),
            "HarnessOperatorCredential([REDACTED])",
        );
        assert!(!HarnessOperatorApiError::MalformedCredential
            .to_string().contains(operator.expose()));
    }

    #[test]
    fn operator_frames_enforce_exact_cursor_and_collection_bounds() {
        assert!(HarnessOperatorRequestV1::TasksList {
            after_task_id: None,
            state: None,
            parent_task_id: None,
            limit: HARNESS_ENTITY_PAGE_LIMIT_MAX,
        }.validate().is_ok());
        assert!(HarnessOperatorRequestV1::TasksList {
            after_task_id: None,
            state: None,
            parent_task_id: None,
            limit: HARNESS_ENTITY_PAGE_LIMIT_MAX + 1,
        }.validate().is_err());
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        assert!(HarnessOperatorRequestV1::TimelineRead {
            run_id,
            after_sequence: Some(0),
            limit: 1,
        }.validate().is_err());
    }

    #[test]
    fn legacy_raw_schedule_variant_json_is_rejected() {
        let legacy = r#"{"kind":"schedule-ready-task","request":{}}"#;
        assert!(serde_json::from_str::<HarnessOperatorRequestV1>(legacy).is_err());
    }

    #[test]
    fn operator_intent_is_authority_free() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::SubmitIntent {
            intent: HarnessOperatorIntentV1 {
                request_ref: HarnessOperatorRequestRefV1::new(format!(
                    "hireq_{}",
                    "1".repeat(24),
                )).unwrap(),
                submitted_at_unix_ms: 10,
                action: HarnessOperatorActionV1::CreateTask {
                    title: "Harness-owned identity".to_owned(),
                    body: "Typed user intent".to_owned(),
                    parent_task_id: None,
                    dependencies: Vec::new(),
                    initial_state: HarnessTaskStateV1::Backlog,
                },
            },
        };
        let encoded_request = serde_json::to_string(&request).unwrap();
        assert!(!encoded_request.contains("authority"));
        assert!(!encoded_request.contains("operation_id"));
        assert!(!encoded_request.contains("idempotency_ref"));
        assert!(!encoded_request.contains("\"task_id\":"));
        let envelope = HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        };
        assert!(envelope.validate().is_ok());
        let decoded: HarnessOperatorEnvelopeV1 = serde_json::from_slice(
            &serde_json::to_vec(&envelope).unwrap(),
        ).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn operator_run_correlation_is_exact() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::RunCorrelationGet {
            run_id: HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap(),
        };
        let envelope = HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        };
        envelope.validate().unwrap();
        let decoded: HarnessOperatorEnvelopeV1 = serde_json::from_slice(
            &serde_json::to_vec(&envelope).unwrap(),
        ).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn operator_run_transfer_is_exact_private() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let summary = run_transfer_summary();
        summary.validate().unwrap();
        let request = HarnessOperatorRequestV1::RunTransferGet {
            run_id: summary.run_id.clone(),
        };
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();

        let response = HarnessOperatorResponseV1::RunTransfer(summary.clone());
        response.validate().unwrap();
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, response);
        for forbidden in [
            "provider_home",
            "provider_native",
            "session_id",
            "source_root",
            "component_path",
            "component_name",
            "blob_bytes",
            "prompt",
            "node_store",
            "credential",
            "auth",
            "C:\\\\",
        ] {
            assert!(!encoded.contains(forbidden), "response exposed {forbidden}");
        }

        let mut wrong_target = summary.clone();
        wrong_target.continuation.as_mut().unwrap().target_run_id =
            HarnessRunId::new(format!("hrun_{}", "9".repeat(24))).unwrap();
        assert!(matches!(
            wrong_target.validate(),
            Err(HarnessOperatorApiError::InvalidRunTransfer),
        ));

        let mut invalid_delivery = summary.clone();
        invalid_delivery.delivery.as_mut().unwrap().committed_at_unix_ms = None;
        assert!(matches!(
            invalid_delivery.validate(),
            Err(HarnessOperatorApiError::InvalidRunTransfer),
        ));

        let mut invalid_context = summary;
        invalid_context.continuation.as_mut().unwrap()
            .context.as_mut().unwrap().truncated = false;
        assert!(matches!(
            invalid_context.validate(),
            Err(HarnessOperatorApiError::InvalidRunTransfer),
        ));
    }

    fn ordinary_launch_plan(plan_id: &str, digest: char) -> HarnessLaunchPlanSummaryV1 {
        HarnessLaunchPlanSummaryV1 {
            scheduled_launch: HarnessScheduledLaunchRefV2 {
                plan: HarnessLaunchPlanRefV1 {
                    plan_id: HarnessSelectorV1::new(plan_id).unwrap(),
                    revision: HarnessRevision::new(2).unwrap(),
                    digest: HarnessRequestDigest::new(digest.to_string().repeat(64)).unwrap(),
                },
                authority: HarnessLaunchAuthorityRefV1::OrdinaryOperator,
            },
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            worktree: HarnessWorktreeIntentV1::Managed {
                worktree_ref: HarnessSelectorV1::new("worktree-a").unwrap(),
            },
            provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
            provider_id: HarnessSelectorV1::new("codex").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
        }
    }

    fn task_launch_options() -> HarnessTaskLaunchOptionsV1 {
        let task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let legacy_plan = ordinary_launch_plan("plan-a", 'a');
        let plan = HarnessOrdinaryLaunchPlanOptionV1 {
            plan: legacy_plan.scheduled_launch.plan,
            node_id: legacy_plan.node_id,
            source_workspace_id: legacy_plan.workspace_id,
            provider_profile: legacy_plan.provider_profile,
            provider_id: legacy_plan.provider_id,
            mode: legacy_plan.mode,
        };
        let context = HarnessContextSourceSelectionV1 {
            source_run_id: HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap(),
            source_run_revision: HarnessRevision::new(7).unwrap(),
            observed_at_unix_ms: 90,
            metadata_digest: HarnessRequestDigest::new("d".repeat(64)).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("source-workspace").unwrap(),
            session_record_id: HarnessSelectorV1::new("record-a").unwrap(),
            active_session: Some(HarnessRuntimeIdentityV1 { instance_id: 41, generation: 3 }),
            message_count: 12,
            message_count_exact: true,
            completed_turn_count: Some(5),
            total_tokens: Some(4096),
            availability: HarnessContextSourceAvailabilityV1::Live,
            context_pack: None,
        };
        let delivery = HarnessDeliveryBundleSelectionV1 {
            bundle: HarnessDeliveryBundleV1 {
                selector: HarnessSelectorV1::new("review-kit").unwrap(),
                bundle_id: HarnessDeliveryBundleIdV1::new("bundle.review-kit").unwrap(),
                revision: HarnessDeliveryBundleRevisionV1::new("revision-7").unwrap(),
                digest: HarnessDeliveryBundleDigestV1::new(format!(
                    "sha256:{}",
                    "e".repeat(64),
                )).unwrap(),
                manifest_digest: HarnessDeliveryManifestDigestV2::new(format!(
                    "sha256:{}",
                    "f".repeat(64),
                )).unwrap(),
            },
            component_counts: vec![HarnessDeliveryComponentCountV1 {
                kind: HarnessDeliveryComponentKindV1::Skill,
                workspace_count: 2,
                session_count: 1,
            }],
        };
        let issuance = HarnessTaskLaunchIssuanceRefV1 {
            issuance_id: HarnessTaskLaunchIssuanceId::new(format!(
                "hissue_{}",
                "1".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(2).unwrap(),
            digest: HarnessRequestDigest::new("2".repeat(64)).unwrap(),
        };
        HarnessTaskLaunchOptionsV1 {
            task_id: task_id.clone(),
            task_revision: HarnessRevision::new(5).unwrap(),
            policy_digest: HarnessRequestDigest::new("3".repeat(64)).unwrap(),
            plans: vec![plan],
            managed_worktree_profiles: vec![HarnessManagedWorktreeProfileOptionV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                profile_id: HarnessSelectorV1::new("review").unwrap(),
                profile_revision: HarnessSelectorV1::new("review.r7").unwrap(),
                retention: HarnessManagedWorktreeRetentionV1::RemoveWhenReleased,
                observed_at_unix_ms: 90,
            }],
            context_sources: vec![context],
            delivery_bundles: vec![delivery],
            current_issued_spec: Some(HarnessIssuedExecutionSpecSummaryV1 {
                task_id,
                execution_spec_id: HarnessExecutionSpecId::new(format!(
                    "hespec_{}",
                    "4".repeat(24),
                )).unwrap(),
                revision: HarnessRevision::new(2).unwrap(),
                launch_issuance: issuance,
                review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
                created_at_unix_ms: 80,
                updated_at_unix_ms: 90,
            }),
            truncated: false,
            next_after: None,
            context_source_exclusions: vec![ContextSourceExclusionEntryV1 {
                run_id: HarnessRunId::new(format!("hrun_{}", "9".repeat(24))).unwrap(),
                exclusion: ContextSourceExclusionV1::NodeIncarnationUnknown {
                    node_id: HarnessSelectorV1::new("node-a").unwrap(),
                    node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
                    known_incarnations: vec![HarnessSelectorV1::new("08".repeat(16)).unwrap()],
                },
            }],
        }
    }

    #[test]
    fn operator_task_launch_contract_is_exact_and_private() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let options = task_launch_options();
        options.validate().unwrap();
        let task_id = options.task_id.clone();
        let authority = HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "5".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "6".repeat(24),
            )).unwrap(),
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
            now_unix_ms: 100,
        };
        let selection = HarnessReviewedTaskLaunchSelectionV1 {
            plan: options.plans[0].clone(),
            worktree: HarnessReviewedWorktreeSelectionV1::Managed {
                profile: options.managed_worktree_profiles[0].clone(),
            },
            context_source: Some(options.context_sources[0].clone()),
            delivery: Some(options.delivery_bundles[0].clone()),
            review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
        };
        let replace = HarnessOperatorRequestV1::ReplaceTaskExecutionSpecV2 {
            request: HarnessReplaceTaskExecutionSpecRequestV2 {
                authority: authority.clone(),
                task_id: task_id.clone(),
                expected_task_revision: options.task_revision,
                expected_execution_spec_revision:
                    HarnessExpectedExecutionSpecRevisionV1::Exact(HarnessRevision::new(2).unwrap()),
                selection: selection.clone(),
            },
        };
        let issuance = options.current_issued_spec.as_ref().unwrap().launch_issuance.clone();
        let start = HarnessOperatorRequestV1::StartTaskV2 {
            request: HarnessStartTaskRequestV2 {
                authority: authority.clone(),
                task_id: task_id.clone(),
                expected_task_revision: options.task_revision,
                expected_execution_spec_revision: HarnessRevision::new(2).unwrap(),
                expected_launch_issuance: issuance,
            },
        };
        for request in [
            HarnessOperatorRequestV1::TaskLaunchOptionsGet {
                task_id: task_id.clone(),
                provider: None,
                workspace: None,
                plan_id: None,
                after: None,
            },
            replace.clone(),
            start.clone(),
        ] {
            request.validate().unwrap();
        }
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request: replace.clone(),
        }.validate().unwrap();

        let replace_json = serde_json::to_string(&replace).unwrap();
        for forbidden in [
            "issuance_id",
            "policy_digest",
            "provider_home",
            "credential",
            "canonical_root",
            "display_root",
            "prompt",
        ] {
            assert!(!replace_json.contains(forbidden), "leaked {forbidden}");
        }
        let mut unknown = serde_json::to_value(start).unwrap();
        unknown["request"]["caller_issuance_digest"] = serde_json::json!("untrusted");
        assert!(serde_json::from_value::<HarnessOperatorRequestV1>(unknown).is_err());

        let intent = HarnessOperatorIntentV1 {
            request_ref: HarnessOperatorRequestRefV1::new(format!(
                "hireq_{}",
                "7".repeat(24),
            )).unwrap(),
            submitted_at_unix_ms: 100,
            action: HarnessOperatorActionV1::ReplaceTaskExecutionSpecV2 {
                task_id,
                expected_task_revision: options.task_revision,
                expected_execution_spec_revision:
                    HarnessExpectedExecutionSpecRevisionV1::Exact(HarnessRevision::new(2).unwrap()),
                selection,
            },
        };
        let intent_json = serde_json::to_string(&intent).unwrap();
        assert!(!intent_json.contains("authority"));
        let start_intent = HarnessOperatorIntentV1 {
            request_ref: HarnessOperatorRequestRefV1::new(format!(
                "hireq_{}",
                "8".repeat(24),
            )).unwrap(),
            submitted_at_unix_ms: 101,
            action: HarnessOperatorActionV1::StartTaskV2 {
                task_id: options.task_id.clone(),
                expected_task_revision: options.task_revision,
                expected_execution_spec_revision: HarnessRevision::new(2).unwrap(),
                expected_launch_issuance: options.current_issued_spec.as_ref().unwrap()
                    .launch_issuance.clone(),
            },
        };
        let start_intent_json = serde_json::to_string(&start_intent).unwrap();
        assert!(!start_intent_json.contains("authority"));
    }

    #[test]
    fn task_launch_options_are_bounded_canonical_correlated_and_redacted() {
        let options = task_launch_options();
        options.validate_for(&options.task_id).unwrap();
        let encoded = serde_json::to_string(&HarnessOperatorResponseV1::TaskLaunchOptions(
            options.clone(),
        )).unwrap();
        for forbidden in [
            "canonical_root",
            "display_root",
            "provider_session_id",
            "provider_home",
            "credential",
            "auth_token",
            "source_path",
            "payload",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }

        let mut duplicate = options.clone();
        duplicate.plans.push(duplicate.plans[0].clone());
        assert!(matches!(
            duplicate.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ));
        let mut oversized = options.clone();
        oversized.context_sources = vec![
            options.context_sources[0].clone();
            HARNESS_TASK_LAUNCH_OPTIONS_MAX + 1
        ];
        assert!(matches!(
            oversized.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ));
        let other_task = HarnessTaskId::new(format!("htask_{}", "8".repeat(24))).unwrap();
        assert!(matches!(
            options.validate_for(&other_task),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ));
    }

    /// The launch-catalogue ceiling fix's read-side pagination: `provider`/
    /// `workspace`/`plan_id`/`after` on the request and `next_after` on the
    /// response are additive over the pre-pagination wire shape, so an older
    /// peer's JSON (never carrying any of them) must still decode.
    #[test]
    fn task_launch_options_paging_fields_are_additive_and_next_after_is_checked() {
        let task_id = HarnessTaskId::new(format!("htask_{}", "9".repeat(24))).unwrap();
        let legacy_request_json = serde_json::json!({
            "kind": "task-launch-options-get",
            "task_id": task_id.as_str(),
        });
        let decoded: HarnessOperatorRequestV1 =
            serde_json::from_value(legacy_request_json).unwrap();
        assert!(matches!(
            decoded,
            HarnessOperatorRequestV1::TaskLaunchOptionsGet {
                provider: None, workspace: None, plan_id: None, after: None, ..
            },
        ));

        let mut options = task_launch_options();
        let legacy_response_json = serde_json::to_value(&options).unwrap();
        assert!(legacy_response_json.get("next_after").is_some());
        let mut without_next_after = legacy_response_json.clone();
        without_next_after.as_object_mut().unwrap().remove("next_after");
        let redecoded: HarnessTaskLaunchOptionsV1 =
            serde_json::from_value(without_next_after).unwrap();
        assert_eq!(redecoded.next_after, None);
        redecoded.validate().unwrap();

        let plan_id = options.plans[0].plan.plan_id.clone();
        options.next_after = Some(plan_id.clone());
        assert!(matches!(
            options.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ), "next_after without truncated must be refused");
        options.truncated = true;
        options.validate().unwrap();
        options.next_after = Some(HarnessSelectorV1::new("not-the-last-plan").unwrap());
        assert!(matches!(
            options.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ), "next_after must name the last plan on the page");
    }

    /// `context_source_exclusions` is additive over the pre-instrumentation
    /// wire shape (same discipline as `next_after` above), round-trips
    /// exactly, and is bounded/ordered independently of the other four
    /// lists.
    #[test]
    fn context_source_exclusions_are_additive_bounded_and_ordered() {
        let options = task_launch_options();
        options.validate_for(&options.task_id).unwrap();
        assert_eq!(options.context_source_exclusions.len(), 1);

        let mut legacy_json = serde_json::to_value(&options).unwrap();
        legacy_json.as_object_mut().unwrap().remove("context_source_exclusions");
        let redecoded: HarnessTaskLaunchOptionsV1 = serde_json::from_value(legacy_json).unwrap();
        assert!(redecoded.context_source_exclusions.is_empty());
        redecoded.validate().unwrap();

        let entry = options.context_source_exclusions[0].clone();
        let encoded = serde_json::to_string(&entry).unwrap();
        assert!(encoded.contains("\"reason\":\"node-incarnation-unknown\""));
        let redecoded_entry: ContextSourceExclusionEntryV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(redecoded_entry, entry);

        let mut oversized = options.clone();
        oversized.context_source_exclusions =
            vec![entry.clone(); HARNESS_CONTEXT_SOURCE_EXCLUSIONS_MAX + 1];
        assert!(matches!(
            oversized.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ));

        let mut duplicated = options.clone();
        duplicated.context_source_exclusions.push(entry);
        assert!(matches!(
            duplicated.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ), "context_source_exclusions must stay sorted and duplicate-free by run_id");
    }

    /// Every [`ContextSourceExclusionV1`] variant round-trips through JSON
    /// exactly and validates -- covers every field-carrying shape the
    /// enum's `#[serde(tag = "reason")]` produces, not just the one sample
    /// `task_launch_options()` carries above.
    #[test]
    fn context_source_exclusion_variants_round_trip_every_shape() {
        let samples = [
            ContextSourceExclusionV1::LifecycleNotLive {
                lifecycle: HarnessRunLifecycleV1::Cancelled,
            },
            ContextSourceExclusionV1::NoBinding,
            ContextSourceExclusionV1::NoActiveManagedSession,
            ContextSourceExclusionV1::NodeIncarnationUnknown {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
                known_incarnations: vec![HarnessSelectorV1::new("08".repeat(16)).unwrap()],
            },
            ContextSourceExclusionV1::ManagedSessionRecordMismatch {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                instance_id: 7,
                generation: 3,
                node_has_records: 4,
            },
            ContextSourceExclusionV1::ManagedSessionsPageTruncated {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                page_len: 128,
                total: 185,
            },
            ContextSourceExclusionV1::MonitorUnavailable,
            ContextSourceExclusionV1::ProjectionNotLive {
                availability: ProjectionAvailabilityV1::Partial,
                freshness: ProjectionFreshnessV1::Stale,
                transport_incomplete: true,
            },
            ContextSourceExclusionV1::DurableNodeUnknown {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
            },
        ];
        for exclusion in samples {
            exclusion.validate().unwrap();
            let encoded = serde_json::to_string(&exclusion).unwrap();
            let decoded: ContextSourceExclusionV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, exclusion);
        }

        let oversized = ContextSourceExclusionV1::NodeIncarnationUnknown {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
            known_incarnations: vec![
                HarnessSelectorV1::new("08".repeat(16)).unwrap();
                CONTEXT_SOURCE_EXCLUSION_KNOWN_INCARNATIONS_MAX + 1
            ],
        };
        assert!(matches!(
            oversized.validate(),
            Err(HarnessOperatorApiError::InvalidTaskLaunchOptions),
        ));
    }

    #[test]
    fn operator_execution_requests_are_exact() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let authority = HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "4".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "5".repeat(24),
            )).unwrap(),
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
            now_unix_ms: 40,
        };
        let task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let requests = [
            HarnessOperatorRequestV1::LaunchPlansList {
                after_plan_id: Some(HarnessSelectorV1::new("plan-a").unwrap()),
                limit: HARNESS_LAUNCH_PLAN_PAGE_LIMIT_MAX,
            },
            HarnessOperatorRequestV1::TaskExecutionSpecGet {
                task_id: task_id.clone(),
            },
            HarnessOperatorRequestV1::ReplaceTaskExecutionSpec {
                request: HarnessReplaceTaskExecutionSpecRequestV1 {
                    authority: authority.clone(),
                    task_id: task_id.clone(),
                    expected_task_revision: HarnessRevision::new(3).unwrap(),
                    expected_execution_spec_revision:
                        HarnessExpectedExecutionSpecRevisionV1::Absent,
                    spec: HarnessTaskExecutionSpecInputV1 {
                        scheduled_launch: ordinary_launch_plan("plan-a", 'c').scheduled_launch,
                        review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
                    },
                },
            },
            HarnessOperatorRequestV1::StartTask {
                request: HarnessStartTaskRequestV1 {
                    authority,
                    task_id,
                    expected_task_revision: HarnessRevision::new(3).unwrap(),
                    expected_execution_spec_revision: HarnessRevision::new(1).unwrap(),
                    expected_scheduled_launch_digest: HarnessRequestDigest::new(
                        "d".repeat(64),
                    ).unwrap(),
                },
            },
        ];
        for request in requests {
            assert!(request.validate().is_ok());
            assert!(HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().is_ok());
        }

        assert!(matches!(
            HarnessOperatorRequestV1::LaunchPlansList {
                after_plan_id: None,
                limit: HARNESS_LAUNCH_PLAN_PAGE_LIMIT_MAX + 1,
            }.validate(),
            Err(HarnessOperatorApiError::InvalidLimit),
        ));
    }

    #[test]
    fn launch_plan_page_is_bounded_canonical_and_redacted() {
        let first = ordinary_launch_plan("plan-a", 'a');
        let second = ordinary_launch_plan("plan-b", 'b');
        let page = HarnessLaunchPlanPageV1 {
            plans: vec![first.clone(), second.clone()],
            next_plan_id: Some(second.scheduled_launch.plan.plan_id.clone()),
        };
        page.validate().unwrap();
        let encoded = serde_json::to_string(&page).unwrap();
        for sentinel in [
            "delivery",
            "continuation",
            "harness_mcp",
            "spawn_spec",
            "environment",
            "provider_home",
            "credential",
            "raw_path",
        ] {
            assert!(!encoded.contains(sentinel));
        }
        assert_eq!(serde_json::from_str::<HarnessLaunchPlanPageV1>(&encoded).unwrap(), page);

        assert!(matches!(
            HarnessLaunchPlanPageV1 {
                plans: vec![second, first],
                next_plan_id: None,
            }.validate(),
            Err(HarnessOperatorApiError::InvalidLaunchPlans),
        ));
    }

    #[test]
    fn execution_spec_and_start_intents_are_authority_free() {
        let intent = HarnessOperatorIntentV1 {
            request_ref: HarnessOperatorRequestRefV1::new(format!(
                "hireq_{}",
                "1".repeat(24),
            )).unwrap(),
            submitted_at_unix_ms: 50,
            action: HarnessOperatorActionV1::StartTask {
                task_id: HarnessTaskId::new(format!("htask_{}", "2".repeat(24))).unwrap(),
                expected_task_revision: HarnessRevision::new(7).unwrap(),
                expected_execution_spec_revision: HarnessRevision::new(3).unwrap(),
                expected_scheduled_launch_digest: HarnessRequestDigest::new(
                    "c".repeat(64),
                ).unwrap(),
            },
        };
        intent.validate().unwrap();
        let request = HarnessOperatorRequestV1::SubmitIntent { intent };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(!encoded.contains("authority"));
        assert!(!encoded.contains("operation_id"));
        assert!(!encoded.contains("idempotency_ref"));

        let replace = HarnessOperatorRequestV1::SubmitIntent {
            intent: HarnessOperatorIntentV1 {
                request_ref: HarnessOperatorRequestRefV1::new(format!(
                    "hireq_{}",
                    "3".repeat(24),
                )).unwrap(),
                submitted_at_unix_ms: 51,
                action: HarnessOperatorActionV1::ReplaceTaskExecutionSpec {
                    task_id: HarnessTaskId::new(format!(
                        "htask_{}",
                        "4".repeat(24),
                    )).unwrap(),
                    expected_task_revision: HarnessRevision::new(2).unwrap(),
                    expected_execution_spec_revision:
                        HarnessExpectedExecutionSpecRevisionV1::Absent,
                    spec: HarnessTaskExecutionSpecInputV1 {
                        scheduled_launch: ordinary_launch_plan("plan-a", 'd').scheduled_launch,
                        review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
                    },
                },
            },
        };
        replace.validate().unwrap();
    }

    #[test]
    fn run_correlation_round_trip_preserves_atomic_identity_and_redacts() {
        let correlation = managed_run_correlation();
        correlation.validate().unwrap();
        let response = HarnessOperatorResponseV1::RunCorrelation(correlation.clone());
        response.validate().unwrap();
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: HarnessOperatorResponseV1 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, response);
        let text = String::from_utf8(encoded.clone()).unwrap();
        for forbidden in [
            "g4aho_",
            "credential",
            "provider_identity",
            "session_id",
            "spawn_spec",
            "environment",
            "C:\\\\",
        ] {
            assert!(!text.contains(forbidden));
        }
        assert!(text.contains("provider_profile"));
        assert!(text.contains("node_incarnation_id"));
        assert!(text.contains("active_session"));

        let mut without_generation: serde_json::Value =
            serde_json::from_slice(&encoded).unwrap();
        without_generation["value"]["session"]["value"]["active_session"]
            .as_object_mut().unwrap().remove("generation");
        assert!(serde_json::from_value::<HarnessOperatorResponseV1>(
            without_generation,
        ).is_err());
    }

    #[test]
    fn run_correlation_rejects_inconsistent_availability_and_unbounded_fields() {
        let mut correlation = managed_run_correlation();
        correlation.availability = HarnessRunCorrelationAvailabilityV1::NotObserved;
        assert!(matches!(
            correlation.validate(),
            Err(HarnessOperatorApiError::InvalidRunCorrelation),
        ));
        correlation.observed_at_unix_ms = None;
        correlation.validate().unwrap();

        correlation.availability = HarnessRunCorrelationAvailabilityV1::Dormant;
        correlation.observed_at_unix_ms = Some(101);
        assert!(matches!(
            correlation.validate(),
            Err(HarnessOperatorApiError::InvalidRunCorrelation),
        ));

        let unknown = r#"{
            "run_id":"hrun_aaaaaaaaaaaaaaaaaaaaaaaa",
            "run_revision":1,
            "task_id":"htask_bbbbbbbbbbbbbbbbbbbbbbbb",
            "node_id":"node-a",
            "node_incarnation_id":"07070707070707070707070707070707",
            "workspace_id":"workspace-a",
            "provider_profile":"codex-default",
            "mode":"inline",
            "worktree":{"kind":"existing"},
            "session":{"kind":"inline","value":{"inline_ref":"hinline_cccccccccccccccccccccccc"}},
            "availability":"not-observed",
            "observed_at_unix_ms":null,
            "raw_path":"C:\\\\private"
        }"#;
        assert!(serde_json::from_str::<HarnessRunCorrelationV1>(unknown).is_err());
        assert!(HarnessNodeIncarnationV1::new("x".repeat(129)).is_err());
        assert!(HarnessNodeIncarnationV1::new("0".repeat(31)).is_err());
        assert!(HarnessNodeIncarnationV1::new("A".repeat(32)).is_err());
    }

    #[test]
    fn runtime_inventory_request_validates() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::RuntimeInventoryList {
            after_node_id: None,
            limit: HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX,
        };
        assert!(HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().is_ok());
    }

    /// Plan item 4 (`gate4agent-blocked-action-event-2026-09-02.md` §2): the
    /// fleet-wide `blocked_count`/`last_blocked_at_ms` instrument (added
    /// under the old hand-bumped wire-pin scheme; the envelope now checks
    /// `BUILD_STAMP` for exact equality instead, so this field's own arrival
    /// is no longer asserted by a version number here) round-trips through
    /// JSON intact, and its own invariant (a count of zero implies no
    /// timestamp, and vice versa) is enforced the same way every other
    /// paired count/timestamp field in this crate is.
    #[test]
    fn managed_session_blocked_stats_round_trip() {
        let mut session = sample_managed_session("record-a");
        session.blocked_count = 2;
        session.last_blocked_at_ms = Some(2_000);
        session.validate().expect("blocked_count with a timestamp is valid");
        let encoded = serde_json::to_string(&session).unwrap();
        let decoded: HarnessRuntimeManagedSessionV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, session);

        let mut zero_with_timestamp = sample_managed_session("record-a");
        zero_with_timestamp.last_blocked_at_ms = Some(1);
        assert!(zero_with_timestamp.validate().is_err());

        let mut counted_without_timestamp = sample_managed_session("record-a");
        counted_without_timestamp.blocked_count = 1;
        assert!(counted_without_timestamp.validate().is_err());
    }

    fn sample_runtime_inventory(
        managed_sessions: Vec<HarnessRuntimeManagedSessionV1>,
    ) -> HarnessRuntimeInventoryV1 {
        HarnessRuntimeInventoryV1 {
            enabled_providers: Vec::new(),
            workspaces: BTreeMap::new(),
            workspace_count: 0,
            workspaces_truncated: false,
            session_count: 0,
            sessions_truncated: false,
            managed_session_count: managed_sessions.len(),
            managed_sessions_truncated: false,
            managed_sessions,
            retired_count: 0,
            launch_inventory: None,
        }
    }

    /// Regression for `ed4fa61`: the producer (`SlimNodeInventory::
    /// from_snapshot`/`from_c2_snapshot`, c2-protocol) orders a managed-
    /// session page by `managed_session_liveness_rank` (Live, IdentityPending,
    /// Dormant, Unavailable) first and `record_id` only within a rank, so a
    /// real page's `record_id`s are routinely NOT ascending overall. Before
    /// this fix `validate` demanded flat ascending `record_id` order and
    /// rejected every such page outright.
    #[test]
    fn managed_session_ordering_is_liveness_rank_then_record_id() {
        let mut live = sample_managed_session("zzz-live");
        live.state = HarnessRuntimeManagedStateV1::Live;
        let mut pending = sample_managed_session("aaa-pending");
        pending.state = HarnessRuntimeManagedStateV1::IdentityPending;
        let mut unavailable = sample_managed_session("mmm-unavailable");
        unavailable.state = HarnessRuntimeManagedStateV1::Unavailable;

        let ordered = sample_runtime_inventory(vec![
            live.clone(),
            pending,
            unavailable,
        ]);
        ordered.validate().expect(
            "liveness-rank-then-record_id order validates even though record_id is not ascending overall",
        );

        let duplicate = sample_runtime_inventory(vec![live.clone(), live.clone()]);
        assert!(duplicate.validate().is_err());

        let mut dormant = sample_managed_session("aaa-dormant");
        dormant.state = HarnessRuntimeManagedStateV1::Dormant;
        let rank_out_of_order = sample_runtime_inventory(vec![dormant, live]);
        assert!(rank_out_of_order.validate().is_err());
    }

    /// Slice R (`gate4agent-node`'s session-record retention sweep):
    /// `retired_count` is additive, defaults to `0` for a pre-existing
    /// payload that never had it, and round-trips intact once a producer
    /// sets a real, nonzero value.
    #[test]
    fn runtime_inventory_retired_count_round_trips_and_defaults_to_zero() {
        let mut inventory = sample_runtime_inventory(Vec::new());
        assert_eq!(inventory.retired_count, 0);
        inventory.validate().expect("a fresh sample inventory is valid");

        inventory.retired_count = 5;
        let encoded = serde_json::to_string(&inventory).unwrap();
        assert!(encoded.contains("\"retired_count\":5"));
        let decoded: HarnessRuntimeInventoryV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, inventory);
        decoded.validate().expect("a nonzero retired_count is still a valid inventory");
    }

    #[test]
    fn runtime_inventory_launch_inventory_round_trips_and_fails_closed() {
        let node = HarnessRuntimeNodeInventoryV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "07".repeat(16),
            observed_at_unix_ms: 1_000,
            event_sequence: 4,
            inventory: HarnessRuntimeInventoryV1 {
                enabled_providers: vec!["claude".to_owned()],
                workspaces: BTreeMap::new(),
                workspace_count: 0,
                workspaces_truncated: false,
                session_count: 0,
                sessions_truncated: false,
                managed_sessions: Vec::new(),
                managed_session_count: 0,
                managed_sessions_truncated: false,
                retired_count: 0,
                launch_inventory: Some(HarnessRuntimeLaunchInventoryV1 {
                    spawn_profiles: Some(vec![HarnessRuntimeSpawnProfileSummaryV1 {
                        id: "default".to_owned(),
                        revision: "rev-1".to_owned(),
                        environment_profile: Some(HarnessRuntimeEnvironmentProfileReceiptV1 {
                            profile_id: "env-a".to_owned(),
                            profile_revision: "env-rev-1".to_owned(),
                        }),
                    }]),
                    bundles: Some(vec![HarnessRuntimeBundleReceiptV1 {
                        id: "bundle-a".to_owned(),
                        revision: "bundle-rev-1".to_owned(),
                        digest: format!("sha256:{}", "a".repeat(64)),
                    }]),
                }),
            },
        };
        node.validate().expect("valid launch inventory");
        let encoded = serde_json::to_string(&node).unwrap();
        let decoded: HarnessRuntimeNodeInventoryV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, node);

        let mut missing_component = node.clone();
        missing_component.inventory.launch_inventory = Some(HarnessRuntimeLaunchInventoryV1 {
            spawn_profiles: None,
            bundles: None,
        });
        assert!(missing_component.validate().is_err());

        let mut duplicate_profile = node.clone();
        let profile = duplicate_profile.inventory.launch_inventory.as_ref().unwrap()
            .spawn_profiles.as_ref().unwrap()[0].clone();
        duplicate_profile.inventory.launch_inventory.as_mut().unwrap()
            .spawn_profiles.as_mut().unwrap().push(profile);
        assert!(duplicate_profile.validate().is_err());

        let mut bad_digest = node.clone();
        bad_digest.inventory.launch_inventory.as_mut().unwrap()
            .bundles.as_mut().unwrap()[0].digest = "not-a-digest".to_owned();
        assert!(bad_digest.validate().is_err());

        let mut no_launch_inventory = node;
        no_launch_inventory.inventory.launch_inventory = None;
        no_launch_inventory.validate().expect("absent launch inventory stays valid");
    }

    #[test]
    fn native_history_is_exact_bounded_and_excludes_sensitive_fields() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let route = HarnessNativeSessionRouteV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            scope: HarnessNativeSessionCatalogScopeV1::Workspace,
            workspace_id: Some("workspace-a".to_owned()),
            provider: "codex".to_owned(),
        };
        let selection = HarnessNativeSessionSelectionV1 {
            route: route.clone(),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 9,
            selection_id: "selection-a".to_owned(),
        };
        for request in [
            HarnessOperatorRequestV1::CatalogNativeSessions {
                route: route.clone(),
                limit: HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX,
            },
            HarnessOperatorRequestV1::PageNativeSessions {
                route: route.clone(),
                window: HarnessNativeSessionCatalogWindowV1::Recent,
                catalog_revision: 7,
                recent_cutoff_unix_ms: 9,
                after_selection_id: Some("selection-a".to_owned()),
                limit: 1,
            },
            HarnessOperatorRequestV1::PreviewNativeSession {
                selection: selection.clone(),
                message_limit: HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX,
            },
        ] {
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }
        let response = HarnessOperatorResponseV1::NativeSessionPreviewed(
            HarnessNativeSessionPreviewedV1 {
                selection,
                preview: HarnessNativeSessionPreviewV1 {
                    title: Some("Bounded preview".to_owned()),
                    modified_at_unix_ms: Some(10),
                    model: Some("model-a".to_owned()),
                    message_count: 1,
                    message_count_exact: true,
                    completed_turn_count: Some(1),
                    total_tokens: Some(12),
                    truncated: false,
                    messages: vec![HarnessNativeSessionPreviewMessageV1 {
                        role: HarnessNativeSessionPreviewRoleV1::User,
                        text: "bounded history preview".to_owned(),
                    }],
                },
            },
        );
        response.validate().unwrap();
        let encoded = serde_json::to_string(&response).unwrap();
        for forbidden in [
            "g4aho_",
            "credential",
            "provider_identity",
            "session_id",
            "terminal",
            "cwd",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[test]
    fn unknown_request_fields_and_unbounded_limits_fail_closed() {
        let unknown = r#"{"kind":"tasks-list","limit":10,"cursor":"secret"}"#;
        assert!(serde_json::from_str::<HarnessReadRequestV1>(unknown).is_err());
        assert!(HarnessReadRequestV1::TasksList {
            after_task_id: None, state: None, parent_task_id: None, limit: 0,
        }.validate().is_err());
        assert!(HarnessReadRequestV1::TasksList {
            after_task_id: None, state: None, parent_task_id: None, limit: 65,
        }.validate().is_err());
    }

    #[test]
    fn redacted_cross_entity_references_do_not_require_hidden_ids() {
        let redacted_run = r#"{
            "run_id":"hrun_000000000000000000000001",
            "revision":1,
            "parent_run_id":null,
            "task_id":null,
            "operation_id":null,
            "intent":{"mode":"inline","worktree":"existing","has_delivery_bundle":false,"has_continuation":false},
            "lifecycle":"requested",
            "binding":"none",
            "result_disposition":null,
            "failure_category":null,
            "references_redacted":true,
            "created_at_unix_ms":1,
            "updated_at_unix_ms":1
        }"#;
        let run: RedactedRunV1 = serde_json::from_str(redacted_run).expect("redacted run");
        run.validate().expect("valid redacted run");

        let operation_with_grant = r#"{
            "operation_id":"hop_000000000000000000000001",
            "revision":1,
            "kind":"create-task",
            "state":"prepared",
            "task_id":null,
            "run_id":null,
            "grant_id":"hgrant_000000000000000000000001",
            "reconciles_operation_id":null,
            "references_redacted":true,
            "failure_category":null,
            "outcome_unknown_reason":null,
            "reconciliation_outcome":null,
            "created_at_unix_ms":1,
            "updated_at_unix_ms":1,
            "dispatched_at_unix_ms":null,
            "finished_at_unix_ms":null
        }"#;
        assert!(serde_json::from_str::<RedactedOperationV1>(operation_with_grant).is_err());
    }

    #[test]
    fn operator_run_workspace_requests_are_exact_harness_only_round_trips() {
        let run_id = run_workspace_origin().run_id;
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let object_id = HarnessGitObjectIdV1::new("a".repeat(40)).unwrap();
        let requests = vec![
            HarnessOperatorRequestV1::InspectRunWorkspace {
                run_id: run_id.clone(),
            },
            HarnessOperatorRequestV1::ReadRunWorkspaceFile {
                run_id: run_id.clone(),
                path: path.clone(),
            },
            HarnessOperatorRequestV1::ReadRunGitHistory {
                run_id: run_id.clone(),
                path: Some(path.clone()),
                before: Some(object_id.clone()),
                limit: HARNESS_GIT_HISTORY_LIMIT_MAX,
            },
            HarnessOperatorRequestV1::ReadRunGitDiff {
                run_id,
                mode: HarnessGitDiffModeV1::Commit { revision: object_id },
                path: Some(path),
            },
        ];
        for request in requests {
            request.validate().expect("valid workspace request");
            let encoded = serde_json::to_string(&request).unwrap();
            for forbidden in [
                "node_id",
                "workspace_id",
                "endpoint",
                "root",
                "workspace_root",
                "worktree",
                "worktree_path",
                "environment",
                "spawn_spec",
                "provider_home",
            ] {
                assert!(!encoded.contains(forbidden), "request exposed {forbidden}");
            }
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
        }
    }

    #[test]
    fn operator_v4_workspace_replies_round_trip_without_host_or_diagnostic_fields() {
        let origin = run_workspace_origin();
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let commit = git_commit('a');
        let responses = vec![
            HarnessOperatorResponseV1::RunWorkspaceInspected(
                HarnessRunWorkspaceInspectionV1 {
                    origin: origin.clone(),
                    entries: vec![HarnessWorkspaceTreeEntryV1 {
                        relative_path: path.clone(),
                        kind: HarnessWorkspaceEntryKindV1::File,
                    }],
                    tree_truncated: false,
                    git: HarnessGitSummaryV1 {
                        is_repository: true,
                        branch: Some("main".to_owned()),
                        status: vec![HarnessGitStatusEntryV1 {
                            index_status: HarnessGitStatusCodeV1::Unmodified,
                            worktree_status: HarnessGitStatusCodeV1::Modified,
                            path: path.clone(),
                            previous_path: None,
                        }],
                        recent_commits: vec![HarnessGitCommitSummaryV1 {
                            id: commit.id.clone(),
                            summary: commit.subject.clone(),
                        }],
                        truncated: false,
                    },
                    truncation: None,
                },
            ),
            HarnessOperatorResponseV1::RunWorkspaceFileRead(HarnessRunWorkspaceFileV1 {
                origin: origin.clone(),
                path: path.clone(),
                content: HarnessWorkspaceFileContentV1::Utf8 {
                    text: "fn main() {}\n".to_owned(),
                    byte_len: 13,
                },
                revision: Some(HarnessWorkspaceFileRevisionV1::new("b".repeat(64)).unwrap()),
            }),
            HarnessOperatorResponseV1::RunGitHistoryRead(HarnessRunGitHistoryPageV1 {
                origin: origin.clone(),
                path: Some(path.clone()),
                commits: vec![commit],
                next_before: None,
                truncated: false,
            }),
            HarnessOperatorResponseV1::RunGitDiffRead(HarnessRunGitDiffV1 {
                origin,
                mode: HarnessGitDiffModeV1::Working,
                path: Some(path),
                text: "diff --git a/src/lib.rs b/src/lib.rs\n".to_owned(),
                truncated: false,
            }),
        ];
        for response in responses {
            response.validate().expect("valid V4 workspace response");
            let encoded = serde_json::to_string(&response).unwrap();
            assert_json_has_no_forbidden_keys(&encoded, &[
                "root",
                "worktree",
                "endpoint",
                "diagnostic",
                "author_email",
                "committer_email",
                "environment",
                "spawn_spec",
                "provider_home",
            ]);
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn operator_v4_repository_paths_and_git_cursors_fail_closed() {
        for value in [
            "",
            ".",
            "..",
            "../secret",
            "src/../secret",
            "src//lib.rs",
            "/absolute",
            "C:/absolute",
            r"src\lib.rs",
            "src/\nlib.rs",
        ] {
            assert!(HarnessRepositoryPathV1::new(value).is_err(), "accepted {value:?}");
        }
        assert!(HarnessRepositoryPathV1::new("x".repeat(HARNESS_REPOSITORY_PATH_MAX_BYTES)).is_ok());
        assert!(HarnessRepositoryPathV1::new("x".repeat(HARNESS_REPOSITORY_PATH_MAX_BYTES + 1)).is_err());
        assert!(HarnessGitObjectIdV1::new("a".repeat(40)).is_ok());
        assert!(HarnessGitObjectIdV1::new("a".repeat(64)).is_ok());
        assert!(HarnessGitObjectIdV1::new("A".repeat(40)).is_err());
        assert!(HarnessGitObjectIdV1::new("a".repeat(39)).is_err());

        let unknown_route = format!(
            r#"{{"kind":"read-run-workspace-file","run_id":"hrun_{}","path":"src/lib.rs","workspace_id":"forbidden"}}"#,
            "a".repeat(24),
        );
        assert!(serde_json::from_str::<HarnessOperatorRequestV1>(&unknown_route).is_err());
    }

    #[test]
    fn operator_v4_workspace_payloads_enforce_exact_bounds() {
        let origin = run_workspace_origin();
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let at_file_limit = HarnessWorkspaceFileContentV1::Utf8 {
            text: "x".repeat(HARNESS_WORKSPACE_FILE_MAX_BYTES),
            byte_len: HARNESS_WORKSPACE_FILE_MAX_BYTES as u32,
        };
        assert!(at_file_limit.validate().is_ok());
        assert!(HarnessWorkspaceFileContentV1::Utf8 {
            text: "x".repeat(HARNESS_WORKSPACE_FILE_MAX_BYTES + 1),
            byte_len: (HARNESS_WORKSPACE_FILE_MAX_BYTES + 1) as u32,
        }.validate().is_err());
        assert!(HarnessRunGitDiffV1 {
            origin: origin.clone(),
            mode: HarnessGitDiffModeV1::Working,
            path: Some(path.clone()),
            text: "x".repeat(HARNESS_GIT_DIFF_MAX_BYTES),
            truncated: false,
        }.validate().is_ok());
        assert!(HarnessRunGitDiffV1 {
            origin: origin.clone(),
            mode: HarnessGitDiffModeV1::Working,
            path: Some(path.clone()),
            text: "x".repeat(HARNESS_GIT_DIFF_MAX_BYTES + 1),
            truncated: true,
        }.validate().is_err());
        assert!(HarnessOperatorRequestV1::ReadRunGitHistory {
            run_id: origin.run_id.clone(),
            path: Some(path.clone()),
            before: None,
            limit: HARNESS_GIT_HISTORY_LIMIT_MAX + 1,
        }.validate().is_err());

        let entries: Vec<_> = (0..HARNESS_WORKSPACE_TREE_ENTRIES_MAX)
            .map(|index| HarnessWorkspaceTreeEntryV1 {
                relative_path: HarnessRepositoryPathV1::new(format!("entry-{index:04}")).unwrap(),
                kind: HarnessWorkspaceEntryKindV1::File,
            })
            .collect();
        let empty_git = HarnessGitSummaryV1 {
            is_repository: false,
            branch: None,
            status: Vec::new(),
            recent_commits: Vec::new(),
            truncated: false,
        };
        assert!(HarnessRunWorkspaceInspectionV1 {
            origin: origin.clone(),
            entries: entries.clone(),
            tree_truncated: false,
            git: empty_git.clone(),
            truncation: None,
        }.validate().is_ok());
        let mut too_many_entries = entries;
        too_many_entries.push(HarnessWorkspaceTreeEntryV1 {
            relative_path: HarnessRepositoryPathV1::new("entry-0512").unwrap(),
            kind: HarnessWorkspaceEntryKindV1::File,
        });
        assert!(HarnessRunWorkspaceInspectionV1 {
            origin: origin.clone(),
            entries: too_many_entries,
            tree_truncated: true,
            git: empty_git,
            truncation: None,
        }.validate().is_err());

        let status: Vec<_> = (0..HARNESS_GIT_STATUS_ENTRIES_MAX)
            .map(|index| HarnessGitStatusEntryV1 {
                index_status: HarnessGitStatusCodeV1::Unmodified,
                worktree_status: HarnessGitStatusCodeV1::Modified,
                path: HarnessRepositoryPathV1::new(format!("status-{index:04}")).unwrap(),
                previous_path: None,
            })
            .collect();
        let recent_commits: Vec<_> = (0..HARNESS_GIT_RECENT_COMMITS_MAX)
            .map(|index| HarnessGitCommitSummaryV1 {
                id: HarnessGitObjectIdV1::new(format!("{index:040x}")).unwrap(),
                summary: format!("commit {index}"),
            })
            .collect();
        assert!(HarnessGitSummaryV1 {
            is_repository: true,
            branch: Some("main".to_owned()),
            status: status.clone(),
            recent_commits: recent_commits.clone(),
            truncated: false,
        }.validate().is_ok());
        let mut too_many_status = status;
        too_many_status.push(HarnessGitStatusEntryV1 {
            index_status: HarnessGitStatusCodeV1::Unmodified,
            worktree_status: HarnessGitStatusCodeV1::Modified,
            path: HarnessRepositoryPathV1::new("status-0128").unwrap(),
            previous_path: None,
        });
        assert!(HarnessGitSummaryV1 {
            is_repository: true,
            branch: Some("main".to_owned()),
            status: too_many_status,
            recent_commits: Vec::new(),
            truncated: true,
        }.validate().is_err());
        let mut too_many_recent = recent_commits;
        too_many_recent.push(HarnessGitCommitSummaryV1 {
            id: HarnessGitObjectIdV1::new(format!("{:040x}", HARNESS_GIT_RECENT_COMMITS_MAX)).unwrap(),
            summary: "one too many".to_owned(),
        });
        assert!(HarnessGitSummaryV1 {
            is_repository: true,
            branch: Some("main".to_owned()),
            status: Vec::new(),
            recent_commits: too_many_recent,
            truncated: true,
        }.validate().is_err());

        let commits: Vec<_> = (0..HARNESS_GIT_HISTORY_LIMIT_MAX)
            .map(|index| {
                let mut commit = git_commit('a');
                commit.id = HarnessGitObjectIdV1::new(format!("{index:040x}")).unwrap();
                commit
            })
            .collect();
        assert!(HarnessRunGitHistoryPageV1 {
            origin: origin.clone(),
            path: Some(path.clone()),
            commits: commits.clone(),
            next_before: None,
            truncated: false,
        }.validate().is_ok());
        let next_before = commits.last().map(|commit| commit.id.clone());
        assert!(HarnessRunGitHistoryPageV1 {
            origin: origin.clone(),
            path: Some(path.clone()),
            commits: commits.clone(),
            next_before: next_before.clone(),
            truncated: false,
        }.validate().is_ok());
        assert!(HarnessRunGitHistoryPageV1 {
            origin: origin.clone(),
            path: Some(path.clone()),
            commits: commits.clone(),
            next_before: None,
            truncated: true,
        }.validate().is_ok());
        assert!(HarnessRunGitHistoryPageV1 {
            origin: origin.clone(),
            path: Some(path.clone()),
            commits: commits.clone(),
            next_before,
            truncated: true,
        }.validate().is_ok());
        let mut too_many_commits = commits;
        let mut extra_commit = git_commit('a');
        extra_commit.id = HarnessGitObjectIdV1::new(format!("{:040x}", HARNESS_GIT_HISTORY_LIMIT_MAX)).unwrap();
        too_many_commits.push(extra_commit);
        assert!(HarnessRunGitHistoryPageV1 {
            origin,
            path: Some(path),
            commits: too_many_commits,
            next_before: None,
            truncated: false,
        }.validate().is_err());
    }

    fn monitor_with_mixed_capabilities() -> SessionMonitorV1 {
        SessionMonitorV1 {
            run_id: HarnessRunId::new("hrun_000000000000000000000001").unwrap(),
            visibility: HarnessMonitoringVisibilityV1::Detail,
            availability: ProjectionAvailabilityV1::Frozen,
            freshness: ProjectionFreshnessV1::ReplacedIncarnation,
            transport_incomplete: false,
            features: MonitorFeatureStatesV1 {
                todo: FeatureObservationStateV1::Observed,
                tools: FeatureObservationStateV1::NotSupportedByObservedSources,
                subagents: FeatureObservationStateV1::Unknown,
                interactions: FeatureObservationStateV1::SupportedNotObserved,
                owned_processes: FeatureObservationStateV1::Observed,
                files: FeatureObservationStateV1::Observed,
                usage: FeatureObservationStateV1::SupportedNotObserved,
                history: FeatureObservationStateV1::Unknown,
            },
            todo_total: 1,
            todo_completed: 0,
            active_tools: 0,
            active_subagents: 0,
            active_interactions: 0,
            active_blocks: 0,
            active_processes: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            context_window_tokens: None,
            history: None,
            detail: Some(SessionMonitorDetailV1 {
                todo_facts: vec![TodoFactV1 {
                    state: TodoStateV1::Unknown,
                    todo_id: Some("todo-1".to_owned()),
                    label: Some("review result".to_owned()),
                    evidence: ObservationEvidenceV1::PtyHint,
                }],
                tool_facts: Vec::new(),
                subagent_facts: Vec::new(),
                interaction_facts: Vec::new(),
                block_facts: Vec::new(),
                process_facts: vec![ActivityFactV1 {
                    class: ActivityClassV1::OwnedProcess,
                    state: ActivityStateV1::UnknownAfterGap,
                    label: Some("compiler".to_owned()),
                    correlation: Some(1),
                    evidence: ObservationEvidenceV1::ManagedHook,
                }],
                file_facts: vec![FileFactV1 {
                    action: FileActionV1::Changed,
                    relative_path: Some("src/lib.rs".to_owned()),
                    evidence: ObservationEvidenceV1::StructuredProvider,
                }],
            }),
        }
    }

    #[test]
    fn monitor_preserves_mixed_capability_and_frozen_replaced_truth() {
        let monitor = monitor_with_mixed_capabilities();
        monitor.validate().expect("truthful mixed monitor");
        let encoded = serde_json::to_string(&monitor).expect("encode monitor");
        let decoded: SessionMonitorV1 = serde_json::from_str(&encoded).expect("decode monitor");
        assert_eq!(decoded, monitor);
        assert_eq!(decoded.availability, ProjectionAvailabilityV1::Frozen);
        assert_eq!(decoded.freshness, ProjectionFreshnessV1::ReplacedIncarnation);
        assert_eq!(decoded.features.tools, FeatureObservationStateV1::NotSupportedByObservedSources);
        let detail = decoded.detail.unwrap();
        assert_eq!(detail.todo_facts[0].state, TodoStateV1::Unknown);
        assert_eq!(detail.todo_facts[0].todo_id.as_deref(), Some("todo-1"));
        assert_eq!(detail.todo_facts[0].label.as_deref(), Some("review result"));
        assert_eq!(detail.process_facts[0].correlation, Some(1));
        assert_eq!(detail.file_facts[0].relative_path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn monitor_rejects_inference_and_raw_detail_sentinels() {
        let mut monitor = monitor_with_mixed_capabilities();
        monitor.detail.as_mut().unwrap().tool_facts.push(ActivityFactV1 {
            class: ActivityClassV1::Tool,
            state: ActivityStateV1::Active,
            label: Some("command".to_owned()),
            correlation: Some(1),
            evidence: ObservationEvidenceV1::StructuredProvider,
        });
        assert!(monitor.validate().is_err());

        let raw_file = r#"{"action":"changed","evidence":"managed-hook","path":"C:\\private\\prompt.txt"}"#;
        assert!(serde_json::from_str::<FileFactV1>(raw_file).is_err());
        let encoded = serde_json::to_string(&monitor_with_mixed_capabilities()).unwrap();
        for sentinel in ["private", "prompt.txt", "provider_id", "correlation_id"] {
            assert!(!encoded.contains(sentinel));
        }

        let mut partial = monitor_with_mixed_capabilities();
        partial.availability = ProjectionAvailabilityV1::Partial;
        partial.freshness = ProjectionFreshnessV1::IncompleteAfterGap;
        partial.transport_incomplete = true;
        partial.validate().expect("truthful partial gap");
    }

    #[test]
    fn timeline_structured_fields_round_trip_without_raw_correlation_shape() {
        let run_id = HarnessRunId::new("hrun_000000000000000000000001").unwrap();
        let page = TimelinePageV1 {
            run_id: run_id.clone(),
            availability: ProjectionAvailabilityV1::Current,
            freshness: ProjectionFreshnessV1::Live,
            transport_incomplete: false,
            entries: vec![TimelineEntryV1 {
                sequence: 7,
                received_at_ms: 10,
                category: TimelineCategoryV1::Tool,
                label: Some("command".to_owned()),
                state: TimelineStateV1::Completed,
                correlation: Some(1),
                evidence: ObservationEvidenceV1::StructuredProvider,
            }],
            next_cursor: None,
        };
        page.validate_for(&run_id).unwrap();
        let encoded = serde_json::to_string(&page).unwrap();
        assert!(encoded.contains("\"label\":\"command\""));
        assert!(encoded.contains("\"state\":\"completed\""));
        assert!(encoded.contains("\"correlation\":1"));
        assert!(!encoded.contains("correlation_id"));
        assert_eq!(serde_json::from_str::<TimelinePageV1>(&encoded).unwrap(), page);

        let old_entry: TimelineEntryV1 = serde_json::from_str(
            r#"{"sequence":1,"received_at_ms":2,"category":"usage","evidence":"managed-hook"}"#,
        ).unwrap();
        assert_eq!(old_entry.state, TimelineStateV1::Unknown);
        assert_eq!(old_entry.correlation, None);
        old_entry.validate().unwrap();
    }

    #[test]
    fn monitor_history_summary_matches_categorical_feature_state() {
        let mut monitor = monitor_with_mixed_capabilities();
        monitor.features.history = FeatureObservationStateV1::Observed;
        monitor.history = Some(SessionMonitorHistoryV1 {
            message_count: 12,
            message_count_exact: true,
            completed_turn_count: Some(3),
            total_tokens: Some(800),
        });
        monitor.validate().unwrap();
        let encoded = serde_json::to_string(&monitor).unwrap();
        let decoded: SessionMonitorV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.history, monitor.history);

        monitor.history = None;
        monitor.validate().expect("redacted observed history remains valid");
        monitor.features.history = FeatureObservationStateV1::NotSupportedByObservedSources;
        monitor.history = Some(SessionMonitorHistoryV1 {
            message_count: 1,
            message_count_exact: false,
            completed_turn_count: None,
            total_tokens: None,
        });
        assert!(monitor.validate().is_err());
    }

    #[test]
    fn structured_file_fact_rejects_absolute_or_traversing_paths() {
        for relative_path in ["C:/private.txt", "/private.txt", "src/../private.txt", "src\\private.txt"] {
            let fact = FileFactV1 {
                action: FileActionV1::Changed,
                relative_path: Some(relative_path.to_owned()),
                evidence: ObservationEvidenceV1::WorkspaceObservation,
            };
            assert!(fact.validate().is_err(), "accepted {relative_path}");
        }
    }

    #[test]
    fn operator_reverse_attribution_is_exact_and_private() {
        let subject = HarnessReverseAttributionSubjectV1::FileScope {
            workspace: reverse_attribution_workspace(),
            relative_path: HarnessRepositoryPathV1::new("src/lib.rs").unwrap(),
        };
        let request = HarnessOperatorRequestV1::ReverseAttributionGet {
            subject: subject.clone(),
        };
        request.validate().unwrap();
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();

        let response = HarnessReverseAttributionV1 {
            subject,
            outcome: HarnessReverseAttributionOutcomeV1::Attributed,
            links: vec![reverse_attribution_link('2')],
        };
        response.validate().unwrap();
        let encoded = serde_json::to_value(
            HarnessOperatorResponseV1::ReverseAttribution(response),
        ).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "reverse-attribution",
                "value": {
                    "subject": {
                        "kind": "file-scope",
                        "workspace": {
                            "node_id": "node-a",
                            "node_incarnation_id": "07070707070707070707070707070707",
                            "workspace_id": "workspace-a"
                        },
                        "relative_path": "src/lib.rs"
                    },
                    "outcome": "attributed",
                    "links": [{
                        "task_id": "htask_111111111111111111111111",
                        "run_id": "hrun_222222222222222222222222",
                        "run_revision": 3,
                        "binding": {
                            "kind": "workspace",
                            "workspace": {
                                "node_id": "node-a",
                                "node_incarnation_id": "07070707070707070707070707070707",
                                "workspace_id": "workspace-a"
                            }
                        },
                        "relation": "workspace-scope"
                    }]
                }
            }),
        );
        let encoded = encoded.to_string();
        for forbidden in [
            "produced-by",
            "modified-by",
            "provider_session",
            "provider_profile",
            "provider_home",
            "credential",
            "auth",
            "canonical_root",
            "display_root",
            "source_path",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }

        let mut unknown = serde_json::to_value(HarnessOperatorRequestV1::ReverseAttributionGet {
            subject: HarnessReverseAttributionSubjectV1::Workspace {
                workspace: reverse_attribution_workspace(),
            },
        }).unwrap();
        unknown["subject"]["provider_session_id"] = serde_json::json!("forbidden");
        assert!(serde_json::from_value::<HarnessOperatorRequestV1>(unknown).is_err());
    }

    #[test]
    fn reverse_attribution_is_bounded_canonical_and_exactly_correlated() {
        let subject = HarnessReverseAttributionSubjectV1::FileScope {
            workspace: reverse_attribution_workspace(),
            relative_path: HarnessRepositoryPathV1::new("src/lib.rs").unwrap(),
        };
        let first = reverse_attribution_link('2');
        let second = reverse_attribution_link('3');
        let exact = HarnessReverseAttributionV1 {
            subject: subject.clone(),
            outcome: HarnessReverseAttributionOutcomeV1::Attributed,
            links: vec![first.clone(), second.clone()],
        };
        exact.validate_for(&subject).unwrap();

        let mut noncanonical = exact.clone();
        noncanonical.links.reverse();
        assert!(matches!(
            noncanonical.validate(),
            Err(HarnessOperatorApiError::InvalidReverseAttribution),
        ));
        let duplicate = HarnessReverseAttributionV1 {
            links: vec![first.clone(), first],
            ..exact.clone()
        };
        assert!(duplicate.validate().is_err());
        let over_limit = HarnessReverseAttributionV1 {
            links: (0..=HARNESS_REVERSE_ATTRIBUTION_LINKS_MAX)
                .map(|index| HarnessReverseAttributionLinkV1 {
                    task_id: HarnessTaskId::new(format!("htask_{index:024x}")).unwrap(),
                    ..reverse_attribution_link('4')
                })
                .collect(),
            ..exact.clone()
        };
        assert!(over_limit.validate().is_err());

        let unattributed = HarnessReverseAttributionV1 {
            subject: subject.clone(),
            outcome: HarnessReverseAttributionOutcomeV1::Unattributed,
            links: Vec::new(),
        };
        unattributed.validate().unwrap();
        let attributed_empty = HarnessReverseAttributionV1 {
            outcome: HarnessReverseAttributionOutcomeV1::Attributed,
            ..unattributed.clone()
        };
        assert!(attributed_empty.validate().is_err());
        let unattributed_linked = HarnessReverseAttributionV1 {
            outcome: HarnessReverseAttributionOutcomeV1::Unattributed,
            links: vec![second],
            ..exact.clone()
        };
        assert!(unattributed_linked.validate().is_err());

        let other_subject = HarnessReverseAttributionSubjectV1::FileScope {
            workspace: reverse_attribution_workspace(),
            relative_path: HarnessRepositoryPathV1::new("src/other.rs").unwrap(),
        };
        assert!(matches!(
            exact.validate_for(&other_subject),
            Err(HarnessOperatorApiError::InvalidReverseAttribution),
        ));
    }

    #[test]
    fn reverse_attribution_relations_reject_false_file_and_binding_claims() {
        let file_subject = HarnessReverseAttributionSubjectV1::FileScope {
            workspace: reverse_attribution_workspace(),
            relative_path: HarnessRepositoryPathV1::new("src/lib.rs").unwrap(),
        };
        let mut wrong_relation = reverse_attribution_link('2');
        wrong_relation.relation = HarnessReverseAttributionRelationV1::WorkspaceBinding;
        assert!(HarnessReverseAttributionV1 {
            subject: file_subject,
            outcome: HarnessReverseAttributionOutcomeV1::Attributed,
            links: vec![wrong_relation],
        }.validate().is_err());

        let managed_subject = HarnessReverseAttributionSubjectV1::ManagedRecord {
            workspace: reverse_attribution_workspace(),
            record_id: HarnessSelectorV1::new("record-a").unwrap(),
        };
        let mismatched_record = HarnessReverseAttributionLinkV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap(),
            run_id: HarnessRunId::new(format!("hrun_{}", "2".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(3).unwrap(),
            binding: HarnessReverseAttributionBindingV1::ManagedRecord {
                workspace: reverse_attribution_workspace(),
                record_id: HarnessSelectorV1::new("record-b").unwrap(),
                active_instance_id: Some(41),
                active_generation: Some(3),
            },
            relation: HarnessReverseAttributionRelationV1::ManagedRecordBinding,
        };
        assert!(HarnessReverseAttributionV1 {
            subject: managed_subject,
            outcome: HarnessReverseAttributionOutcomeV1::Attributed,
            links: vec![mismatched_record],
        }.validate().is_err());

        let invalid_active_pair = HarnessReverseAttributionBindingV1::ManagedRecord {
            workspace: reverse_attribution_workspace(),
            record_id: HarnessSelectorV1::new("record-a").unwrap(),
            active_instance_id: Some(41),
            active_generation: None,
        };
        assert!(invalid_active_pair.validate().is_err());
    }

    #[test]
    fn operator_node_workspace_requests_are_exact_round_trips() {
        let node_id = node_workspace_origin().node_id;
        let workspace_id = node_workspace_origin().workspace_id;
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let object_id = HarnessGitObjectIdV1::new("a".repeat(40)).unwrap();
        let requests = vec![
            HarnessOperatorRequestV1::InspectNodeWorkspace {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
            },
            HarnessOperatorRequestV1::ReadNodeWorkspaceFile {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            },
            HarnessOperatorRequestV1::ReadNodeGitHistory {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: Some(path.clone()),
                before: Some(object_id.clone()),
                limit: HARNESS_GIT_HISTORY_LIMIT_MAX,
            },
            HarnessOperatorRequestV1::ReadNodeGitDiff {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                mode: HarnessGitDiffModeV1::Commit { revision: object_id },
                path: Some(path),
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid node-workspace request");
            let encoded = serde_json::to_string(&request).unwrap();
            for forbidden in ["run_id", "endpoint", "root", "worktree", "environment"] {
                assert!(!encoded.contains(forbidden), "request exposed {forbidden}");
            }
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }

        assert!(HarnessOperatorRequestV1::InspectNodeWorkspace {
            node_id: String::new(),
            workspace_id,
        }.validate().is_err());
        assert!(HarnessOperatorRequestV1::InspectNodeWorkspace {
            node_id,
            workspace_id: String::new(),
        }.validate().is_err());
    }

    #[test]
    fn operator_v9_node_workspace_replies_round_trip_without_run_or_diagnostic_fields() {
        let origin = node_workspace_origin();
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let commit = git_commit('a');
        let responses = vec![
            HarnessOperatorResponseV1::NodeWorkspaceInspected(
                HarnessNodeWorkspaceInspectionV1 {
                    origin: origin.clone(),
                    entries: vec![HarnessWorkspaceTreeEntryV1 {
                        relative_path: path.clone(),
                        kind: HarnessWorkspaceEntryKindV1::File,
                    }],
                    tree_truncated: false,
                    git: HarnessGitSummaryV1 {
                        is_repository: true,
                        branch: Some("main".to_owned()),
                        status: vec![HarnessGitStatusEntryV1 {
                            index_status: HarnessGitStatusCodeV1::Unmodified,
                            worktree_status: HarnessGitStatusCodeV1::Modified,
                            path: path.clone(),
                            previous_path: None,
                        }],
                        recent_commits: vec![HarnessGitCommitSummaryV1 {
                            id: commit.id.clone(),
                            summary: commit.subject.clone(),
                        }],
                        truncated: false,
                    },
                    truncation: None,
                },
            ),
            HarnessOperatorResponseV1::NodeWorkspaceFileRead(HarnessNodeWorkspaceFileV1 {
                origin: origin.clone(),
                path: path.clone(),
                content: HarnessWorkspaceFileContentV1::Utf8 {
                    text: "fn main() {}\n".to_owned(),
                    byte_len: 13,
                },
                revision: Some(HarnessWorkspaceFileRevisionV1::new("b".repeat(64)).unwrap()),
            }),
            HarnessOperatorResponseV1::NodeGitHistoryRead(HarnessNodeGitHistoryPageV1 {
                origin: origin.clone(),
                path: Some(path.clone()),
                commits: vec![commit],
                next_before: None,
                truncated: false,
            }),
            HarnessOperatorResponseV1::NodeGitDiffRead(HarnessNodeGitDiffV1 {
                origin,
                mode: HarnessGitDiffModeV1::Working,
                path: Some(path),
                text: "diff --git a/src/lib.rs b/src/lib.rs\n".to_owned(),
                truncated: false,
            }),
        ];
        for response in responses {
            response.validate().expect("valid V9 node-workspace response");
            let encoded = serde_json::to_string(&response).unwrap();
            assert_json_has_no_forbidden_keys(&encoded, &[
                "run_id",
                "run_revision",
                "root",
                "worktree",
                "endpoint",
                "diagnostic",
                "author_email",
                "committer_email",
                "environment",
            ]);
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }

        let mismatched_workspace = HarnessNodeWorkspaceOriginV1 {
            workspace_id: "other-workspace".to_owned(),
            ..node_workspace_origin()
        };
        assert!(mismatched_workspace.validate_for("node-a", "workspace-a").is_err());
        mismatched_workspace.validate_for("node-a", "other-workspace").unwrap();
    }

    #[test]
    fn operator_node_workspace_write_verbs_are_exact_round_trips() {
        let node_id = node_workspace_origin().node_id;
        let workspace_id = node_workspace_origin().workspace_id;
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let revision = HarnessWorkspaceFileRevisionV1::new("b".repeat(64)).unwrap();
        let requests = vec![
            HarnessOperatorRequestV1::WriteNodeWorkspaceFile {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
                content: "fn main() {}\n".to_owned(),
                expected_revision: revision.clone(),
            },
            HarnessOperatorRequestV1::CreateNodeWorkspaceFile {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            },
            HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory {
                node_id: node_id.clone(),
                workspace_id: workspace_id.clone(),
                path: path.clone(),
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid node-workspace write request");
            let encoded = serde_json::to_string(&request).unwrap();
            for forbidden in ["run_id", "endpoint", "root", "worktree", "environment"] {
                assert!(!encoded.contains(forbidden), "request exposed {forbidden}");
            }
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }

        let origin = node_workspace_origin();
        let responses = vec![
            HarnessOperatorResponseV1::NodeWorkspaceFileWritten(HarnessNodeWorkspaceFileV1 {
                origin: origin.clone(),
                path: path.clone(),
                content: HarnessWorkspaceFileContentV1::Utf8 {
                    text: "fn main() {}\n".to_owned(),
                    byte_len: 13,
                },
                revision: Some(revision.clone()),
            }),
            HarnessOperatorResponseV1::NodeWorkspaceFileCreated(HarnessNodeWorkspaceFileV1 {
                origin: origin.clone(),
                path: path.clone(),
                content: HarnessWorkspaceFileContentV1::Utf8 {
                    text: String::new(),
                    byte_len: 0,
                },
                revision: Some(revision.clone()),
            }),
            HarnessOperatorResponseV1::NodeWorkspaceDirectoryCreated(
                HarnessNodeWorkspaceDirectoryV1 {
                    origin: origin.clone(),
                    entry: HarnessWorkspaceTreeEntryV1 {
                        relative_path: path.clone(),
                        kind: HarnessWorkspaceEntryKindV1::Directory,
                    },
                },
            ),
        ];
        for response in responses {
            response.validate().expect("valid V11 node-workspace write response");
            let encoded = serde_json::to_string(&response).unwrap();
            assert_json_has_no_forbidden_keys(&encoded, &[
                "run_id",
                "run_revision",
                "root",
                "worktree",
                "endpoint",
                "diagnostic",
                "environment",
            ]);
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }

        let wrong_kind_directory = HarnessNodeWorkspaceDirectoryV1 {
            origin,
            entry: HarnessWorkspaceTreeEntryV1 {
                relative_path: path,
                kind: HarnessWorkspaceEntryKindV1::File,
            },
        };
        assert!(wrong_kind_directory.validate().is_err());
    }

    #[test]
    fn node_workspace_write_v11_requests_reject_malformed_fields() {
        let node_id = node_workspace_origin().node_id;
        let workspace_id = node_workspace_origin().workspace_id;
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let revision = HarnessWorkspaceFileRevisionV1::new("b".repeat(64)).unwrap();

        let oversized_content = HarnessOperatorRequestV1::WriteNodeWorkspaceFile {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            content: "x".repeat(HARNESS_WORKSPACE_FILE_MAX_BYTES + 1),
            expected_revision: revision.clone(),
        };
        assert!(matches!(
            oversized_content.validate(),
            Err(HarnessOperatorApiError::InvalidWorkspaceFile),
        ));
        let bounded_content = HarnessOperatorRequestV1::WriteNodeWorkspaceFile {
            node_id: node_id.clone(),
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            content: "x".repeat(HARNESS_WORKSPACE_FILE_MAX_BYTES),
            expected_revision: revision.clone(),
        };
        bounded_content.validate().unwrap();

        assert!(HarnessWorkspaceFileRevisionV1::new("not-a-sha256-digest").is_err());
        assert!(HarnessWorkspaceFileRevisionV1::new("A".repeat(64)).is_err());

        let empty_node_write = HarnessOperatorRequestV1::WriteNodeWorkspaceFile {
            node_id: String::new(),
            workspace_id: workspace_id.clone(),
            path: path.clone(),
            content: "ok".to_owned(),
            expected_revision: revision,
        };
        assert!(empty_node_write.validate().is_err());

        let empty_workspace_create_file = HarnessOperatorRequestV1::CreateNodeWorkspaceFile {
            node_id: node_id.clone(),
            workspace_id: String::new(),
            path: path.clone(),
        };
        assert!(empty_workspace_create_file.validate().is_err());

        let empty_node_create_directory = HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory {
            node_id: String::new(),
            workspace_id,
            path,
        };
        assert!(empty_node_create_directory.validate().is_err());
    }

    #[test]
    fn operator_context_source_observation_is_exact_and_private() {
        let run_id = HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap();
        let request = HarnessOperatorRequestV1::ObserveRunContextSource {
            run_id: run_id.clone(),
        };
        request.validate().unwrap();
        let request_json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            request_json,
            serde_json::json!({
                "kind": "observe-run-context-source",
                "run_id": run_id,
            }),
        );
        for forbidden in ["authority", "operation_id", "idempotency_ref", "task_id"] {
            assert!(!request_json.to_string().contains(forbidden), "leaked {forbidden}");
        }
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();

        let response = HarnessRunContextSourceObservationV1 {
            run_id: HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(9).unwrap(),
            feature_state: FeatureObservationStateV1::Observed,
            message_count: 17,
            message_count_exact: true,
            completed_turn_count: Some(6),
            total_tokens: Some(4_096),
            observed_at_unix_ms: Some(1_000),
        };
        response.validate().unwrap();
        let response_json = serde_json::to_value(
            HarnessOperatorResponseV1::RunContextSourceObserved(response),
        ).unwrap();
        assert_eq!(
            response_json,
            serde_json::json!({
                "kind": "run-context-source-observed",
                "value": {
                    "run_id": format!("hrun_{}", "8".repeat(24)),
                    "run_revision": 9,
                    "feature_state": "observed",
                    "message_count": 17,
                    "message_count_exact": true,
                    "completed_turn_count": 6,
                    "total_tokens": 4096,
                    "observed_at_unix_ms": 1000,
                }
            }),
        );
        let response_json = response_json.to_string();
        for forbidden in [
            "transcript",
            "message_text",
            "provider_session",
            "provider_profile",
            "provider_home",
            "credential",
            "auth",
            "path",
            "model",
        ] {
            assert!(!response_json.contains(forbidden), "leaked {forbidden}");
        }

        let mut unknown = request_json;
        unknown["provider_session_id"] = serde_json::json!("forbidden");
        assert!(serde_json::from_value::<HarnessOperatorRequestV1>(unknown).is_err());
    }

    #[test]
    fn context_source_observation_rejects_inexact_or_mismatched_eligibility_claims() {
        let run_id = HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap();
        let observed = HarnessRunContextSourceObservationV1 {
            run_id: run_id.clone(),
            run_revision: HarnessRevision::new(9).unwrap(),
            feature_state: FeatureObservationStateV1::Observed,
            message_count: 17,
            message_count_exact: true,
            completed_turn_count: Some(6),
            total_tokens: Some(4_096),
            observed_at_unix_ms: Some(1_000),
        };
        observed.validate_for(&run_id).unwrap();

        let mut inexact = observed.clone();
        inexact.message_count_exact = false;
        assert!(inexact.validate().is_err());
        let mut empty = observed.clone();
        empty.message_count = 0;
        assert!(empty.validate().is_err());
        let mut impossible_turns = observed.clone();
        impossible_turns.completed_turn_count = Some(18);
        assert!(impossible_turns.validate().is_err());
        let unobserved = HarnessRunContextSourceObservationV1 {
            feature_state: FeatureObservationStateV1::SupportedNotObserved,
            message_count: 0,
            message_count_exact: false,
            completed_turn_count: None,
            total_tokens: None,
            observed_at_unix_ms: None,
            ..observed.clone()
        };
        unobserved.validate().unwrap();
        let mut leaked_count = unobserved;
        leaked_count.total_tokens = Some(1);
        assert!(leaked_count.validate().is_err());

        let other_run_id = HarnessRunId::new(format!("hrun_{}", "9".repeat(24))).unwrap();
        assert!(matches!(
            observed.validate_for(&other_run_id),
            Err(HarnessOperatorApiError::InvalidRunContextSourceObservation),
        ));
    }

    fn session_address(instance_id: u64, generation: u64) -> HarnessRuntimeSessionAddressV1 {
        HarnessRuntimeSessionAddressV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "07".repeat(16),
            workspace_id: "workspace-a".to_owned(),
            instance_id,
            generation,
        }
    }

    fn sample_agent_stream_chunk(source_sequence: u64) -> HarnessAgentStreamChunkV1 {
        HarnessAgentStreamChunkV1 {
            source_sequence,
            kind: HarnessAgentStreamChunkKindV1::Text {
                text: format!("chunk-{source_sequence}"),
                is_delta: true,
            },
        }
    }

    fn sample_terminal_frame(sequence: u64) -> HarnessRuntimeTerminalFrameV1 {
        HarnessRuntimeTerminalFrameV1 {
            sequence,
            size: HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
            cursor_row: 0,
            cursor_column: 0,
            formatted: format!("frame-{sequence}").into_bytes(),
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: HarnessRuntimeMouseProtocolEncodingV1::Default,
            produced_at_unix_ms: 1_000,
            screen_state: None,
            bracketed_paste: None,
        }
    }

    fn sample_runtime_session(instance_id: u64, generation: u64) -> HarnessRuntimeSessionV1 {
        HarnessRuntimeSessionV1 {
            instance_id,
            generation,
            provider: "codex".to_owned(),
            transport: HarnessRuntimeTransportV1::Pty,
            status: HarnessRuntimeSessionStatusV1::Running,
            process_id: Some(1234),
            terminal_size: Some(HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 }),
            operation_pending: false,
            input_pending: false,
            screen_state: None,
        }
    }

    /// A fully-populated `OperatorGateStateV1` -- one accepted option, one
    /// declined, matching the shape `gate4agent-shell-native`'s numbered-list
    /// parser actually produces (see that crate's own tests for the parser
    /// itself; this crate only owns the wire mirror and its bounds).
    fn sample_operator_gate_state() -> OperatorGateStateV1 {
        OperatorGateStateV1 {
            kind: OperatorGateKindV1::HookTrust,
            subject: OperatorGateSubjectV1::Hooks { count: Some(6) },
            input: OperatorGateInputV1::NumberedList,
            options: vec![
                OperatorGateOptionV1 {
                    text: "Trust all and continue".to_owned(),
                    semantics: OperatorGateOptionSemanticsV1::Accept,
                    selected: false,
                },
                OperatorGateOptionV1 {
                    text: "Continue without trusting".to_owned(),
                    semantics: OperatorGateOptionSemanticsV1::Decline,
                    selected: true,
                },
            ],
        }
    }

    /// Every `PtyScreenStateV1` variant, for the round-trip and validation
    /// sweeps below -- kept as one list so a variant added to the enum
    /// without a matching addition here is caught by a stale sweep rather
    /// than silently under-covered.
    fn every_screen_state_variant() -> Vec<PtyScreenStateV1> {
        vec![
            PtyScreenStateV1::Unknown,
            PtyScreenStateV1::NotAgent { observed_process: "installer.exe".to_owned() },
            PtyScreenStateV1::OperatorGate { gate: sample_operator_gate_state() },
            PtyScreenStateV1::Failing { reason: "crash-loop".to_owned() },
            PtyScreenStateV1::Ready,
        ]
    }

    /// The literal proof the `deny_unknown_fields` hazard is closed: a
    /// pre-V13 reply (`screen_state: None`) must omit the KEY entirely, not
    /// serialize it as `null` -- an older client's struct has no such field
    /// at all, so a `null` value would still trip `deny_unknown_fields`.
    #[test]
    fn a_terminal_frame_with_no_screen_state_serializes_without_the_key_at_all() {
        let frame = sample_terminal_frame(1);
        assert!(frame.screen_state.is_none());
        let value = serde_json::to_value(&frame).unwrap();
        assert!(
            value.as_object().unwrap().get("screen_state").is_none(),
            "expected no screen_state key in {value}",
        );
    }

    /// Sibling of the above for `HarnessRuntimeSessionV1` -- the session
    /// inventory's own reply carries the identical hazard and the identical
    /// fix.
    #[test]
    fn a_runtime_session_with_no_screen_state_serializes_without_the_key_at_all() {
        let session = sample_runtime_session(1, 1);
        assert!(session.screen_state.is_none());
        let value = serde_json::to_value(&session).unwrap();
        assert!(
            value.as_object().unwrap().get("screen_state").is_none(),
            "expected no screen_state key in {value}",
        );
    }

    /// `Some(..)` round-trips byte-exact for every variant, on both wire
    /// types that carry it.
    #[test]
    fn some_screen_state_round_trips_exactly_for_every_variant_on_both_carriers() {
        for variant in every_screen_state_variant() {
            let mut frame = sample_terminal_frame(1);
            frame.screen_state = Some(variant.clone());
            let decoded_frame: HarnessRuntimeTerminalFrameV1 = serde_json::from_slice(
                &serde_json::to_vec(&frame).unwrap(),
            ).unwrap();
            assert_eq!(decoded_frame, frame);

            let mut session = sample_runtime_session(1, 1);
            session.screen_state = Some(variant.clone());
            let decoded_session: HarnessRuntimeSessionV1 = serde_json::from_slice(
                &serde_json::to_vec(&session).unwrap(),
            ).unwrap();
            assert_eq!(decoded_session, session);
        }
    }

    /// `validate()` rejects an oversized carried string inside a present
    /// `screen_state`, on both carriers -- the bound is enforced regardless
    /// of which wire type happens to hold the value.
    #[test]
    fn validate_rejects_an_oversized_string_inside_a_present_screen_state() {
        let oversized_process = "x".repeat(HARNESS_SCREEN_STATE_PROCESS_MAX_BYTES + 1);
        let oversized_gate = "x".repeat(HARNESS_SCREEN_STATE_GATE_MAX_BYTES + 1);
        let cases = [
            PtyScreenStateV1::NotAgent { observed_process: oversized_process },
            PtyScreenStateV1::Failing { reason: oversized_gate },
        ];
        for case in cases {
            let mut frame = sample_terminal_frame(1);
            frame.screen_state = Some(case.clone());
            assert!(
                matches!(frame.validate(), Err(HarnessOperatorApiError::InvalidRuntimeInventory)),
                "expected frame validate() to reject {case:?}",
            );

            let mut session = sample_runtime_session(1, 1);
            session.screen_state = Some(case.clone());
            assert!(
                matches!(session.validate(), Err(HarnessOperatorApiError::InvalidRuntimeInventory)),
                "expected session validate() to reject {case:?}",
            );
        }
    }

    /// Sibling of the sweep above for `OperatorGateStateV1`'s own nested
    /// bounds -- an oversized `Directory` path and an oversized option
    /// label are each, on their own, enough to fail `validate()`, on both
    /// carriers.
    #[test]
    fn validate_rejects_an_oversized_field_nested_inside_an_operator_gate() {
        let oversized_path = "x".repeat(HARNESS_GATE_PATH_MAX_BYTES + 1);
        let oversized_option_text = "x".repeat(HARNESS_GATE_OPTION_TEXT_MAX_BYTES + 1);
        let cases = [
            PtyScreenStateV1::OperatorGate {
                gate: OperatorGateStateV1 {
                    subject: OperatorGateSubjectV1::Directory { path: Some(oversized_path) },
                    ..sample_operator_gate_state()
                },
            },
            PtyScreenStateV1::OperatorGate {
                gate: OperatorGateStateV1 {
                    options: vec![OperatorGateOptionV1 {
                        text: oversized_option_text,
                        semantics: OperatorGateOptionSemanticsV1::Accept,
                        selected: false,
                    }],
                    ..sample_operator_gate_state()
                },
            },
        ];
        for case in cases {
            let mut frame = sample_terminal_frame(1);
            frame.screen_state = Some(case.clone());
            assert!(
                matches!(frame.validate(), Err(HarnessOperatorApiError::InvalidRuntimeInventory)),
                "expected frame validate() to reject {case:?}",
            );

            let mut session = sample_runtime_session(1, 1);
            session.screen_state = Some(case.clone());
            assert!(
                matches!(session.validate(), Err(HarnessOperatorApiError::InvalidRuntimeInventory)),
                "expected session validate() to reject {case:?}",
            );
        }
    }

    #[test]
    fn operator_session_verbs_are_exact_round_trips() {
        let session = session_address(41, 3);
        let requests = vec![
            HarnessOperatorRequestV1::SpawnSession {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                provider: "claude".to_owned(),
                provider_profile: "claude-default".to_owned(),
                mode: HarnessExecutionModeV1::Pty,
                terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 40, columns: 120 },
                approval_level: None,
            },
            HarnessOperatorRequestV1::WriteSessionInput {
                session: session.clone(),
                text: "echo hi\n".to_owned(),
            },
            HarnessOperatorRequestV1::ResizeSession {
                session: session.clone(),
                terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 50, columns: 160 },
            },
            HarnessOperatorRequestV1::StopSession {
                session: session.clone(),
                force: true,
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid session verb request");
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }

        let responses = vec![
            HarnessOperatorResponseV1::SessionSpawned(session.clone()),
            HarnessOperatorResponseV1::SessionInputWritten,
            HarnessOperatorResponseV1::SessionResized,
            HarnessOperatorResponseV1::SessionStopped,
        ];
        for response in responses {
            response.validate().expect("valid V10 session verb response");
            let encoded = serde_json::to_string(&response).unwrap();
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }
    }

    /// `PromptSession`'s own round trip, kept separate from the V10 session-
    /// verb test above: it landed on the wire under a later version and
    /// carries its own PTY-refusal doctrine (see its doc comment), not just
    /// another `WriteSessionInput`-shaped verb.
    #[test]
    fn operator_prompt_session_is_exact_round_trip() {
        let session = session_address(41, 3);
        let request = HarnessOperatorRequestV1::PromptSession {
            session: session.clone(),
            text: "please continue".to_owned(),
        };
        request.validate().expect("valid prompt-session request");
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();

        let response = HarnessOperatorResponseV1::SessionPrompted;
        response.validate().expect("valid prompt-session response");
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, response);
    }

    #[test]
    fn session_spawn_and_control_requests_reject_malformed_fields() {
        let session = session_address(41, 3);
        let valid_spawn = HarnessOperatorRequestV1::SpawnSession {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: "claude".to_owned(),
            provider_profile: "claude-default".to_owned(),
            mode: HarnessExecutionModeV1::Pty,
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 40, columns: 120 },
            approval_level: None,
        };
        valid_spawn.validate().unwrap();

        let mut empty_node = valid_spawn.clone();
        if let HarnessOperatorRequestV1::SpawnSession { node_id, .. } = &mut empty_node {
            *node_id = String::new();
        }
        assert!(matches!(
            empty_node.validate(),
            Err(HarnessOperatorApiError::InvalidWorkspaceOrigin),
        ));

        let mut bad_provider = valid_spawn.clone();
        if let HarnessOperatorRequestV1::SpawnSession { provider, .. } = &mut bad_provider {
            *provider = "not a provider id!".to_owned();
        }
        assert!(matches!(
            bad_provider.validate(),
            Err(HarnessOperatorApiError::InvalidSessionSpawn),
        ));

        let mut zero_rows = valid_spawn.clone();
        if let HarnessOperatorRequestV1::SpawnSession { terminal_size, .. } = &mut zero_rows {
            terminal_size.rows = 0;
        }
        assert!(matches!(
            zero_rows.validate(),
            Err(HarnessOperatorApiError::InvalidSessionSpawn),
        ));

        let oversized_input = HarnessOperatorRequestV1::WriteSessionInput {
            session: session.clone(),
            text: "x".repeat(HARNESS_SESSION_INPUT_MAX_BYTES + 1),
        };
        assert!(matches!(
            oversized_input.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));
        let bounded_input = HarnessOperatorRequestV1::WriteSessionInput {
            session: session.clone(),
            text: "x".repeat(HARNESS_SESSION_INPUT_MAX_BYTES),
        };
        bounded_input.validate().unwrap();

        let oversized_prompt = HarnessOperatorRequestV1::PromptSession {
            session: session.clone(),
            text: "x".repeat(HARNESS_SESSION_INPUT_MAX_BYTES + 1),
        };
        assert!(matches!(
            oversized_prompt.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));
        let bounded_prompt = HarnessOperatorRequestV1::PromptSession {
            session: session.clone(),
            text: "x".repeat(HARNESS_SESSION_INPUT_MAX_BYTES),
        };
        bounded_prompt.validate().unwrap();

        let zero_resize = HarnessOperatorRequestV1::ResizeSession {
            session: session.clone(),
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 0 },
        };
        assert!(matches!(
            zero_resize.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let mut malformed_session = session.clone();
        malformed_session.instance_id = 0;
        let bad_stop = HarnessOperatorRequestV1::StopSession {
            session: malformed_session,
            force: false,
        };
        assert!(bad_stop.validate().is_err());
    }

    #[test]
    fn operator_session_control_verbs_are_exact_round_trips() {
        let session = session_address(41, 3);
        let requests = vec![
            HarnessOperatorRequestV1::ControlSession {
                session: session.clone(),
                control: HarnessTerminalControlV1::Enter,
            },
            HarnessOperatorRequestV1::WriteSessionBytes {
                session: session.clone(),
                bytes: vec![0x1b, b'[', b'A'],
            },
            HarnessOperatorRequestV1::PasteSession {
                session: session.clone(),
                text: "pasted text\n".to_owned(),
            },
            HarnessOperatorRequestV1::RemoveSession {
                session: session.clone(),
            },
            HarnessOperatorRequestV1::ResumeSession {
                session: session.clone(),
                terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid session control verb request");
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }

        let responses = vec![
            HarnessOperatorResponseV1::SessionControlled,
            HarnessOperatorResponseV1::SessionBytesWritten,
            HarnessOperatorResponseV1::SessionPasted,
            HarnessOperatorResponseV1::SessionRemoved,
            HarnessOperatorResponseV1::SessionResumed,
        ];
        for response in responses {
            response.validate().expect("valid V11 session control verb response");
            let encoded = serde_json::to_string(&response).unwrap();
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn session_control_v11_requests_reject_malformed_fields() {
        let session = session_address(41, 3);

        let oversized_bytes = HarnessOperatorRequestV1::WriteSessionBytes {
            session: session.clone(),
            bytes: vec![0u8; HARNESS_SESSION_BYTES_MAX_BYTES + 1],
        };
        assert!(matches!(
            oversized_bytes.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));
        let empty_bytes = HarnessOperatorRequestV1::WriteSessionBytes {
            session: session.clone(),
            bytes: Vec::new(),
        };
        assert!(matches!(
            empty_bytes.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));
        let bounded_bytes = HarnessOperatorRequestV1::WriteSessionBytes {
            session: session.clone(),
            bytes: vec![0u8; HARNESS_SESSION_BYTES_MAX_BYTES],
        };
        bounded_bytes.validate().unwrap();

        let oversized_paste = HarnessOperatorRequestV1::PasteSession {
            session: session.clone(),
            text: "x".repeat(HARNESS_SESSION_INPUT_MAX_BYTES + 1),
        };
        assert!(matches!(
            oversized_paste.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));
        let bounded_paste = HarnessOperatorRequestV1::PasteSession {
            session: session.clone(),
            text: "x".repeat(HARNESS_SESSION_INPUT_MAX_BYTES),
        };
        bounded_paste.validate().unwrap();

        let zero_resume_size = HarnessOperatorRequestV1::ResumeSession {
            session: session.clone(),
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 0, columns: 80 },
        };
        assert!(matches!(
            zero_resume_size.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let mut malformed_session = session.clone();
        malformed_session.instance_id = 0;
        let bad_remove = HarnessOperatorRequestV1::RemoveSession {
            session: malformed_session.clone(),
        };
        assert!(bad_remove.validate().is_err());
        let bad_control = HarnessOperatorRequestV1::ControlSession {
            session: malformed_session,
            control: HarnessTerminalControlV1::Interrupt,
        };
        assert!(bad_control.validate().is_err());
    }

    fn sample_native_session_selection() -> HarnessNativeSessionSelectionV1 {
        HarnessNativeSessionSelectionV1 {
            route: HarnessNativeSessionRouteV1 {
                node_id: "node-a".to_owned(),
                incarnation_id: "1".repeat(32),
                scope: HarnessNativeSessionCatalogScopeV1::Workspace,
                workspace_id: Some("workspace-a".to_owned()),
                provider: "codex".to_owned(),
            },
            catalog_revision: 7,
            recent_cutoff_unix_ms: 9,
            selection_id: "selection-a".to_owned(),
        }
    }

    fn sample_managed_session(record_id: &str) -> HarnessRuntimeManagedSessionV1 {
        HarnessRuntimeManagedSessionV1 {
            record_id: record_id.to_owned(),
            display_name: "Session display name".to_owned(),
            display_name_truncated: false,
            provider: "codex".to_owned(),
            mode: HarnessRuntimeManagedModeV1::Pty,
            state: HarnessRuntimeManagedStateV1::Dormant,
            workspace_id: "workspace-a".to_owned(),
            active_binding: None,
            provider_identity_present: true,
            updated_at_unix_ms: 10,
            blocked_count: 0,
            last_blocked_at_ms: None,
            unread_mail: 0,
        }
    }

    #[test]
    fn operator_session_record_family_requests_are_exact_round_trips() {
        let requests = vec![
            HarnessOperatorRequestV1::PreviewSessionRecord {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
                message_limit: HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX,
            },
            HarnessOperatorRequestV1::ResumeSessionRecord {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
                terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
                initial_prompt: Some("continue".to_owned()),
            },
            HarnessOperatorRequestV1::RenameSessionRecord {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
                display_name: "Renamed session".to_owned(),
            },
            HarnessOperatorRequestV1::SetSessionTask {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
                expected_revision: 3,
                target: HarnessSessionTaskTargetV1::Existing {
                    task_id: format!("task-{}", "a".repeat(24)),
                },
            },
            HarnessOperatorRequestV1::ForgetSessionRecord {
                node_id: "node-a".to_owned(),
                record_id: "record-a".to_owned(),
            },
            HarnessOperatorRequestV1::IndexProviderSession {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                provider: "codex".to_owned(),
                identity: HarnessProviderSessionIdentityV1 {
                    key: HarnessProviderSessionKeyV1::SessionId,
                    id: "session-123".to_owned(),
                    transcript_path: Some("/tmp/transcript.json".to_owned()),
                },
                display_name: "Indexed session".to_owned(),
            },
            HarnessOperatorRequestV1::IndexNativeSession {
                selection: sample_native_session_selection(),
                display_name: "Native indexed".to_owned(),
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid session-record family request");
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }
    }

    #[test]
    fn operator_session_record_family_responses_are_exact_round_trips() {
        let record = sample_managed_session("record-a");
        let responses = vec![
            HarnessOperatorResponseV1::SessionRecordPreviewed(HarnessSessionRecordPreviewedV1 {
                record_id: "record-a".to_owned(),
                preview: HarnessNativeSessionPreviewV1 {
                    title: Some("Preview title".to_owned()),
                    modified_at_unix_ms: Some(10),
                    model: Some("gpt".to_owned()),
                    message_count: 1,
                    message_count_exact: true,
                    completed_turn_count: Some(1),
                    total_tokens: Some(100),
                    truncated: false,
                    messages: vec![HarnessNativeSessionPreviewMessageV1 {
                        role: HarnessNativeSessionPreviewRoleV1::User,
                        text: "hello".to_owned(),
                    }],
                },
            }),
            HarnessOperatorResponseV1::SessionRecordResumed(HarnessSessionRecordResumedV1 {
                record: record.clone(),
                session: session_address(41, 3),
            }),
            HarnessOperatorResponseV1::SessionRecordUpdated(record.clone()),
            HarnessOperatorResponseV1::SessionRecordForgotten { record_id: "record-a".to_owned() },
            HarnessOperatorResponseV1::ProviderSessionIndexed(record.clone()),
            HarnessOperatorResponseV1::NativeSessionIndexed(HarnessNativeSessionIndexedV1 {
                selection: sample_native_session_selection(),
                record,
            }),
        ];
        for response in responses {
            response.validate().expect("valid session-record family response");
            let encoded = serde_json::to_string(&response).unwrap();
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn session_record_family_requests_reject_malformed_fields() {
        let bad_node_id = HarnessOperatorRequestV1::PreviewSessionRecord {
            node_id: String::new(),
            record_id: "record-a".to_owned(),
            message_limit: 1,
        };
        assert!(matches!(
            bad_node_id.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let bad_limit = HarnessOperatorRequestV1::PreviewSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            message_limit: 0,
        };
        assert!(matches!(bad_limit.validate(), Err(HarnessOperatorApiError::InvalidLimit)));

        let empty_display_name = HarnessOperatorRequestV1::RenameSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            display_name: String::new(),
        };
        assert!(matches!(
            empty_display_name.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let oversized_display_name = HarnessOperatorRequestV1::RenameSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            display_name: "x".repeat(HARNESS_SESSION_RECORD_DISPLAY_NAME_MAX_BYTES + 1),
        };
        assert!(matches!(
            oversized_display_name.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let bounded_display_name = HarnessOperatorRequestV1::RenameSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            display_name: "x".repeat(HARNESS_SESSION_RECORD_DISPLAY_NAME_MAX_BYTES),
        };
        bounded_display_name.validate().unwrap();

        let zero_resume_size = HarnessOperatorRequestV1::ResumeSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 0, columns: 80 },
            initial_prompt: None,
        };
        assert!(matches!(
            zero_resume_size.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let empty_initial_prompt = HarnessOperatorRequestV1::ResumeSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 24, columns: 80 },
            initial_prompt: Some(String::new()),
        };
        assert!(matches!(
            empty_initial_prompt.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let bad_task_id = HarnessOperatorRequestV1::SetSessionTask {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            expected_revision: 1,
            target: HarnessSessionTaskTargetV1::Existing { task_id: String::new() },
        };
        assert!(matches!(
            bad_task_id.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let valid_clear_task = HarnessOperatorRequestV1::SetSessionTask {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            expected_revision: 1,
            target: HarnessSessionTaskTargetV1::Clear,
        };
        valid_clear_task.validate().unwrap();

        let bad_identity_id = HarnessOperatorRequestV1::IndexProviderSession {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: "codex".to_owned(),
            identity: HarnessProviderSessionIdentityV1 {
                key: HarnessProviderSessionKeyV1::SessionId,
                id: String::new(),
                transcript_path: None,
            },
            display_name: "Indexed session".to_owned(),
        };
        assert!(matches!(
            bad_identity_id.validate(),
            Err(HarnessOperatorApiError::InvalidSessionRecordRequest),
        ));

        let mut malformed_selection = sample_native_session_selection();
        malformed_selection.catalog_revision = 0;
        let bad_index_native = HarnessOperatorRequestV1::IndexNativeSession {
            selection: malformed_selection.clone(),
            display_name: "Native indexed".to_owned(),
        };
        assert!(bad_index_native.validate().is_err());
        malformed_selection.catalog_revision = 7;
        HarnessOperatorRequestV1::IndexNativeSession {
            selection: malformed_selection,
            display_name: "Native indexed".to_owned(),
        }.validate().unwrap();
    }

    #[test]
    fn session_record_family_responses_reject_malformed_fields() {
        let mut malformed_record = sample_managed_session("record-a");
        malformed_record.record_id = String::new();
        assert!(
            HarnessOperatorResponseV1::SessionRecordUpdated(malformed_record.clone())
                .validate()
                .is_err()
        );
        assert!(
            HarnessOperatorResponseV1::ProviderSessionIndexed(malformed_record.clone())
                .validate()
                .is_err()
        );
        assert!(
            HarnessOperatorResponseV1::SessionRecordResumed(HarnessSessionRecordResumedV1 {
                record: malformed_record,
                session: session_address(41, 3),
            }).validate().is_err()
        );

        assert!(
            HarnessOperatorResponseV1::SessionRecordForgotten { record_id: String::new() }
                .validate()
                .is_err()
        );

        let mut malformed_session = session_address(41, 3);
        malformed_session.instance_id = 0;
        assert!(
            HarnessOperatorResponseV1::SessionRecordResumed(HarnessSessionRecordResumedV1 {
                record: sample_managed_session("record-a"),
                session: malformed_session,
            }).validate().is_err()
        );

        let mut malformed_selection = sample_native_session_selection();
        malformed_selection.selection_id = String::new();
        assert!(
            HarnessOperatorResponseV1::NativeSessionIndexed(HarnessNativeSessionIndexedV1 {
                selection: malformed_selection,
                record: sample_managed_session("record-a"),
            }).validate().is_err()
        );
    }

    fn host_path(value: &str) -> HarnessHostPathV1 {
        HarnessHostPathV1::new(value).unwrap()
    }

    fn sample_workspace_snapshot(workspace_id: &str) -> HarnessWorkspaceSnapshotV1 {
        HarnessWorkspaceSnapshotV1 {
            workspace_id: workspace_id.to_owned(),
            canonical_root: host_path(r"C:\fixtures\workspace"),
            worktree_service_mode: Some(HarnessWorktreeServiceModeV1::Manual),
        }
    }

    fn sample_git_worktree_snapshot() -> HarnessGitWorktreeSnapshotV1 {
        HarnessGitWorktreeSnapshotV1 {
            path: host_path(r"C:\fixtures\worktree"),
            head: "a".repeat(40),
            branch: Some("feature/resource-mutation".to_owned()),
            is_bare: false,
            is_main: false,
            locked: false,
            prunable: false,
            workspace_id: Some("worktree-workspace".to_owned()),
        }
    }

    #[test]
    fn operator_resource_mutation_family_requests_are_exact_round_trips() {
        let requests = vec![
            HarnessOperatorRequestV1::BrowseHostDirectories {
                node_id: "node-a".to_owned(),
                directory: Some(host_path(r"C:\fixtures")),
                after: Some(host_path(r"C:\fixtures\a")),
            },
            HarnessOperatorRequestV1::RegisterWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-b".to_owned(),
                root: host_path(r"C:\fixtures\workspace-b"),
            },
            HarnessOperatorRequestV1::UnregisterWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-b".to_owned(),
            },
            HarnessOperatorRequestV1::CreateStandaloneWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-c".to_owned(),
                root: host_path(r"C:\fixtures\workspace-c"),
                initial_branch: Some("main".to_owned()),
            },
            HarnessOperatorRequestV1::CreateWorktree {
                node_id: "node-a".to_owned(),
                source_workspace_id: "workspace-a".to_owned(),
                workspace_id: "worktree-workspace".to_owned(),
                target_root: host_path(r"C:\fixtures\worktree"),
                branch: "feature/resource-mutation".to_owned(),
                base: Some("main".to_owned()),
            },
            HarnessOperatorRequestV1::RemoveWorktree {
                node_id: "node-a".to_owned(),
                source_workspace_id: "workspace-a".to_owned(),
                target_root: host_path(r"C:\fixtures\worktree"),
            },
            HarnessOperatorRequestV1::ExportContextPack { session: session_address(41, 3) },
            HarnessOperatorRequestV1::ForgetContextPack {
                node_id: "node-a".to_owned(),
                context_id: HarnessSelectorV1::new("context-a").unwrap(),
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "b".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid resource-mutation family request");
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }
    }

    #[test]
    fn operator_resource_mutation_family_responses_are_exact_round_trips() {
        let responses = vec![
            HarnessOperatorResponseV1::HostDirectoriesBrowsed(HarnessHostDirectoryListingV1 {
                directory: Some(host_path(r"C:\fixtures")),
                parent: Some(host_path(r"C:\")),
                entries: vec![HarnessHostDirectoryEntryV1 {
                    path: host_path(r"C:\fixtures\workspace"),
                    display_name: "workspace".to_owned(),
                    is_link: false,
                }],
                next_after: Some(host_path(r"C:\fixtures\workspace")),
                incomplete: true,
            }),
            HarnessOperatorResponseV1::WorkspaceRegistered(sample_workspace_snapshot("workspace-b")),
            HarnessOperatorResponseV1::StandaloneWorkspaceCreated(
                sample_workspace_snapshot("workspace-c"),
            ),
            HarnessOperatorResponseV1::WorkspaceUnregistered {
                workspace_id: "workspace-b".to_owned(),
            },
            HarnessOperatorResponseV1::WorktreeCreated {
                worktree: sample_git_worktree_snapshot(),
                workspace: sample_workspace_snapshot("worktree-workspace"),
            },
            HarnessOperatorResponseV1::WorktreeRemoved {
                target_root: host_path(r"C:\fixtures\worktree"),
                workspace_id: Some("worktree-workspace".to_owned()),
            },
            HarnessOperatorResponseV1::ContextPackExported(HarnessResolvedContextPackReceiptV1 {
                id: HarnessSelectorV1::new("context-a").unwrap(),
                digest: "sha256:".to_owned() + &"a".repeat(64),
                lineage: HarnessContextPackLineageV1 {
                    source_node_id: HarnessSelectorV1::new("node-a").unwrap(),
                    source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                    source_instance_id: 41,
                    source_generation: 3,
                    source_provider: HarnessSelectorV1::new("codex").unwrap(),
                },
                source_message_count: 4,
                retained_message_count: 4,
                byte_len: 128,
                truncated: false,
            }),
            HarnessOperatorResponseV1::ContextPackForgotten { context_id: "context-a".to_owned() },
        ];
        for response in responses {
            response.validate().expect("valid resource-mutation family response");
            let encoded = serde_json::to_string(&response).unwrap();
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn resource_mutation_family_requests_reject_malformed_fields() {
        let bad_node_id = HarnessOperatorRequestV1::BrowseHostDirectories {
            node_id: String::new(),
            directory: None,
            after: None,
        };
        assert!(matches!(
            bad_node_id.validate(),
            Err(HarnessOperatorApiError::InvalidHostDirectoryBrowseRequest),
        ));

        assert!(matches!(
            HarnessHostPathV1::new(""),
            Err(HarnessOperatorApiError::InvalidHostPath),
        ));
        assert!(matches!(
            HarnessHostPathV1::new("x".repeat(HARNESS_HOST_PATH_MAX_BYTES + 1)),
            Err(HarnessOperatorApiError::InvalidHostPath),
        ));
        HarnessHostPathV1::new("x".repeat(HARNESS_HOST_PATH_MAX_BYTES)).unwrap();

        let bad_workspace_id = HarnessOperatorRequestV1::RegisterWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: String::new(),
            root: host_path(r"C:\fixtures\workspace-b"),
        };
        assert!(matches!(
            bad_workspace_id.validate(),
            Err(HarnessOperatorApiError::InvalidResourceMutationRequest),
        ));

        let empty_branch = HarnessOperatorRequestV1::CreateWorktree {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            workspace_id: "worktree-workspace".to_owned(),
            target_root: host_path(r"C:\fixtures\worktree"),
            branch: String::new(),
            base: None,
        };
        assert!(matches!(
            empty_branch.validate(),
            Err(HarnessOperatorApiError::InvalidResourceMutationRequest),
        ));

        let control_char_base = HarnessOperatorRequestV1::CreateWorktree {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            workspace_id: "worktree-workspace".to_owned(),
            target_root: host_path(r"C:\fixtures\worktree"),
            branch: "feature/resource-mutation".to_owned(),
            base: Some("main\u{0007}".to_owned()),
        };
        assert!(matches!(
            control_char_base.validate(),
            Err(HarnessOperatorApiError::InvalidResourceMutationRequest),
        ));

        let mut malformed_session = session_address(41, 3);
        malformed_session.instance_id = 0;
        assert!(
            HarnessOperatorRequestV1::ExportContextPack { session: malformed_session }
                .validate()
                .is_err()
        );

        let bad_context_id = HarnessOperatorRequestV1::ForgetContextPack {
            node_id: "node-a".to_owned(),
            context_id: HarnessSelectorV1::new("context-a").unwrap(),
        };
        bad_context_id.validate().unwrap();
        let empty_forget_node_id = HarnessOperatorRequestV1::ForgetContextPack {
            node_id: String::new(),
            context_id: HarnessSelectorV1::new("context-a").unwrap(),
        };
        assert!(matches!(
            empty_forget_node_id.validate(),
            Err(HarnessOperatorApiError::InvalidResourceMutationRequest),
        ));
    }

    #[test]
    fn resource_mutation_family_responses_reject_malformed_fields() {
        assert!(
            HarnessOperatorResponseV1::WorkspaceUnregistered { workspace_id: String::new() }
                .validate()
                .is_err()
        );

        let mut malformed_workspace = sample_workspace_snapshot("workspace-b");
        malformed_workspace.workspace_id = String::new();
        assert!(
            HarnessOperatorResponseV1::WorkspaceRegistered(malformed_workspace)
                .validate()
                .is_err()
        );

        let mut malformed_worktree = sample_git_worktree_snapshot();
        malformed_worktree.head = String::new();
        assert!(
            HarnessOperatorResponseV1::WorktreeCreated {
                worktree: malformed_worktree,
                workspace: sample_workspace_snapshot("worktree-workspace"),
            }.validate().is_err()
        );

        assert!(
            HarnessOperatorResponseV1::WorktreeRemoved {
                target_root: host_path(r"C:\fixtures\worktree"),
                workspace_id: Some(String::new()),
            }.validate().is_err()
        );

        assert!(
            HarnessOperatorResponseV1::ContextPackForgotten { context_id: String::new() }
                .validate()
                .is_err()
        );
    }

    fn sample_redacted_task(id_byte: char) -> RedactedTaskV1 {
        RedactedTaskV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", id_byte.to_string().repeat(24)))
                .unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "Task title".to_owned(),
            body: String::new(),
            creator: TaskCreatorCategoryV1::User,
            parent_task_id: None,
            dependency_ids: Vec::new(),
            state: HarnessTaskStateV1::Backlog,
            run_ids: Vec::new(),
            references_redacted: false,
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        }
    }

    fn sample_redacted_run(id_byte: char) -> RedactedRunV1 {
        RedactedRunV1 {
            run_id: HarnessRunId::new(format!("hrun_{}", id_byte.to_string().repeat(24)))
                .unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: None,
            operation_id: None,
            intent: RedactedRunIntentV1 {
                mode: HarnessExecutionModeV1::Pty,
                worktree: RedactedWorktreeIntentV1::Existing,
                has_delivery_bundle: false,
                has_continuation: false,
            },
            lifecycle: HarnessRunLifecycleV1::Requested,
            binding: RedactedBindingStateV1::None,
            result_disposition: None,
            failure_category: None,
            context_pack: None,
            git_facts: None,
            references_redacted: false,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        }
    }

    fn simple_node_inventory(node_id: &str) -> HarnessRuntimeNodeInventoryV1 {
        HarnessRuntimeNodeInventoryV1 {
            node_id: node_id.to_owned(),
            incarnation_id: "07".repeat(16),
            observed_at_unix_ms: 1_000,
            event_sequence: 1,
            inventory: HarnessRuntimeInventoryV1 {
                enabled_providers: Vec::new(),
                workspaces: BTreeMap::new(),
                workspace_count: 0,
                workspaces_truncated: false,
                session_count: 0,
                sessions_truncated: false,
                managed_sessions: Vec::new(),
                managed_session_count: 0,
                managed_sessions_truncated: false,
                retired_count: 0,
                launch_inventory: None,
            },
        }
    }

    #[test]
    fn operator_subscribe_events_is_exact_round_trip() {
        let request = HarnessOperatorRequestV1::SubscribeEvents {};
        request.validate().expect("valid subscribe request");
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);

        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();
    }

    #[test]
    fn operator_subscribe_terminal_is_exact_round_trip() {
        let request = HarnessOperatorRequestV1::SubscribeTerminal {
            sessions: vec![session_address(1, 1), session_address(2, 1)],
        };
        request.validate().expect("valid subscribe-terminal request");
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);

        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();
    }

    /// The single accepted build stamp: a foreign stamp is rejected, and the
    /// error names both the expected and the received stamp -- the
    /// diagnostic a silent binary-skew incident previously had neither of.
    #[test]
    fn envelope_with_a_foreign_build_stamp_is_rejected_naming_both_values() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let foreign_stamp = "f".repeat(40);
        let envelope = HarnessOperatorEnvelopeV1 {
            build_stamp: foreign_stamp.clone(),
            credential,
            request: HarnessOperatorRequestV1::SubscribeEvents {},
        };
        let error = envelope
            .validate()
            .expect_err("a foreign build stamp must be rejected");
        assert!(matches!(
            &error,
            HarnessOperatorApiError::BuildStampMismatch { expected, received }
                if expected == BUILD_STAMP && received == &foreign_stamp,
        ));
        let message = error.to_string();
        assert!(message.contains(BUILD_STAMP));
        assert!(message.contains(&foreign_stamp));
    }

    #[test]
    fn subscribe_terminal_rejects_empty_oversized_and_duplicate_session_lists() {
        assert!(matches!(
            HarnessOperatorRequestV1::SubscribeTerminal { sessions: Vec::new() }.validate(),
            Err(HarnessOperatorApiError::InvalidTerminalPage),
        ));

        let oversized = (0..=HARNESS_TERMINAL_SUBSCRIPTION_SESSIONS_MAX as u64)
            .map(|instance_id| session_address(instance_id + 1, 1))
            .collect::<Vec<_>>();
        assert!(matches!(
            HarnessOperatorRequestV1::SubscribeTerminal { sessions: oversized }.validate(),
            Err(HarnessOperatorApiError::InvalidTerminalPage),
        ));

        let duplicated = vec![session_address(1, 1), session_address(1, 1)];
        assert!(matches!(
            HarnessOperatorRequestV1::SubscribeTerminal { sessions: duplicated }.validate(),
            Err(HarnessOperatorApiError::InvalidTerminalPage),
        ));

        let mut malformed = session_address(1, 1);
        malformed.node_id = String::new();
        assert!(HarnessOperatorRequestV1::SubscribeTerminal { sessions: vec![malformed] }
            .validate()
            .is_err());
    }

    #[test]
    fn operator_event_variants_are_exact_round_trips() {
        let task = sample_redacted_task('a');
        let run = sample_redacted_run('b');
        let node = simple_node_inventory("node-a");
        let events = vec![
            HarnessOperatorEventV1::SnapshotBaseline {
                sequence: 0,
                tasks: vec![task.clone()],
                runs: vec![run.clone()],
                nodes: vec![node.clone()],
            },
            HarnessOperatorEventV1::TaskChanged { sequence: 1, task: task.clone() },
            HarnessOperatorEventV1::RunChanged { sequence: 2, run: run.clone() },
            HarnessOperatorEventV1::RuntimeInventoryChanged { sequence: 3, node: node.clone() },
            HarnessOperatorEventV1::RuntimeInventoryRemoved {
                sequence: 4,
                node_id: "node-a".to_owned(),
            },
            HarnessOperatorEventV1::Lagged { sequence: 5 },
        ];
        for event in events {
            event.validate().expect("valid operator event");
            let encoded = serde_json::to_string(&event).unwrap();
            let decoded: HarnessOperatorEventV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn operator_terminal_event_variants_are_exact_round_trips() {
        let events = vec![
            HarnessOperatorTerminalEventV1::TerminalFrame {
                sequence: 0,
                session: session_address(1, 1),
                frame: sample_terminal_frame(1),
                coalesced_since_last: 0,
            },
            HarnessOperatorTerminalEventV1::TerminalFrame {
                sequence: 1,
                session: session_address(1, 1),
                frame: sample_terminal_frame(2),
                coalesced_since_last: 3,
            },
            HarnessOperatorTerminalEventV1::Ping { sequence: 2 },
        ];
        for event in events {
            event.validate().expect("valid operator terminal event");
            let encoded = serde_json::to_string(&event).unwrap();
            let decoded: HarnessOperatorTerminalEventV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn operator_terminal_event_validate_rejects_a_malformed_frame_or_session() {
        let mut bad_session = session_address(1, 1);
        bad_session.node_id = String::new();
        let bad_session_event = HarnessOperatorTerminalEventV1::TerminalFrame {
            sequence: 0,
            session: bad_session,
            frame: sample_terminal_frame(1),
            coalesced_since_last: 0,
        };
        assert!(matches!(
            bad_session_event.validate(),
            Err(HarnessOperatorApiError::InvalidTerminalPage),
        ));

        let mut zero_size_frame = sample_terminal_frame(1);
        zero_size_frame.size.rows = 0;
        let bad_frame_event = HarnessOperatorTerminalEventV1::TerminalFrame {
            sequence: 0,
            session: session_address(1, 1),
            frame: zero_size_frame,
            coalesced_since_last: 0,
        };
        assert!(matches!(
            bad_frame_event.validate(),
            Err(HarnessOperatorApiError::InvalidTerminalPage),
        ));
    }

    #[test]
    fn operator_event_validate_rejects_malformed_frames() {
        let task_a = sample_redacted_task('a');
        let task_b = sample_redacted_task('b');
        let out_of_order = HarnessOperatorEventV1::SnapshotBaseline {
            sequence: 0,
            tasks: vec![task_b, task_a],
            runs: Vec::new(),
            nodes: Vec::new(),
        };
        assert!(out_of_order.validate().is_err());

        let bad_node_id = HarnessOperatorEventV1::RuntimeInventoryRemoved {
            sequence: 0,
            node_id: String::new(),
        };
        assert!(matches!(
            bad_node_id.validate(),
            Err(HarnessOperatorApiError::InvalidRuntimeInventory),
        ));

        let mut malformed_task = sample_redacted_task('c');
        malformed_task.title = "\0".to_owned();
        let bad_task = HarnessOperatorEventV1::TaskChanged { sequence: 0, task: malformed_task };
        assert!(bad_task.validate().is_err());
    }

    /// `HarnessOperatorHostErrorV1::Unsupported` (added alongside
    /// `gate4agent-harness-light`, see its own doc comment): a purely
    /// additive unit variant, so this only needs to prove it round-trips --
    /// on its own, and wrapped in the `HarnessOperatorReplyV1::Error` shape
    /// every operator host error actually rides on the wire.
    #[test]
    fn operator_host_error_unsupported_round_trips() {
        let error = HarnessOperatorHostErrorV1::Unsupported;
        let encoded = serde_json::to_string(&error).unwrap();
        assert_eq!(encoded, "\"unsupported\"");
        assert_eq!(serde_json::from_str::<HarnessOperatorHostErrorV1>(&encoded).unwrap(), error);

        let reply = HarnessOperatorReplyV1::Error { error };
        assert!(reply.validate().is_ok());
        let encoded_reply = serde_json::to_vec(&reply).unwrap();
        let decoded_reply: HarnessOperatorReplyV1 = serde_json::from_slice(&encoded_reply).unwrap();
        assert_eq!(decoded_reply, reply);
    }

    #[test]
    fn operator_acp_control_verbs_are_exact_round_trips() {
        let session = session_address(7, 2);
        let requests = vec![
            HarnessOperatorRequestV1::ResolveInteraction {
                session: session.clone(),
                correlation_id: "a".repeat(HARNESS_OBSERVATION_LABEL_MAX_BYTES),
                response: HarnessProviderInteractionResponseV1::ApproveOnce,
            },
            HarnessOperatorRequestV1::ResolveInteraction {
                session: session.clone(),
                correlation_id: "correlation-1".to_owned(),
                response: HarnessProviderInteractionResponseV1::Deny,
            },
            HarnessOperatorRequestV1::ResolveInteraction {
                session: session.clone(),
                correlation_id: "correlation-2".to_owned(),
                response: HarnessProviderInteractionResponseV1::Answer {
                    text: "yes, proceed".to_owned(),
                },
            },
            HarnessOperatorRequestV1::SetSessionMode {
                session: session.clone(),
                mode_id: "plan".to_owned(),
            },
            HarnessOperatorRequestV1::SetSessionConfigOption {
                session: session.clone(),
                option_id: "reasoning-effort".to_owned(),
                value_json: "\"high\"".to_owned(),
            },
            HarnessOperatorRequestV1::SetSessionModel {
                session: session.clone(),
                model_id: "grok-4".to_owned(),
            },
        ];
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        for request in requests {
            request.validate().expect("valid ACP control verb request");
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            HarnessOperatorEnvelopeV1 {
                build_stamp: BUILD_STAMP.to_string(),
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }

        let responses = vec![
            HarnessOperatorResponseV1::InteractionResolved,
            HarnessOperatorResponseV1::SessionModeSet,
            HarnessOperatorResponseV1::SessionConfigOptionSet,
            HarnessOperatorResponseV1::SessionModelSet,
        ];
        for response in responses {
            response.validate().expect("valid ACP control verb response");
            let encoded = serde_json::to_string(&response).unwrap();
            let decoded: HarnessOperatorResponseV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, response);
        }
    }

    #[test]
    fn acp_control_requests_reject_malformed_fields() {
        let session = session_address(7, 2);

        let empty_correlation = HarnessOperatorRequestV1::ResolveInteraction {
            session: session.clone(),
            correlation_id: String::new(),
            response: HarnessProviderInteractionResponseV1::ApproveOnce,
        };
        assert!(matches!(
            empty_correlation.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let oversized_correlation = HarnessOperatorRequestV1::ResolveInteraction {
            session: session.clone(),
            correlation_id: "a".repeat(HARNESS_OBSERVATION_LABEL_MAX_BYTES + 1),
            response: HarnessProviderInteractionResponseV1::Deny,
        };
        assert!(matches!(
            oversized_correlation.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let oversized_answer = HarnessOperatorRequestV1::ResolveInteraction {
            session: session.clone(),
            correlation_id: "correlation-1".to_owned(),
            response: HarnessProviderInteractionResponseV1::Answer {
                text: "x".repeat(HARNESS_ACP_INTERACTION_RESPONSE_MAX_BYTES + 1),
            },
        };
        assert!(matches!(
            oversized_answer.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let empty_mode_id = HarnessOperatorRequestV1::SetSessionMode {
            session: session.clone(),
            mode_id: String::new(),
        };
        assert!(matches!(
            empty_mode_id.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let control_char_option_id = HarnessOperatorRequestV1::SetSessionConfigOption {
            session: session.clone(),
            option_id: "bad\u{0007}id".to_owned(),
            value_json: "true".to_owned(),
        };
        assert!(matches!(
            control_char_option_id.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let empty_value_json = HarnessOperatorRequestV1::SetSessionConfigOption {
            session: session.clone(),
            option_id: "reasoning-effort".to_owned(),
            value_json: String::new(),
        };
        assert!(matches!(
            empty_value_json.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let oversized_model_id = HarnessOperatorRequestV1::SetSessionModel {
            session: session.clone(),
            model_id: "x".repeat(HARNESS_AGENT_STREAM_ID_MAX_BYTES + 1),
        };
        assert!(matches!(
            oversized_model_id.validate(),
            Err(HarnessOperatorApiError::InvalidSessionControl),
        ));

        let mut malformed_session = session.clone();
        malformed_session.instance_id = 0;
        let bad_session = HarnessOperatorRequestV1::SetSessionModel {
            session: malformed_session,
            model_id: "grok-4".to_owned(),
        };
        assert!(bad_session.validate().is_err());
    }

    #[test]
    fn operator_subscribe_agent_stream_is_exact_round_trip() {
        let request = HarnessOperatorRequestV1::SubscribeAgentStream {
            sessions: vec![session_address(1, 1), session_address(2, 1)],
        };
        request.validate().expect("valid subscribe-agent-stream request");
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);

        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            build_stamp: BUILD_STAMP.to_string(),
            credential,
            request,
        }.validate().unwrap();
    }

    #[test]
    fn subscribe_agent_stream_rejects_empty_oversized_and_duplicate_session_lists() {
        assert!(matches!(
            HarnessOperatorRequestV1::SubscribeAgentStream { sessions: Vec::new() }.validate(),
            Err(HarnessOperatorApiError::InvalidAgentStream),
        ));

        let oversized = (0..=HARNESS_AGENT_STREAM_SUBSCRIPTION_SESSIONS_MAX as u64)
            .map(|instance_id| session_address(instance_id + 1, 1))
            .collect::<Vec<_>>();
        assert!(matches!(
            HarnessOperatorRequestV1::SubscribeAgentStream { sessions: oversized }.validate(),
            Err(HarnessOperatorApiError::InvalidAgentStream),
        ));

        let duplicated = vec![session_address(1, 1), session_address(1, 1)];
        assert!(matches!(
            HarnessOperatorRequestV1::SubscribeAgentStream { sessions: duplicated }.validate(),
            Err(HarnessOperatorApiError::InvalidAgentStream),
        ));

        let mut malformed = session_address(1, 1);
        malformed.node_id = String::new();
        assert!(HarnessOperatorRequestV1::SubscribeAgentStream { sessions: vec![malformed] }
            .validate()
            .is_err());
    }

    #[test]
    fn operator_agent_event_variants_are_exact_round_trips() {
        let session = session_address(1, 1);
        let chunks = vec![
            HarnessAgentStreamChunkKindV1::Text { text: "hello".to_owned(), is_delta: false },
            HarnessAgentStreamChunkKindV1::Thinking { text: "considering options".to_owned() },
            HarnessAgentStreamChunkKindV1::InteractionPrompt {
                correlation_id: "correlation-1".to_owned(),
                interaction_kind: HarnessProviderInteractionKindV1::Approval,
                tool_name: "execute".to_owned(),
                title: Some("Run command?".to_owned()),
                prompt: "rm -rf /tmp/scratch".to_owned(),
                options: vec![HarnessAgentStreamInteractionOptionV1 {
                    option_id: "allow".to_owned(),
                    name: "Allow".to_owned(),
                    kind: "allow_once".to_owned(),
                }],
            },
            HarnessAgentStreamChunkKindV1::ModeCatalog {
                current: Some("plan".to_owned()),
                available: vec![HarnessAgentStreamNamedIdV1 {
                    id: "plan".to_owned(),
                    name: "Plan".to_owned(),
                    description: None,
                }],
            },
            HarnessAgentStreamChunkKindV1::ConfigOptions {
                options: vec![HarnessProviderConfigOptionV1 {
                    id: "reasoning-effort".to_owned(),
                    name: "Reasoning effort".to_owned(),
                    description: Some("How hard to think".to_owned()),
                    category: Some("model".to_owned()),
                    kind: HarnessProviderConfigOptionKindV1::Select,
                    value_json: "\"high\"".to_owned(),
                    choices: vec![HarnessProviderConfigChoiceV1 {
                        value_json: "\"high\"".to_owned(),
                        label: Some("High".to_owned()),
                    }],
                }],
            },
            HarnessAgentStreamChunkKindV1::ModelCatalog {
                current: Some("grok-4".to_owned()),
                available: vec![HarnessAgentStreamNamedIdV1 {
                    id: "grok-4".to_owned(),
                    name: "Grok 4".to_owned(),
                    description: None,
                }],
            },
        ];

        let mut events: Vec<HarnessOperatorAgentEventV1> = chunks
            .into_iter()
            .enumerate()
            .map(|(index, kind)| HarnessOperatorAgentEventV1::AgentChunk {
                sequence: index as u64,
                session: session.clone(),
                chunk: HarnessAgentStreamChunkV1 { source_sequence: index as u64, kind },
                published_at_ms: 1_000 + index as u64,
            })
            .collect();
        events.push(HarnessOperatorAgentEventV1::Lagged {
            sequence: 100,
            session: session.clone(),
            dropped: 3,
        });
        events.push(HarnessOperatorAgentEventV1::ReplayBoundary {
            session: session.clone(),
            replayed: 6,
            dropped_before_replay: 44,
        });
        events.push(HarnessOperatorAgentEventV1::Ping { sequence: 101 });

        for event in events {
            event.validate().expect("valid operator agent event");
            let encoded = serde_json::to_string(&event).unwrap();
            let decoded: HarnessOperatorAgentEventV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, event);
        }
    }

    /// D3/Slice C: `HarnessMailFetchResultV1` round-trips for all three
    /// outcomes, and `Refused`'s reason string is bounded the same way
    /// `HarnessMailSendResultV1::Refused`'s already is.
    #[test]
    fn mail_fetch_result_round_trips_for_run_result_and_refused() {
        let run_result = HarnessMailFetchResultV1::Run {
            monitor: monitor_with_mixed_capabilities(),
            timeline: TimelinePageV1 {
                run_id: HarnessRunId::new("hrun_000000000000000000000001").unwrap(),
                availability: ProjectionAvailabilityV1::Current,
                freshness: ProjectionFreshnessV1::Live,
                transport_incomplete: false,
                entries: Vec::new(),
                next_cursor: None,
            },
        };
        run_result.validate().unwrap();
        let encoded = serde_json::to_string(&run_result).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), run_result);

        let result_outcome = HarnessMailFetchResultV1::Result { run: sample_redacted_run('2') };
        result_outcome.validate().unwrap();
        let encoded = serde_json::to_string(&result_outcome).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), result_outcome);

        let refused = HarnessMailFetchResultV1::Refused {
            reason: "mail message hmail_000000000000000000000001 is not addressed to this reader".to_owned(),
        };
        refused.validate().unwrap();
        let encoded = serde_json::to_string(&refused).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), refused);

        let empty_reason = HarnessMailFetchResultV1::Refused { reason: String::new() };
        assert!(matches!(empty_reason.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let oversized_reason = HarnessMailFetchResultV1::Refused {
            reason: "x".repeat(HARNESS_MAIL_REFUSAL_REASON_MAX_BYTES + 1),
        };
        assert!(matches!(oversized_reason.validate(), Err(HarnessReadApiError::InvalidMailFetch)));
    }

    /// D3/Slice C2: `HarnessMailFetchResultV1::ContextPack` round-trips, and
    /// `bytes_hex` must be exactly `byte_len * 2` lower-hex bytes -- a short
    /// or long `bytes_hex` (declared length lying about the payload) is
    /// `InvalidMailFetch`, the same shape every other length-declaring wire
    /// value in this crate is checked.
    #[test]
    fn mail_fetch_result_context_pack_round_trips_and_bounds_bytes_hex_to_byte_len() {
        let context_pack = HarnessMailFetchResultV1::ContextPack {
            digest: format!("sha256:{}", "a".repeat(64)),
            id: "ctx-deadbeef".to_owned(),
            byte_len: 3,
            bytes_hex: "0a1b2c".to_owned(),
        };
        context_pack.validate().unwrap();
        let encoded = serde_json::to_string(&context_pack).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), context_pack);

        let short_bytes_hex = HarnessMailFetchResultV1::ContextPack {
            digest: format!("sha256:{}", "a".repeat(64)),
            id: "ctx-deadbeef".to_owned(),
            byte_len: 3,
            bytes_hex: "0a1b".to_owned(),
        };
        assert!(matches!(short_bytes_hex.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let long_bytes_hex = HarnessMailFetchResultV1::ContextPack {
            digest: format!("sha256:{}", "a".repeat(64)),
            id: "ctx-deadbeef".to_owned(),
            byte_len: 3,
            bytes_hex: "0a1b2c3d".to_owned(),
        };
        assert!(matches!(long_bytes_hex.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let zero_byte_len = HarnessMailFetchResultV1::ContextPack {
            digest: format!("sha256:{}", "a".repeat(64)),
            id: "ctx-deadbeef".to_owned(),
            byte_len: 0,
            bytes_hex: String::new(),
        };
        assert!(matches!(zero_byte_len.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let bad_digest = HarnessMailFetchResultV1::ContextPack {
            digest: "not-a-digest".to_owned(),
            id: "ctx-deadbeef".to_owned(),
            byte_len: 3,
            bytes_hex: "0a1b2c".to_owned(),
        };
        assert!(matches!(bad_digest.validate(), Err(HarnessReadApiError::InvalidMailFetch)));
    }

    /// D3/Slice C2: the two named `ContextPack`-fetch refusals round-trip and
    /// bound their own inputs to `SpawnContextDigest`'s own `sha256:<64 hex>`
    /// rendering -- unlike `WorkspacePath`'s sibling refusals, whose digests
    /// are bare (no `sha256:` prefix).
    #[test]
    fn mail_fetch_context_pack_refusals_round_trip_and_bound_their_inputs() {
        let unknown = HarnessMailFetchResultV1::MailRefContextPackUnknown {
            digest: format!("sha256:{}", "a".repeat(64)),
        };
        unknown.validate().unwrap();
        let encoded = serde_json::to_string(&unknown).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), unknown);

        let bare_digest = HarnessMailFetchResultV1::MailRefContextPackUnknown {
            digest: "a".repeat(64),
        };
        assert!(matches!(bare_digest.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let digest_mismatch = HarnessMailFetchResultV1::MailedContextPackDigestMismatch {
            expected: format!("sha256:{}", "a".repeat(64)),
            recomputed: format!("sha256:{}", "b".repeat(64)),
        };
        digest_mismatch.validate().unwrap();
        let encoded = serde_json::to_string(&digest_mismatch).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), digest_mismatch);

        let bare_mismatch = HarnessMailFetchResultV1::MailedContextPackDigestMismatch {
            expected: "a".repeat(64),
            recomputed: format!("sha256:{}", "b".repeat(64)),
        };
        assert!(matches!(bare_mismatch.validate(), Err(HarnessReadApiError::InvalidMailFetch)));
    }

    /// D3/Slice C3: `HarnessMailFetchResultV1::WorkspacePath` round-trips,
    /// `text.len()` must equal `byte_len` exactly (a lying declared length is
    /// `InvalidMailFetch`, same shape as `ContextPack`'s `bytes_hex` bound),
    /// and `sha256` must be a bare (unprefixed) 64-lowercase-hex digest --
    /// unlike `ContextPack.digest`, which carries the `sha256:` prefix.
    #[test]
    fn mail_fetch_result_workspace_path_round_trips_and_bounds_text_to_byte_len() {
        let workspace_path = HarnessMailFetchResultV1::WorkspacePath {
            workspace: "primary".to_owned(),
            path: "reports/handoff.md".to_owned(),
            byte_len: 5,
            sha256: "a".repeat(64),
            text: "hello".to_owned(),
        };
        workspace_path.validate().unwrap();
        let encoded = serde_json::to_string(&workspace_path).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), workspace_path);

        let short_byte_len = HarnessMailFetchResultV1::WorkspacePath {
            workspace: "primary".to_owned(),
            path: "reports/handoff.md".to_owned(),
            byte_len: 4,
            sha256: "a".repeat(64),
            text: "hello".to_owned(),
        };
        assert!(matches!(short_byte_len.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let prefixed_digest = HarnessMailFetchResultV1::WorkspacePath {
            workspace: "primary".to_owned(),
            path: "reports/handoff.md".to_owned(),
            byte_len: 5,
            sha256: format!("sha256:{}", "a".repeat(64)),
            text: "hello".to_owned(),
        };
        assert!(matches!(prefixed_digest.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let empty_path = HarnessMailFetchResultV1::WorkspacePath {
            workspace: "primary".to_owned(),
            path: String::new(),
            byte_len: 5,
            sha256: "a".repeat(64),
            text: "hello".to_owned(),
        };
        assert!(matches!(empty_path.validate(), Err(HarnessReadApiError::InvalidMailFetch)));
    }

    /// D3/Slice C3: the five named `WorkspacePath`-fetch refusals round-trip
    /// and bound their own inputs -- `MailRefWorkspaceAmbiguous.nodes` must
    /// name at least two nodes (one node is not an ambiguity) and never more
    /// than `HARNESS_MAIL_FETCH_AMBIGUOUS_NODES_MAX`.
    #[test]
    fn mail_fetch_workspace_path_refusals_round_trip_and_bound_their_inputs() {
        let unknown = HarnessMailFetchResultV1::MailRefWorkspaceUnknown { workspace: "primary".to_owned() };
        unknown.validate().unwrap();
        let encoded = serde_json::to_string(&unknown).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), unknown);

        let ambiguous = HarnessMailFetchResultV1::MailRefWorkspaceAmbiguous {
            workspace: "primary".to_owned(),
            nodes: vec!["node-a".to_owned(), "node-b".to_owned()],
        };
        ambiguous.validate().unwrap();
        let encoded = serde_json::to_string(&ambiguous).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), ambiguous);

        let one_node = HarnessMailFetchResultV1::MailRefWorkspaceAmbiguous {
            workspace: "primary".to_owned(),
            nodes: vec!["node-a".to_owned()],
        };
        assert!(matches!(one_node.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let too_many_nodes = HarnessMailFetchResultV1::MailRefWorkspaceAmbiguous {
            workspace: "primary".to_owned(),
            nodes: (0..HARNESS_MAIL_FETCH_AMBIGUOUS_NODES_MAX + 1)
                .map(|index| format!("node-{index}"))
                .collect(),
        };
        assert!(matches!(too_many_nodes.validate(), Err(HarnessReadApiError::InvalidMailFetch)));

        let path_unknown = HarnessMailFetchResultV1::MailRefWorkspacePathUnknown {
            workspace: "primary".to_owned(),
            path: "reports/handoff.md".to_owned(),
        };
        path_unknown.validate().unwrap();
        let encoded = serde_json::to_string(&path_unknown).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), path_unknown);

        let not_fetchable = HarnessMailFetchResultV1::MailedWorkspacePathContentNotFetchable {
            workspace: "primary".to_owned(),
            path: "reports/handoff.md".to_owned(),
            why: "file exceeds the 262144-byte limit".to_owned(),
        };
        not_fetchable.validate().unwrap();
        let encoded = serde_json::to_string(&not_fetchable).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), not_fetchable);

        let digest_mismatch = HarnessMailFetchResultV1::MailedWorkspacePathDigestMismatch {
            expected: "a".repeat(64),
            recomputed: "b".repeat(64),
        };
        digest_mismatch.validate().unwrap();
        let encoded = serde_json::to_string(&digest_mismatch).unwrap();
        assert_eq!(serde_json::from_str::<HarnessMailFetchResultV1>(&encoded).unwrap(), digest_mismatch);

        let prefixed_mismatch = HarnessMailFetchResultV1::MailedWorkspacePathDigestMismatch {
            expected: format!("sha256:{}", "a".repeat(64)),
            recomputed: "b".repeat(64),
        };
        assert!(matches!(prefixed_mismatch.validate(), Err(HarnessReadApiError::InvalidMailFetch)));
    }

    /// C1: `HarnessMailInboxEntryV1` written before `refs` existed
    /// deserializes with an empty ref list.
    #[test]
    fn mail_inbox_entry_old_shape_without_refs_deserializes() {
        let old_shape = serde_json::json!({
            "message_id": format!("hmail_{}", "1".repeat(24)),
            "from": { "record_id": "record-a", "provider": "claude" },
            "to": { "kind": "session", "record_id": "record-b" },
            "subject": "status",
            "body": "handing off the result",
            "reply_to": null,
            "task_id": null,
            "created_at_unix_ms": 1_000,
            "acked": false,
            "thread": [],
        });
        let decoded: HarnessMailInboxEntryV1 = serde_json::from_value(old_shape).unwrap();
        assert!(decoded.refs.is_empty());
        decoded.validate().unwrap();
    }

    /// D3/Slice C: `MailSend` accepts `refs`, bounded by
    /// `HARNESS_MAIL_REFS_MAX`, and an old-shape request (no `refs` key)
    /// still validates.
    #[test]
    fn mail_send_request_accepts_bounded_refs_and_old_shape_defaults_empty() {
        let run_id = HarnessRunId::new("hrun_000000000000000000000001").unwrap();
        let request = HarnessReadRequestV1::MailSend {
            to: HarnessMailAddressV1::Session { record_id: HarnessSelectorV1::new("record-b").unwrap() },
            subject: "status".to_owned(),
            body: "handing off the result".to_owned(),
            reply_to: None,
            refs: vec![HarnessMailRefV1::Run { run_id }],
        };
        request.validate().unwrap();

        let old_shape = serde_json::json!({
            "kind": "mail-send",
            "to": { "kind": "session", "record_id": "record-b" },
            "subject": "status",
            "body": "handing off the result",
            "reply_to": null,
        });
        let decoded: HarnessReadRequestV1 = serde_json::from_value(old_shape).unwrap();
        match &decoded {
            HarnessReadRequestV1::MailSend { refs, .. } => assert!(refs.is_empty()),
            other => panic!("expected MailSend, got {other:?}"),
        }
        decoded.validate().unwrap();
    }

    /// `TasksList`/`RunsList` gain a parent filter ("what did this task/run
    /// spawn") additive over the pre-existing shape, the same
    /// `#[serde(default)]` discipline `MailSend`'s own `refs` field
    /// established above: an old-shape request with no `parent_task_id`/
    /// `parent_run_id` key still decodes (defaulting to `None`), and a set
    /// value round-trips. A malformed id can never reach `parent_task_id`/
    /// `parent_run_id` in the first place -- `HarnessTaskId::new`/
    /// `HarnessRunId::new` are the validation, refusing a malformed string
    /// before a request carrying one could ever be built (the CLI's own
    /// rejection of a malformed `--parent`/`--parent-run` flag, tested in
    /// `gate4agent-harnessctl`, goes through this exact constructor).
    #[test]
    fn parent_filters_default_on_old_shape_and_round_trip() {
        let task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();
        let tasks_request = HarnessReadRequestV1::TasksList {
            after_task_id: None,
            state: None,
            parent_task_id: Some(task_id.clone()),
            limit: 10,
        };
        tasks_request.validate().unwrap();
        let encoded_tasks = serde_json::to_string(&tasks_request).unwrap();
        assert_eq!(
            serde_json::from_str::<HarnessReadRequestV1>(&encoded_tasks).unwrap(),
            tasks_request,
        );

        let old_shape_tasks = serde_json::json!({
            "kind": "tasks-list",
            "after_task_id": null,
            "state": null,
            "limit": 10,
        });
        let decoded_tasks: HarnessReadRequestV1 = serde_json::from_value(old_shape_tasks).unwrap();
        match &decoded_tasks {
            HarnessReadRequestV1::TasksList { parent_task_id, .. } => assert!(parent_task_id.is_none()),
            other => panic!("expected TasksList, got {other:?}"),
        }
        decoded_tasks.validate().unwrap();
        assert!(HarnessTaskId::new("not-a-task-id").is_err());

        let run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let runs_request = HarnessReadRequestV1::RunsList {
            task_id: None,
            after_run_id: None,
            lifecycle: None,
            parent_run_id: Some(run_id.clone()),
            limit: 10,
        };
        runs_request.validate().unwrap();
        let encoded_runs = serde_json::to_string(&runs_request).unwrap();
        assert_eq!(
            serde_json::from_str::<HarnessReadRequestV1>(&encoded_runs).unwrap(),
            runs_request,
        );

        let old_shape_runs = serde_json::json!({
            "kind": "runs-list",
            "task_id": null,
            "after_run_id": null,
            "lifecycle": null,
            "limit": 10,
        });
        let decoded_runs: HarnessReadRequestV1 = serde_json::from_value(old_shape_runs).unwrap();
        match &decoded_runs {
            HarnessReadRequestV1::RunsList { parent_run_id, .. } => assert!(parent_run_id.is_none()),
            other => panic!("expected RunsList, got {other:?}"),
        }
        decoded_runs.validate().unwrap();
        assert!(HarnessRunId::new("not-a-run-id").is_err());
    }

    #[test]
    fn operator_agent_event_validate_rejects_malformed_chunks_or_sessions() {
        let session = session_address(1, 1);

        let mut bad_session = session.clone();
        bad_session.node_id = String::new();
        let bad_session_event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence: 0,
            session: bad_session,
            chunk: sample_agent_stream_chunk(0),
            published_at_ms: 1_000,
        };
        assert!(bad_session_event.validate().is_err());

        let oversized_text_chunk = HarnessAgentStreamChunkV1 {
            source_sequence: 0,
            kind: HarnessAgentStreamChunkKindV1::Text {
                text: "x".repeat(HARNESS_AGENT_STREAM_TEXT_MAX_BYTES + 1),
                is_delta: false,
            },
        };
        let bad_chunk_event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence: 0,
            session: session.clone(),
            chunk: oversized_text_chunk,
            published_at_ms: 1_000,
        };
        assert!(matches!(
            bad_chunk_event.validate(),
            Err(HarnessOperatorApiError::InvalidAgentStream),
        ));

        let empty_correlation_prompt = HarnessAgentStreamChunkV1 {
            source_sequence: 0,
            kind: HarnessAgentStreamChunkKindV1::InteractionPrompt {
                correlation_id: String::new(),
                interaction_kind: HarnessProviderInteractionKindV1::Question,
                tool_name: "ask".to_owned(),
                title: None,
                prompt: "What next?".to_owned(),
                options: Vec::new(),
            },
        };
        let bad_prompt_event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence: 0,
            session: session.clone(),
            chunk: empty_correlation_prompt,
            published_at_ms: 1_000,
        };
        assert!(matches!(
            bad_prompt_event.validate(),
            Err(HarnessOperatorApiError::InvalidAgentStream),
        ));
    }

    /// D5, Slice D: `g4a_task_create`'s wire request round-trips and bounds
    /// title/body at this coarse wire-level ceiling -- the authoritative
    /// rule (trim/control-character checks) is the engine's own
    /// `HarnessTaskV1::validate`, reached at apply time, not this validate().
    #[test]
    fn task_create_request_round_trips_and_bounds_title_and_body() {
        let request = HarnessReadRequestV1::TaskCreate {
            title: "Investigate the flaky retry".to_owned(),
            body: "See the timeline for the failing run.".to_owned(),
            parent_task_id: Some(HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap()),
        };
        request.validate().unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<HarnessReadRequestV1>(&encoded).unwrap(), request);

        let no_parent = HarnessReadRequestV1::TaskCreate {
            title: "Child under my own task".to_owned(),
            body: String::new(),
            parent_task_id: None,
        };
        no_parent.validate().unwrap();

        let empty_title = HarnessReadRequestV1::TaskCreate {
            title: String::new(),
            body: String::new(),
            parent_task_id: None,
        };
        assert!(matches!(empty_title.validate(), Err(HarnessReadApiError::InvalidTaskCreate)));

        let oversized_title = HarnessReadRequestV1::TaskCreate {
            title: "x".repeat(HARNESS_TITLE_MAX_BYTES + 1),
            body: String::new(),
            parent_task_id: None,
        };
        assert!(matches!(oversized_title.validate(), Err(HarnessReadApiError::InvalidTaskCreate)));

        let oversized_body = HarnessReadRequestV1::TaskCreate {
            title: "ok".to_owned(),
            body: "x".repeat(HARNESS_BODY_MAX_BYTES + 1),
            parent_task_id: None,
        };
        assert!(matches!(oversized_body.validate(), Err(HarnessReadApiError::InvalidTaskCreate)));
    }

    /// D5, Slice D: `g4a_task_move`'s wire request round-trips.
    #[test]
    fn task_move_request_round_trips() {
        let request = HarnessReadRequestV1::TaskMove {
            task_id: HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap(),
            expected_revision: HarnessRevision::new(3).unwrap(),
            to: HarnessTaskStateV1::Ready,
        };
        request.validate().unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<HarnessReadRequestV1>(&encoded).unwrap(), request);
    }

    /// S10: `g4a_run_finish`'s wire request round-trips, `summary` defaults
    /// to `None` for a caller that omits it, and an oversized summary is a
    /// named validation failure the same bound `g4a_mail_send`'s own `body`
    /// already enforces.
    #[test]
    fn run_finish_request_round_trips_and_bounds_summary() {
        let request = HarnessReadRequestV1::RunFinish {
            outcome: HarnessRunFinishOutcomeV1::Done,
            summary: Some("handed off cleanly".to_owned()),
        };
        request.validate().unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<HarnessReadRequestV1>(&encoded).unwrap(), request);

        let without_summary: HarnessReadRequestV1 = serde_json::from_value(serde_json::json!({
            "kind": "run-finish",
            "outcome": "failed",
        })).unwrap();
        assert_eq!(
            without_summary,
            HarnessReadRequestV1::RunFinish { outcome: HarnessRunFinishOutcomeV1::Failed, summary: None },
        );
        without_summary.validate().unwrap();

        let oversized_summary = HarnessReadRequestV1::RunFinish {
            outcome: HarnessRunFinishOutcomeV1::Done,
            summary: Some("x".repeat(HARNESS_BODY_MAX_BYTES + 1)),
        };
        assert!(matches!(
            oversized_summary.validate(),
            Err(HarnessReadApiError::InvalidText("run finish summary")),
        ));
    }

    /// S10: `HarnessRunFinishResultV1` round-trips through the read response
    /// wire (`HarnessReadResponseV1::RunFinish`).
    #[test]
    fn run_finish_result_round_trips_through_the_response_wire() {
        let finished = HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::Finished {
            run_id: HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap(),
            task_id: HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap(),
            result: HarnessRunFinishOutcomeV1::Done,
        });
        finished.validate().unwrap();
        let encoded = serde_json::to_string(&finished).unwrap();
        assert_eq!(serde_json::from_str::<HarnessReadResponseV1>(&encoded).unwrap(), finished);

        let already_finished = HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::AlreadyFinished {
            run_id: HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap(),
            lifecycle: HarnessRunLifecycleV1::Failed,
        });
        already_finished.validate().unwrap();
    }

    /// D5, Slice D: every `HarnessTaskCreateResultV1`/`HarnessTaskMoveResultV1`
    /// variant round-trips through the read response wire
    /// (`HarnessReadResponseV1::TaskCreate`/`TaskMove`).
    #[test]
    fn task_create_and_move_results_round_trip_every_variant() {
        let created = HarnessReadResponseV1::TaskCreate(HarnessTaskCreateResultV1::Created {
            task_id: HarnessTaskId::new(format!("htask_{}", "c".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
        });
        created.validate().unwrap();
        let encoded = serde_json::to_string(&created).unwrap();
        assert_eq!(serde_json::from_str::<HarnessReadResponseV1>(&encoded).unwrap(), created);

        let parent_outside = HarnessReadResponseV1::TaskCreate(
            HarnessTaskCreateResultV1::ParentOutsideOwnSubtree {
                parent_task_id: HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap(),
                own_task_id: HarnessTaskId::new(format!("htask_{}", "e".repeat(24))).unwrap(),
            },
        );
        parent_outside.validate().unwrap();

        let parent_terminal = HarnessReadResponseV1::TaskCreate(HarnessTaskCreateResultV1::ParentTerminal {
            parent_task_id: HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap(),
            state: HarnessTaskStateV1::Done,
        });
        parent_terminal.validate().unwrap();

        let title_invalid = HarnessReadResponseV1::TaskCreate(HarnessTaskCreateResultV1::TitleInvalid {
            why: "task title is empty, unbounded, padded, or contains control characters".to_owned(),
        });
        title_invalid.validate().unwrap();

        let body_invalid = HarnessReadResponseV1::TaskCreate(HarnessTaskCreateResultV1::BodyInvalid {
            why: "task body is unbounded".to_owned(),
        });
        body_invalid.validate().unwrap();

        let dependencies_invalid = HarnessReadResponseV1::TaskCreate(
            HarnessTaskCreateResultV1::DependenciesInvalid { why: "dependencies must not self-link".to_owned() },
        );
        dependencies_invalid.validate().unwrap();

        let moved = HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::Moved {
            task_id: HarnessTaskId::new(format!("htask_{}", "f".repeat(24))).unwrap(),
            revision: HarnessRevision::new(2).unwrap(),
            from: HarnessTaskStateV1::Backlog,
            to: HarnessTaskStateV1::Ready,
        });
        moved.validate().unwrap();
        let encoded = serde_json::to_string(&moved).unwrap();
        assert_eq!(serde_json::from_str::<HarnessReadResponseV1>(&encoded).unwrap(), moved);

        let task_is_own = HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::TaskIsOwn {
            task_id: HarnessTaskId::new(format!("htask_{}", "f".repeat(24))).unwrap(),
        });
        task_is_own.validate().unwrap();

        let outside_subtree = HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::TaskOutsideOwnSubtree {
            task_id: HarnessTaskId::new(format!("htask_{}", "f".repeat(24))).unwrap(),
            own_task_id: HarnessTaskId::new(format!("htask_{}", "0".repeat(24))).unwrap(),
        });
        outside_subtree.validate().unwrap();

        let illegal_transition = HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::IllegalTransition {
            task_id: HarnessTaskId::new(format!("htask_{}", "f".repeat(24))).unwrap(),
            from: HarnessTaskStateV1::Done,
            to: HarnessTaskStateV1::Ready,
        });
        illegal_transition.validate().unwrap();

        let revision_conflict = HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::RevisionConflict {
            task_id: HarnessTaskId::new(format!("htask_{}", "f".repeat(24))).unwrap(),
            expected: HarnessRevision::new(2).unwrap(),
            current: HarnessRevision::new(3).unwrap(),
        });
        revision_conflict.validate().unwrap();
    }

    fn base_session_context() -> SessionContextV1 {
        SessionContextV1 {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "a".repeat(24))).unwrap(),
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run: CallerRunV1 {
                run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
                task_id: None,
                parent_run_id: None,
                lifecycle: HarnessRunLifecycleV1::Running,
                references_redacted: false,
            },
            task: None,
            sibling_runs: Vec::new(),
            read_permissions: HarnessReadPermissionsV1 {
                tasks: HarnessEntityReadScopeV1::None,
                runs: HarnessEntityReadScopeV1::None,
                operations: HarnessEntityReadScopeV1::None,
            },
            monitoring_visibility: HarnessMonitoringVisibilityV1::None,
            child_task_count: 0,
            child_task_subtree_depth: 0,
            mail: false,
            task_create: false,
            task_mutate: false,
            allowed_tool_ids: vec!["g4a_context_get".to_owned(), "g4a_run_finish".to_owned()],
            history_message_count: None,
            completed_turn_count: None,
            total_tokens: None,
        }
    }

    /// D5, Slice D: `expected_allowed_tool_ids` derives `g4a_task_create`/
    /// `g4a_task_move` from `task_permissions.create`/`.mutate` exactly like
    /// `mail` already derives its own four siblings -- create-only advertises
    /// `g4a_task_create` alone, and a grant cannot claim a tool it was not
    /// actually granted (`InvalidAllowedTools`).
    #[test]
    fn expected_allowed_tool_ids_derives_task_tools_from_task_permissions() {
        let base = base_session_context();
        base.validate().unwrap();

        let create_only = SessionContextV1 {
            task_create: true,
            allowed_tool_ids: vec![
                "g4a_context_get".to_owned(),
                "g4a_run_finish".to_owned(),
                "g4a_task_create".to_owned(),
            ],
            ..base.clone()
        };
        create_only.validate().unwrap();

        let both = SessionContextV1 {
            task_create: true,
            task_mutate: true,
            allowed_tool_ids: vec![
                "g4a_context_get".to_owned(),
                "g4a_run_finish".to_owned(),
                "g4a_task_create".to_owned(),
                "g4a_task_move".to_owned(),
            ],
            ..base.clone()
        };
        both.validate().unwrap();

        let create_only_claims_move = SessionContextV1 {
            task_create: true,
            allowed_tool_ids: vec![
                "g4a_context_get".to_owned(),
                "g4a_task_create".to_owned(),
                "g4a_task_move".to_owned(),
            ],
            ..base
        };
        assert!(matches!(
            create_only_claims_move.validate(),
            Err(HarnessReadApiError::InvalidAllowedTools),
        ));
    }

    /// The removed `maximum_child_count`/`maximum_child_depth` wire fields
    /// stated a cap that was checked for range but never compared against an
    /// actual child count anywhere -- this proves their replacements
    /// (`child_task_count`/`child_task_subtree_depth`) carry whatever the
    /// engine observed, with no ceiling of their own: a value far past the
    /// old `HARNESS_CHILD_COUNT_MAX`/`HARNESS_CHILD_DEPTH_MAX` bounds still
    /// validates and round-trips over the wire under the new field names.
    #[test]
    fn child_task_observations_carry_unbounded_values() {
        let base = base_session_context();
        let many_children = SessionContextV1 {
            child_task_count: u64::from(HARNESS_CHILD_COUNT_MAX) * 1_000,
            child_task_subtree_depth: u64::from(HARNESS_CHILD_DEPTH_MAX) * 1_000,
            ..base
        };
        many_children.validate().unwrap();
        let encoded = serde_json::to_value(&many_children).unwrap();
        assert_eq!(
            encoded.get("child_task_count").and_then(serde_json::Value::as_u64),
            Some(u64::from(HARNESS_CHILD_COUNT_MAX) * 1_000),
        );
        assert_eq!(
            encoded.get("child_task_subtree_depth").and_then(serde_json::Value::as_u64),
            Some(u64::from(HARNESS_CHILD_DEPTH_MAX) * 1_000),
        );
        assert!(encoded.get("maximum_child_count").is_none());
        assert!(encoded.get("maximum_child_depth").is_none());
        let decoded: SessionContextV1 = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, many_children);
    }
}
