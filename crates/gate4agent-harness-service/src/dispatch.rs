use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessContinuationRef, HarnessDeliveryRef, HarnessDispatchIntentV1,
    HarnessExecutionModeV1, HarnessIdempotencyRef, HarnessLaunchAuthorityRefV1,
    HarnessLaunchPlanRefV1, HarnessOperationId, HarnessOperatorAuthorityV1, HarnessReceiptRef,
    HarnessRequestDigest, HarnessResultRef, HarnessRevision, HarnessRunId, HarnessRunIntentV1,
    HarnessScheduledLaunchRefV2, HarnessScheduleRequestV1, HarnessSelectorV1,
    HarnessSessionIdentityV1, HarnessTaskId, HarnessTaskStateV1, HarnessTaskV1,
    HarnessWorktreeIntentV1, SessionGrantId,
};
use gate4agent_c2_protocol::{C2ControlEventKind, C2NodeEvent, RoutedNodeEvent};
use gate4agent_node_protocol::{
    DeliveryComponentKindV2, DeliveryRelativePathV2, DeliveryScopeV2,
    HarnessMcpReservationId, NodeId, NodeIncarnationId, SessionMode, SpawnBundleId, SpawnBundleRevision,
    SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides, SpawnProfileId,
    SpawnProfileRevision, SpawnPrompt, SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceId,
};
use gate4agent_harness_delivery::{
    compile_reviewed_delivery_bundle_v2, DeliveryCatalogV2, ReviewedDeliverySourceV2,
};
use gate4agent_types::{AgentId, TerminalSize};
use gate4agent_node_wire::local_hmac_sha256;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use thiserror::Error;

