//! Bounded, privacy-minimized domain contract for the adjacent harness kernel.
//!
//! Harness workflow state is explicit operator/agent intent. Monitoring facts
//! are deliberately absent from this crate and cannot mutate task state.

use std::{cmp::Ordering, fmt};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const HARNESS_PROTOCOL_VERSION_V1: u16 = 1;
pub const HARNESS_TITLE_MAX_BYTES: usize = 256;
pub const HARNESS_BODY_MAX_BYTES: usize = 8 * 1024;
pub const HARNESS_SELECTOR_MAX_BYTES: usize = 128;
pub const HARNESS_LINKS_MAX: usize = 128;
pub const HARNESS_DEPENDENCIES_MAX: usize = 64;
pub const HARNESS_RESULTS_MAX: usize = 64;
pub const HARNESS_ARTIFACTS_MAX: usize = 128;
pub const HARNESS_ALLOWED_TARGETS_MAX: usize = 128;
pub const HARNESS_CHILD_COUNT_MAX: u16 = 256;
pub const HARNESS_CHILD_DEPTH_MAX: u16 = 32;
pub const HARNESS_TIMEOUT_MIN_MS: u64 = 100;
pub const HARNESS_TIMEOUT_MAX_MS: u64 = 24 * 60 * 60 * 1_000;
pub const HARNESS_DELIVERY_RESOURCE_MAX_BYTES: usize = 128;
pub const HARNESS_DELIVERIES_MAX: usize = 4_096;
pub const HARNESS_CONTINUATIONS_MAX: usize = 4_096;
pub const HARNESS_CONTEXT_PACK_MAX_BYTES: u32 = 256 * 1024;
pub const HARNESS_CONTEXT_PACK_RETAINED_MESSAGES_MAX: u64 = 256;
pub const HARNESS_SCHEDULER_SCAN_MAX: usize = 8_192;

