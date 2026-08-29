//! Privacy-minimized read wire contract for the harness localhost host.
//!
//! Each loopback TCP connection carries exactly one JSON request frame followed
//! by `\n`. The caller must then half-close its write side; EOF is the request
//! boundary and the host sends exactly one newline-terminated reply. A newline
//! without the write half-close is incomplete and expires categorically as
//! `HarnessReadHostErrorV1::Deadline`.

use std::{collections::BTreeMap, fmt};

pub use gate4agent_harness_protocol::{
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
    HarnessRunGitFactsV1, HarnessRunLifecycleV1,
    HarnessRuntimeIdentityV1, HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
    HarnessValidationError, HarnessWorktreeIntentV1, SessionGrantId,
    HARNESS_ARTIFACTS_MAX, HARNESS_BODY_MAX_BYTES,
    HARNESS_CHILD_COUNT_MAX, HARNESS_CHILD_DEPTH_MAX, HARNESS_DEPENDENCIES_MAX,
    HARNESS_CONTEXT_PACK_MAX_BYTES, HARNESS_CONTEXT_PACK_RETAINED_MESSAGES_MAX,
    HARNESS_LINKS_MAX, HARNESS_RESULTS_MAX, HARNESS_TITLE_MAX_BYTES,
};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const HARNESS_READ_WIRE_VERSION_V1: u16 = 1;
pub const HARNESS_READ_REQUEST_MAX_BYTES: usize = 64 * 1024;
pub const HARNESS_READ_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
pub const HARNESS_READ_LIMIT_MAX: u16 = 256;
pub const HARNESS_ENTITY_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_TIMELINE_PAGE_LIMIT_MAX: u16 = 128;
pub const HARNESS_MONITOR_FACTS_MAX: usize = 128;
pub const HARNESS_OBSERVATION_LABEL_MAX_BYTES: usize = 64;
pub const HARNESS_OBSERVATION_TODO_TEXT_MAX_BYTES: usize = 256;
pub const HARNESS_OBSERVATION_PATH_MAX_BYTES: usize = 1_024;
pub const HARNESS_READ_CREDENTIAL_MAX_BYTES: usize = 8 * 1024;
pub const HARNESS_MCP_AUDIENCE: &str = "gate4agent-harness-mcp-read-v1";
pub const HARNESS_OPERATOR_WIRE_VERSION_V1: u16 = 1;
pub const HARNESS_OPERATOR_WIRE_VERSION_V2: u16 = 2;
pub const HARNESS_OPERATOR_WIRE_VERSION_V3: u16 = 3;
pub const HARNESS_OPERATOR_WIRE_VERSION_V4: u16 = 4;
pub const HARNESS_OPERATOR_WIRE_VERSION_V5: u16 = 5;
pub const HARNESS_OPERATOR_WIRE_VERSION_V6: u16 = 6;
pub const HARNESS_OPERATOR_WIRE_VERSION_V7: u16 = 7;
pub const HARNESS_OPERATOR_WIRE_VERSION_V8: u16 = 8;
pub const HARNESS_OPERATOR_WIRE_VERSION_V9: u16 = 9;
pub const HARNESS_OPERATOR_WIRE_VERSION_V10: u16 = 10;
pub const HARNESS_OPERATOR_WIRE_VERSION_V11: u16 = 11;
pub const HARNESS_OPERATOR_WIRE_VERSION_V12: u16 = 12;
/// V13 gates the two `screen_state` fields on `HarnessRuntimeTerminalFrameV1`
/// and `HarnessRuntimeSessionV1` (see `PtyScreenStateV1`). Deliberately NOT
/// wired into `requires_v13()`/`minimum_wire_version()`: those exist to
/// reject an OLDER peer's request for something newer than it declared
/// (e.g. an action added at V6 sent by a V4 caller). V13 gates the opposite
/// direction -- what a RESPONSE field contains for a peer that already
/// asked at any accepted version -- and every request is servable at any
/// accepted version regardless of whether its reply happens to carry
/// `screen_state`. Do not add `requires_v13()` by analogy with V4..V12;
/// there is no request this version makes newly valid.
pub const HARNESS_OPERATOR_WIRE_VERSION_V13: u16 = 13;
// Realistic multi-pane ceiling with headroom; bounds the harness-side
// per-subscriber HashSet<RuntimeSessionKey> and the connect-time seed burst
// `SubscribeTerminal`'s handler sends immediately after registering (see
// `gate4agent-harness-service::runtime`'s `HostCommand::SubscribeTerminal`
// arm).
pub const HARNESS_TERMINAL_SUBSCRIPTION_SESSIONS_MAX: usize = 32;
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
    pub version: u16,
    pub credential: HarnessOperatorCredential,
    pub request: HarnessOperatorRequestV1,
}