pub const HARNESS_LAUNCH_CATALOG_MAX: usize = 128;
pub const HARNESS_LAUNCH_PLAN_JSON_MAX_BYTES: usize = 4 * 1024;
pub const HARNESS_DELIVERY_BUNDLE_JSON_MAX_BYTES: usize = 16 * 1024;
pub const HARNESS_DELIVERY_SOURCES_MAX: usize = 128;
const HARNESS_LAUNCH_PLAN_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-launch-plan-digest-v1";
const HARNESS_SCHEDULED_RUN_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-scheduled-run-id-v1\0";
const HARNESS_DELIVERY_REF_DOMAIN: &[u8] = b"gate4agent-harness-delivery-ref-v1\0";
const HARNESS_DELIVERY_RECEIPT_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-delivery-receipt-ref-v1\0";
const HARNESS_CONTINUATION_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-continuation-ref-v1\0";
const HARNESS_CONTINUATION_RECEIPT_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-continuation-receipt-ref-v1\0";
const HARNESS_MCP_RESERVATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-mcp-reservation-id-v1\0";
const HARNESS_LIFECYCLE_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-lifecycle-operation-id-v1\0";
const HARNESS_LIFECYCLE_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-lifecycle-idempotency-ref-v1\0";
const HARNESS_LIFECYCLE_REQUEST_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-lifecycle-request-digest-v1\0";
const HARNESS_CONTEXT_PACK_RECORD_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-context-pack-record-operation-id-v1\0";
const HARNESS_CONTEXT_PACK_RECORD_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-context-pack-record-idempotency-ref-v1\0";
const HARNESS_CONTEXT_PACK_RECORD_REQUEST_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-context-pack-record-request-digest-v1\0";
const HARNESS_GIT_FACTS_RECORD_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-git-facts-record-operation-id-v1\0";
const HARNESS_GIT_FACTS_RECORD_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-git-facts-record-idempotency-ref-v1\0";
const HARNESS_GIT_FACTS_RECORD_REQUEST_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-git-facts-record-request-digest-v1\0";
const HARNESS_RESULT_REF_RECORD_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-result-ref-record-operation-id-v1\0";
const HARNESS_RESULT_REF_RECORD_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-result-ref-record-idempotency-ref-v1\0";
const HARNESS_RESULT_REF_RECORD_REQUEST_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-result-ref-record-request-digest-v1\0";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessPromptSourceV1 {
    TaskBody,
    Clear,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessContinuationPolicyV1 {
    None,
    ParentRun,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessMcpPolicyV1 {
    #[default]
    Disabled,
    GrantBound,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliveryPolicyV1 {
    pub selector: HarnessSelectorV1,
    pub bundle_id: SpawnBundleId,
}

impl HarnessDeliveryPolicyV1 {
    pub fn validate(&self) -> Result<(), HarnessDispatchError> {
        self.selector.validate()?;
        SpawnBundleId::new(self.bundle_id.as_str())?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliverySourceConfigV1 {
    pub root: PathBuf,
    pub mount_path: Option<DeliveryRelativePathV2>,
    pub kind: DeliveryComponentKindV2,
    pub scope: DeliveryScopeV2,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HarnessDeliveryBundleConfigV1 {
    pub bundle_id: SpawnBundleId,
    pub revision: SpawnBundleRevision,
    pub sources: Vec<HarnessDeliverySourceConfigV1>,
}

pub fn compile_delivery_catalog_from_json_arguments(
    encoded_bundles: impl IntoIterator<Item = String>,
) -> Result<DeliveryCatalogV2, HarnessDispatchError> {
    let mut compiled = Vec::new();
    for encoded in encoded_bundles {
        if compiled.len() == HARNESS_LAUNCH_CATALOG_MAX {
            return Err(HarnessDispatchError::CatalogTooLarge);
        }
        if encoded.is_empty() || encoded.len() > HARNESS_DELIVERY_BUNDLE_JSON_MAX_BYTES {
            return Err(HarnessDispatchError::DeliveryJsonSize);
        }
        let config: HarnessDeliveryBundleConfigV1 = serde_json::from_str(&encoded)?;
        if config.sources.is_empty() || config.sources.len() > HARNESS_DELIVERY_SOURCES_MAX {
            return Err(HarnessDispatchError::DeliverySourceCount);
        }
        let sources = config.sources.into_iter().map(|source| {
            ReviewedDeliverySourceV2::new(
                source.root,
                source.mount_path,
                source.kind,
                source.scope,
            )
        }).collect::<Result<Vec<_>, _>>()?;
        compiled.push(compile_reviewed_delivery_bundle_v2(
            config.bundle_id,
            config.revision,
            &sources,
        )?);
    }
    Ok(DeliveryCatalogV2::new(compiled)?)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessGrantPolicyV1 {
    Operator,
    Exact {
        grant_id: SessionGrantId,
        revision: HarnessRevision,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessExactGrantRefV1 {
    pub grant_id: SessionGrantId,
    pub revision: HarnessRevision,
}

impl HarnessExactGrantRefV1 {
    pub fn validate(&self) -> Result<(), HarnessDispatchError> {
        self.grant_id.validate()?;
        self.revision.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessLaunchPlanV1 {
    pub plan_id: HarnessSelectorV1,
    pub revision: HarnessRevision,
    pub node_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub worktree: HarnessWorktreeIntentV1,
    pub provider_profile: HarnessSelectorV1,
    pub provider: AgentId,
    pub mode: HarnessExecutionModeV1,
    pub terminal_size: TerminalSize,
    pub prompt_source: HarnessPromptSourceV1,
    pub delivery: Option<HarnessDeliveryPolicyV1>,
    pub continuation: HarnessContinuationPolicyV1,
    pub grant: HarnessGrantPolicyV1,
    #[serde(default)]
    pub harness_mcp: HarnessMcpPolicyV1,
    pub deadline_ms: u64,
}

impl HarnessLaunchPlanV1 {
    pub fn is_ordinary_dispatch(&self) -> bool {
        self.delivery.is_none()
            && self.continuation == HarnessContinuationPolicyV1::None
            && self.harness_mcp == HarnessMcpPolicyV1::Disabled
    }

    pub fn validate(&self) -> Result<(), HarnessDispatchError> {
        self.plan_id.validate()?;
        self.revision.validate()?;
        self.node_id.validate()?;
        self.workspace_id.validate()?;
        self.worktree.validate()?;
        self.provider_profile.validate()?;
        if let Some(delivery) = &self.delivery {
            delivery.validate()?;
        }
        if let HarnessGrantPolicyV1::Exact { grant_id, revision } = &self.grant {
            HarnessExactGrantRefV1 {
                grant_id: grant_id.clone(),
                revision: *revision,
            }.validate()?;
        }
        if matches!(self.grant, HarnessGrantPolicyV1::Operator)
            && (self.delivery.is_some()
                || self.continuation != HarnessContinuationPolicyV1::None
                || self.harness_mcp != HarnessMcpPolicyV1::Disabled)
        {
            return Err(HarnessDispatchError::OperatorPrivilegedFlow);
        }
        if self.harness_mcp == HarnessMcpPolicyV1::GrantBound
            && !matches!(self.grant, HarnessGrantPolicyV1::Exact { .. })
        {
            return Err(HarnessDispatchError::HarnessMcpGrantRequired);
        }
        AgentId::new(self.provider.as_str())?;
        NodeId::new(self.node_id.as_str())?;
        WorkspaceId::new(self.workspace_id.as_str())?;
        SpawnProfileId::new(self.provider_profile.as_str())?;
        if let HarnessWorktreeIntentV1::Managed { worktree_ref } = &self.worktree {
            WorkspaceId::new(worktree_ref.as_str())?;
        }
        if self.terminal_size.rows == 0 || self.terminal_size.columns == 0 {
            return Err(HarnessDispatchError::InvalidTerminalSize);
        }
        SpawnDeadlineMs::new(self.deadline_ms)?;
        Ok(())
    }

    pub fn digest(&self) -> Result<HarnessRequestDigest, HarnessDispatchError> {
        self.validate()?;
        let canonical = serde_json::to_vec(self)?;
        let digest = local_hmac_sha256(HARNESS_LAUNCH_PLAN_DIGEST_DOMAIN, &canonical)
            .map_err(HarnessDispatchError::Digest)?;
        let mut encoded = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(HarnessRequestDigest::new(encoded)?)
    }

    pub fn plan_ref(&self) -> Result<HarnessLaunchPlanRefV1, HarnessDispatchError> {
        Ok(HarnessLaunchPlanRefV1 {
            plan_id: self.plan_id.clone(),
            revision: self.revision,
            digest: self.digest()?,
        })
    }

    pub fn scheduled_ref(&self) -> Result<HarnessScheduledLaunchRefV1, HarnessDispatchError> {
        let grant = match &self.grant {
            HarnessGrantPolicyV1::Operator => None,
            HarnessGrantPolicyV1::Exact { grant_id, revision } => Some(HarnessExactGrantRefV1 {
                grant_id: grant_id.clone(),
                revision: *revision,
            }),
        };
        Ok(HarnessScheduledLaunchRefV1 { plan: self.plan_ref()?, grant })
    }

    pub fn ordinary_scheduled_ref(
        &self,
    ) -> Result<HarnessScheduledLaunchRefV2, HarnessDispatchError> {
        self.validate()?;
        if !self.is_ordinary_dispatch()
            || !matches!(self.grant, HarnessGrantPolicyV1::Operator)
        {
            return Err(HarnessDispatchError::OperatorPrivilegedFlow);
        }
        Ok(HarnessScheduledLaunchRefV2 {
            plan: self.plan_ref()?,
            authority: HarnessLaunchAuthorityRefV1::OrdinaryOperator,
        })
    }

    pub fn validate_intent(
        &self,
        intent: &HarnessRunIntentV1,
    ) -> Result<(), HarnessDispatchError> {
        self.validate()?;
        intent.validate()?;
        if self.node_id != intent.node_id
            || self.workspace_id != intent.workspace_id
            || self.worktree != intent.worktree
            || self.provider_profile != intent.provider_profile
            || self.mode != intent.mode
            || self.delivery.as_ref().map(|delivery| &delivery.selector)
                != intent.delivery_bundle.as_ref()
        {
            return Err(HarnessDispatchError::IntentMismatch);
        }
        Ok(())
    }

    pub fn run_authority_and_intent(
        &self,
        task: &HarnessTaskV1,
    ) -> Result<(HarnessActorV1, Option<HarnessRunId>, HarnessRunIntentV1), HarnessDispatchError> {
        self.validate()?;
        task.validate()?;
        let (actor, parent_run_id) = match (&task.creator, &self.grant) {
            (HarnessActorV1::User { .. }, HarnessGrantPolicyV1::Operator) => {
                (task.creator.clone(), None)
            }
            (HarnessActorV1::ParentRun { run_id }, HarnessGrantPolicyV1::Exact { .. }) => {
                (task.creator.clone(), Some(run_id.clone()))
            }
            _ => return Err(HarnessDispatchError::GrantAuthorityMismatch),
        };
        let continuation = match self.continuation {
            HarnessContinuationPolicyV1::None => None,
            HarnessContinuationPolicyV1::ParentRun => {
                let parent = parent_run_id.as_ref()
                    .ok_or(HarnessDispatchError::ContinuationAuthorityMismatch)?;
                Some(HarnessSelectorV1::new(parent.as_str())?)
            }
        };
        let intent = HarnessRunIntentV1 {
            node_id: self.node_id.clone(),
            workspace_id: self.workspace_id.clone(),
            worktree: self.worktree.clone(),
            provider_profile: self.provider_profile.clone(),
            mode: self.mode,
            delivery_bundle: self.delivery.as_ref()
                .map(|delivery| delivery.selector.clone()),
            continuation,
        };
        intent.validate()?;
        Ok((actor, parent_run_id, intent))
    }

    pub fn spawn_spec(
        &self,
        dispatch: &HarnessDispatchIntentV1,
        task: &HarnessTaskV1,
        expected_profile_revision: SpawnProfileRevision,
    ) -> Result<SpawnSpec, HarnessDispatchError> {
        dispatch.validate()?;
        task.validate()?;
        self.validate_intent(&dispatch.intent)?;
        let grant_matches = match self.grant {
            HarnessGrantPolicyV1::Operator => dispatch.parent_run_id.is_none(),
            HarnessGrantPolicyV1::Exact { .. } => dispatch.parent_run_id.is_some(),
        };
        let continuation_matches = match self.continuation {
            HarnessContinuationPolicyV1::None => dispatch.intent.continuation.is_none(),
            HarnessContinuationPolicyV1::ParentRun => {
                dispatch.parent_run_id.as_ref().is_some_and(|parent_run_id| {
                    dispatch.intent.continuation.as_ref().is_some_and(|continuation| {
                        continuation.as_str() == parent_run_id.as_str()
                    })
                })
            }
        };
        if dispatch.task_id != task.task_id
            || dispatch.task_revision != task.revision
            || task.state != HarnessTaskStateV1::Running
            || task.run_ids.binary_search(&dispatch.run_id).is_err()
            || !grant_matches
            || !continuation_matches
        {
            return Err(HarnessDispatchError::TaskMismatch);
        }
        let prompt = match self.prompt_source {
            HarnessPromptSourceV1::TaskBody => SpawnOverride::Set {
                value: SpawnPrompt::new(task.body.clone())?,
            },
            HarnessPromptSourceV1::Clear => SpawnOverride::Clear,
        };
        let worktree_id = match &self.worktree {
            HarnessWorktreeIntentV1::Existing => None,
            HarnessWorktreeIntentV1::Managed { worktree_ref } => {
                Some(WorkspaceId::new(worktree_ref.as_str())?)
            }
            HarnessWorktreeIntentV1::ManagedProfile { .. } => {
                return Err(HarnessDispatchError::SpecializedDispatchUnavailable);
            }
        };
        Ok(SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new(self.node_id.as_str())?,
                workspace_id: WorkspaceId::new(self.workspace_id.as_str())?,
                worktree_id,
            },
            profile_id: SpawnProfileId::new(self.provider_profile.as_str())?,
            expected_profile_revision,
            overrides: SpawnOverrides {
                provider: SpawnOverride::Set { value: self.provider.clone() },
                mode: SpawnOverride::Set { value: execution_mode(self.mode) },
                terminal_size: SpawnOverride::Set { value: self.terminal_size },
                prompt,
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Clear,
                environment_profile_id: SpawnOverride::Clear,
            },
            deadline_ms: SpawnDeadlineMs::new(self.deadline_ms)?,
            idempotency_key: spawn_idempotency_key(&dispatch.idempotency_ref)?,
            required_capabilities: SpawnRequiredCapabilities::default(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessScheduledLaunchRefV1 {
    pub plan: HarnessLaunchPlanRefV1,
    pub grant: Option<HarnessExactGrantRefV1>,
}

impl HarnessScheduledLaunchRefV1 {
    pub fn validate(&self) -> Result<(), HarnessDispatchError> {
        self.plan.validate()?;
        if let Some(grant) = &self.grant { grant.validate()?; }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct HarnessLaunchCatalog {
    plans: BTreeMap<HarnessSelectorV1, HarnessLaunchPlanV1>,
}

impl HarnessLaunchCatalog {
    pub fn new(
        plans: impl IntoIterator<Item = HarnessLaunchPlanV1>,
    ) -> Result<Self, HarnessDispatchError> {
        let mut catalog = Self::default();
        for plan in plans {
            if catalog.plans.len() == HARNESS_LAUNCH_CATALOG_MAX {
                return Err(HarnessDispatchError::CatalogTooLarge);
            }
            plan.validate()?;
            let plan_id = plan.plan_id.clone();
            if catalog.plans.insert(plan_id.clone(), plan).is_some() {
                return Err(HarnessDispatchError::DuplicatePlan(plan_id));
            }
        }
        Ok(catalog)
    }

    pub fn from_json_arguments(
        encoded_plans: impl IntoIterator<Item = String>,
    ) -> Result<Self, HarnessDispatchError> {
        let mut plans = Vec::new();
        for encoded in encoded_plans {
            if plans.len() == HARNESS_LAUNCH_CATALOG_MAX {
                return Err(HarnessDispatchError::CatalogTooLarge);
            }
            if encoded.is_empty() || encoded.len() > HARNESS_LAUNCH_PLAN_JSON_MAX_BYTES {
                return Err(HarnessDispatchError::PlanJsonSize);
            }
            plans.push(serde_json::from_str::<HarnessLaunchPlanV1>(&encoded)?);
        }
        Self::new(plans)
    }

    pub fn is_empty(&self) -> bool { self.plans.is_empty() }

    pub fn len(&self) -> usize { self.plans.len() }

    pub fn supports_ordinary_coordinator(&self) -> bool {
        self.plans.values().all(HarnessLaunchPlanV1::is_ordinary_dispatch)
    }

    pub(crate) fn ordinary_plans(
        &self,
    ) -> impl Iterator<Item = &HarnessLaunchPlanV1> {
        self.plans.values().filter(|plan| {
            plan.is_ordinary_dispatch()
                && matches!(plan.grant, HarnessGrantPolicyV1::Operator)
        })
    }

    pub fn validate_delivery_catalog(
        &self,
        delivery_catalog: &DeliveryCatalogV2,
    ) -> Result<(), HarnessDispatchError> {
        for plan in self.plans.values() {
            if let Some(delivery) = &plan.delivery {
                if delivery_catalog.get(&delivery.bundle_id).is_none() {
                    return Err(HarnessDispatchError::DeliveryBundleMissing(
                        delivery.bundle_id.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn plan_ref(
        &self,
        plan_id: &HarnessSelectorV1,
    ) -> Result<HarnessLaunchPlanRefV1, HarnessDispatchError> {
        self.plans.get(plan_id)
            .ok_or_else(|| HarnessDispatchError::PlanMissing(plan_id.clone()))?
            .plan_ref()
    }

    pub fn select(
        &self,
        plan_id: Option<&HarnessSelectorV1>,
    ) -> Result<&HarnessLaunchPlanV1, HarnessDispatchError> {
        match plan_id {
            Some(plan_id) => self.plans.get(plan_id)
                .ok_or_else(|| HarnessDispatchError::PlanMissing(plan_id.clone())),
            None if self.plans.len() == 1 => Ok(self.plans.values().next()
                .expect("one-plan catalog has one value")),
            None => Err(HarnessDispatchError::PlanSelectionRequired),
        }
    }

    pub fn sole_plan_ref(&self) -> Result<Option<HarnessLaunchPlanRefV1>, HarnessDispatchError> {
        if self.plans.len() != 1 {
            return Ok(None);
        }
        self.plans.values().next().map(HarnessLaunchPlanV1::plan_ref).transpose()
    }

    pub fn resolve(
        &self,
        plan_ref: &HarnessLaunchPlanRefV1,
    ) -> Result<&HarnessLaunchPlanV1, HarnessDispatchError> {
        plan_ref.validate()?;
        let plan = self.plans.get(&plan_ref.plan_id)
            .ok_or_else(|| HarnessDispatchError::PlanMissing(plan_ref.plan_id.clone()))?;
        let current_ref = plan.plan_ref()?;
        if current_ref.revision != plan_ref.revision || current_ref.digest != plan_ref.digest {
            return Err(HarnessDispatchError::PlanIdentityMismatch);
        }
        Ok(plan)
    }

    pub fn resolve_scheduled(
        &self,
        scheduled: &HarnessScheduledLaunchRefV1,
    ) -> Result<&HarnessLaunchPlanV1, HarnessDispatchError> {
        scheduled.validate()?;
        let plan = self.resolve(&scheduled.plan)?;
        let current = plan.scheduled_ref()?;
        if &current != scheduled {
            return Err(HarnessDispatchError::PlanIdentityMismatch);
        }
        Ok(plan)
    }

    pub fn resolve_ordinary_scheduled(
        &self,
        scheduled: &HarnessScheduledLaunchRefV2,
    ) -> Result<&HarnessLaunchPlanV1, HarnessDispatchError> {
        scheduled.validate()?;
        if scheduled.authority != HarnessLaunchAuthorityRefV1::OrdinaryOperator {
            return Err(HarnessDispatchError::OperatorPrivilegedFlow);
        }
        let plan = self.resolve(&scheduled.plan)?;
        if plan.ordinary_scheduled_ref()? != *scheduled {
            return Err(HarnessDispatchError::PlanIdentityMismatch);
        }
        Ok(plan)
    }
}

pub(crate) fn execution_mode(mode: HarnessExecutionModeV1) -> SessionMode {
    match mode {
        HarnessExecutionModeV1::Pty => SessionMode::Pty,
        HarnessExecutionModeV1::Inline => SessionMode::Inline,
    }
}

fn spawn_idempotency_key(
    idempotency_ref: &HarnessIdempotencyRef,
) -> Result<SpawnIdempotencyKey, HarnessDispatchError> {
    idempotency_ref.validate()?;
    Ok(SpawnIdempotencyKey::new(idempotency_ref.as_str())?)
}

#[derive(Debug, Error)]
pub enum HarnessDispatchError {
    #[error(transparent)]
    Harness(#[from] gate4agent_harness_protocol::HarnessValidationError),
    #[error("launch catalog exceeds the 128-plan limit")]
    CatalogTooLarge,
    #[error("launch catalog contains duplicate plan {0:?}")]
    DuplicatePlan(HarnessSelectorV1),
    #[error("launch plan JSON is empty or exceeds the 4096-byte limit")]
    PlanJsonSize,
    #[error("delivery bundle JSON is empty or exceeds the 16384-byte limit")]
    DeliveryJsonSize,
    #[error("delivery bundle must contain 1..=128 reviewed sources")]
    DeliverySourceCount,
    #[error("launch plan references absent compiled delivery bundle {0}")]
    DeliveryBundleMissing(SpawnBundleId),
    #[error("launch plan {0:?} is absent from the runtime catalog")]
    PlanMissing(HarnessSelectorV1),
    #[error("omitted launch plan id requires exactly one runtime plan")]
    PlanSelectionRequired,
    #[error("durable launch plan identity does not match the runtime catalog")]
    PlanIdentityMismatch,
    #[error("launch plan does not exactly match the durable run intent")]
    IntentMismatch,
    #[error("scheduled task does not match the dispatch intent")]
    TaskMismatch,
    #[error("scheduler selected a task that is not Ready")]
    TaskNotReady,
    #[error("launch plan grant policy does not match the selected task creator")]
    GrantAuthorityMismatch,
    #[error("parent-run continuation requires a parent-run task creator")]
    ContinuationAuthorityMismatch,
    #[error("operator launch authority cannot request delivery or continuation")]
    OperatorPrivilegedFlow,
    #[error("grant-bound harness MCP requires an exact grant policy")]
    HarnessMcpGrantRequired,
    #[error("specialized delivery, continuation, or harness MCP dispatch is unavailable")]
    SpecializedDispatchUnavailable,
    #[error("launch plan terminal size must be nonzero")]
    InvalidTerminalSize,
    #[error("launch plan contains an invalid provider")]
    InvalidProvider(#[from] gate4agent_types::AgentIdError),
    #[error("launch plan contains an invalid Node identifier")]
    InvalidNodeIdentifier(#[from] gate4agent_node_protocol::NodeIdentifierError),
    #[error("launch plan contains an invalid SpawnSpec identifier")]
    InvalidSpawnIdentifier(#[from] gate4agent_node_protocol::SpawnIdentifierError),
    #[error("launch plan contains an invalid spawn deadline")]
    InvalidSpawnDeadline(#[from] gate4agent_node_protocol::SpawnDeadlineError),
    #[error("task body cannot be represented as a bounded spawn prompt")]
    InvalidPrompt(#[from] gate4agent_node_protocol::SpawnPromptError),
    #[error("launch plan canonical encoding failed")]
    Serialize(#[from] serde_json::Error),
    #[error("launch identity digest failed")]
    Digest(String),
    #[error("deterministic dispatch identity could not be represented")]
    DerivedIdentity,
    #[error("lifecycle event sequence must be nonzero")]
    InvalidEventSequence,
    #[error(transparent)]
    DeliveryCompile(#[from] gate4agent_harness_delivery::DeliveryCompileError),
    #[error(transparent)]
    DeliveryCatalog(#[from] gate4agent_harness_delivery::DeliveryCatalogError),
}

pub fn deterministic_run_id(
    operation_id: &HarnessOperationId,
) -> Result<HarnessRunId, HarnessDispatchError> {
    operation_id.validate()?;
    let digest = local_hmac_sha256(
        HARNESS_SCHEDULED_RUN_ID_DOMAIN,
        operation_id.as_str().as_bytes(),
    ).map_err(HarnessDispatchError::Digest)?;
    let mut nonce = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessRunId::new(format!("{}{}", HarnessRunId::PREFIX, nonce))?)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessDerivedDispatchIdsV1 {
    pub run_id: HarnessRunId,
    pub delivery_ref: Option<HarnessDeliveryRef>,
    pub delivery_receipt_ref: Option<HarnessReceiptRef>,
    pub continuation_ref: Option<HarnessContinuationRef>,
    pub continuation_receipt_ref: Option<HarnessReceiptRef>,
    pub harness_mcp_reservation_id: Option<HarnessMcpReservationId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessLifecycleEventKindV1 {
    InteractionRequested,
    InteractionResolved,
    Running,
    ExitedSuccess,
    ExitedFailure,
    ExitedForced,
    Failed,
    GapWaiting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessLifecycleProjectionV1 {
    Running,
    Waiting,
    CompletedReview,
    Failed,
    Cancelled,
}

pub fn project_control_lifecycle(
    event: &C2ControlEventKind,
) -> Option<(HarnessLifecycleEventKindV1, HarnessLifecycleProjectionV1)> {
    match event {
        C2ControlEventKind::InteractionRequested => Some((
            HarnessLifecycleEventKindV1::InteractionRequested,
            HarnessLifecycleProjectionV1::Waiting,
        )),
        C2ControlEventKind::InteractionResolved => Some((
            HarnessLifecycleEventKindV1::InteractionResolved,
            HarnessLifecycleProjectionV1::Running,
        )),
        C2ControlEventKind::Running => Some((
            HarnessLifecycleEventKindV1::Running,
            HarnessLifecycleProjectionV1::Running,
        )),
        C2ControlEventKind::Exited { forced: true, .. } => Some((
            HarnessLifecycleEventKindV1::ExitedForced,
            HarnessLifecycleProjectionV1::Cancelled,
        )),
        C2ControlEventKind::Exited { exit_code: Some(0), forced: false } => Some((
            HarnessLifecycleEventKindV1::ExitedSuccess,
            HarnessLifecycleProjectionV1::CompletedReview,
        )),
        C2ControlEventKind::Exited { exit_code: Some(_), forced: false } => Some((
            HarnessLifecycleEventKindV1::ExitedFailure,
            HarnessLifecycleProjectionV1::Failed,
        )),
        C2ControlEventKind::Failed => Some((
            HarnessLifecycleEventKindV1::Failed,
            HarnessLifecycleProjectionV1::Failed,
        )),
        C2ControlEventKind::Exited { exit_code: None, forced: false } => None,
        _ => None,
    }
}

pub fn exact_bound_control_lifecycle(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    routed: &RoutedNodeEvent,
) -> Option<(
    u64,
    HarnessLifecycleEventKindV1,
    HarnessLifecycleProjectionV1,
)> {
    let binding = run.binding.as_ref()?;
    let HarnessSessionIdentityV1::Managed { active_session: Some(active), .. } =
        &binding.session
    else {
        return None;
    };
    let C2NodeEvent::Control { address, event } = &routed.event else {
        return None;
    };
    if binding.node_id.as_str() != routed.node_id.as_str()
        || binding.node_incarnation.as_str() != routed.cursor.incarnation_id.to_string()
        || binding.workspace_id.as_str() != address.workspace_id.as_str()
        || active.instance_id != address.session.instance_id.0
        || active.generation != address.session.generation.0
        || event.instance_id != address.session.instance_id
        || event.generation != address.session.generation
        || routed.cursor.sequence == 0
    {
        return None;
    }
    let (kind, projection) = project_control_lifecycle(&event.event)?;
    Some((routed.cursor.sequence, kind, projection))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessLifecycleAuthorityIdsV1 {
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub request_digest: HarnessRequestDigest,
}

pub fn deterministic_lifecycle_authority_ids(
    run_id: &HarnessRunId,
    node_id: &NodeId,
    incarnation_id: &NodeIncarnationId,
    event_sequence: u64,
    kind: HarnessLifecycleEventKindV1,
) -> Result<HarnessLifecycleAuthorityIdsV1, HarnessDispatchError> {
    run_id.validate()?;
    if event_sequence == 0 {
        return Err(HarnessDispatchError::InvalidEventSequence);
    }
    let incarnation = incarnation_id.to_string();
    let material = serde_json::to_vec(&(
        run_id.as_str(),
        node_id.as_str(),
        incarnation.as_str(),
        event_sequence,
        kind,
    ))?;
    let operation_id = derived_id_from_material(
        HarnessOperationId::PREFIX,
        HARNESS_LIFECYCLE_OPERATION_ID_DOMAIN,
        &material,
        HarnessOperationId::new,
    )?;
    let idempotency_ref = derived_id_from_material(
        HarnessIdempotencyRef::PREFIX,
        HARNESS_LIFECYCLE_IDEMPOTENCY_REF_DOMAIN,
        &material,
        HarnessIdempotencyRef::new,
    )?;
    let request_digest = local_hmac_sha256(
        HARNESS_LIFECYCLE_REQUEST_DIGEST_DOMAIN,
        &material,
    ).map_err(HarnessDispatchError::Digest)?;
    let request_digest = HarnessRequestDigest::new(
        request_digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    )?;
    Ok(HarnessLifecycleAuthorityIdsV1 {
        operation_id,
        idempotency_ref,
        request_digest,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessContextPackRecordAuthorityIdsV1 {
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub request_digest: HarnessRequestDigest,
}

/// Keyed by `(run_id, node_id, incarnation_id, receipt digest)` rather than
/// an event sequence number: `RecordRunContextPack` fires off an idempotent
/// snapshot poll (§3.5), not a discrete lifecycle event, so the exact same
/// receipt observed on a later resync must derive the identical operation
/// identity and replay cleanly instead of conflicting.
pub fn deterministic_context_pack_record_ids(
    run_id: &HarnessRunId,
    node_id: &NodeId,
    incarnation_id: &NodeIncarnationId,
    receipt_digest: &str,
) -> Result<HarnessContextPackRecordAuthorityIdsV1, HarnessDispatchError> {
    run_id.validate()?;
    let incarnation = incarnation_id.to_string();
    let material = serde_json::to_vec(&(
        run_id.as_str(),
        node_id.as_str(),
        incarnation.as_str(),
        receipt_digest,
    ))?;
    let operation_id = derived_id_from_material(
        HarnessOperationId::PREFIX,
        HARNESS_CONTEXT_PACK_RECORD_OPERATION_ID_DOMAIN,
        &material,
        HarnessOperationId::new,
    )?;
    let idempotency_ref = derived_id_from_material(
        HarnessIdempotencyRef::PREFIX,
        HARNESS_CONTEXT_PACK_RECORD_IDEMPOTENCY_REF_DOMAIN,
        &material,
        HarnessIdempotencyRef::new,
    )?;
    let request_digest = local_hmac_sha256(
        HARNESS_CONTEXT_PACK_RECORD_REQUEST_DIGEST_DOMAIN,
        &material,
    ).map_err(HarnessDispatchError::Digest)?;
    let request_digest = HarnessRequestDigest::new(
        request_digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    )?;
    Ok(HarnessContextPackRecordAuthorityIdsV1 {
        operation_id,
        idempotency_ref,
        request_digest,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessGitFactsRecordAuthorityIdsV1 {
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub request_digest: HarnessRequestDigest,
}

/// Keyed by `(run_id, node_id, incarnation_id)`, deliberately without the
/// captured outcome: unlike `RecordRunContextPack`, a git-facts capture is
/// single-attempt by design (A3 design §2/§9 risk 2), so there is no
/// legitimate "second, different capture" this identity needs to
/// distinguish. Two attempts against the same binding either observe the
/// same real workspace state — a harmless value-based replay the engine
/// recognizes before ever consulting the operation ledger — or diverge, in
/// which case the engine rejects the second one as a hard conflict
/// regardless of what operation id either attempt carried.
pub fn deterministic_run_git_facts_record_ids(
    run_id: &HarnessRunId,
    node_id: &NodeId,
    incarnation_id: &NodeIncarnationId,
) -> Result<HarnessGitFactsRecordAuthorityIdsV1, HarnessDispatchError> {
    run_id.validate()?;
    let incarnation = incarnation_id.to_string();
    let material = serde_json::to_vec(&(
        run_id.as_str(),
        node_id.as_str(),
        incarnation.as_str(),
    ))?;
    let operation_id = derived_id_from_material(
        HarnessOperationId::PREFIX,
        HARNESS_GIT_FACTS_RECORD_OPERATION_ID_DOMAIN,
        &material,
        HarnessOperationId::new,
    )?;
    let idempotency_ref = derived_id_from_material(
        HarnessIdempotencyRef::PREFIX,
        HARNESS_GIT_FACTS_RECORD_IDEMPOTENCY_REF_DOMAIN,
        &material,
        HarnessIdempotencyRef::new,
    )?;
    let request_digest = local_hmac_sha256(
        HARNESS_GIT_FACTS_RECORD_REQUEST_DIGEST_DOMAIN,
        &material,
    ).map_err(HarnessDispatchError::Digest)?;
    let request_digest = HarnessRequestDigest::new(
        request_digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    )?;
    Ok(HarnessGitFactsRecordAuthorityIdsV1 {
        operation_id,
        idempotency_ref,
        request_digest,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessResultRefRecordAuthorityIdsV1 {
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub request_digest: HarnessRequestDigest,
}

/// Keyed by `(task_id, result_ref)`: `RecordTaskResultRef` fires once per
/// distinct run whose result lands on this task, so the ref itself — not an
/// event sequence — is what must derive a stable, replay-safe identity.
pub fn deterministic_task_result_ref_record_ids(
    task_id: &HarnessTaskId,
    result_ref: &HarnessResultRef,
) -> Result<HarnessResultRefRecordAuthorityIdsV1, HarnessDispatchError> {
    task_id.validate()?;
    result_ref.validate()?;
    let material = serde_json::to_vec(&(task_id.as_str(), result_ref.as_str()))?;
    let operation_id = derived_id_from_material(
        HarnessOperationId::PREFIX,
        HARNESS_RESULT_REF_RECORD_OPERATION_ID_DOMAIN,
        &material,
        HarnessOperationId::new,
    )?;
    let idempotency_ref = derived_id_from_material(
        HarnessIdempotencyRef::PREFIX,
        HARNESS_RESULT_REF_RECORD_IDEMPOTENCY_REF_DOMAIN,
        &material,
        HarnessIdempotencyRef::new,
    )?;
    let request_digest = local_hmac_sha256(
        HARNESS_RESULT_REF_RECORD_REQUEST_DIGEST_DOMAIN,
        &material,
    ).map_err(HarnessDispatchError::Digest)?;
    let request_digest = HarnessRequestDigest::new(
        request_digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    )?;
    Ok(HarnessResultRefRecordAuthorityIdsV1 {
        operation_id,
        idempotency_ref,
        request_digest,
    })
}

pub fn deterministic_dispatch_ids(
    operation_id: &HarnessOperationId,
    plan: &HarnessLaunchPlanV1,
) -> Result<HarnessDerivedDispatchIdsV1, HarnessDispatchError> {
    operation_id.validate()?;
    plan.validate()?;
    let delivery_ref = plan.delivery.as_ref().map(|_| {
        derived_harness_id(
            HarnessDeliveryRef::PREFIX,
            HARNESS_DELIVERY_REF_DOMAIN,
            operation_id,
            HarnessDeliveryRef::new,
        )
    }).transpose()?;
    let delivery_receipt_ref = plan.delivery.as_ref().map(|_| {
        derived_harness_id(
            HarnessReceiptRef::PREFIX,
            HARNESS_DELIVERY_RECEIPT_REF_DOMAIN,
            operation_id,
            HarnessReceiptRef::new,
        )
    }).transpose()?;
    let continuation_ref = (plan.continuation == HarnessContinuationPolicyV1::ParentRun)
        .then(|| derived_harness_id(
            HarnessContinuationRef::PREFIX,
            HARNESS_CONTINUATION_REF_DOMAIN,
            operation_id,
            HarnessContinuationRef::new,
        )).transpose()?;
    let continuation_receipt_ref =
        (plan.continuation == HarnessContinuationPolicyV1::ParentRun)
            .then(|| derived_harness_id(
                HarnessReceiptRef::PREFIX,
                HARNESS_CONTINUATION_RECEIPT_REF_DOMAIN,
                operation_id,
                HarnessReceiptRef::new,
            )).transpose()?;
    let harness_mcp_reservation_id =
        (plan.harness_mcp == HarnessMcpPolicyV1::GrantBound)
            .then(|| derived_mcp_reservation_id(operation_id)).transpose()?;
    Ok(HarnessDerivedDispatchIdsV1 {
        run_id: deterministic_run_id(operation_id)?,
        delivery_ref,
        delivery_receipt_ref,
        continuation_ref,
        continuation_receipt_ref,
        harness_mcp_reservation_id,
    })
}

pub fn deterministic_issued_dispatch_ids(
    operation_id: &HarnessOperationId,
    has_delivery: bool,
    has_continuation: bool,
) -> Result<HarnessDerivedDispatchIdsV1, HarnessDispatchError> {
    operation_id.validate()?;
    let delivery_ref = has_delivery.then(|| derived_harness_id(
        HarnessDeliveryRef::PREFIX,
        HARNESS_DELIVERY_REF_DOMAIN,
        operation_id,
        HarnessDeliveryRef::new,
    )).transpose()?;
    let delivery_receipt_ref = has_delivery.then(|| derived_harness_id(
        HarnessReceiptRef::PREFIX,
        HARNESS_DELIVERY_RECEIPT_REF_DOMAIN,
        operation_id,
        HarnessReceiptRef::new,
    )).transpose()?;
    let continuation_ref = has_continuation.then(|| derived_harness_id(
        HarnessContinuationRef::PREFIX,
        HARNESS_CONTINUATION_REF_DOMAIN,
        operation_id,
        HarnessContinuationRef::new,
    )).transpose()?;
    let continuation_receipt_ref = has_continuation.then(|| derived_harness_id(
        HarnessReceiptRef::PREFIX,
        HARNESS_CONTINUATION_RECEIPT_REF_DOMAIN,
        operation_id,
        HarnessReceiptRef::new,
    )).transpose()?;
    Ok(HarnessDerivedDispatchIdsV1 {
        run_id: deterministic_run_id(operation_id)?,
        delivery_ref,
        delivery_receipt_ref,
        continuation_ref,
        continuation_receipt_ref,
        harness_mcp_reservation_id: None,
    })
}

pub fn derive_schedule_request(
    plan: &HarnessLaunchPlanV1,
    task: &HarnessTaskV1,
    authority: &HarnessOperatorAuthorityV1,
) -> Result<(HarnessScheduleRequestV1, HarnessScheduledLaunchRefV1), HarnessDispatchError> {
    authority.validate()?;
    if task.state != HarnessTaskStateV1::Ready {
        return Err(HarnessDispatchError::TaskNotReady);
    }
    let (actor, parent_run_id, intent) = plan.run_authority_and_intent(task)?;
    let ids = deterministic_dispatch_ids(&authority.operation_id, plan)?;
    Ok((HarnessScheduleRequestV1 {
        operation_id: authority.operation_id.clone(),
        idempotency_ref: authority.idempotency_ref.clone(),
        actor,
        run_id: ids.run_id,
        parent_run_id,
        intent,
        now_unix_ms: authority.now_unix_ms,
    }, plan.scheduled_ref()?))
}

fn derived_harness_id<T, F>(
    prefix: &str,
    domain: &[u8],
    operation_id: &HarnessOperationId,
    constructor: F,
) -> Result<T, HarnessDispatchError>
where
    F: FnOnce(String) -> Result<T, gate4agent_harness_protocol::HarnessValidationError>,
{
    let nonce = derived_nonce(domain, operation_id, 12)?;
    Ok(constructor(format!("{prefix}{nonce}"))?)
}

fn derived_id_from_material<T, F>(
    prefix: &str,
    domain: &[u8],
    material: &[u8],
    constructor: F,
) -> Result<T, HarnessDispatchError>
where
    F: FnOnce(String) -> Result<T, gate4agent_harness_protocol::HarnessValidationError>,
{
    let digest = local_hmac_sha256(domain, material).map_err(HarnessDispatchError::Digest)?;
    let mut nonce = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(constructor(format!("{prefix}{nonce}"))?)
}

fn derived_mcp_reservation_id(
    operation_id: &HarnessOperationId,
) -> Result<HarnessMcpReservationId, HarnessDispatchError> {
    let nonce = derived_nonce(HARNESS_MCP_RESERVATION_ID_DOMAIN, operation_id, 12)?;
    HarnessMcpReservationId::new(format!("hmcpres_{nonce}"))
        .map_err(|_| HarnessDispatchError::DerivedIdentity)
}

fn derived_nonce(
    domain: &[u8],
    operation_id: &HarnessOperationId,
    byte_len: usize,
) -> Result<String, HarnessDispatchError> {
    let digest = local_hmac_sha256(domain, operation_id.as_str().as_bytes())
        .map_err(HarnessDispatchError::Digest)?;
    let mut nonce = String::with_capacity(byte_len * 2);
    for byte in &digest[..byte_len] {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(nonce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_protocol::{
        HarnessTaskId,
    };

    fn selector(value: &str) -> HarnessSelectorV1 {
        HarnessSelectorV1::new(value).unwrap()
    }

    fn plan(prompt_source: HarnessPromptSourceV1) -> HarnessLaunchPlanV1 {
        HarnessLaunchPlanV1 {
            plan_id: selector("default"),
            revision: HarnessRevision::new(7).unwrap(),
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            worktree: HarnessWorktreeIntentV1::Managed {
                worktree_ref: selector("worktree-a"),
            },
            provider_profile: selector("codex-default"),
            provider: AgentId::new("codex").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            terminal_size: TerminalSize { rows: 40, columns: 120 },
            prompt_source,
            delivery: None,
            continuation: HarnessContinuationPolicyV1::None,
            grant: HarnessGrantPolicyV1::Operator,
            harness_mcp: HarnessMcpPolicyV1::Disabled,
            deadline_ms: 30_000,
        }
    }

    fn dispatch() -> HarnessDispatchIntentV1 {
        HarnessDispatchIntentV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap(),
            task_revision: HarnessRevision::new(2).unwrap(),
            run_id: HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(1).unwrap(),
            operation_id: HarnessOperationId::new(format!("hop_{}", "c".repeat(24))).unwrap(),
            operation_revision: HarnessRevision::new(1).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(
                format!("hidem_{}", "d".repeat(24)),
            ).unwrap(),
            parent_run_id: None,
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Managed {
                    worktree_ref: selector("worktree-a"),
                },
                provider_profile: selector("codex-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
        }
    }

    fn task(body: &str) -> HarnessTaskV1 {
        let dispatch = dispatch();
        HarnessTaskV1 {
            task_id: dispatch.task_id,
            revision: dispatch.task_revision,
            title: "Dispatch task".to_owned(),
            body: body.to_owned(),
            creator: HarnessActorV1::User { actor_id: selector("operator") },
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: vec![dispatch.run_id],
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
        }
    }

    #[test]
    fn launch_identity_domains_are_stable_and_distinct() {
        let digest = local_hmac_sha256(HARNESS_LAUNCH_PLAN_DIGEST_DOMAIN, b"abc").unwrap();
        let encoded = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        assert_eq!(
            encoded,
            "14195d6a9421d75b29c691884f81aa5e4da2cd754022ae9ac9af5dd76049d466",
        );
        let operation_id = HarnessOperationId::new(
            format!("hop_{}", "a".repeat(24)),
        ).unwrap();
        assert_eq!(
            deterministic_run_id(&operation_id).unwrap().as_str(),
            "hrun_dc200d6891851b717e4203aa",
        );
        assert_ne!(HARNESS_LAUNCH_PLAN_DIGEST_DOMAIN, HARNESS_SCHEDULED_RUN_ID_DOMAIN);
    }

    #[test]
    fn lifecycle_authority_is_exact_stable_and_domain_separated() {
        let run_id = HarnessRunId::new(format!("hrun_{}", "1".repeat(24))).unwrap();
        let node_id = NodeId::new("node-a").unwrap();
        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let first = deterministic_lifecycle_authority_ids(
            &run_id,
            &node_id,
            &incarnation,
            9,
            HarnessLifecycleEventKindV1::ExitedSuccess,
        ).unwrap();
        let replay = deterministic_lifecycle_authority_ids(
            &run_id,
            &node_id,
            &incarnation,
            9,
            HarnessLifecycleEventKindV1::ExitedSuccess,
        ).unwrap();
        assert_eq!(first, replay);
        assert_ne!(first.operation_id.as_str(), first.idempotency_ref.as_str());
        let failure = deterministic_lifecycle_authority_ids(
            &run_id,
            &node_id,
            &incarnation,
            9,
            HarnessLifecycleEventKindV1::ExitedFailure,
        ).unwrap();
        assert_ne!(first.operation_id, failure.operation_id);
        assert!(matches!(
            deterministic_lifecycle_authority_ids(
                &run_id,
                &node_id,
                &incarnation,
                0,
                HarnessLifecycleEventKindV1::Running,
            ),
            Err(HarnessDispatchError::InvalidEventSequence),
        ));
        assert_eq!(
            project_control_lifecycle(&C2ControlEventKind::Exited {
                exit_code: Some(0),
                forced: false,
            }),
            Some((
                HarnessLifecycleEventKindV1::ExitedSuccess,
                HarnessLifecycleProjectionV1::CompletedReview,
            )),
        );
        assert_eq!(
            project_control_lifecycle(&C2ControlEventKind::Exited {
                exit_code: None,
                forced: false,
            }),
            None,
        );
        assert_eq!(project_control_lifecycle(&C2ControlEventKind::Removed), None);
    }

    #[test]
    fn catalog_requires_exact_revision_and_digest_after_restart() {
        let catalog = HarnessLaunchCatalog::new([plan(HarnessPromptSourceV1::TaskBody)]).unwrap();
        assert_eq!(catalog.select(None).unwrap().plan_id.as_str(), "default");
        let plan_ref = catalog.plan_ref(&selector("default")).unwrap();
        assert_eq!(catalog.resolve(&plan_ref).unwrap().plan_id, selector("default"));
        let mut changed = plan_ref.clone();
        changed.revision = HarnessRevision::new(8).unwrap();
        assert!(matches!(
            catalog.resolve(&changed),
            Err(HarnessDispatchError::PlanIdentityMismatch),
        ));
        let empty = HarnessLaunchCatalog::default();
        assert!(matches!(
            empty.select(None),
            Err(HarnessDispatchError::PlanSelectionRequired),
        ));
        changed = plan_ref;
        changed.digest = HarnessRequestDigest::new("f".repeat(64)).unwrap();
        assert!(matches!(
            catalog.resolve(&changed),
            Err(HarnessDispatchError::PlanIdentityMismatch),
        ));
    }

    #[test]
    fn launch_json_admission_rejects_raw_policy_fields_and_oversize() {
        let encoded = serde_json::to_string(&plan(HarnessPromptSourceV1::TaskBody)).unwrap();
        assert_eq!(HarnessLaunchCatalog::from_json_arguments([encoded]).unwrap().len(), 1);
        let mut raw = serde_json::to_value(plan(HarnessPromptSourceV1::TaskBody)).unwrap();
        raw.as_object_mut().unwrap().insert(
            "raw_prompt".to_owned(),
            serde_json::Value::String("must-not-be-admitted".to_owned()),
        );
        assert!(matches!(
            HarnessLaunchCatalog::from_json_arguments([raw.to_string()]),
            Err(HarnessDispatchError::Serialize(_)),
        ));
        assert!(matches!(
            HarnessLaunchCatalog::from_json_arguments([
                "x".repeat(HARNESS_LAUNCH_PLAN_JSON_MAX_BYTES + 1),
            ]),
            Err(HarnessDispatchError::PlanJsonSize),
        ));
    }

    #[test]
    fn privileged_plan_requires_exact_grant_and_derives_stable_distinct_ids() {
        let mut privileged = plan(HarnessPromptSourceV1::TaskBody);
        privileged.delivery = Some(HarnessDeliveryPolicyV1 {
            selector: selector("skills"),
            bundle_id: SpawnBundleId::new("skills-bundle").unwrap(),
        });
        assert!(matches!(
            privileged.validate(),
            Err(HarnessDispatchError::OperatorPrivilegedFlow),
        ));
        privileged.continuation = HarnessContinuationPolicyV1::ParentRun;
        privileged.harness_mcp = HarnessMcpPolicyV1::GrantBound;
        privileged.grant = HarnessGrantPolicyV1::Exact {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "e".repeat(24))).unwrap(),
            revision: HarnessRevision::new(3).unwrap(),
        };
        privileged.validate().unwrap();
        let operation_id = HarnessOperationId::new(
            format!("hop_{}", "9".repeat(24)),
        ).unwrap();
        let first = deterministic_dispatch_ids(&operation_id, &privileged).unwrap();
        let replay = deterministic_dispatch_ids(&operation_id, &privileged).unwrap();
        assert_eq!(first, replay);
        let identities = [
            first.run_id.as_str(),
            first.delivery_ref.as_ref().unwrap().as_str(),
            first.delivery_receipt_ref.as_ref().unwrap().as_str(),
            first.continuation_ref.as_ref().unwrap().as_str(),
            first.continuation_receipt_ref.as_ref().unwrap().as_str(),
            first.harness_mcp_reservation_id.as_ref().unwrap().as_str(),
        ];
        for (index, identity) in identities.iter().enumerate() {
            assert!(!identity.is_empty());
            assert!(!identities[..index].contains(identity));
        }
    }

    #[test]
    fn spawn_spec_uses_only_task_body_and_explicit_safe_overrides() {
        const BODY: &str = "intentional workload text";
        let plan = plan(HarnessPromptSourceV1::TaskBody);
        let dispatch = dispatch();
        let spec = plan.spawn_spec(
            &dispatch,
            &task(BODY),
            SpawnProfileRevision::new("r1").unwrap(),
        ).unwrap();
        assert_eq!(spec.idempotency_key.as_str(), dispatch.idempotency_ref.as_str());
        assert_eq!(spec.target.worktree_id.unwrap().as_str(), "worktree-a");
        assert!(matches!(
            spec.overrides.prompt,
            SpawnOverride::Set { value } if value.as_str() == BODY,
        ));
        assert!(matches!(spec.overrides.bundle_id, SpawnOverride::Clear));
        assert!(matches!(spec.overrides.context_id, SpawnOverride::Clear));
        assert!(matches!(spec.overrides.environment_profile_id, SpawnOverride::Clear));
        let encoded_plan = serde_json::to_string(&plan).unwrap();
        assert!(!encoded_plan.contains(BODY));
        assert!(!encoded_plan.contains("environment"));
    }

    #[test]
    fn spawn_spec_exact_grant_without_continuation_keeps_parent_authority() {
        let mut plan = plan(HarnessPromptSourceV1::Clear);
        plan.grant = HarnessGrantPolicyV1::Exact {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "e".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
        };
        let parent = HarnessRunId::new(format!("hrun_{}", "e".repeat(24))).unwrap();
        let mut dispatch = dispatch();
        dispatch.parent_run_id = Some(parent);
        assert!(dispatch.intent.continuation.is_none());
        plan.spawn_spec(
            &dispatch,
            &task("delivery-only child"),
            SpawnProfileRevision::new("r1").unwrap(),
        ).unwrap();
    }

    #[test]
    fn spawn_spec_operator_grant_rejects_injected_parent_authority() {
        let plan = plan(HarnessPromptSourceV1::Clear);
        let mut dispatch = dispatch();
        dispatch.parent_run_id = Some(
            HarnessRunId::new(format!("hrun_{}", "e".repeat(24))).unwrap(),
        );
        assert!(matches!(
            plan.spawn_spec(
                &dispatch,
                &task("operator child"),
                SpawnProfileRevision::new("r1").unwrap(),
            ),
            Err(HarnessDispatchError::TaskMismatch),
        ));
    }

    #[test]
    fn spawn_spec_parent_continuation_matches_exact_parent() {
        let mut plan = plan(HarnessPromptSourceV1::Clear);
        plan.grant = HarnessGrantPolicyV1::Exact {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "e".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
        };
        plan.continuation = HarnessContinuationPolicyV1::ParentRun;
        let parent = HarnessRunId::new(format!("hrun_{}", "e".repeat(24))).unwrap();
        let mut dispatch = dispatch();
        dispatch.parent_run_id = Some(parent.clone());
        dispatch.intent.continuation = Some(selector(parent.as_str()));
        plan.spawn_spec(
            &dispatch,
            &task("continuation child"),
            SpawnProfileRevision::new("r1").unwrap(),
        ).unwrap();
    }

    #[test]
    fn schedule_request_derives_run_authority_from_task_not_operator_actor() {
        let plan = plan(HarnessPromptSourceV1::TaskBody);
        let mut ready = task("workload");
        ready.state = HarnessTaskStateV1::Ready;
        ready.run_ids.clear();
        let authority = HarnessOperatorAuthorityV1 {
            actor_id: selector("untrusted-ui-label"),
            operation_id: HarnessOperationId::new(format!("hop_{}", "8".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(
                format!("hidem_{}", "7".repeat(24)),
            ).unwrap(),
            now_unix_ms: 44,
        };
        let (request, scheduled) = derive_schedule_request(&plan, &ready, &authority).unwrap();
        assert_eq!(request.actor, ready.creator);
        assert_eq!(request.parent_run_id, None);
        assert_eq!(request.operation_id, authority.operation_id);
        assert_eq!(request.idempotency_ref, authority.idempotency_ref);
        assert_eq!(scheduled, plan.scheduled_ref().unwrap());
    }

    #[test]
    fn plan_rejects_non_exact_intent_and_clear_prompt_is_explicit() {
        let plan = plan(HarnessPromptSourceV1::Clear);
        let mut dispatch = dispatch();
        let spec = plan.spawn_spec(
            &dispatch,
            &task("not dispatched"),
            SpawnProfileRevision::new("r1").unwrap(),
        ).unwrap();
        assert!(matches!(spec.overrides.prompt, SpawnOverride::Clear));
        dispatch.intent.workspace_id = selector("workspace-b");
        assert!(matches!(
            plan.spawn_spec(
                &dispatch,
                &task("not dispatched"),
                SpawnProfileRevision::new("r1").unwrap(),
            ),
            Err(HarnessDispatchError::IntentMismatch),
        ));
    }
}