macro_rules! opaque_id {
    ($name:ident, $prefix:literal, $label:literal) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn new(value: impl Into<String>) -> Result<Self, HarnessValidationError> {
                let value = value.into();
                validate_prefixed_hex_id($label, &value, Self::PREFIX, 24)?;
                Ok(Self(value))
            }

            pub fn validate(&self) -> Result<(), HarnessValidationError> {
                validate_prefixed_hex_id($label, &self.0, Self::PREFIX, 24)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(HarnessTaskId, "htask_", "task id");
opaque_id!(HarnessRunId, "hrun_", "run id");
opaque_id!(SessionGrantId, "hgrant_", "grant id");
opaque_id!(HarnessOperationId, "hop_", "operation id");
opaque_id!(HarnessResultRef, "hresult_", "result reference");
opaque_id!(HarnessArtifactRef, "hartifact_", "artifact reference");
opaque_id!(HarnessReceiptRef, "hreceipt_", "receipt reference");
opaque_id!(HarnessInlineRef, "hinline_", "inline reference");
opaque_id!(HarnessIdempotencyRef, "hidem_", "idempotency reference");
opaque_id!(HarnessDeliveryRef, "hdelivery_", "delivery reference");
opaque_id!(HarnessContinuationRef, "hcontinuation_", "continuation reference");

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessRevision(u64);

impl HarnessRevision {
    pub fn new(value: u64) -> Result<Self, HarnessValidationError> {
        if value == 0 {
            return Err(HarnessValidationError::ZeroRevision);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn validate(self) -> Result<(), HarnessValidationError> {
        Self::new(self.0).map(|_| ())
    }
}

impl<'de> Deserialize<'de> for HarnessRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessSelectorV1(String);

impl HarnessSelectorV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessValidationError> {
        let value = value.into();
        validate_selector(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        validate_selector(&self.0)
    }
}

impl fmt::Debug for HarnessSelectorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("HarnessSelectorV1").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for HarnessSelectorV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessActorV1 {
    User { actor_id: HarnessSelectorV1 },
    ParentRun { run_id: HarnessRunId },
}

impl HarnessActorV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        match self {
            Self::User { actor_id } => actor_id.validate(),
            Self::ParentRun { run_id } => run_id.validate(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessTaskStateV1 {
    Backlog,
    Ready,
    Running,
    Waiting,
    Review,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTaskV1 {
    pub task_id: HarnessTaskId,
    pub revision: HarnessRevision,
    pub title: String,
    pub body: String,
    pub creator: HarnessActorV1,
    pub parent_task_id: Option<HarnessTaskId>,
    pub dependencies: Vec<HarnessTaskId>,
    pub state: HarnessTaskStateV1,
    pub run_ids: Vec<HarnessRunId>,
    pub result_refs: Vec<HarnessResultRef>,
    pub artifact_refs: Vec<HarnessArtifactRef>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl HarnessTaskV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.task_id.validate()?;
        self.revision.validate()?;
        validate_title(&self.title)?;
        validate_body(&self.body)?;
        self.creator.validate()?;
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;
        if self.parent_task_id.as_ref() == Some(&self.task_id) {
            return Err(HarnessValidationError::SelfLink { field: "parent_task_id" });
        }
        if self.dependencies.iter().any(|task_id| task_id == &self.task_id) {
            return Err(HarnessValidationError::SelfLink { field: "dependencies" });
        }
        validate_sorted_ids("dependencies", &self.dependencies, HARNESS_DEPENDENCIES_MAX)?;
        validate_sorted_ids("run_ids", &self.run_ids, HARNESS_LINKS_MAX)?;
        validate_sorted_ids("result_refs", &self.result_refs, HARNESS_RESULTS_MAX)?;
        validate_sorted_ids("artifact_refs", &self.artifact_refs, HARNESS_ARTIFACTS_MAX)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperatorAuthorityV1 {
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub actor_id: HarnessSelectorV1,
    pub now_unix_ms: u64,
}

impl HarnessOperatorAuthorityV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.operation_id.validate()?;
        self.idempotency_ref.validate()?;
        self.actor_id.validate()?;
        if self.now_unix_ms == 0 {
            return Err(HarnessValidationError::InvalidTimestamps);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCreateTaskRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub title: String,
    pub body: String,
    pub parent_task_id: Option<HarnessTaskId>,
    pub dependencies: Vec<HarnessTaskId>,
    pub initial_state: HarnessTaskStateV1,
}

impl HarnessCreateTaskRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        self.task_id.validate()?;
        validate_title(&self.title)?;
        validate_body(&self.body)?;
        if self.parent_task_id.as_ref() == Some(&self.task_id) {
            return Err(HarnessValidationError::SelfLink { field: "parent_task_id" });
        }
        if self.dependencies.iter().any(|dependency| dependency == &self.task_id) {
            return Err(HarnessValidationError::SelfLink { field: "dependencies" });
        }
        validate_sorted_ids("dependencies", &self.dependencies, HARNESS_DEPENDENCIES_MAX)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReplaceTaskRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub expected_revision: HarnessRevision,
    pub title: String,
    pub body: String,
    pub parent_task_id: Option<HarnessTaskId>,
    pub dependencies: Vec<HarnessTaskId>,
}

impl HarnessReplaceTaskRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        self.task_id.validate()?;
        self.expected_revision.validate()?;
        validate_title(&self.title)?;
        validate_body(&self.body)?;
        if self.parent_task_id.as_ref() == Some(&self.task_id) {
            return Err(HarnessValidationError::SelfLink { field: "parent_task_id" });
        }
        if self.dependencies.iter().any(|dependency| dependency == &self.task_id) {
            return Err(HarnessValidationError::SelfLink { field: "dependencies" });
        }
        validate_sorted_ids("dependencies", &self.dependencies, HARNESS_DEPENDENCIES_MAX)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMoveTaskRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub expected_revision: HarnessRevision,
    pub state: HarnessTaskStateV1,
}

impl HarnessMoveTaskRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        self.task_id.validate()?;
        self.expected_revision.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCancelTaskRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub expected_revision: HarnessRevision,
}

impl HarnessCancelTaskRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        self.task_id.validate()?;
        self.expected_revision.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRetryTaskRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub task_id: HarnessTaskId,
    pub expected_revision: HarnessRevision,
}

impl HarnessRetryTaskRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        self.task_id.validate()?;
        self.expected_revision.validate()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessExecutionModeV1 {
    Pty,
    Inline,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessWorktreeIntentV1 {
    Existing,
    Managed { worktree_ref: HarnessSelectorV1 },
}

impl HarnessWorktreeIntentV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        match self {
            Self::Existing => Ok(()),
            Self::Managed { worktree_ref } => worktree_ref.validate(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunIntentV1 {
    pub node_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub worktree: HarnessWorktreeIntentV1,
    pub provider_profile: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
    pub delivery_bundle: Option<HarnessSelectorV1>,
    pub continuation: Option<HarnessSelectorV1>,
}

impl HarnessRunIntentV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.node_id.validate()?;
        self.workspace_id.validate()?;
        self.worktree.validate()?;
        self.provider_profile.validate()?;
        if let Some(delivery_bundle) = &self.delivery_bundle {
            delivery_bundle.validate()?;
        }
        if let Some(continuation) = &self.continuation {
            continuation.validate()?;
            HarnessRunId::new(continuation.as_str())
                .map_err(|_| HarnessValidationError::InvalidContinuationSelector)?;
        }
        Ok(())
    }
}

impl HarnessRunIntentV1 {
    /// Resolves continuation only as an exact source HarnessRunId. It is never
    /// interpreted as a raw Node SpawnContextId.
    pub fn continuation_source_run_id(
        &self,
    ) -> Result<Option<HarnessRunId>, HarnessValidationError> {
        self.continuation.as_ref().map(|selector| {
            HarnessRunId::new(selector.as_str())
                .map_err(|_| HarnessValidationError::InvalidContinuationSelector)
        }).transpose()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessScheduleRequestV1 {
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub actor: HarnessActorV1,
    pub run_id: HarnessRunId,
    pub parent_run_id: Option<HarnessRunId>,
    pub intent: HarnessRunIntentV1,
    pub now_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessScheduleNextRequestV1 {
    pub authority: HarnessOperatorAuthorityV1,
    pub plan_id: Option<HarnessSelectorV1>,
}

impl HarnessScheduleNextRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.authority.validate()?;
        if let Some(plan_id) = &self.plan_id { plan_id.validate()?; }
        Ok(())
    }
}

impl HarnessScheduleRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.operation_id.validate()?;
        self.idempotency_ref.validate()?;
        self.actor.validate()?;
        self.run_id.validate()?;
        if self.parent_run_id.as_ref() == Some(&self.run_id) {
            return Err(HarnessValidationError::SelfLink { field: "parent_run_id" });
        }
        if let Some(parent_run_id) = &self.parent_run_id {
            parent_run_id.validate()?;
        }
        self.intent.validate()?;
        if self.now_unix_ms == 0 {
            return Err(HarnessValidationError::InvalidTimestamps);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDispatchIntentV1 {
    pub task_id: HarnessTaskId,
    pub task_revision: HarnessRevision,
    pub run_id: HarnessRunId,
    pub run_revision: HarnessRevision,
    pub operation_id: HarnessOperationId,
    pub operation_revision: HarnessRevision,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub parent_run_id: Option<HarnessRunId>,
    pub intent: HarnessRunIntentV1,
}

impl HarnessDispatchIntentV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.task_id.validate()?;
        self.task_revision.validate()?;
        self.run_id.validate()?;
        self.run_revision.validate()?;
        self.operation_id.validate()?;
        self.operation_revision.validate()?;
        self.idempotency_ref.validate()?;
        if self.parent_run_id.as_ref() == Some(&self.run_id) {
            return Err(HarnessValidationError::SelfLink { field: "parent_run_id" });
        }
        if let Some(parent_run_id) = &self.parent_run_id {
            parent_run_id.validate()?;
        }
        self.intent.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", content = "dispatch", rename_all = "kebab-case")]
pub enum HarnessScheduleOutcomeV1 {
    Idle,
    Dispatch(HarnessDispatchIntentV1),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeIdentityV1 {
    pub instance_id: u64,
    pub generation: u64,
}

impl HarnessRuntimeIdentityV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        if self.instance_id == 0 || self.generation == 0 {
            return Err(HarnessValidationError::InvalidSessionIdentity);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessSessionIdentityV1 {
    Managed { record_id: HarnessSelectorV1, active_session: Option<HarnessRuntimeIdentityV1> },
    Inline { inline_ref: HarnessInlineRef },
}

impl HarnessSessionIdentityV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        match self {
            Self::Managed { record_id, active_session } => {
                record_id.validate()?;
                if let Some(active_session) = active_session {
                    active_session.validate()?;
                }
                Ok(())
            }
            Self::Inline { inline_ref } => inline_ref.validate(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSessionBindingV1 {
    pub node_id: HarnessSelectorV1,
    pub node_incarnation: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub session: HarnessSessionIdentityV1,
}

macro_rules! delivery_resource_id {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, HarnessValidationError> {
                let value = value.into();
                validate_delivery_resource_id($label, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str { &self.0 }

            pub fn validate(&self) -> Result<(), HarnessValidationError> {
                validate_delivery_resource_id($label, &self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

delivery_resource_id!(HarnessDeliveryBundleIdV1, "delivery bundle id");
delivery_resource_id!(HarnessDeliveryBundleRevisionV1, "delivery bundle revision");

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessDeliveryBundleDigestV1(String);

impl HarnessDeliveryBundleDigestV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessValidationError> {
        let value = value.into();
        validate_delivery_digest(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        validate_delivery_digest(&self.0)
    }
}

impl fmt::Debug for HarnessDeliveryBundleDigestV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("HarnessDeliveryBundleDigestV1").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for HarnessDeliveryBundleDigestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessDeliveryManifestDigestV2(String);

impl HarnessDeliveryManifestDigestV2 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessValidationError> {
        let value = value.into();
        validate_delivery_digest(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        validate_delivery_digest(&self.0)
    }
}

impl fmt::Debug for HarnessDeliveryManifestDigestV2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("HarnessDeliveryManifestDigestV2").field(&self.0).finish()
    }
}

impl<'de> Deserialize<'de> for HarnessDeliveryManifestDigestV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliveryBundleV1 {
    pub selector: HarnessSelectorV1,
    pub bundle_id: HarnessDeliveryBundleIdV1,
    pub revision: HarnessDeliveryBundleRevisionV1,
    pub digest: HarnessDeliveryBundleDigestV1,
    pub manifest_digest: HarnessDeliveryManifestDigestV2,
}

impl HarnessDeliveryBundleV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.selector.validate()?;
        self.bundle_id.validate()?;
        self.revision.validate()?;
        self.digest.validate()
            .and_then(|_| self.manifest_digest.validate())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessDeliveryStateV1 { Prepared, Staged, Committed }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliveryStageReceiptV1 {
    pub node_id: HarnessSelectorV1,
    pub node_incarnation: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub bundle: HarnessDeliveryBundleV1,
    pub staged_at_unix_ms: u64,
}

impl HarnessDeliveryStageReceiptV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.node_id.validate()?;
        self.node_incarnation.validate()?;
        self.workspace_id.validate()?;
        self.bundle.validate()?;
        if self.staged_at_unix_ms == 0 {
            return Err(HarnessValidationError::InvalidTimestamps);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliveryReceiptV1 {
    pub receipt_ref: HarnessReceiptRef,
    pub delivery_ref: HarnessDeliveryRef,
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub task_id: HarnessTaskId,
    pub run_id: HarnessRunId,
    pub operation_id: HarnessOperationId,
    pub binding: HarnessSessionBindingV1,
    pub bundle: HarnessDeliveryBundleV1,
    pub committed_at_unix_ms: u64,
}

impl HarnessDeliveryReceiptV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.receipt_ref.validate()?;
        self.delivery_ref.validate()?;
        self.grant_id.validate()?;
        self.grant_revision.validate()?;
        self.task_id.validate()?;
        self.run_id.validate()?;
        self.operation_id.validate()?;
        self.binding.validate()?;
        self.bundle.validate()?;
        if self.committed_at_unix_ms == 0 {
            return Err(HarnessValidationError::InvalidTimestamps);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliveryV1 {
    pub delivery_ref: HarnessDeliveryRef,
    pub revision: HarnessRevision,
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub task_id: HarnessTaskId,
    pub run_id: HarnessRunId,
    pub operation_id: HarnessOperationId,
    pub bundle: HarnessDeliveryBundleV1,
    pub state: HarnessDeliveryStateV1,
    pub stage_receipt: Option<HarnessDeliveryStageReceiptV1>,
    pub receipt: Option<HarnessDeliveryReceiptV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessContinuationStateV1 {
    Prepared,
    Exporting,
    Exported,
    Bound,
    OutcomeUnknown,
    Expired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessContinuationOutcomeUnknownReasonV1 {
    Transport,
    RouteMismatch,
    ReceiptMismatch,
    UnexpectedResponse,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessContinuationCleanupStateV1 { Retained, NotRequired }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessContextPackLineageV1 {
    pub source_node_id: HarnessSelectorV1,
    pub source_workspace_id: HarnessSelectorV1,
    pub source_instance_id: u64,
    pub source_generation: u64,
    pub source_provider: HarnessSelectorV1,
}

impl HarnessContextPackLineageV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.source_node_id.validate()?;
        self.source_workspace_id.validate()?;
        self.source_provider.validate()?;
        if self.source_instance_id == 0 || self.source_generation == 0 {
            return Err(HarnessValidationError::InvalidSessionIdentity);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessResolvedContextPackReceiptV1 {
    pub id: HarnessSelectorV1,
    pub digest: String,
    pub lineage: HarnessContextPackLineageV1,
    pub source_message_count: u64,
    pub retained_message_count: u64,
    pub byte_len: u32,
    pub truncated: bool,
}

impl HarnessResolvedContextPackReceiptV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.id.validate()?;
        validate_context_digest(&self.digest)?;
        self.lineage.validate()?;
        if self.source_message_count == 0
            || self.retained_message_count == 0
            || self.retained_message_count > self.source_message_count
            || self.retained_message_count > HARNESS_CONTEXT_PACK_RETAINED_MESSAGES_MAX
            || self.byte_len == 0
            || self.byte_len > HARNESS_CONTEXT_PACK_MAX_BYTES
            || self.truncated != (self.source_message_count > self.retained_message_count)
        {
            return Err(HarnessValidationError::InvalidContextPackReceipt);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessContinuationV1 {
    pub continuation_ref: HarnessContinuationRef,
    pub receipt_ref: HarnessReceiptRef,
    pub revision: HarnessRevision,
    pub state: HarnessContinuationStateV1,
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub source_run_id: HarnessRunId,
    pub target_run_id: HarnessRunId,
    pub operation_id: HarnessOperationId,
    pub node_id: HarnessSelectorV1,
    pub node_incarnation: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub source_provider: HarnessSelectorV1,
    pub source_binding: HarnessSessionBindingV1,
    pub context: Option<HarnessResolvedContextPackReceiptV1>,
    pub target_binding: Option<HarnessSessionBindingV1>,
    pub prepared_at_unix_ms: u64,
    pub exporting_at_unix_ms: Option<u64>,
    pub exported_at_unix_ms: Option<u64>,
    pub bound_at_unix_ms: Option<u64>,
    pub expired_at_unix_ms: Option<u64>,
    pub outcome_unknown_at_unix_ms: Option<u64>,
    pub outcome_unknown_reason: Option<HarnessContinuationOutcomeUnknownReasonV1>,
    pub cleanup_state: HarnessContinuationCleanupStateV1,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl HarnessContinuationV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.continuation_ref.validate()?;
        self.receipt_ref.validate()?;
        self.revision.validate()?;
        self.grant_id.validate()?;
        self.grant_revision.validate()?;
        self.source_run_id.validate()?;
        self.target_run_id.validate()?;
        self.operation_id.validate()?;
        self.node_id.validate()?;
        self.node_incarnation.validate()?;
        self.workspace_id.validate()?;
        self.source_provider.validate()?;
        self.source_binding.validate()?;
        if let Some(context) = &self.context { context.validate()?; }
        if let Some(binding) = &self.target_binding { binding.validate()?; }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;
        for (field, value) in [
            ("prepared_at_unix_ms", Some(self.prepared_at_unix_ms)),
            ("exporting_at_unix_ms", self.exporting_at_unix_ms),
            ("exported_at_unix_ms", self.exported_at_unix_ms),
            ("bound_at_unix_ms", self.bound_at_unix_ms),
            ("expired_at_unix_ms", self.expired_at_unix_ms),
            ("outcome_unknown_at_unix_ms", self.outcome_unknown_at_unix_ms),
        ] {
            validate_optional_timestamp(field, value, self.created_at_unix_ms, self.updated_at_unix_ms)?;
        }
        let exact_route = self.source_binding.node_id == self.node_id
            && self.source_binding.node_incarnation == self.node_incarnation
            && self.source_binding.workspace_id == self.workspace_id;
        let timestamp_order = self.exporting_at_unix_ms
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
                && self.target_binding.is_none()
                && self.exporting_at_unix_ms.is_none()
                && self.exported_at_unix_ms.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::Exporting => self.context.is_none()
                && self.target_binding.is_none()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::Exported => self.context.is_some()
                && self.target_binding.is_none()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_some()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::Bound => self.context.is_some()
                && self.target_binding.is_some()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_some()
                && self.bound_at_unix_ms.is_some()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
            HarnessContinuationStateV1::OutcomeUnknown => self.target_binding.is_none()
                && self.exporting_at_unix_ms.is_some()
                && self.exported_at_unix_ms.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_none()
                && self.outcome_unknown_at_unix_ms.is_some()
                && self.outcome_unknown_reason.is_some(),
            HarnessContinuationStateV1::Expired => self.target_binding.is_none()
                && self.bound_at_unix_ms.is_none()
                && self.expired_at_unix_ms.is_some()
                && self.outcome_unknown_at_unix_ms.is_none()
                && self.outcome_unknown_reason.is_none(),
        };
        if self.source_run_id == self.target_run_id
            || !exact_route
            || !timestamp_order
            || !state_fields
            || self.prepared_at_unix_ms != self.created_at_unix_ms
            || self.cleanup_state != HarnessContinuationCleanupStateV1::Retained
        {
            return Err(HarnessValidationError::InvalidContinuationLink);
        }
        if let Some(context) = &self.context {
            if context.lineage.source_node_id != self.node_id
                || context.lineage.source_workspace_id != self.workspace_id
                || context.lineage.source_provider != self.source_provider
            {
                return Err(HarnessValidationError::InvalidContinuationLink);
            }
        }
        if let Some(binding) = &self.target_binding {
            if binding.node_id != self.node_id
                || binding.node_incarnation != self.node_incarnation
                || binding.workspace_id != self.workspace_id
            {
                return Err(HarnessValidationError::InvalidContinuationLink);
            }
        }
        Ok(())
    }
}

impl HarnessDeliveryV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.delivery_ref.validate()?;
        self.revision.validate()?;
        self.grant_id.validate()?;
        self.grant_revision.validate()?;
        self.task_id.validate()?;
        self.run_id.validate()?;
        self.operation_id.validate()?;
        self.bundle.validate()?;
        if let Some(stage) = &self.stage_receipt { stage.validate()?; }
        if let Some(receipt) = &self.receipt { receipt.validate()?; }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;
        let fields_match = match self.state {
            HarnessDeliveryStateV1::Prepared => {
                self.stage_receipt.is_none() && self.receipt.is_none()
            }
            HarnessDeliveryStateV1::Staged => {
                self.stage_receipt.is_some() && self.receipt.is_none()
            }
            HarnessDeliveryStateV1::Committed => {
                self.stage_receipt.is_some() && self.receipt.is_some()
            }
        };
        if !fields_match {
            return Err(HarnessValidationError::InvalidDeliveryStateFields);
        }
        if let Some(stage) = &self.stage_receipt {
            if stage.bundle != self.bundle
                || stage.staged_at_unix_ms < self.created_at_unix_ms
                || stage.staged_at_unix_ms > self.updated_at_unix_ms
            {
                return Err(HarnessValidationError::InvalidDeliveryLink);
            }
        }
        if let Some(receipt) = &self.receipt {
            let staged_at_unix_ms = self.stage_receipt.as_ref()
                .map(|stage| stage.staged_at_unix_ms)
                .ok_or(HarnessValidationError::InvalidDeliveryLink)?;
            if receipt.delivery_ref != self.delivery_ref
                || receipt.grant_id != self.grant_id
                || receipt.grant_revision != self.grant_revision
                || receipt.task_id != self.task_id
                || receipt.run_id != self.run_id
                || receipt.operation_id != self.operation_id
                || receipt.bundle != self.bundle
                || receipt.committed_at_unix_ms < staged_at_unix_ms
                || receipt.committed_at_unix_ms < self.created_at_unix_ms
                || receipt.committed_at_unix_ms > self.updated_at_unix_ms
            {
                return Err(HarnessValidationError::InvalidDeliveryLink);
            }
        }
        Ok(())
    }
}

impl HarnessSessionBindingV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.node_id.validate()?;
        self.node_incarnation.validate()?;
        self.workspace_id.validate()?;
        self.session.validate()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRunLifecycleV1 {
    Requested,
    Preparing,
    Dispatching,
    OutcomeUnknown,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessResultDispositionV1 {
    Succeeded,
    Failed,
    Cancelled,
    Detached,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessFailureCategoryV1 {
    Validation,
    Conflict,
    PermissionDenied,
    TargetUnavailable,
    Transport,
    Timeout,
    Rejected,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessFailureV1 {
    pub category: HarnessFailureCategoryV1,
    pub retryable: bool,
}

impl HarnessFailureV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRunV1 {
    pub run_id: HarnessRunId,
    pub revision: HarnessRevision,
    pub parent_run_id: Option<HarnessRunId>,
    pub task_id: HarnessTaskId,
    pub operation_id: HarnessOperationId,
    pub intent: HarnessRunIntentV1,
    pub delivery_receipt: Option<HarnessReceiptRef>,
    pub continuation_receipt: Option<HarnessReceiptRef>,
    pub binding: Option<HarnessSessionBindingV1>,
    pub lifecycle: HarnessRunLifecycleV1,
    pub result_disposition: Option<HarnessResultDispositionV1>,
    pub failure: Option<HarnessFailureV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl HarnessRunV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.run_id.validate()?;
        self.revision.validate()?;
        if self.parent_run_id.as_ref() == Some(&self.run_id) {
            return Err(HarnessValidationError::SelfLink { field: "parent_run_id" });
        }
        self.task_id.validate()?;
        self.operation_id.validate()?;
        self.intent.validate()?;
        if let Some(receipt) = &self.delivery_receipt { receipt.validate()?; }
        if let Some(receipt) = &self.continuation_receipt { receipt.validate()?; }
        if let Some(binding) = &self.binding {
            binding.validate()?;
            if binding.node_id != self.intent.node_id
                || binding.workspace_id != self.intent.workspace_id
            {
                return Err(HarnessValidationError::BindingIntentMismatch);
            }
        }
        if let Some(failure) = &self.failure { failure.validate()?; }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;

        let needs_binding = matches!(self.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting | HarnessRunLifecycleV1::Completed);
        if needs_binding && self.binding.is_none() {
            return Err(HarnessValidationError::MissingFieldForState { field: "binding" });
        }
        if matches!(
            self.lifecycle,
            HarnessRunLifecycleV1::Requested
                | HarnessRunLifecycleV1::Preparing
                | HarnessRunLifecycleV1::Dispatching
                | HarnessRunLifecycleV1::OutcomeUnknown
        ) && self.binding.is_some()
        {
            return Err(HarnessValidationError::InvalidBindingForState);
        }
        if matches!(self.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting)
            && matches!(
                self.binding.as_ref().map(|binding| &binding.session),
                Some(HarnessSessionIdentityV1::Managed {
                    active_session: None,
                    ..
                })
            )
        {
            return Err(HarnessValidationError::MissingFieldForState {
                field: "binding.active_session",
            });
        }
        let disposition_valid = match self.lifecycle {
            HarnessRunLifecycleV1::Completed => matches!(self.result_disposition, Some(HarnessResultDispositionV1::Succeeded | HarnessResultDispositionV1::Detached)),
            HarnessRunLifecycleV1::Failed => self.result_disposition == Some(HarnessResultDispositionV1::Failed),
            HarnessRunLifecycleV1::Cancelled => self.result_disposition == Some(HarnessResultDispositionV1::Cancelled),
            _ => self.result_disposition.is_none(),
        };
        if !disposition_valid {
            return Err(HarnessValidationError::InvalidResultDisposition);
        }
        if matches!(self.lifecycle, HarnessRunLifecycleV1::Failed) != self.failure.is_some() {
            return Err(HarnessValidationError::InvalidFailureForState);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionGrantStateV1 { Active, Revoked }

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessMonitoringVisibilityV1 { None, Summary, Detail, Timeline }

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
/// Entity-local read authority rooted at the grant's actor run.
///
/// `SelfOnly` means the actor run itself (or an entity directly attributed to
/// it); `Descendants` includes the actor run and its run-tree descendants.
/// Scopes for tasks, runs, and operations are independent and never widen one
/// another through stored cross-entity links.
pub enum HarnessEntityReadScopeV1 { None, SelfOnly, Descendants }

impl Default for HarnessEntityReadScopeV1 {
    fn default() -> Self { Self::None }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// Independent object-level read scopes. A visible object's references do not
/// themselves authorize reading the referenced objects.
pub struct HarnessReadPermissionsV1 {
    pub tasks: HarnessEntityReadScopeV1,
    pub runs: HarnessEntityReadScopeV1,
    pub operations: HarnessEntityReadScopeV1,
}

impl HarnessReadPermissionsV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> { Ok(()) }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTaskPermissionsV1 {
    pub read: bool,
    pub create: bool,
    pub mutate: bool,
    pub request_run: bool,
}

impl HarnessTaskPermissionsV1 { pub fn validate(&self) -> Result<(), HarnessValidationError> { Ok(()) } }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessContextPermissionsV1 { pub export: bool, pub restore: bool }

impl HarnessContextPermissionsV1 { pub fn validate(&self) -> Result<(), HarnessValidationError> { Ok(()) } }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperationTimeoutsV1 {
    pub dispatch_ms: u64,
    pub wait_ms: u64,
    pub reconciliation_ms: u64,
}

impl HarnessOperationTimeoutsV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        for (field, value) in [("dispatch_ms", self.dispatch_ms), ("wait_ms", self.wait_ms), ("reconciliation_ms", self.reconciliation_ms)] {
            if !(HARNESS_TIMEOUT_MIN_MS..=HARNESS_TIMEOUT_MAX_MS).contains(&value) {
                return Err(HarnessValidationError::InvalidTimeout { field });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessGrantTargetV1 {
    pub node_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub provider_profile: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
}

impl HarnessGrantTargetV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.node_id.validate()?;
        self.workspace_id.validate()?;
        self.provider_profile.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGrantV1 {
    pub grant_id: SessionGrantId,
    pub revision: HarnessRevision,
    pub actor_run_id: HarnessRunId,
    pub allowed_targets: Vec<HarnessGrantTargetV1>,
    pub allowed_delivery_bundles: Vec<HarnessSelectorV1>,
    pub maximum_child_count: u16,
    pub maximum_child_depth: u16,
    pub operation_timeouts: HarnessOperationTimeoutsV1,
    pub task_permissions: HarnessTaskPermissionsV1,
    #[serde(default)]
    pub read_permissions: HarnessReadPermissionsV1,
    pub monitoring_visibility: HarnessMonitoringVisibilityV1,
    pub context_permissions: HarnessContextPermissionsV1,
    pub state: SessionGrantStateV1,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl SessionGrantV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.grant_id.validate()?;
        self.revision.validate()?;
        self.actor_run_id.validate()?;
        validate_sorted_ids("allowed_targets", &self.allowed_targets, HARNESS_ALLOWED_TARGETS_MAX)?;
        for target in &self.allowed_targets {
            target.validate()?;
        }
        validate_sorted_ids("allowed_delivery_bundles", &self.allowed_delivery_bundles, HARNESS_ALLOWED_TARGETS_MAX)?;
        if self.allowed_targets.is_empty() {
            return Err(HarnessValidationError::EmptyGrantScope);
        }
        if self.maximum_child_count > HARNESS_CHILD_COUNT_MAX {
            return Err(HarnessValidationError::InvalidChildLimit { field: "maximum_child_count" });
        }
        if self.maximum_child_depth > HARNESS_CHILD_DEPTH_MAX {
            return Err(HarnessValidationError::InvalidChildLimit { field: "maximum_child_depth" });
        }
        self.operation_timeouts.validate()?;
        self.task_permissions.validate()?;
        self.read_permissions.validate()?;
        if !self.task_permissions.read
            && self.read_permissions.tasks != HarnessEntityReadScopeV1::None
        {
            return Err(HarnessValidationError::ReadScopeRequiresTaskRead);
        }
        self.context_permissions.validate()?;
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)
    }

    pub fn allows_target(
        &self,
        node_id: &HarnessSelectorV1,
        workspace_id: &HarnessSelectorV1,
        provider_profile: &HarnessSelectorV1,
        mode: HarnessExecutionModeV1,
    ) -> bool {
        if self.state != SessionGrantStateV1::Active {
            return false;
        }
        self.allowed_targets.binary_search_by(|target| {
            (
                &target.node_id,
                &target.workspace_id,
                &target.provider_profile,
                target.mode,
            )
                .cmp(&(node_id, workspace_id, provider_profile, mode))
        }).is_ok()
    }

    pub fn allows_delivery_bundle(&self, selector: &HarnessSelectorV1) -> bool {
        self.state == SessionGrantStateV1::Active
            && self.allowed_delivery_bundles.binary_search(selector).is_ok()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOperationKindV1 {
    CreateTask,
    MutateTask,
    CreateRun,
    BindRun,
    MutateRun,
    CreateGrant,
    MutateGrant,
    RevokeGrant,
    Reconcile,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOperationStateV1 { Prepared, Dispatching, OutcomeUnknown, Succeeded, Failed, Reconciled }

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOutcomeUnknownReasonV1 { TransportClosed, Timeout, ReplyLost }

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessReconciliationOutcomeV1 { Succeeded, Failed, NoEffect }

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessRequestDigest(String);

impl HarnessRequestDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessValidationError> {
        let value = value.into();
        validate_lower_hex("request digest", &value, 64)?;
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str { &self.0 }
    pub fn validate(&self) -> Result<(), HarnessValidationError> { validate_lower_hex("request digest", &self.0, 64) }
}

impl fmt::Debug for HarnessRequestDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.debug_tuple("HarnessRequestDigest").field(&self.0).finish() }
}

impl<'de> Deserialize<'de> for HarnessRequestDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: Deserializer<'de> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperationV1 {
    pub operation_id: HarnessOperationId,
    pub revision: HarnessRevision,
    pub actor: HarnessActorV1,
    pub kind: HarnessOperationKindV1,
    pub state: HarnessOperationStateV1,
    pub task_id: Option<HarnessTaskId>,
    pub run_id: Option<HarnessRunId>,
    pub grant_id: Option<SessionGrantId>,
    pub reconciles_operation_id: Option<HarnessOperationId>,
    pub expected_revision: Option<HarnessRevision>,
    pub request_digest: HarnessRequestDigest,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub failure: Option<HarnessFailureV1>,
    pub outcome_unknown_reason: Option<HarnessOutcomeUnknownReasonV1>,
    pub reconciliation_outcome: Option<HarnessReconciliationOutcomeV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub dispatched_at_unix_ms: Option<u64>,
    pub finished_at_unix_ms: Option<u64>,
}

impl HarnessOperationV1 {
    pub fn validate(&self) -> Result<(), HarnessValidationError> {
        self.operation_id.validate()?;
        self.revision.validate()?;
        self.actor.validate()?;
        if let Some(task_id) = &self.task_id { task_id.validate()?; }
        if let Some(run_id) = &self.run_id { run_id.validate()?; }
        if let Some(grant_id) = &self.grant_id { grant_id.validate()?; }
        if let Some(operation_id) = &self.reconciles_operation_id {
            operation_id.validate()?;
            if operation_id == &self.operation_id {
                return Err(HarnessValidationError::SelfLink { field: "reconciles_operation_id" });
            }
        }
        if let Some(revision) = self.expected_revision { revision.validate()?; }
        self.request_digest.validate()?;
        self.idempotency_ref.validate()?;
        if let Some(failure) = &self.failure { failure.validate()?; }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;
        validate_optional_timestamp("dispatched_at_unix_ms", self.dispatched_at_unix_ms, self.created_at_unix_ms, self.updated_at_unix_ms)?;
        validate_optional_timestamp("finished_at_unix_ms", self.finished_at_unix_ms, self.created_at_unix_ms, self.updated_at_unix_ms)?;
        if matches!((self.dispatched_at_unix_ms, self.finished_at_unix_ms), (Some(dispatched), Some(finished)) if finished < dispatched) {
            return Err(HarnessValidationError::InvalidOperationStateFields);
        }
        self.validate_kind_targets()?;

        match self.state {
            HarnessOperationStateV1::Prepared => {
                require_state_fields(self.dispatched_at_unix_ms.is_none() && self.finished_at_unix_ms.is_none() && self.failure.is_none() && self.outcome_unknown_reason.is_none() && self.reconciliation_outcome.is_none())?;
            }
            HarnessOperationStateV1::Dispatching => {
                require_state_fields(self.dispatched_at_unix_ms.is_some() && self.finished_at_unix_ms.is_none() && self.failure.is_none() && self.outcome_unknown_reason.is_none() && self.reconciliation_outcome.is_none())?;
            }
            HarnessOperationStateV1::OutcomeUnknown => {
                require_state_fields(self.dispatched_at_unix_ms.is_some() && self.finished_at_unix_ms.is_none() && self.failure.is_none() && self.outcome_unknown_reason.is_some() && self.reconciliation_outcome.is_none())?;
            }
            HarnessOperationStateV1::Succeeded => {
                require_state_fields(self.finished_at_unix_ms.is_some() && self.failure.is_none() && self.outcome_unknown_reason.is_none() && self.reconciliation_outcome.is_none())?;
                if self.kind == HarnessOperationKindV1::CreateRun && self.dispatched_at_unix_ms.is_none() {
                    return Err(HarnessValidationError::InvalidOperationStateFields);
                }
            }
            HarnessOperationStateV1::Failed => {
                require_state_fields(self.finished_at_unix_ms.is_some() && self.failure.is_some() && self.outcome_unknown_reason.is_none() && self.reconciliation_outcome.is_none())?;
            }
            HarnessOperationStateV1::Reconciled => {
                require_state_fields(self.dispatched_at_unix_ms.is_some() && self.finished_at_unix_ms.is_some() && self.outcome_unknown_reason.is_none() && self.reconciliation_outcome.is_some())?;
                if self.reconciliation_outcome == Some(HarnessReconciliationOutcomeV1::Failed) && self.failure.is_none() {
                    return Err(HarnessValidationError::InvalidFailureForState);
                }
                if self.reconciliation_outcome != Some(HarnessReconciliationOutcomeV1::Failed) && self.failure.is_some() {
                    return Err(HarnessValidationError::InvalidFailureForState);
                }
            }
        }
        Ok(())
    }

    fn validate_kind_targets(&self) -> Result<(), HarnessValidationError> {
        let valid = match self.kind {
            HarnessOperationKindV1::CreateTask => self.task_id.is_some() && self.run_id.is_none() && self.grant_id.is_none() && self.reconciles_operation_id.is_none() && self.expected_revision.is_none(),
            HarnessOperationKindV1::MutateTask => self.task_id.is_some() && self.run_id.is_none() && self.grant_id.is_none() && self.reconciles_operation_id.is_none() && self.expected_revision.is_some(),
            HarnessOperationKindV1::CreateRun => self.task_id.is_some() && self.run_id.is_some() && self.grant_id.is_none() && self.reconciles_operation_id.is_none() && self.expected_revision.is_some(),
            HarnessOperationKindV1::BindRun | HarnessOperationKindV1::MutateRun => self.task_id.is_none() && self.run_id.is_some() && self.grant_id.is_none() && self.reconciles_operation_id.is_none() && self.expected_revision.is_some(),
            HarnessOperationKindV1::CreateGrant => self.task_id.is_none() && self.run_id.is_none() && self.grant_id.is_some() && self.reconciles_operation_id.is_none() && self.expected_revision.is_none(),
            HarnessOperationKindV1::MutateGrant | HarnessOperationKindV1::RevokeGrant => self.task_id.is_none() && self.run_id.is_none() && self.grant_id.is_some() && self.reconciles_operation_id.is_none() && self.expected_revision.is_some(),
            HarnessOperationKindV1::Reconcile => self.task_id.is_none() && self.run_id.is_some() && self.grant_id.is_none() && self.reconciles_operation_id.is_some() && self.expected_revision.is_some(),
        };
        if !valid {
            return Err(HarnessValidationError::InvalidOperationTarget);
        }
        Ok(())
    }
}

fn validate_prefixed_hex_id(label: &'static str, value: &str, prefix: &str, hex_len: usize) -> Result<(), HarnessValidationError> {
    let Some(hex) = value.strip_prefix(prefix) else { return Err(HarnessValidationError::InvalidOpaqueId { field: label }); };
    validate_lower_hex(label, hex, hex_len)
}

fn validate_lower_hex(label: &'static str, value: &str, expected_len: usize) -> Result<(), HarnessValidationError> {
    if value.len() != expected_len || !value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return Err(HarnessValidationError::InvalidOpaqueId { field: label });
    }
    Ok(())
}

fn validate_selector(value: &str) -> Result<(), HarnessValidationError> {
    if value.is_empty() || value.len() > HARNESS_SELECTOR_MAX_BYTES || !value.is_ascii()
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b'+'))
    {
        return Err(HarnessValidationError::InvalidSelector);
    }
    Ok(())
}

fn validate_delivery_resource_id(
    field: &'static str,
    value: &str,
) -> Result<(), HarnessValidationError> {
    if value.is_empty()
        || value.len() > HARNESS_DELIVERY_RESOURCE_MAX_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\')
        })
    {
        return Err(HarnessValidationError::InvalidDeliveryResource { field });
    }
    Ok(())
}

fn validate_delivery_digest(value: &str) -> Result<(), HarnessValidationError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(HarnessValidationError::InvalidDeliveryDigest);
    };
    validate_lower_hex("delivery bundle digest", digest, 64)
        .map_err(|_| HarnessValidationError::InvalidDeliveryDigest)
}

fn validate_context_digest(value: &str) -> Result<(), HarnessValidationError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(HarnessValidationError::InvalidContextPackDigest);
    };
    validate_lower_hex("context pack digest", digest, 64)
        .map_err(|_| HarnessValidationError::InvalidContextPackDigest)
}

fn validate_title(value: &str) -> Result<(), HarnessValidationError> {
    if value.trim() != value || value.is_empty() || value.len() > HARNESS_TITLE_MAX_BYTES || value.chars().any(char::is_control) {
        return Err(HarnessValidationError::InvalidTitle);
    }
    Ok(())
}

fn validate_body(value: &str) -> Result<(), HarnessValidationError> {
    if value.len() > HARNESS_BODY_MAX_BYTES || value.chars().any(|character| character.is_control() && !matches!(character, '\n' | '\t')) {
        return Err(HarnessValidationError::InvalidBody);
    }
    Ok(())
}

fn validate_sorted_ids<T: Ord>(field: &'static str, values: &[T], maximum: usize) -> Result<(), HarnessValidationError> {
    if values.len() > maximum {
        return Err(HarnessValidationError::CollectionTooLarge { field, maximum });
    }
    if values.windows(2).any(|pair| pair[0].cmp(&pair[1]) != Ordering::Less) {
        return Err(HarnessValidationError::CollectionNotCanonical { field });
    }
    Ok(())
}

fn validate_timestamps(created: u64, updated: u64) -> Result<(), HarnessValidationError> {
    if created == 0 || updated == 0 || updated < created {
        return Err(HarnessValidationError::InvalidTimestamps);
    }
    Ok(())
}

fn validate_optional_timestamp(field: &'static str, value: Option<u64>, created: u64, updated: u64) -> Result<(), HarnessValidationError> {
    if value.is_some_and(|value| value < created || value > updated) {
        return Err(HarnessValidationError::InvalidOptionalTimestamp { field });
    }
    Ok(())
}

fn require_state_fields(valid: bool) -> Result<(), HarnessValidationError> {
    if !valid { return Err(HarnessValidationError::InvalidOperationStateFields); }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum HarnessValidationError {
    #[error("{field} is not a valid bounded opaque identifier")]
    InvalidOpaqueId { field: &'static str },
    #[error("harness revision must be nonzero")]
    ZeroRevision,
    #[error("harness selector is empty, unbounded, non-ASCII, or contains path/control syntax")]
    InvalidSelector,
    #[error("continuation selector must be an exact HarnessRunId")]
    InvalidContinuationSelector,
    #[error("task title is empty, unbounded, padded, or contains control characters")]
    InvalidTitle,
    #[error("task body is unbounded or contains prohibited control characters")]
    InvalidBody,
    #[error("{field} cannot link the record to itself")]
    SelfLink { field: &'static str },
    #[error("{field} exceeds its bounded maximum of {maximum}")]
    CollectionTooLarge { field: &'static str, maximum: usize },
    #[error("{field} must be strictly sorted and duplicate-free")]
    CollectionNotCanonical { field: &'static str },
    #[error("created/updated timestamps are zero or non-monotonic")]
    InvalidTimestamps,
    #[error("{field} is outside the record timestamp bounds")]
    InvalidOptionalTimestamp { field: &'static str },
    #[error("managed or Inline session identity is invalid")]
    InvalidSessionIdentity,
    #[error("{field} is required by the current state")]
    MissingFieldForState { field: &'static str },
    #[error("run result disposition does not match lifecycle")]
    InvalidResultDisposition,
    #[error("run session binding is not authoritative in the current lifecycle")]
    InvalidBindingForState,
    #[error("run session binding does not match its immutable node/workspace intent")]
    BindingIntentMismatch,
    #[error("categorical failure does not match lifecycle/state")]
    InvalidFailureForState,
    #[error("grant target scope must be explicit and nonempty")]
    EmptyGrantScope,
    #[error("{field} is outside the supported child bound")]
    InvalidChildLimit { field: &'static str },
    #[error("{field} is outside the supported timeout bound")]
    InvalidTimeout { field: &'static str },
    #[error("operation timestamps/failure/reconciliation fields do not match state")]
    InvalidOperationStateFields,
    #[error("operation kind, target identifiers, and expected revision do not form a valid mutation")]
    InvalidOperationTarget,
    #[error("object-level task read scope requires task read permission")]
    ReadScopeRequiresTaskRead,
    #[error("{field} is not a valid bounded delivery resource identifier")]
    InvalidDeliveryResource { field: &'static str },
    #[error("delivery bundle digest must be sha256 followed by lowercase hexadecimal")]
    InvalidDeliveryDigest,
    #[error("delivery state does not match staged and committed receipt fields")]
    InvalidDeliveryStateFields,
    #[error("delivery receipt does not match its immutable authority")]
    InvalidDeliveryLink,
    #[error("context pack digest must be sha256 followed by lowercase hexadecimal")]
    InvalidContextPackDigest,
    #[error("context pack receipt is empty, inconsistent, or exceeds protocol limits")]
    InvalidContextPackReceipt,
    #[error("continuation authority has invalid state fields or exact links")]
    InvalidContinuationLink,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_id(hex: char) -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{}", hex.to_string().repeat(24))).unwrap()
    }

    fn run_id(hex: char) -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", hex.to_string().repeat(24))).unwrap()
    }

    fn operation_id(hex: char) -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", hex.to_string().repeat(24))).unwrap()
    }

    fn grant_id(hex: char) -> SessionGrantId {
        SessionGrantId::new(format!("hgrant_{}", hex.to_string().repeat(24))).unwrap()
    }

    fn selector(value: &str) -> HarnessSelectorV1 {
        HarnessSelectorV1::new(value).unwrap()
    }

    fn valid_task() -> HarnessTaskV1 {
        HarnessTaskV1 {
            task_id: task_id('1'),
            revision: HarnessRevision::new(1).unwrap(),
            title: "Implement durable operation ledger".to_owned(),
            body: "Explicit harness state only.".to_owned(),
            creator: HarnessActorV1::User { actor_id: selector("operator") },
            parent_task_id: None,
            dependencies: vec![task_id('2')],
            state: HarnessTaskStateV1::Ready,
            run_ids: vec![],
            result_refs: vec![],
            artifact_refs: vec![],
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 1_000,
        }
    }

    fn valid_grant() -> SessionGrantV1 {
        SessionGrantV1 {
            grant_id: grant_id('3'),
            revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: run_id('4'),
            allowed_targets: vec![HarnessGrantTargetV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                provider_profile: selector("claude"),
                mode: HarnessExecutionModeV1::Pty,
            }],
            allowed_delivery_bundles: vec![],
            maximum_child_count: 0,
            maximum_child_depth: 0,
            operation_timeouts: HarnessOperationTimeoutsV1 {
                dispatch_ms: 30_000,
                wait_ms: 60_000,
                reconciliation_ms: 30_000,
            },
            task_permissions: HarnessTaskPermissionsV1 {
                read: true,
                create: false,
                mutate: false,
                request_run: false,
            },
            read_permissions: HarnessReadPermissionsV1::default(),
            monitoring_visibility: HarnessMonitoringVisibilityV1::Summary,
            context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
            state: SessionGrantStateV1::Active,
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 1_000,
        }
    }

    fn valid_run() -> HarnessRunV1 {
        HarnessRunV1 {
            run_id: run_id('4'),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id('1'),
            operation_id: operation_id('5'),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("claude"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 1_000,
        }
    }

    #[test]
    fn harness_task_id_and_bounds_reject_invalid() {
        assert!(HarnessTaskId::new("htask_123456789012345678901234").is_ok());
        assert!(HarnessTaskId::new("hrun_123456789012345678901234").is_err());
        assert!(HarnessTaskId::new("htask_12345678901234567890123").is_err());
        assert!(HarnessTaskId::new("htask_12345678901234567890123A").is_err());
        assert!(HarnessRunId::new("htask_123456789012345678901234").is_err());
        assert!(SessionGrantId::new("hgrant_123456789012345678901234").is_ok());
        assert!(HarnessOperationId::new("hop_abcdefabcdefabcdefabcdef").is_ok());
        assert!(HarnessRevision::new(0).is_err());

        let mut task = valid_task();
        task.title = "x".repeat(HARNESS_TITLE_MAX_BYTES + 1);
        assert_eq!(task.validate(), Err(HarnessValidationError::InvalidTitle));

        let mut task = valid_task();
        task.dependencies = vec![task_id('3'), task_id('2')];
        assert_eq!(
            task.validate(),
            Err(HarnessValidationError::CollectionNotCanonical { field: "dependencies" })
        );
    }

    #[test]
    fn harness_task_and_grant_validation_is_fail_closed() {
        let task = valid_task();
        task.validate().unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        assert!(encoded.get("observation").is_none());

        let mut self_linked = task.clone();
        self_linked.parent_task_id = Some(self_linked.task_id.clone());
        assert_eq!(
            self_linked.validate(),
            Err(HarnessValidationError::SelfLink { field: "parent_task_id" })
        );

        let grant = valid_grant();
        grant.validate().unwrap();
        let mut unscoped = grant.clone();
        unscoped.allowed_targets.clear();
        assert_eq!(unscoped.validate(), Err(HarnessValidationError::EmptyGrantScope));

        let mut noncanonical = grant.clone();
        noncanonical.allowed_targets = vec![
            HarnessGrantTargetV1 {
                node_id: selector("node-b"),
                workspace_id: selector("workspace-a"),
                provider_profile: selector("claude"),
                mode: HarnessExecutionModeV1::Pty,
            },
            HarnessGrantTargetV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-b"),
                provider_profile: selector("codex"),
                mode: HarnessExecutionModeV1::Inline,
            },
        ];
        assert_eq!(
            noncanonical.validate(),
            Err(HarnessValidationError::CollectionNotCanonical {
                field: "allowed_targets",
            })
        );

        let mut wire = serde_json::to_value(grant).unwrap();
        wire.as_object_mut().unwrap().insert("bearer_token".to_owned(), serde_json::json!("secret"));
        assert!(serde_json::from_value::<SessionGrantV1>(wire).is_err());

        let mut legacy_cross_product = serde_json::to_value(valid_grant()).unwrap();
        let object = legacy_cross_product.as_object_mut().unwrap();
        object.remove("allowed_targets");
        object.insert("allowed_node_ids".to_owned(), serde_json::json!(["node-a"]));
        object.insert(
            "allowed_workspace_ids".to_owned(),
            serde_json::json!(["workspace-a"]),
        );
        assert!(serde_json::from_value::<SessionGrantV1>(legacy_cross_product).is_err());

        let mut task_wire = serde_json::to_value(task).unwrap();
        task_wire.as_object_mut().unwrap().insert("monitoring_state".to_owned(), serde_json::json!("done"));
        assert!(serde_json::from_value::<HarnessTaskV1>(task_wire).is_err());
    }

    #[test]
    fn harness_grant_target_scope_is_exact_not_cross_product() {
        let mut grant = valid_grant();
        grant.allowed_targets = vec![
            HarnessGrantTargetV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                provider_profile: selector("claude"),
                mode: HarnessExecutionModeV1::Pty,
            },
            HarnessGrantTargetV1 {
                node_id: selector("node-b"),
                workspace_id: selector("workspace-b"),
                provider_profile: selector("codex"),
                mode: HarnessExecutionModeV1::Inline,
            },
        ];
        grant.validate().unwrap();

        assert!(grant.allows_target(
            &selector("node-a"),
            &selector("workspace-a"),
            &selector("claude"),
            HarnessExecutionModeV1::Pty,
        ));
        assert!(!grant.allows_target(
            &selector("node-a"),
            &selector("workspace-b"),
            &selector("claude"),
            HarnessExecutionModeV1::Pty,
        ));
        assert!(!grant.allows_target(
            &selector("node-a"),
            &selector("workspace-a"),
            &selector("codex"),
            HarnessExecutionModeV1::Inline,
        ));

        grant.state = SessionGrantStateV1::Revoked;
        assert!(!grant.allows_target(
            &selector("node-a"),
            &selector("workspace-a"),
            &selector("claude"),
            HarnessExecutionModeV1::Pty,
        ));
    }

    #[test]
    fn harness_read_permissions_are_legacy_safe_and_task_scope_is_bounded() {
        let grant = valid_grant();
        let mut legacy_wire = serde_json::to_value(&grant).unwrap();
        legacy_wire.as_object_mut().unwrap().remove("read_permissions");
        let decoded: SessionGrantV1 = serde_json::from_value(legacy_wire).unwrap();
        assert_eq!(decoded.read_permissions, HarnessReadPermissionsV1::default());
        assert_eq!(
            serde_json::to_value(&decoded.read_permissions).unwrap(),
            serde_json::json!({
                "tasks": "none",
                "runs": "none",
                "operations": "none",
            }),
        );

        let mut scoped = grant;
        scoped.task_permissions.read = false;
        scoped.read_permissions.tasks = HarnessEntityReadScopeV1::SelfOnly;
        assert_eq!(
            scoped.validate(),
            Err(HarnessValidationError::ReadScopeRequiresTaskRead),
        );
    }

    #[test]
    fn harness_unknown_run_cannot_claim_unverified_session_binding() {
        let mut run = valid_run();
        run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
        run.binding = Some(HarnessSessionBindingV1 {
            node_id: selector("node-a"),
            node_incarnation: selector("incarnation-a"),
            workspace_id: selector("workspace-a"),
            session: HarnessSessionIdentityV1::Managed {
                record_id: selector("record-a"),
                active_session: None,
            },
        });
        assert_eq!(run.validate(), Err(HarnessValidationError::InvalidBindingForState));

        run.binding = None;
        run.validate().unwrap();
    }

    #[test]
    fn harness_run_binding_must_match_immutable_target_intent() {
        let mut run = valid_run();
        run.lifecycle = HarnessRunLifecycleV1::Running;
        run.binding = Some(HarnessSessionBindingV1 {
            node_id: selector("node-b"),
            node_incarnation: selector("incarnation-a"),
            workspace_id: selector("workspace-a"),
            session: HarnessSessionIdentityV1::Managed {
                record_id: selector("record-a"),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 1,
                    generation: 1,
                }),
            },
        });
        assert_eq!(run.validate(), Err(HarnessValidationError::BindingIntentMismatch));

        run.binding.as_mut().unwrap().node_id = selector("node-a");
        run.validate().unwrap();
    }

    #[test]
    fn harness_operation_state_is_bounded_and_private() {
        let prepared = HarnessOperationV1 {
            operation_id: operation_id('5'),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User { actor_id: selector("operator") },
            kind: HarnessOperationKindV1::CreateTask,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task_id('6')),
            run_id: None,
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: None,
            request_digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{}", "b".repeat(24))).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 1_000,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        prepared.validate().unwrap();

        let json = serde_json::to_string(&prepared).unwrap();
        for private_name in ["prompt", "transcript", "credential", "bearer", "working_directory", "provider_home"] {
            assert!(!json.contains(private_name));
        }

        let mut unknown = prepared.clone();
        unknown.state = HarnessOperationStateV1::OutcomeUnknown;
        unknown.updated_at_unix_ms = 1_100;
        unknown.dispatched_at_unix_ms = Some(1_050);
        unknown.outcome_unknown_reason = Some(HarnessOutcomeUnknownReasonV1::ReplyLost);
        unknown.validate().unwrap();

        let mut invalid_failed = unknown.clone();
        invalid_failed.state = HarnessOperationStateV1::Failed;
        invalid_failed.finished_at_unix_ms = Some(1_100);
        invalid_failed.outcome_unknown_reason = None;
        assert_eq!(invalid_failed.validate(), Err(HarnessValidationError::InvalidOperationStateFields));

        let mut invalid_target = prepared.clone();
        invalid_target.run_id = Some(run_id('7'));
        assert_eq!(invalid_target.validate(), Err(HarnessValidationError::InvalidOperationTarget));

        let mut self_reconcile = prepared.clone();
        self_reconcile.kind = HarnessOperationKindV1::Reconcile;
        self_reconcile.task_id = None;
        self_reconcile.run_id = Some(run_id('7'));
        self_reconcile.reconciles_operation_id = Some(self_reconcile.operation_id.clone());
        self_reconcile.expected_revision = Some(HarnessRevision::new(1).unwrap());
        assert_eq!(
            self_reconcile.validate(),
            Err(HarnessValidationError::SelfLink { field: "reconciles_operation_id" })
        );

        let mut targetless_reconcile = self_reconcile.clone();
        targetless_reconcile.reconciles_operation_id = Some(operation_id('8'));
        targetless_reconcile.run_id = None;
        assert_eq!(targetless_reconcile.validate(), Err(HarnessValidationError::InvalidOperationTarget));

        let mut undispatched_create_run = prepared.clone();
        undispatched_create_run.kind = HarnessOperationKindV1::CreateRun;
        undispatched_create_run.run_id = Some(run_id('7'));
        undispatched_create_run.expected_revision = Some(HarnessRevision::new(1).unwrap());
        undispatched_create_run.state = HarnessOperationStateV1::Succeeded;
        undispatched_create_run.updated_at_unix_ms = 1_100;
        undispatched_create_run.finished_at_unix_ms = Some(1_100);
        assert_eq!(
            undispatched_create_run.validate(),
            Err(HarnessValidationError::InvalidOperationStateFields)
        );

        let mut dispatched_create_run = undispatched_create_run;
        dispatched_create_run.dispatched_at_unix_ms = Some(1_050);
        dispatched_create_run.validate().unwrap();

        let mut wire = serde_json::to_value(prepared).unwrap();
        wire.as_object_mut().unwrap().insert("raw_spawn_spec".to_owned(), serde_json::json!({"prompt":"private"}));
        assert!(serde_json::from_value::<HarnessOperationV1>(wire).is_err());
    }

    #[test]
    fn harness_delivery_receipts_are_exact_bounded_and_private() {
        let bundle = HarnessDeliveryBundleV1 {
            selector: selector("review-kit"),
            bundle_id: HarnessDeliveryBundleIdV1::new("bundle.review-kit").unwrap(),
            revision: HarnessDeliveryBundleRevisionV1::new("rev-7").unwrap(),
            digest: HarnessDeliveryBundleDigestV1::new(format!(
                "sha256:{}",
                "d".repeat(64),
            )).unwrap(),
            manifest_digest: HarnessDeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "e".repeat(64),
            )).unwrap(),
        };
        let delivery_ref = HarnessDeliveryRef::new(format!(
            "hdelivery_{}",
            "d".repeat(24),
        )).unwrap();
        let receipt = HarnessDeliveryReceiptV1 {
            receipt_ref: HarnessReceiptRef::new(format!(
                "hreceipt_{}",
                "e".repeat(24),
            )).unwrap(),
            delivery_ref: delivery_ref.clone(),
            grant_id: grant_id('4'),
            grant_revision: HarnessRevision::new(2).unwrap(),
            task_id: task_id('1'),
            run_id: run_id('2'),
            operation_id: operation_id('3'),
            binding: HarnessSessionBindingV1 {
                node_id: selector("node-a"),
                node_incarnation: selector("incarnation-a"),
                workspace_id: selector("workspace-a"),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: selector("record-a"),
                    active_session: Some(HarnessRuntimeIdentityV1 {
                        instance_id: 7,
                        generation: 3,
                    }),
                },
            },
            bundle: bundle.clone(),
            committed_at_unix_ms: 30,
        };
        let delivery = HarnessDeliveryV1 {
            delivery_ref,
            revision: HarnessRevision::new(3).unwrap(),
            grant_id: grant_id('4'),
            grant_revision: HarnessRevision::new(2).unwrap(),
            task_id: task_id('1'),
            run_id: run_id('2'),
            operation_id: operation_id('3'),
            bundle: bundle.clone(),
            state: HarnessDeliveryStateV1::Committed,
            stage_receipt: Some(HarnessDeliveryStageReceiptV1 {
                node_id: selector("node-a"),
                node_incarnation: selector("incarnation-a"),
                workspace_id: selector("workspace-a"),
                bundle,
                staged_at_unix_ms: 20,
            }),
            receipt: Some(receipt),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 30,
        };
        delivery.validate().unwrap();
        let wire = serde_json::to_string(&delivery).unwrap();
        for sentinel in ["prompt", "transcript", "bearer", "credential", "provider_home"] {
            assert!(!wire.contains(sentinel));
        }

        let mut changed = delivery;
        changed.receipt.as_mut().unwrap().bundle.manifest_digest =
            HarnessDeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "f".repeat(64),
            )).unwrap();
        assert_eq!(changed.validate(), Err(HarnessValidationError::InvalidDeliveryLink));
    }
}