impl HarnessOperatorEnvelopeV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !matches!(
            self.version,
            HARNESS_OPERATOR_WIRE_VERSION_V2
                | HARNESS_OPERATOR_WIRE_VERSION_V3
                | HARNESS_OPERATOR_WIRE_VERSION_V4
                | HARNESS_OPERATOR_WIRE_VERSION_V5
                | HARNESS_OPERATOR_WIRE_VERSION_V6
                | HARNESS_OPERATOR_WIRE_VERSION_V7
                | HARNESS_OPERATOR_WIRE_VERSION_V8
                | HARNESS_OPERATOR_WIRE_VERSION_V9
                | HARNESS_OPERATOR_WIRE_VERSION_V10
                | HARNESS_OPERATOR_WIRE_VERSION_V11
                | HARNESS_OPERATOR_WIRE_VERSION_V12
                | HARNESS_OPERATOR_WIRE_VERSION_V13
        ) || self.version < self.request.minimum_wire_version()
        {
            return Err(HarnessOperatorApiError::UnsupportedVersion);
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

    pub fn requires_v4(&self) -> bool {
        matches!(
            self,
            Self::ReplaceTaskExecutionSpec { .. } | Self::StartTask { .. }
        )
    }

    pub fn requires_v6(&self) -> bool {
        matches!(
            self,
            Self::ReplaceTaskExecutionSpecV2 { .. } | Self::StartTaskV2 { .. }
        )
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
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
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
    TaskLaunchOptionsGet { task_id: HarnessTaskId },
    RuntimeInventoryList {
        after_node_id: Option<String>,
        limit: u16,
    },
    TerminalRead {
        session: HarnessRuntimeSessionAddressV1,
        after_sequence: Option<u64>,
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
    },
    WriteSessionInput {
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
            Self::TasksList { after_task_id, limit, .. } => {
                if let Some(task_id) = after_task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                validate_operator_limit(*limit)
            }
            Self::TaskGet { task_id } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::RunsList { task_id, after_run_id, limit, .. } => {
                if let Some(task_id) = task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(run_id) = after_run_id {
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
            Self::TaskExecutionSpecGet { task_id }
            | Self::TaskLaunchOptionsGet { task_id } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)
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
        }
    }

    pub fn requires_v3(&self) -> bool {
        matches!(
            self,
            Self::RuntimeInventoryList { .. }
                | Self::CatalogNativeSessions { .. }
                | Self::PageNativeSessions { .. }
                | Self::PreviewNativeSession { .. }
                | Self::SubmitIntent { .. }
        )
    }

    pub fn requires_v4(&self) -> bool {
        matches!(
            self,
            Self::RunCorrelationGet { .. }
                | Self::InspectRunWorkspace { .. }
                | Self::ReadRunWorkspaceFile { .. }
                | Self::ReadRunGitHistory { .. }
                | Self::ReadRunGitDiff { .. }
                | Self::LaunchPlansList { .. }
                | Self::TaskExecutionSpecGet { .. }
                | Self::ReplaceTaskExecutionSpec { .. }
                | Self::StartTask { .. }
        ) || matches!(self, Self::SubmitIntent { intent } if intent.action.requires_v4())
    }

    pub fn requires_v5(&self) -> bool {
        matches!(self, Self::RunTransferGet { .. })
    }

    pub fn requires_v6(&self) -> bool {
        matches!(
            self,
            Self::TaskLaunchOptionsGet { .. }
                | Self::ReplaceTaskExecutionSpecV2 { .. }
                | Self::StartTaskV2 { .. }
        ) || matches!(self, Self::SubmitIntent { intent } if intent.action.requires_v6())
    }

    pub fn requires_v7(&self) -> bool {
        matches!(self, Self::ReverseAttributionGet { .. })
    }

    pub fn requires_v8(&self) -> bool {
        matches!(self, Self::ObserveRunContextSource { .. })
    }

    pub fn requires_v9(&self) -> bool {
        matches!(
            self,
            Self::InspectNodeWorkspace { .. }
                | Self::ReadNodeWorkspaceFile { .. }
                | Self::ReadNodeGitHistory { .. }
                | Self::ReadNodeGitDiff { .. }
        )
    }

    pub fn requires_v10(&self) -> bool {
        matches!(
            self,
            Self::SpawnSession { .. }
                | Self::WriteSessionInput { .. }
                | Self::ResizeSession { .. }
                | Self::StopSession { .. }
        )
    }

    pub fn requires_v11(&self) -> bool {
        matches!(
            self,
            Self::SubscribeEvents {}
                | Self::ControlSession { .. }
                | Self::WriteSessionBytes { .. }
                | Self::PasteSession { .. }
                | Self::RemoveSession { .. }
                | Self::ResumeSession { .. }
                | Self::WriteNodeWorkspaceFile { .. }
                | Self::CreateNodeWorkspaceFile { .. }
                | Self::CreateNodeWorkspaceDirectory { .. }
                | Self::PreviewSessionRecord { .. }
                | Self::ResumeSessionRecord { .. }
                | Self::RenameSessionRecord { .. }
                | Self::SetSessionTask { .. }
                | Self::ForgetSessionRecord { .. }
                | Self::IndexProviderSession { .. }
                | Self::IndexNativeSession { .. }
                | Self::BrowseHostDirectories { .. }
                | Self::RegisterWorkspace { .. }
                | Self::UnregisterWorkspace { .. }
                | Self::CreateStandaloneWorkspace { .. }
                | Self::CreateWorktree { .. }
                | Self::RemoveWorktree { .. }
                | Self::ExportContextPack { .. }
                | Self::ForgetContextPack { .. }
        )
    }

    pub fn requires_v12(&self) -> bool {
        matches!(self, Self::SubscribeTerminal { .. })
    }

    pub fn minimum_wire_version(&self) -> u16 {
        if self.requires_v12() {
            HARNESS_OPERATOR_WIRE_VERSION_V12
        } else if self.requires_v11() {
            HARNESS_OPERATOR_WIRE_VERSION_V11
        } else if self.requires_v10() {
            HARNESS_OPERATOR_WIRE_VERSION_V10
        } else if self.requires_v9() {
            HARNESS_OPERATOR_WIRE_VERSION_V9
        } else if self.requires_v8() {
            HARNESS_OPERATOR_WIRE_VERSION_V8
        } else if self.requires_v7() {
            HARNESS_OPERATOR_WIRE_VERSION_V7
        } else if self.requires_v6() {
            HARNESS_OPERATOR_WIRE_VERSION_V6
        } else if self.requires_v5() {
            HARNESS_OPERATOR_WIRE_VERSION_V5
        } else if self.requires_v4() {
            HARNESS_OPERATOR_WIRE_VERSION_V4
        } else if self.requires_v3() {
            HARNESS_OPERATOR_WIRE_VERSION_V3
        } else {
            HARNESS_OPERATOR_WIRE_VERSION_V2
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorResponseV1 {
    Monitor(SessionMonitorV1),
    Timeline(TimelinePageV1),
    Tasks(TaskPageV1),
    Task(RedactedTaskV1),
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
    SessionSpawned(HarnessRuntimeSessionAddressV1),
    SessionInputWritten,
    SessionResized,
    SessionStopped,
    SessionControlled,
    SessionBytesWritten,
    SessionPasted,
    SessionRemoved,
    SessionResumed,
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
            Self::SessionSpawned(value) => value.validate(),
            Self::SessionInputWritten
            | Self::SessionResized
            | Self::SessionStopped
            | Self::SessionControlled
            | Self::SessionBytesWritten
            | Self::SessionPasted
            | Self::SessionRemoved
            | Self::SessionResumed => Ok(()),
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
            || self.managed_sessions.windows(2).any(|pair| pair[0].record_id >= pair[1].record_id)
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
/// both `PtyScreenStateV1::OperatorGate`'s `gate` and `Failing`'s `reason`.
pub const HARNESS_SCREEN_STATE_GATE_MAX_BYTES: usize = 128;

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
    OperatorGate { gate: String },
    Failing { reason: String },
    Ready,
}

impl PtyScreenStateV1 {
    /// True only for `Ready` -- mirrors
    /// `gate4agent_types::PtyScreenState::admits_blind_write`, so a wire
    /// consumer has the same single predicate and no call site open-codes
    /// `matches!(.., Ready)` and gets `Unknown` wrong.
    pub fn admits_blind_write(&self) -> bool {
        matches!(self, Self::Ready)
    }

    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let ok = match self {
            Self::Unknown | Self::Ready => true,
            Self::NotAgent { observed_process } => {
                valid_native_single_line(observed_process, HARNESS_SCREEN_STATE_PROCESS_MAX_BYTES, true)
            }
            Self::OperatorGate { gate } => {
                valid_native_single_line(gate, HARNESS_SCREEN_STATE_GATE_MAX_BYTES, true)
            }
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
    /// The node's current screen classification, present only when the
    /// requesting peer declared `>= HARNESS_OPERATOR_WIRE_VERSION_V13`.
    /// `skip_serializing_if` is load-bearing: the key must be ABSENT from
    /// the JSON, not `null`, or a pre-V13 `deny_unknown_fields` decoder
    /// rejects the whole message. `None` means "this peer did not ask for a
    /// version that carries it" -- it is NOT the same as `Some(Unknown)`
    /// ("asked, and nothing is classified yet"); a consumer must not
    /// conflate the two.
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
    /// was materialized, present only when the requesting peer declared
    /// `>= HARNESS_OPERATOR_WIRE_VERSION_V13`. `skip_serializing_if` is
    /// load-bearing: the key must be ABSENT from the JSON, not `null`, or a
    /// pre-V13 `deny_unknown_fields` decoder rejects the whole message.
    /// `None` means "this peer did not ask for a version that carries it"
    /// -- it is NOT the same as `Some(Unknown)` ("asked, and nothing is
    /// classified yet"); a consumer must not conflate the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_state: Option<PtyScreenStateV1>,
    /// Mirrors `gate4agent_types::TerminalFrame::bracketed_paste` onto the
    /// operator wire, carried through unconditionally (unlike
    /// `screen_state`, this is not gated on the requesting peer's declared
    /// wire version -- `None` already means "no value", so there is no
    /// separate "this peer didn't ask" state to distinguish it from).
    /// `skip_serializing_if` is load-bearing the same way it is on
    /// `screen_state`: the key must be ABSENT from the JSON, not `null`, or
    /// a pre-existing `deny_unknown_fields` decoder rejects the whole
    /// message. `#[serde(default)]` so an operator client built before this
    /// field existed still decodes the frame.
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
pub enum HarnessRuntimeManagedModeV1 { Pty, Inline }

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReadEnvelopeV1 {
    pub version: u16,
    pub credential: HarnessReadCredential,
    pub request: HarnessReadRequestV1,
}

impl HarnessReadEnvelopeV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_version(self.version)?;
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
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    },
    RunGet { run_id: HarnessRunId },
    OperationGet { operation_id: HarnessOperationId },
}

impl HarnessReadRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::TimelineRead { run_id, after_sequence, limit } => {
                if let Some(run_id) = run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if after_sequence == &Some(0) {
                    return Err(HarnessReadApiError::InvalidCursor);
                }
                validate_limit(*limit, HARNESS_TIMELINE_PAGE_LIMIT_MAX)
            }
            Self::TasksList { after_task_id, limit, .. } => {
                if let Some(task_id) = after_task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                validate_limit(*limit, HARNESS_ENTITY_PAGE_LIMIT_MAX)
            }
            Self::RunsList { task_id, after_run_id, limit, .. } => {
                if let Some(task_id) = task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if let Some(run_id) = after_run_id {
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
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessReadHostErrorV1 {
    InvalidRequest,
    Unauthorized,
    NotFoundOrDenied,
    TooLarge,
    Deadline,
    Internal,
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
    pub maximum_child_count: u16,
    pub maximum_child_depth: u16,
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
        if self.maximum_child_count > HARNESS_CHILD_COUNT_MAX
            || self.maximum_child_depth > HARNESS_CHILD_DEPTH_MAX
        {
            return Err(HarnessReadApiError::InvalidChildLimits);
        }
        if self.allowed_tool_ids.len() > HARNESS_READ_TOOL_IDS.len()
            || self.allowed_tool_ids.windows(2).any(|pair| pair[0] >= pair[1])
            || self.allowed_tool_ids.iter().any(|id| !HARNESS_READ_TOOL_IDS.contains(&id.as_str()))
            || self.allowed_tool_ids != expected_allowed_tool_ids(self)
        {
            return Err(HarnessReadApiError::InvalidAllowedTools);
        }
        Ok(())
    }
}

fn expected_allowed_tool_ids(context: &SessionContextV1) -> Vec<String> {
    let mut tools = vec!["g4a_context_get"];
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
    pub process_facts: Vec<ActivityFactV1>,
    pub file_facts: Vec<FileFactV1>,
}

impl SessionMonitorDetailV1 {
    pub fn validate(&self, features: &MonitorFeatureStatesV1) -> Result<(), HarnessReadApiError> {
        validate_monitor_facts(&self.todo_facts)?;
        validate_monitor_facts(&self.tool_facts)?;
        validate_monitor_facts(&self.subagent_facts)?;
        validate_monitor_facts(&self.interaction_facts)?;
        validate_monitor_facts(&self.process_facts)?;
        validate_monitor_facts(&self.file_facts)?;
        for fact in &self.todo_facts { fact.validate()?; }
        for fact in &self.tool_facts { fact.validate()?; }
        for fact in &self.subagent_facts { fact.validate()?; }
        for fact in &self.interaction_facts { fact.validate()?; }
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
    #[error("unsupported harness read wire version")]
    UnsupportedVersion,
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
    #[error("harness child limits are invalid")]
    InvalidChildLimits,
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
    #[error("harness protocol value is invalid: {0}")]
    Protocol(#[from] gate4agent_harness_protocol::HarnessValidationError),
}

#[derive(Debug, Error)]
pub enum HarnessOperatorApiError {
    #[error("unsupported harness operator wire version")]
    UnsupportedVersion,
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
    #[error("harness session record request is invalid")]
    InvalidSessionRecordRequest,
    #[error("harness host path is invalid")]
    InvalidHostPath,
    #[error("harness host directory browse request is invalid")]
    InvalidHostDirectoryBrowseRequest,
    #[error("harness workspace or worktree resource mutation request is invalid")]
    InvalidResourceMutationRequest,
    #[error("harness operator response is invalid")]
    Read(#[source] HarnessReadApiError),
    #[error("harness protocol value is invalid: {0}")]
    Protocol(#[source] gate4agent_harness_protocol::HarnessValidationError),
}

fn validate_version(version: u16) -> Result<(), HarnessReadApiError> {
    if version != HARNESS_READ_WIRE_VERSION_V1 {
        return Err(HarnessReadApiError::UnsupportedVersion);
    }
    Ok(())
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
            limit: HARNESS_ENTITY_PAGE_LIMIT_MAX,
        }.validate().is_ok());
        assert!(HarnessOperatorRequestV1::TasksList {
            after_task_id: None,
            state: None,
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
    fn operator_v2_rejects_v1_wire_and_legacy_raw_schedule_variant() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let envelope = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V1,
            credential,
            request: HarnessOperatorRequestV1::TasksList {
                after_task_id: None,
                state: None,
                limit: 1,
            },
        };
        assert!(matches!(
            envelope.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        let legacy = r#"{"kind":"schedule-ready-task","request":{}}"#;
        assert!(serde_json::from_str::<HarnessOperatorRequestV1>(legacy).is_err());
    }

    #[test]
    fn operator_v3_intent_is_authority_free_and_fails_closed_on_v2() {
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
        let v2 = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V2,
            credential: credential.clone(),
            request: request.clone(),
        };
        assert!(matches!(
            v2.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        let v3 = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V3,
            credential,
            request,
        };
        assert!(v3.validate().is_ok());
        let decoded: HarnessOperatorEnvelopeV1 = serde_json::from_slice(
            &serde_json::to_vec(&v3).unwrap(),
        ).unwrap();
        assert_eq!(decoded, v3);
    }

    #[test]
    fn operator_v4_run_correlation_is_exact_and_fails_closed_on_v3() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::RunCorrelationGet {
            run_id: HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap(),
        };
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V4);
        assert!(request.requires_v4());
        assert!(!request.requires_v3());
        assert!(matches!(
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V3,
                credential: credential.clone(),
                request: request.clone(),
            }.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        let envelope = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V4,
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
    fn operator_v5_run_transfer_is_exact_private_and_fails_closed_on_v4() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let summary = run_transfer_summary();
        summary.validate().unwrap();
        let request = HarnessOperatorRequestV1::RunTransferGet {
            run_id: summary.run_id.clone(),
        };
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V5);
        assert!(request.requires_v5());
        assert!(!request.requires_v4());
        assert!(matches!(
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V4,
                credential: credential.clone(),
                request: request.clone(),
            }.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V5,
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
        }
    }

    #[test]
    fn operator_v6_task_launch_contract_is_exact_private_and_fails_closed_on_v5() {
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
            HarnessOperatorRequestV1::TaskLaunchOptionsGet { task_id: task_id.clone() },
            replace.clone(),
            start.clone(),
        ] {
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V6);
            request.validate().unwrap();
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V5,
                    credential: credential.clone(),
                    request,
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
        }
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V6,
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
        assert_eq!(
            HarnessOperatorRequestV1::SubmitIntent { intent }.minimum_wire_version(),
            HARNESS_OPERATOR_WIRE_VERSION_V6,
        );
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
        assert_eq!(
            HarnessOperatorRequestV1::SubmitIntent { intent: start_intent }
                .minimum_wire_version(),
            HARNESS_OPERATOR_WIRE_VERSION_V6,
        );
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

    #[test]
    fn operator_v4_execution_requests_are_exact_and_fail_closed_on_v3() {
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
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V4);
            assert!(request.validate().is_ok());
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V3,
                    credential: credential.clone(),
                    request,
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
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
    fn execution_spec_and_start_intents_are_authority_free_v4() {
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
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V4);
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
        assert_eq!(replace.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V4);
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
    fn runtime_inventory_request_requires_v3() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::RuntimeInventoryList {
            after_node_id: None,
            limit: HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX,
        };
        let v2 = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V2,
            credential: credential.clone(),
            request: request.clone(),
        };
        assert!(matches!(
            v2.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        assert!(HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V3,
            credential,
            request,
        }.validate().is_ok());
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
    fn native_history_v3_is_exact_bounded_and_excludes_sensitive_fields() {
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
            assert!(request.requires_v3());
            assert!(HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V2,
                credential: credential.clone(),
                request: request.clone(),
            }.validate().is_err());
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V3,
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
        assert!(HarnessReadRequestV1::TasksList { after_task_id: None, state: None, limit: 0 }.validate().is_err());
        assert!(HarnessReadRequestV1::TasksList { after_task_id: None, state: None, limit: 65 }.validate().is_err());
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
    fn operator_v4_run_workspace_requests_are_exact_harness_only_round_trips() {
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
            request.validate().expect("valid V4 workspace request");
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V4);
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
    fn operator_v7_reverse_attribution_is_exact_private_and_fails_closed_on_v6() {
        let subject = HarnessReverseAttributionSubjectV1::FileScope {
            workspace: reverse_attribution_workspace(),
            relative_path: HarnessRepositoryPathV1::new("src/lib.rs").unwrap(),
        };
        let request = HarnessOperatorRequestV1::ReverseAttributionGet {
            subject: subject.clone(),
        };
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V7);
        request.validate().unwrap();
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        assert!(matches!(
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V6,
                credential: credential.clone(),
                request: request.clone(),
            }.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V7,
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
    fn operator_v9_node_workspace_requests_are_exact_round_trips_and_fail_closed_on_v8() {
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
            request.validate().expect("valid V9 node-workspace request");
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V9);
            let encoded = serde_json::to_string(&request).unwrap();
            for forbidden in ["run_id", "endpoint", "root", "worktree", "environment"] {
                assert!(!encoded.contains(forbidden), "request exposed {forbidden}");
            }
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V8,
                    credential: credential.clone(),
                    request: request.clone(),
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V9,
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
    fn operator_v11_node_workspace_write_verbs_are_exact_round_trips_and_fail_closed_on_v10() {
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
            request.validate().expect("valid V11 node-workspace write request");
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V11);
            let encoded = serde_json::to_string(&request).unwrap();
            for forbidden in ["run_id", "endpoint", "root", "worktree", "environment"] {
                assert!(!encoded.contains(forbidden), "request exposed {forbidden}");
            }
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V10,
                    credential: credential.clone(),
                    request: request.clone(),
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V11,
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
    fn operator_v8_context_source_observation_is_exact_private_and_fails_closed_on_v7() {
        let run_id = HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap();
        let request = HarnessOperatorRequestV1::ObserveRunContextSource {
            run_id: run_id.clone(),
        };
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V8);
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
        assert!(matches!(
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V7,
                credential: credential.clone(),
                request: request.clone(),
            }.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V8,
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

    /// Every `PtyScreenStateV1` variant, for the round-trip and validation
    /// sweeps below -- kept as one list so a variant added to the enum
    /// without a matching addition here is caught by a stale sweep rather
    /// than silently under-covered.
    fn every_screen_state_variant() -> Vec<PtyScreenStateV1> {
        vec![
            PtyScreenStateV1::Unknown,
            PtyScreenStateV1::NotAgent { observed_process: "installer.exe".to_owned() },
            PtyScreenStateV1::OperatorGate { gate: "workspace-trust".to_owned() },
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
            PtyScreenStateV1::OperatorGate { gate: oversized_gate.clone() },
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

    #[test]
    fn operator_v10_session_verbs_are_exact_round_trips_and_fail_closed_on_v9() {
        let session = session_address(41, 3);
        let requests = vec![
            HarnessOperatorRequestV1::SpawnSession {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                provider: "claude".to_owned(),
                provider_profile: "claude-default".to_owned(),
                mode: HarnessExecutionModeV1::Pty,
                terminal_size: HarnessRuntimeTerminalSizeV1 { rows: 40, columns: 120 },
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
            request.validate().expect("valid V10 session verb request");
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V10);
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V9,
                    credential: credential.clone(),
                    request: request.clone(),
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V10,
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
    fn operator_v11_session_control_verbs_are_exact_round_trips_and_fail_closed_on_v10() {
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
            request.validate().expect("valid V11 session control verb request");
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V11);
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V10,
                    credential: credential.clone(),
                    request: request.clone(),
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V11,
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
        }
    }

    #[test]
    fn operator_session_record_family_requests_are_exact_round_trips_and_fail_closed_on_v10() {
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
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V11);
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V10,
                    credential: credential.clone(),
                    request: request.clone(),
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V11,
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
    fn operator_resource_mutation_family_requests_are_exact_round_trips_and_fail_closed_on_v10() {
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
            assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V11);
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, request);
            assert!(matches!(
                HarnessOperatorEnvelopeV1 {
                    version: HARNESS_OPERATOR_WIRE_VERSION_V10,
                    credential: credential.clone(),
                    request: request.clone(),
                }.validate(),
                Err(HarnessOperatorApiError::UnsupportedVersion),
            ));
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V11,
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
                launch_inventory: None,
            },
        }
    }

    #[test]
    fn operator_v11_subscribe_events_is_exact_round_trip_and_fails_closed_on_v10() {
        let request = HarnessOperatorRequestV1::SubscribeEvents {};
        request.validate().expect("valid v11 subscribe request");
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V11);
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);

        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        assert!(matches!(
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V10,
                credential: credential.clone(),
                request: request.clone(),
            }.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V11,
            credential,
            request,
        }.validate().unwrap();
    }

    #[test]
    fn operator_v12_subscribe_terminal_is_exact_round_trip_and_fails_closed_on_v11() {
        let request = HarnessOperatorRequestV1::SubscribeTerminal {
            sessions: vec![session_address(1, 1), session_address(2, 1)],
        };
        request.validate().expect("valid v12 subscribe-terminal request");
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V12);
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: HarnessOperatorRequestV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);

        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        assert!(matches!(
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V11,
                credential: credential.clone(),
                request: request.clone(),
            }.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V12,
            credential,
            request,
        }.validate().unwrap();
    }

    /// V13 gates a reply FIELD (`screen_state`), not any request's minimum
    /// version -- so a request that already required V12 must still
    /// validate at V13 unchanged, proving the new accepted-version entry
    /// regresses nothing already served at an older accepted version.
    #[test]
    fn operator_v13_is_accepted_and_a_v12_minimum_request_still_validates_at_v13() {
        let request = HarnessOperatorRequestV1::SubscribeTerminal {
            sessions: vec![session_address(1, 1)],
        };
        assert_eq!(request.minimum_wire_version(), HARNESS_OPERATOR_WIRE_VERSION_V12);
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V13,
            credential,
            request,
        }.validate().unwrap();
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
}
