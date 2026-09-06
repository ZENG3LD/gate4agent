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
use gate4agent_catalog::{approval_level_resolution, ApprovalLevelResolution};
use gate4agent_node_protocol::{
    DeliveryComponentKindV2, DeliveryRelativePathV2, DeliveryScopeV2,
    HarnessMcpReservationId, NodeId, NodeIncarnationId, SessionMode, SpawnBundleId, SpawnBundleRevision,
    SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides, SpawnProfileId,
    SpawnProfileRevision, SpawnPrompt, SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceId,
};
use gate4agent_harness_delivery::{
    compile_reviewed_delivery_bundle_v2, DeliveryCatalogV2, ReviewedDeliverySourceV2,
};
use gate4agent_harness_api::HarnessRuntimeNodeInventoryV1;
use gate4agent_types::{AgentId, ApprovalLevel, TerminalSize};
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
const HARNESS_DEFAULT_GRANT_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-default-grant-id-v1\0";
const HARNESS_DEFAULT_GRANT_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-default-grant-operation-id-v1\0";
const HARNESS_DEFAULT_GRANT_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-default-grant-idempotency-ref-v1\0";

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
    /// Approval-level axis this plan launches its process at -- carried
    /// straight into `SpawnOverrides::approval_level` by [`Self::
    /// spawn_spec`] (`Some(self.approval_level)`, never `None`). Every plan
    /// [`derive_launch_plans_from_inventory`] synthesizes sets this
    /// explicitly, one per (provider x workspace x profile x level)
    /// combination the provider's catalog row
    /// (`gate4agent_catalog::approval_level_resolution`) actually supports
    /// -- see that function's own doc comment. `#[serde(default)]` so an
    /// operator-authored `--launch-plan-json` plan that predates this field
    /// decodes it as `ApprovalLevel::default()` (`FullAuto`), the exact
    /// level the node already resolved an absent `SpawnOverrides::
    /// approval_level` to before this field existed
    /// (`Option::unwrap_or_default()`) -- no observable behaviour changes
    /// for a plan that never sets this.
    #[serde(default)]
    pub approval_level: ApprovalLevel,
    pub deadline_ms: u64,
}

impl HarnessLaunchPlanV1 {
    /// A plan is ordinary -- dispatchable straight from the operator wire,
    /// with no exact grant baked in -- when it carries no delivery bundle,
    /// no continuation, and its harness MCP policy is either `Disabled` or
    /// `GrantBound` paired with `HarnessGrantPolicyV1::Operator`. The
    /// `Operator` pairing is safe because the harness itself mints the
    /// dispatching run's default, self-only grant at dispatch time
    /// (`runtime::resolve_harness_mcp_grant`, gate4agent-arc-mailbox-and-
    /// task-layer Slice A(i)) through the same audited mutation path the
    /// operator wire's own grant mutations use -- an operator never
    /// supplies or reuses an exact grant identity for this shape. A
    /// `GrantBound` plan naming an `Exact` grant instead stays privileged:
    /// that exact grant belongs to a specific parent run and must be
    /// validated against it (`HarnessService::
    /// prepare_scheduled_specialized_authorities`) before anything reads
    /// through it, so it is never treated as ordinary.
    pub fn is_ordinary_dispatch(&self) -> bool {
        self.delivery.is_none()
            && self.continuation == HarnessContinuationPolicyV1::None
            && (self.harness_mcp == HarnessMcpPolicyV1::Disabled
                || matches!(self.grant, HarnessGrantPolicyV1::Operator))
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
                || self.continuation != HarnessContinuationPolicyV1::None)
        {
            return Err(HarnessDispatchError::OperatorPrivilegedFlow);
        }
        // No arm rejects `harness_mcp: GrantBound` paired with
        // `grant: Operator` here. Before gate4agent-arc-mailbox-and-task-
        // layer Slice A(i), a `GrantBound` plan without an `Exact` grant to
        // bind to was rejected as `HarnessMcpGrantRequired`, because
        // nothing ever minted a grant for it. A(i) made
        // `runtime::resolve_harness_mcp_grant` mint the dispatching run's
        // own default, read-only grant through `HarnessService::apply` when
        // the policy is `Operator`, so an `Operator`-granted `GrantBound`
        // plan now has a real grant to arm its harness MCP reservation
        // against; there is nothing left that combination could ask for
        // that a harness-issued default grant cannot give. See
        // `is_ordinary_dispatch`'s doc comment for the resulting dispatch
        // shape.
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

    /// This plan as the node is actually asked to run it: worktree
    /// `Existing`, no delivery, no continuation.
    ///
    /// A specialized launch never reaches the node as a specialized spawn.
    /// The caller issues an ORDINARY spawn and then re-applies the
    /// delivery bundle and the continuation context as node-level
    /// `SpawnSpec` overrides once the durable staging receipts exist
    /// (`runtime::specialized_spawn_spec`). To do that it must hand
    /// [`Self::spawn_spec`] an intent with those same fields cleared —
    /// and [`Self::validate_intent`] compares plan against intent field
    /// by field, so validating a cleared intent against an uncleared
    /// plan rejects every specialized launch outright. Both sides are
    /// reduced to the issued shape here so the comparison is like for
    /// like; nothing about what the node ends up running changes.
    pub fn issued_view(&self) -> Self {
        Self {
            worktree: HarnessWorktreeIntentV1::Existing,
            delivery: None,
            continuation: HarnessContinuationPolicyV1::None,
            ..self.clone()
        }
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
        // Six independent reasons a dispatch intent may no longer match
        // the task it was frozen against, checked one at a time so the
        // refusal names which one fired. They are not interchangeable:
        // five are identity/shape facts that cannot change across a
        // dispatch, while `task_revision` is a counter that goes stale
        // simply by elapsed time, and a single collapsed message cannot
        // tell a caller which kind of failure it is looking at.
        if dispatch.task_id != task.task_id {
            return Err(HarnessDispatchError::TaskMismatch("dispatch names a different task"));
        }
        if dispatch.task_revision != task.revision {
            return Err(HarnessDispatchError::TaskMismatch(
                "task revision moved after the dispatch intent was frozen",
            ));
        }
        if task.state != HarnessTaskStateV1::Running {
            return Err(HarnessDispatchError::TaskMismatch("task is no longer Running"));
        }
        if task.run_ids.binary_search(&dispatch.run_id).is_err() {
            return Err(HarnessDispatchError::TaskMismatch(
                "task does not list the dispatch's own run",
            ));
        }
        if !grant_matches {
            return Err(HarnessDispatchError::TaskMismatch(
                "launch plan grant policy disagrees with the dispatch's parent run",
            ));
        }
        if !continuation_matches {
            return Err(HarnessDispatchError::TaskMismatch(
                "launch plan continuation policy disagrees with the dispatch's continuation",
            ));
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
                // Always the plan's own choice now, never the axis default
                // by omission -- see `approval_level`'s own doc comment on
                // this struct.
                approval_level: Some(self.approval_level),
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

    /// Every plan in this catalog, regardless of dispatch shape or grant
    /// policy -- unlike `ordinary_plans`, nothing is filtered out. Used by
    /// the runtime host to preserve every explicitly configured plan
    /// (including privileged ones) when composing the effective catalog
    /// that derived plans (`derive_launch_plans_from_inventory`) fill in.
    pub(crate) fn all_plans(&self) -> impl Iterator<Item = &HarnessLaunchPlanV1> {
        self.plans.values()
    }

    /// True when `plan_id` is explicitly present in this catalog. Used by
    /// the runtime host to tell an explicitly configured (CLI) plan apart
    /// from one it synthesized from the live runtime inventory, both for
    /// CLI-wins-on-collision precedence and for "was a derived plan just
    /// used" logging.
    pub(crate) fn contains(&self, plan_id: &HarnessSelectorV1) -> bool {
        self.plans.contains_key(plan_id)
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

/// Terminal size every derived launch plan uses: a generous interactive-PTY
/// default (`40x140`), independent of whatever an operator-authored CLI
/// `--launch-plan-json` override picks for its own plan id.
const DERIVED_LAUNCH_PLAN_TERMINAL_SIZE: TerminalSize = TerminalSize { rows: 40, columns: 140 };
/// Spawn deadline every derived launch plan uses, matching the CLI catalog
/// examples this harness has shipped with since `HarnessLaunchPlanV1` was
/// introduced.
const DERIVED_LAUNCH_PLAN_DEADLINE_MS: u64 = 30_000;
const DERIVED_LAUNCH_PLAN_ID_PREFIX: &str = "auto";
/// Suffix appended to a derived plan id to name its harness-MCP sibling --
/// see `derived_launch_plan`'s `harness_mcp` parameter.
const DERIVED_LAUNCH_PLAN_HARNESS_MCP_SUFFIX: &str = "-harness-mcp";
/// Every approval level a derived plan set is synthesized for, in the order
/// `derive_launch_plans_from_inventory` iterates them --
/// [`ApprovalLevel::Unmanaged`] first (the level every provider always
/// resolves `Supported` for, imposing nothing), then the three managed
/// levels, each gated per provider by [`approval_level_resolution`] rather
/// than assumed. See `derive_launch_plans_from_inventory`'s own doc comment
/// for the gating rule itself.
const DERIVED_LAUNCH_PLAN_APPROVAL_LEVELS: [ApprovalLevel; 4] = [
    ApprovalLevel::Unmanaged,
    ApprovalLevel::FullAuto,
    ApprovalLevel::Moderate,
    ApprovalLevel::ReadOnly,
];

/// The plan-id slug for `level` -- the exact token
/// `derived_launch_plan`/`derive_launch_plans_from_inventory` fold into a
/// synthesized plan id (`auto-<provider>-<workspace>-<profile>-<slug>[-
/// harness-mcp]`) so an operator reading a derived plan id can tell which
/// approval level it launches at without resolving the plan first.
fn approval_level_slug(level: ApprovalLevel) -> &'static str {
    match level {
        ApprovalLevel::Unmanaged => "unmanaged",
        ApprovalLevel::FullAuto => "full-auto",
        ApprovalLevel::Moderate => "moderate",
        ApprovalLevel::ReadOnly => "read-only",
    }
}

/// Synthesizes one derived launch plan for a node/workspace/provider/spawn-
/// profile/approval-level combination. `harness_mcp` selects which of the
/// two shapes `derive_launch_plans_from_inventory` synthesizes per
/// combination:
///
/// - `false`: the plain PTY plan -- existing-worktree, no-prompt-override,
///   operator-grant dispatch with delivery, continuation, and harness MCP
///   all disabled, the same shape `HarnessLaunchPlanV1::is_ordinary_dispatch`
///   has always required.
/// - `true`: its harness-MCP sibling -- same node/workspace/provider/
///   profile/level, id suffixed with [`DERIVED_LAUNCH_PLAN_HARNESS_MCP_SUFFIX`],
///   `mode: Acp`, `harness_mcp: GrantBound`. `grant` stays `Operator` for
///   both shapes: `is_ordinary_dispatch` admits `GrantBound` exactly when
///   paired with `Operator` (gate4agent-arc-mailbox-and-task-layer Slice
///   A(i)/A(iii)), so the sibling is ordinary too and never needs an exact
///   grant of its own -- the harness mints one at dispatch time
///   (`runtime::resolve_harness_mcp_grant`).
///
/// `prompt_source` differs between the two shapes for a runtime-admission
/// reason, not a stylistic one:
///
/// - Plain (`Pty`): deliberately `Clear`, not `TaskBody`. The Node's
///   `ProviderRuntimeRequirement` derivation
///   (`gate4agent-node::provider_runtime`) treats any resolved prompt in
///   `Pty` mode as `SemanticPrompt`, which only admits a provider whose
///   runtime probe verifies semantic/structured-prompt readiness -- a raw
///   PTY-only provider (no semantic adapter) is rejected outright and the
///   spawn never starts. The advertised runtime inventory
///   (`HarnessRuntimeSpawnProfileSummaryV1`) carries no signal for which
///   providers clear that bar, so a derived plan cannot tell `TaskBody`
///   would be safe for a given combination; `Clear` maps to `RawPty`
///   admission, which every PTY-capable provider satisfies by construction
///   -- the only prompt source guaranteed to actually launch.
/// - Harness-MCP sibling (`Acp`): `TaskBody`. Under `Acp` mode the Node
///   resolves the SAME `ProviderRuntimeRequirement::Acp` regardless of
///   whether a prompt is present (`gate4agent-node::server::
///   spawn_session_with_deadline`'s `(SessionMode::Acp, _)` arm), so
///   `TaskBody` carries no equivalent admission risk here -- and it is the
///   whole point of the sibling: an ACP session dispatched with harness MCP
///   access and nothing to act on would be a session an operator has to
///   prompt by hand before it can use the grant it was just issued.
///
/// `level` becomes both the plan's own `approval_level` field and, via its
/// [`approval_level_slug`], part of the synthesized plan id -- whether this
/// combination is even offered at `level` at all is decided one call up, in
/// `derive_launch_plans_from_inventory`, against
/// `gate4agent_catalog::approval_level_resolution`; this function trusts
/// its caller already made that call and only ever names the level in the
/// id and the field.
///
/// `include_node_id` disambiguates the synthesized id across multiple
/// nodes advertising the same workspace/provider/profile combination (see
/// `derive_launch_plans_from_inventory`). Returns `None` only if the
/// synthesized id, or one of the component strings, fails to round-trip
/// through its own domain type -- practically unreachable for inventory
/// sourced from `SlimNodeInventory`, whose fields already validated as the
/// matching domain types on the Node side, but this keeps the derivation a
/// pure, total function: an unrepresentable combination is silently
/// omitted rather than panicking or failing every other plan alongside it.
fn derived_launch_plan(
    node_id: &str,
    workspace_id: &str,
    provider: &str,
    profile_id: &str,
    include_node_id: bool,
    level: ApprovalLevel,
    harness_mcp: bool,
) -> Option<HarnessLaunchPlanV1> {
    let suffix = harness_mcp.then_some(DERIVED_LAUNCH_PLAN_HARNESS_MCP_SUFFIX).unwrap_or("");
    let level_slug = approval_level_slug(level);
    let raw_plan_id = if include_node_id {
        format!(
            "{DERIVED_LAUNCH_PLAN_ID_PREFIX}-{provider}-{node_id}-{workspace_id}-{profile_id}-{level_slug}{suffix}",
        )
    } else {
        format!(
            "{DERIVED_LAUNCH_PLAN_ID_PREFIX}-{provider}-{workspace_id}-{profile_id}-{level_slug}{suffix}",
        )
    };
    let (mode, prompt_source, harness_mcp_policy) = if harness_mcp {
        (HarnessExecutionModeV1::Acp, HarnessPromptSourceV1::TaskBody, HarnessMcpPolicyV1::GrantBound)
    } else {
        (HarnessExecutionModeV1::Pty, HarnessPromptSourceV1::Clear, HarnessMcpPolicyV1::Disabled)
    };
    Some(HarnessLaunchPlanV1 {
        plan_id: HarnessSelectorV1::new(raw_plan_id).ok()?,
        revision: HarnessRevision::new(1).ok()?,
        node_id: HarnessSelectorV1::new(node_id).ok()?,
        workspace_id: HarnessSelectorV1::new(workspace_id).ok()?,
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: HarnessSelectorV1::new(profile_id).ok()?,
        provider: AgentId::new(provider).ok()?,
        mode,
        terminal_size: DERIVED_LAUNCH_PLAN_TERMINAL_SIZE,
        prompt_source,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Operator,
        harness_mcp: harness_mcp_policy,
        approval_level: level,
        deadline_ms: DERIVED_LAUNCH_PLAN_DEADLINE_MS,
    })
}

/// Derives the harness's default ordinary launch plan set from the live
/// runtime inventory: up to eight plans -- a plain PTY plan and its
/// harness-MCP/ACP sibling (see `derived_launch_plan`), at each of
/// [`DERIVED_LAUNCH_PLAN_APPROVAL_LEVELS`] the provider's catalog row
/// actually supports -- per node x workspace x enabled provider x
/// advertised spawn profile combination `nodes` currently reports. This is
/// what the harness advertises for a combination no CLI `--launch-plan-
/// json` plan already names -- see `runtime::effective_launch_catalog`,
/// which composes this with the CLI catalog (CLI wins on plan id collision)
/// and enforces `HARNESS_LAUNCH_CATALOG_MAX`.
///
/// [`ApprovalLevel::Unmanaged`] is offered unconditionally for both shapes:
/// `gate4agent_catalog::approval_level_resolution` resolves it `Supported`
/// for every agent id, including one its own table does not recognize, by
/// definition (it imposes nothing). Each of the three managed levels
/// (`FullAuto`, `Moderate`, `ReadOnly`) is offered per shape independently,
/// read from that SAME function rather than assumed or duplicated here:
///
/// - The plain PTY plan is offered at `level` when the row resolves
///   `Supported` at all -- PTY has no host-side enforcement mechanism of
///   its own (a human at the keyboard picks their own agent's permission
///   flags; see `gate4agent-shell-native`'s `host_policy_for_approval_
///   level` doc comment), so "this provider has a verified vendor mode for
///   this level" is the whole gate, the same bar `approval_level_args`
///   already holds itself to for any future PTY-adjacent caller.
/// - The harness-MCP/ACP sibling is offered at `level` when the row ALSO
///   carries a sourced `acp_mode_id: Some(_)` -- the ACP transport applies
///   a level exclusively through `session/set_mode`
///   (`gate4agent-shell-native`'s `required_acp_mode`), which has nothing
///   to call without one, and would otherwise spawn silently at the
///   vendor's own default -- the exact defect naming the mode id exists to
///   prevent. [`ApprovalLevel::Unmanaged`] is the one exception: its row
///   resolves `acp_mode_id: None` for every agent id BY DESIGN (it imposes
///   nothing, so there is nothing to apply), not because no mechanism was
///   sourced, so the sibling is still offered at `Unmanaged` regardless.
///
/// A provider with no verified mode at all for a given managed level
/// (`grok` x `ReadOnly`, resolved `Unsupported`) or no sourced ACP mode id
/// for it (`grok` x `FullAuto`/`Moderate`, and every one of
/// `codex`/`grok`/`kimi`'s rows the catalog has not sourced an id for)
/// simply gets no plan of that shape at that level -- never a fabricated
/// one and never a silent fallback to a wider level, matching
/// `approval_level_resolution`'s own refusal-by-name posture.
///
/// Pure and total: never touches storage, never fails, and reflects
/// exactly the `nodes` slice handed to it -- callers recompute fresh from
/// the current runtime inventory cache on every use rather than caching
/// this output, so a node that joins or leaves the fleet changes the
/// derived set on the very next call, no restart required.
///
/// The node id is folded into every synthesized plan id whenever `nodes`
/// carries more than one node, so two nodes advertising the same
/// workspace/provider/profile combination never collide -- see
/// `derived_launch_plan`.
pub(crate) fn derive_launch_plans_from_inventory(
    nodes: &[HarnessRuntimeNodeInventoryV1],
) -> Vec<HarnessLaunchPlanV1> {
    let include_node_id = nodes.len() > 1;
    let mut plans = Vec::new();
    for node in nodes {
        let Some(spawn_profiles) = node.inventory.launch_inventory.as_ref()
            .and_then(|launch_inventory| launch_inventory.spawn_profiles.as_ref())
        else {
            continue;
        };
        for workspace_id in node.inventory.workspaces.keys() {
            for provider in &node.inventory.enabled_providers {
                let Ok(provider_agent_id) = AgentId::new(provider.as_str()) else {
                    continue;
                };
                for profile in spawn_profiles {
                    for level in DERIVED_LAUNCH_PLAN_APPROVAL_LEVELS {
                        let resolution = approval_level_resolution(&provider_agent_id, level);
                        let pty_supported =
                            matches!(resolution, ApprovalLevelResolution::Supported { .. });
                        // `Unmanaged` resolves `Supported { acp_mode_id: None,
                        // .. }` for every agent id -- by design, not a gap:
                        // it imposes nothing, so `session/set_mode` has
                        // nothing to call, and `required_acp_mode`
                        // (`gate4agent-shell-native`) treats that `None`
                        // as "apply nothing" rather than a refusal. Every
                        // other level's `None` means the opposite: no
                        // sourced mechanism exists at all, so the ACP
                        // sibling is withheld.
                        let acp_supported = level == ApprovalLevel::Unmanaged || matches!(
                            resolution,
                            ApprovalLevelResolution::Supported { acp_mode_id: Some(_), .. },
                        );
                        for harness_mcp in [false, true] {
                            let supported = if harness_mcp { acp_supported } else { pty_supported };
                            if !supported {
                                continue;
                            }
                            if let Some(plan) = derived_launch_plan(
                                node.node_id.as_str(),
                                workspace_id.as_str(),
                                provider.as_str(),
                                profile.id.as_str(),
                                include_node_id,
                                level,
                                harness_mcp,
                            ) {
                                plans.push(plan);
                            }
                        }
                    }
                }
            }
        }
    }
    plans
}

pub(crate) fn execution_mode(mode: HarnessExecutionModeV1) -> SessionMode {
    match mode {
        HarnessExecutionModeV1::Pty => SessionMode::Pty,
        HarnessExecutionModeV1::Inline => SessionMode::Inline,
        HarnessExecutionModeV1::Acp => SessionMode::Acp,
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
    #[error("scheduled task does not match the dispatch intent: {0}")]
    TaskMismatch(&'static str),
    #[error("scheduler selected a task that is not Ready")]
    TaskNotReady,
    #[error("launch plan grant policy does not match the selected task creator")]
    GrantAuthorityMismatch,
    #[error("parent-run continuation requires a parent-run task creator")]
    ContinuationAuthorityMismatch,
    #[error("operator launch authority cannot request delivery or continuation")]
    OperatorPrivilegedFlow,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessDefaultGrantAuthorityIdsV1 {
    pub grant_id: SessionGrantId,
    pub operation_id: HarnessOperationId,
    pub idempotency_ref: HarnessIdempotencyRef,
}

/// Deterministic identity for the default `SessionGrantV1` a dispatch mints
/// for its own run when `HarnessGrantPolicyV1::Operator` leaves no exact
/// grant to bind to (gate4agent-arc-mailbox-and-task-layer Slice A(i)). Keyed
/// by the dispatch's own operation id alone, so a retried dispatch mints the
/// identical grant rather than a second one -- the same replay posture as
/// `derived_mcp_reservation_id` for the reservation it pairs with.
pub fn deterministic_default_grant_ids(
    dispatch_operation_id: &HarnessOperationId,
) -> Result<HarnessDefaultGrantAuthorityIdsV1, HarnessDispatchError> {
    dispatch_operation_id.validate()?;
    Ok(HarnessDefaultGrantAuthorityIdsV1 {
        grant_id: derived_harness_id(
            SessionGrantId::PREFIX,
            HARNESS_DEFAULT_GRANT_ID_DOMAIN,
            dispatch_operation_id,
            SessionGrantId::new,
        )?,
        operation_id: derived_harness_id(
            HarnessOperationId::PREFIX,
            HARNESS_DEFAULT_GRANT_OPERATION_ID_DOMAIN,
            dispatch_operation_id,
            HarnessOperationId::new,
        )?,
        idempotency_ref: derived_harness_id(
            HarnessIdempotencyRef::PREFIX,
            HARNESS_DEFAULT_GRANT_IDEMPOTENCY_REF_DOMAIN,
            dispatch_operation_id,
            HarnessIdempotencyRef::new,
        )?,
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
    use gate4agent_harness_api::{
        HarnessRuntimeInventoryV1, HarnessRuntimeLaunchInventoryV1,
        HarnessRuntimeSpawnProfileSummaryV1, HarnessRuntimeWorkspaceV1,
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
            approval_level: ApprovalLevel::default(),
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

    /// Slice A(i)/A(iii) (gate4agent-arc-mailbox-and-task-layer): a
    /// `harness_mcp: GrantBound` plan paired with `grant: Operator` is now
    /// valid and ordinary -- the harness mints the dispatching run's own
    /// grant at dispatch time (`runtime::resolve_harness_mcp_grant`).
    /// Delivery and continuation stay privileged under `Operator`,
    /// unchanged; the same `harness_mcp` policy under an `Exact` grant
    /// stays specialized, also unchanged.
    #[test]
    fn operator_grant_bound_harness_mcp_is_ordinary_but_delivery_and_continuation_still_arent() {
        let mut mcp_plan = plan(HarnessPromptSourceV1::TaskBody);
        mcp_plan.mode = HarnessExecutionModeV1::Acp;
        mcp_plan.harness_mcp = HarnessMcpPolicyV1::GrantBound;
        mcp_plan.validate().unwrap();
        assert!(mcp_plan.is_ordinary_dispatch());

        let mut delivery_plan = mcp_plan.clone();
        delivery_plan.delivery = Some(HarnessDeliveryPolicyV1 {
            selector: selector("skills"),
            bundle_id: SpawnBundleId::new("skills-bundle").unwrap(),
        });
        assert!(matches!(
            delivery_plan.validate(),
            Err(HarnessDispatchError::OperatorPrivilegedFlow),
        ));

        let mut continuation_plan = mcp_plan.clone();
        continuation_plan.continuation = HarnessContinuationPolicyV1::ParentRun;
        assert!(matches!(
            continuation_plan.validate(),
            Err(HarnessDispatchError::OperatorPrivilegedFlow),
        ));

        let mut exact_mcp_plan = mcp_plan.clone();
        exact_mcp_plan.grant = HarnessGrantPolicyV1::Exact {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "e".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
        };
        exact_mcp_plan.validate().unwrap();
        assert!(!exact_mcp_plan.is_ordinary_dispatch());
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
            Err(HarnessDispatchError::TaskMismatch(_)),
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

    fn node_inventory(
        node_id: &str,
        workspace_ids: &[&str],
        providers: &[&str],
        profile_ids: &[&str],
    ) -> HarnessRuntimeNodeInventoryV1 {
        let workspaces = workspace_ids.iter().map(|workspace_id| {
            ((*workspace_id).to_owned(), HarnessRuntimeWorkspaceV1 {
                workspace_id: (*workspace_id).to_owned(),
                display_root: format!(r"C:\fixture\{workspace_id}"),
                display_root_truncated: false,
                sessions: Vec::new(),
                session_count: 0,
                sessions_truncated: false,
            })
        }).collect::<BTreeMap<_, _>>();
        let spawn_profiles = profile_ids.iter().map(|profile_id| {
            HarnessRuntimeSpawnProfileSummaryV1 {
                id: (*profile_id).to_owned(),
                revision: "r1".to_owned(),
                environment_profile: None,
            }
        }).collect::<Vec<_>>();
        HarnessRuntimeNodeInventoryV1 {
            node_id: node_id.to_owned(),
            incarnation_id: "0".repeat(32),
            observed_at_unix_ms: 1,
            event_sequence: 1,
            inventory: HarnessRuntimeInventoryV1 {
                enabled_providers: providers.iter().map(|provider| (*provider).to_owned())
                    .collect(),
                workspace_count: workspaces.len(),
                workspaces,
                workspaces_truncated: false,
                session_count: 0,
                sessions_truncated: false,
                managed_sessions: Vec::new(),
                managed_session_count: 0,
                managed_sessions_truncated: false,
                retired_count: 0,
                launch_inventory: Some(HarnessRuntimeLaunchInventoryV1 {
                    spawn_profiles: Some(spawn_profiles),
                    bundles: None,
                }),
            },
        }
    }

    #[test]
    fn derive_launch_plans_multi_node_disambiguates_by_node_id() {
        let node_a = node_inventory("node-a", &["workspace-a"], &["codex"], &["codex-default"]);
        let node_b = node_inventory("node-b", &["workspace-a"], &["codex"], &["codex-default"]);
        let plans = derive_launch_plans_from_inventory(&[node_a, node_b]);
        // Each node contributes the full 4-level x 2-shape cross product:
        // codex resolves `Supported` (with a sourced ACP mode id) at every
        // level, so nothing is gated away here.
        assert_eq!(plans.len(), 2 * 4 * 2);
        let mut ids = plans.iter().map(|plan| plan.plan_id.clone()).collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), plans.len());
        for plan in &plans {
            let node_id = plan.node_id.as_str();
            assert!(node_id == "node-a" || node_id == "node-b");
            assert!(plan.plan_id.as_str().contains(node_id));
            plan.validate().unwrap();
            assert!(plan.is_ordinary_dispatch());
        }
        assert_eq!(
            plans.iter().filter(|plan| plan.node_id.as_str() == "node-a").count(),
            4 * 2,
        );
        assert_eq!(
            plans.iter().filter(|plan| plan.node_id.as_str() == "node-b").count(),
            4 * 2,
        );
        assert_eq!(
            plans.iter().filter(|plan| {
                plan.harness_mcp == HarnessMcpPolicyV1::GrantBound
            }).count(),
            2 * 4,
        );
    }

    #[test]
    fn derive_launch_plans_single_node_omits_node_id_from_plan_id() {
        let node = node_inventory("node-a", &["workspace-a"], &["codex"], &["codex-default"]);
        let plans = derive_launch_plans_from_inventory(&[node]);
        assert_eq!(plans.len(), 8);
        for plan in &plans {
            assert!(!plan.plan_id.as_str().contains("node-a"));
        }
        let mut ids = plans.iter()
            .map(|plan| plan.plan_id.as_str().to_owned())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            [
                "auto-codex-workspace-a-codex-default-full-auto",
                "auto-codex-workspace-a-codex-default-full-auto-harness-mcp",
                "auto-codex-workspace-a-codex-default-moderate",
                "auto-codex-workspace-a-codex-default-moderate-harness-mcp",
                "auto-codex-workspace-a-codex-default-read-only",
                "auto-codex-workspace-a-codex-default-read-only-harness-mcp",
                "auto-codex-workspace-a-codex-default-unmanaged",
                "auto-codex-workspace-a-codex-default-unmanaged-harness-mcp",
            ],
        );
    }

    #[test]
    fn derive_launch_plans_cross_product_has_unique_ids_and_ordinary_shape() {
        let node = node_inventory(
            "node-a",
            &["workspace-a", "workspace-b"],
            &["claude", "codex"],
            &["default", "review"],
        );
        let plans = derive_launch_plans_from_inventory(&[node]);
        // claude and codex both resolve `Supported` (with a sourced ACP mode
        // id) at every level, so every combination gets the full 4-level x
        // 2-shape cross product.
        assert_eq!(plans.len(), 2 * 2 * 2 * 4 * 2);
        let mut ids = plans.iter().map(|plan| plan.plan_id.clone()).collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), plans.len());
        for plan in &plans {
            plan.validate().unwrap();
            assert!(plan.is_ordinary_dispatch());
            assert_eq!(plan.worktree, HarnessWorktreeIntentV1::Existing);
            assert_eq!(plan.grant, HarnessGrantPolicyV1::Operator);
            assert_eq!(plan.deadline_ms, DERIVED_LAUNCH_PLAN_DEADLINE_MS);
            assert_eq!(plan.terminal_size, DERIVED_LAUNCH_PLAN_TERMINAL_SIZE);
            assert_eq!(plan.revision, HarnessRevision::new(1).unwrap());
            assert!(plan.plan_id.as_str().contains(approval_level_slug(plan.approval_level)));
            if plan.harness_mcp == HarnessMcpPolicyV1::GrantBound {
                assert!(plan.plan_id.as_str().ends_with("-harness-mcp"));
                assert_eq!(plan.mode, HarnessExecutionModeV1::Acp);
                assert_eq!(plan.prompt_source, HarnessPromptSourceV1::TaskBody);
            } else {
                assert_eq!(plan.mode, HarnessExecutionModeV1::Pty);
                assert_eq!(plan.prompt_source, HarnessPromptSourceV1::Clear);
            }
        }
        assert_eq!(
            plans.iter().filter(|plan| {
                plan.harness_mcp == HarnessMcpPolicyV1::GrantBound
            }).count(),
            2 * 2 * 2 * 4,
        );
    }

    /// grok has no sourced ACP mode id for `FullAuto`/`Moderate` and
    /// resolves `Unsupported` outright for `ReadOnly`. kimi resolves
    /// `Supported` (a real vendor flag/behaviour exists) at every managed
    /// level, but a 2026-09-05 live measurement found its own `session/new`
    /// now offers no ACP modes at all, so every one of ITS managed levels
    /// also carries `acp_mode_id: None` (see
    /// `gate4agent_catalog::launch::approval_level_resolution`'s own doc
    /// comment) -- `Unmanaged` is the only level that yields an ACP/
    /// harness-MCP sibling for either of them. claude and codex each carry a
    /// sourced ACP mode id for all three managed levels, so all four levels
    /// yield a sibling for them.
    #[test]
    fn derive_launch_plans_gates_the_acp_sibling_by_the_catalogs_acp_mode_support() {
        for provider in ["claude", "codex"] {
            let node = node_inventory("node-a", &["workspace-a"], &[provider], &["default"]);
            let plans = derive_launch_plans_from_inventory(&[node]);
            let mut acp_levels = plans.iter()
                .filter(|plan| plan.harness_mcp == HarnessMcpPolicyV1::GrantBound)
                .map(|plan| plan.approval_level)
                .collect::<Vec<_>>();
            assert_eq!(acp_levels.len(), 4, "{provider}");
            acp_levels.sort_by_key(|level| approval_level_slug(*level));
            let mut expected = DERIVED_LAUNCH_PLAN_APPROVAL_LEVELS;
            expected.sort_by_key(|level| approval_level_slug(*level));
            assert_eq!(acp_levels, expected, "{provider}");
        }

        for provider in ["grok", "kimi"] {
            let node = node_inventory("node-a", &["workspace-a"], &[provider], &["default"]);
            let plans = derive_launch_plans_from_inventory(&[node]);
            let acp_levels = plans.iter()
                .filter(|plan| plan.harness_mcp == HarnessMcpPolicyV1::GrantBound)
                .map(|plan| plan.approval_level)
                .collect::<Vec<_>>();
            assert_eq!(acp_levels, [ApprovalLevel::Unmanaged], "{provider}");
        }

        // grok additionally has no verified `ReadOnly` mode at all
        // (`Unsupported` outright), so it loses the PTY plan there too;
        // kimi keeps a verified PTY flag/behaviour at every level -- only
        // its ACP mode ids were lost in the 2026-09-05 measurement.
        let grok_node = node_inventory("node-a", &["workspace-a"], &["grok"], &["default"]);
        let grok_plans = derive_launch_plans_from_inventory(&[grok_node]);
        let grok_pty_levels = grok_plans.iter()
            .filter(|plan| plan.harness_mcp == HarnessMcpPolicyV1::Disabled)
            .map(|plan| plan.approval_level)
            .collect::<Vec<_>>();
        assert_eq!(grok_pty_levels.len(), 3);
        assert!(!grok_pty_levels.contains(&ApprovalLevel::ReadOnly));

        let kimi_node = node_inventory("node-a", &["workspace-a"], &["kimi"], &["default"]);
        let kimi_plans = derive_launch_plans_from_inventory(&[kimi_node]);
        let kimi_pty_levels = kimi_plans.iter()
            .filter(|plan| plan.harness_mcp == HarnessMcpPolicyV1::Disabled)
            .map(|plan| plan.approval_level)
            .collect::<Vec<_>>();
        assert_eq!(kimi_pty_levels.len(), 4);
    }

    /// The regression this whole change closes: a derived plan's
    /// `approval_level` must reach `SpawnOverrides::approval_level` as
    /// `Some(_)`, never the hidden `None` the node used to resolve to
    /// `ApprovalLevel::FullAuto` -- the widest-authority level -- on every
    /// derived plan regardless of which level its id named.
    #[test]
    fn derived_launch_plans_never_carry_a_none_approval_level_into_spawn_spec() {
        let node = node_inventory("node-a", &["workspace-a"], &["codex", "grok"], &["default"]);
        let plans = derive_launch_plans_from_inventory(&[node]);
        assert!(!plans.is_empty());
        for plan in &plans {
            let dispatch = HarnessDispatchIntentV1 {
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
                    node_id: plan.node_id.clone(),
                    workspace_id: plan.workspace_id.clone(),
                    worktree: HarnessWorktreeIntentV1::Existing,
                    provider_profile: plan.provider_profile.clone(),
                    mode: plan.mode,
                    delivery_bundle: None,
                    continuation: None,
                },
            };
            let task = HarnessTaskV1 {
                task_id: dispatch.task_id.clone(),
                revision: dispatch.task_revision,
                title: "Derived".to_owned(),
                body: "inspect the repository".to_owned(),
                creator: HarnessActorV1::User { actor_id: selector("operator") },
                parent_task_id: None,
                dependencies: Vec::new(),
                state: HarnessTaskStateV1::Running,
                run_ids: vec![dispatch.run_id.clone()],
                result_refs: Vec::new(),
                artifact_refs: Vec::new(),
                created_at_unix_ms: 1,
                updated_at_unix_ms: 2,
            };
            let spec = plan.spawn_spec(
                &dispatch,
                &task,
                SpawnProfileRevision::new("r1").unwrap(),
            ).unwrap();
            assert_eq!(spec.overrides.approval_level, Some(plan.approval_level), "{:?}", plan.plan_id);
        }
    }

    #[test]
    fn catalog_ordinary_plans_and_resolve_ordinary_scheduled_admit_the_derived_harness_mcp_sibling() {
        let node = node_inventory("node-a", &["workspace-a"], &["codex"], &["codex-default"]);
        let plans = derive_launch_plans_from_inventory(&[node]);
        let catalog = HarnessLaunchCatalog::new(plans).unwrap();
        let mcp_plan = catalog.ordinary_plans()
            .find(|plan| plan.harness_mcp == HarnessMcpPolicyV1::GrantBound)
            .expect("derived catalog carries a harness-MCP sibling");
        // `HarnessLaunchCatalog` orders plans by id (`BTreeMap`); among the
        // four ACP siblings this combination now derives, `full-auto` sorts
        // first lexicographically.
        assert_eq!(
            mcp_plan.plan_id.as_str(),
            "auto-codex-workspace-a-codex-default-full-auto-harness-mcp",
        );
        assert_eq!(mcp_plan.mode, HarnessExecutionModeV1::Acp);
        assert_eq!(mcp_plan.approval_level, ApprovalLevel::FullAuto);
        let scheduled = mcp_plan.ordinary_scheduled_ref().unwrap();
        assert_eq!(
            catalog.resolve_ordinary_scheduled(&scheduled).unwrap().plan_id,
            mcp_plan.plan_id,
        );
    }

    #[test]
    fn derive_launch_plans_empty_and_missing_inventory_yields_no_plans() {
        assert!(derive_launch_plans_from_inventory(&[]).is_empty());

        let mut node = node_inventory("node-a", &["workspace-a"], &["codex"], &[]);
        node.inventory.launch_inventory = None;
        assert!(derive_launch_plans_from_inventory(&[node.clone()]).is_empty());

        node.inventory.launch_inventory = Some(HarnessRuntimeLaunchInventoryV1 {
            spawn_profiles: None,
            bundles: None,
        });
        assert!(derive_launch_plans_from_inventory(&[node.clone()]).is_empty());

        node.inventory.launch_inventory = Some(HarnessRuntimeLaunchInventoryV1 {
            spawn_profiles: Some(vec![HarnessRuntimeSpawnProfileSummaryV1 {
                id: "codex-default".to_owned(),
                revision: "r1".to_owned(),
                environment_profile: None,
            }]),
            bundles: None,
        });
        node.inventory.enabled_providers = Vec::new();
        assert!(derive_launch_plans_from_inventory(&[node.clone()]).is_empty());

        node.inventory.enabled_providers = vec!["codex".to_owned()];
        node.inventory.workspaces = BTreeMap::new();
        assert!(derive_launch_plans_from_inventory(&[node]).is_empty());
    }

    #[test]
    fn launch_catalog_all_plans_and_contains_see_every_configured_plan() {
        let catalog = HarnessLaunchCatalog::new([plan(HarnessPromptSourceV1::TaskBody)]).unwrap();
        assert_eq!(catalog.all_plans().count(), 1);
        assert!(catalog.contains(&selector("default")));
        assert!(!catalog.contains(&selector("auto-codex-workspace-a-codex-default")));
    }
}
