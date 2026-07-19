use gate4agent_types::{AgentInstanceId, SessionGeneration};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const TOOL_ACTOR_ID_MAX_BYTES: usize = 256;
pub const TOOL_CONSUMER_ID_MAX_BYTES: usize = 256;
pub const TOOL_PROVIDER_ID_MAX_BYTES: usize = 256;
pub const TOOL_CAPABILITY_ID_MAX_BYTES: usize = 256;
pub const TOOL_RESOURCE_SCOPE_ID_MAX_BYTES: usize = 512;
pub const TOOL_CAPABILITY_DESCRIPTION_MAX_BYTES: usize = 2_048;
pub const TOOL_APPROVAL_SUMMARY_MAX_BYTES: usize = 1_024;
pub const TOOL_PAYLOAD_MAX_BYTES: usize = 64 * 1_024;
pub const TOOL_RESULT_MAX_BYTES: u64 = 16 * 1_024 * 1_024;
pub const TOOL_INLINE_RESULT_MAX_BYTES: usize = 256 * 1_024;
pub const TOOL_RESULT_REFERENCE_MAX_BYTES: usize = 2_048;
pub const TOOL_RESULT_SUMMARY_MAX_BYTES: usize = 4_096;
pub const TOOL_FAILURE_MESSAGE_MAX_BYTES: usize = 4_096;
pub const TOOL_MEDIA_TYPE_MAX_BYTES: usize = 256;
pub const TOOL_PROVIDERS_MAX: usize = 64;
pub const TOOL_CAPABILITIES_PER_PROVIDER_MAX: usize = 256;
pub const TOOL_POLICIES_MAX: usize = 4_096;
pub const TOOL_REQUESTS_MAX: usize = 512;
pub const TOOL_EFFECTS_MAX: usize = TOOL_REQUESTS_MAX * 2;
pub const TOOL_COMPLETIONS_MAX: usize = 128;
pub const TOOL_AUDIT_EVENTS_MAX: usize = 4_096;

macro_rules! bounded_id {
    ($name:ident, $field:literal, $max:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ToolValidationError> {
                let value = value.into();
                validate_identifier($field, &value, $max)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ToolValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

bounded_id!(ToolActorId, "tool actor id", TOOL_ACTOR_ID_MAX_BYTES);
bounded_id!(ConsumerId, "tool consumer id", TOOL_CONSUMER_ID_MAX_BYTES);
bounded_id!(
    ResourceScopeId,
    "tool resource scope id",
    TOOL_RESOURCE_SCOPE_ID_MAX_BYTES
);
bounded_id!(
    ToolProviderId,
    "tool provider id",
    TOOL_PROVIDER_ID_MAX_BYTES
);
bounded_id!(
    ToolCapabilityId,
    "tool capability id",
    TOOL_CAPABILITY_ID_MAX_BYTES
);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityRequestId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolOperationId(pub u64);

/// Provider ownership is an admission boundary. `Consumer(owner)` can serve
/// only requests whose exact `consumer_id` equals `owner`; `Gate` may be
/// shared, but only through an otherwise exact policy grant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityOwner {
    Gate,
    Consumer(ConsumerId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityClass {
    Browser,
    ConsumerState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilityDescriptor {
    pub id: ToolCapabilityId,
    pub class: CapabilityClass,
    pub description: String,
}

impl CapabilityDescriptor {
    pub fn new(
        id: ToolCapabilityId,
        class: CapabilityClass,
        description: impl Into<String>,
    ) -> Result<Self, ToolValidationError> {
        let descriptor = Self {
            id,
            class,
            description: description.into(),
        };
        descriptor.validate_admission()?;
        validate_required_text(
            "tool capability description",
            &descriptor.description,
            TOOL_CAPABILITY_DESCRIPTION_MAX_BYTES,
        )?;
        Ok(descriptor)
    }

    fn validate_admission(&self) -> Result<(), ToolValidationError> {
        let normalized = self.id.as_str().to_ascii_lowercase();
        let has_forbidden_namespace = normalized
            .split(['.', ':', '/', '-', '_'])
            .any(|segment| matches!(segment, "shell" | "filesystem" | "fs" | "mcp"));
        let prefix = match self.class {
            CapabilityClass::Browser => "browser.",
            CapabilityClass::ConsumerState => "consumer-state.",
        };
        if has_forbidden_namespace || !normalized.starts_with(prefix) {
            return Err(ToolValidationError::CapabilityOutsideAdmission {
                capability_id: self.id.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilityProviderDescriptor {
    pub id: ToolProviderId,
    pub owner: CapabilityOwner,
    pub capabilities: Vec<CapabilityDescriptor>,
}

impl CapabilityProviderDescriptor {
    pub(crate) fn validate(&self) -> Result<(), ToolValidationError> {
        if self.capabilities.is_empty() {
            return Err(ToolValidationError::Required {
                field: "tool provider capabilities",
            });
        }
        if self.capabilities.len() > TOOL_CAPABILITIES_PER_PROVIDER_MAX {
            return Err(ToolValidationError::TooMany {
                field: "tool provider capabilities",
                max: TOOL_CAPABILITIES_PER_PROVIDER_MAX,
                actual: self.capabilities.len(),
            });
        }
        let mut ids = self
            .capabilities
            .iter()
            .map(|capability| capability.id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ToolValidationError::DuplicateIdentifier {
                field: "tool capability id",
            });
        }
        for capability in &self.capabilities {
            capability.validate_admission()?;
            validate_required_text(
                "tool capability description",
                &capability.description,
                TOOL_CAPABILITY_DESCRIPTION_MAX_BYTES,
            )?;
        }
        Ok(())
    }

    pub(crate) fn has_capability(&self, capability_id: &ToolCapabilityId) -> bool {
        self.capabilities
            .iter()
            .any(|capability| &capability.id == capability_id)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PolicyKey {
    pub consumer_id: ConsumerId,
    pub actor_id: ToolActorId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantMode {
    Allow,
    RequireApproval,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PolicyGrant {
    pub key: PolicyKey,
    pub mode: GrantMode,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct CapabilityRequest {
    pub id: CapabilityRequestId,
    pub consumer_id: ConsumerId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub actor_id: ToolActorId,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
    pub approval_summary: String,
    pub deadline_tick: u64,
    pub payload: Vec<u8>,
}

impl fmt::Debug for CapabilityRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityRequest")
            .field("id", &self.id)
            .field("consumer_id", &self.consumer_id)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("actor_id", &self.actor_id)
            .field("provider_id", &self.provider_id)
            .field("capability_id", &self.capability_id)
            .field("resource_scope_id", &self.resource_scope_id)
            .field("approval_summary", &self.approval_summary)
            .field("deadline_tick", &self.deadline_tick)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

impl CapabilityRequest {
    pub(crate) fn validate(&self, current_tick: u64) -> Result<(), ToolValidationError> {
        if self.id.0 == 0 {
            return Err(ToolValidationError::ZeroIdentifier {
                field: "tool request id",
            });
        }
        if self.deadline_tick <= current_tick {
            return Err(ToolValidationError::DeadlineElapsed {
                current_tick,
                deadline_tick: self.deadline_tick,
            });
        }
        if self.payload.len() > TOOL_PAYLOAD_MAX_BYTES {
            return Err(ToolValidationError::TooLarge {
                field: "tool request payload",
                max: TOOL_PAYLOAD_MAX_BYTES,
                actual: self.payload.len(),
            });
        }
        validate_required_text(
            "tool approval summary",
            &self.approval_summary,
            TOOL_APPROVAL_SUMMARY_MAX_BYTES,
        )?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDenial {
    UnknownInstance,
    StaleGeneration { current: SessionGeneration },
    UnknownProvider,
    UnknownCapability,
    ProviderOwnerMismatch,
    MissingGrant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDecision {
    Deny(PolicyDenial),
    RequireApproval,
    Allow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalDecision {
    ApproveOnce,
    Deny,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApprovalResolution {
    pub request_id: CapabilityRequestId,
    pub accepted_sequence: u64,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub decision: ApprovalDecision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationCancelReason {
    DeadlineElapsed,
    GenerationSuperseded,
    GrantRevoked,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityEffect {
    Invoke {
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
        capability_id: ToolCapabilityId,
        resource_scope_id: ResourceScopeId,
        payload: Vec<u8>,
    },
    Cancel {
        reason: InvocationCancelReason,
    },
}

impl fmt::Debug for CapabilityEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invoke {
                consumer_id,
                actor_id,
                capability_id,
                resource_scope_id,
                payload,
            } => formatter
                .debug_struct("Invoke")
                .field("consumer_id", consumer_id)
                .field("actor_id", actor_id)
                .field("capability_id", capability_id)
                .field("resource_scope_id", resource_scope_id)
                .field("payload_bytes", &payload.len())
                .finish(),
            Self::Cancel { reason } => formatter
                .debug_struct("Cancel")
                .field("reason", reason)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct CapabilityEffectEnvelope {
    pub sequence: u64,
    pub operation_id: ToolOperationId,
    pub request_id: CapabilityRequestId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub deadline_tick: u64,
    pub effect: CapabilityEffect,
}

impl fmt::Debug for CapabilityEffectEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityEffectEnvelope")
            .field("sequence", &self.sequence)
            .field("operation_id", &self.operation_id)
            .field("request_id", &self.request_id)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("deadline_tick", &self.deadline_tick)
            .field("effect", &self.effect)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilityResultMetadata {
    pub byte_len: u64,
    pub media_type: Option<String>,
    pub truncated: bool,
    pub redacted_summary: Option<String>,
}

impl CapabilityResultMetadata {
    pub(crate) fn validate(&self) -> Result<(), ToolValidationError> {
        if self.byte_len > TOOL_RESULT_MAX_BYTES {
            return Err(ToolValidationError::ResultTooLarge {
                max: TOOL_RESULT_MAX_BYTES,
                actual: self.byte_len,
            });
        }
        validate_optional_text(
            "tool result media type",
            self.media_type.as_deref(),
            TOOL_MEDIA_TYPE_MAX_BYTES,
        )?;
        validate_optional_text(
            "tool result redacted summary",
            self.redacted_summary.as_deref(),
            TOOL_RESULT_SUMMARY_MAX_BYTES,
        )
    }
}

/// Bounded result delivery released to the caller through
/// [`crate::ToolEngine::drain_completions`]. Neither variant is retained in
/// snapshots or audit events. An opaque reference is meaningful only to the
/// provider identified by its completion envelope; the core never resolves or
/// interprets it.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityResultDelivery {
    Inline { bytes: Vec<u8> },
    OpaqueReference { reference: String },
}

impl fmt::Debug for CapabilityResultDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline { bytes } => formatter
                .debug_struct("Inline")
                .field("byte_len", &bytes.len())
                .finish(),
            Self::OpaqueReference { reference } => formatter
                .debug_struct("OpaqueReference")
                .field("reference_bytes", &reference.len())
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct CapabilityResult {
    pub metadata: CapabilityResultMetadata,
    pub delivery: CapabilityResultDelivery,
}

impl fmt::Debug for CapabilityResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityResult")
            .field("metadata", &self.metadata)
            .field("delivery", &self.delivery)
            .finish()
    }
}

impl CapabilityResult {
    pub(crate) fn validate(&self) -> Result<(), ToolValidationError> {
        self.metadata.validate()?;
        match &self.delivery {
            CapabilityResultDelivery::Inline { bytes } => {
                if bytes.len() > TOOL_INLINE_RESULT_MAX_BYTES {
                    return Err(ToolValidationError::TooLarge {
                        field: "inline tool result",
                        max: TOOL_INLINE_RESULT_MAX_BYTES,
                        actual: bytes.len(),
                    });
                }
                if self.metadata.byte_len != bytes.len() as u64 {
                    return Err(ToolValidationError::ResultLengthMismatch {
                        declared: self.metadata.byte_len,
                        actual: bytes.len(),
                    });
                }
                Ok(())
            }
            CapabilityResultDelivery::OpaqueReference { reference } => validate_required_text(
                "tool result opaque reference",
                reference,
                TOOL_RESULT_REFERENCE_MAX_BYTES,
            ),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct CapabilityCompletionEnvelope {
    pub sequence: u64,
    pub operation_id: ToolOperationId,
    pub request_id: CapabilityRequestId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub result: CapabilityResult,
}

impl fmt::Debug for CapabilityCompletionEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityCompletionEnvelope")
            .field("sequence", &self.sequence)
            .field("operation_id", &self.operation_id)
            .field("request_id", &self.request_id)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("result", &self.result)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolFailureKind {
    Rejected,
    Unavailable,
    InvalidInput,
    Execution,
    Cancelled,
    ProviderContractViolation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolFailure {
    pub kind: ToolFailureKind,
    pub redacted_message: Option<String>,
}

impl ToolFailure {
    pub(crate) fn validate(&self) -> Result<(), ToolValidationError> {
        validate_optional_text(
            "tool failure redacted message",
            self.redacted_message.as_deref(),
            TOOL_FAILURE_MESSAGE_MAX_BYTES,
        )
    }

    pub(crate) fn provider_contract_violation() -> Self {
        Self {
            kind: ToolFailureKind::ProviderContractViolation,
            redacted_message: None,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityObservation {
    Succeeded { result: CapabilityResult },
    Failed { failure: ToolFailure },
}

impl fmt::Debug for CapabilityObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeeded { result } => formatter
                .debug_struct("Succeeded")
                .field("result", result)
                .finish(),
            Self::Failed { failure } => formatter
                .debug_struct("Failed")
                .field("failure", failure)
                .finish(),
        }
    }
}

impl CapabilityObservation {
    pub(crate) fn validate(&self) -> Result<(), ToolValidationError> {
        match self {
            Self::Succeeded { result } => result.validate(),
            Self::Failed { failure } => failure.validate(),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct CapabilityObservationEnvelope {
    pub operation_id: ToolOperationId,
    pub request_id: CapabilityRequestId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub observation: CapabilityObservation,
}

impl fmt::Debug for CapabilityObservationEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityObservationEnvelope")
            .field("operation_id", &self.operation_id)
            .field("request_id", &self.request_id)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("observation", &self.observation)
            .finish()
    }
}

/// Cancellation remains unconfirmed after a cancel effect is queued. Only
/// `QueuedInvokeRemoved` proves the invocation never left the engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CancellationDisposition {
    NotRequired,
    QueuedInvokeRemoved,
    CancelQueuedUnconfirmed,
    DroppedQueueFull,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityRequestStatus {
    Denied {
        reason: PolicyDenial,
    },
    AwaitingApproval,
    Dispatched {
        operation_id: ToolOperationId,
    },
    Succeeded {
        operation_id: ToolOperationId,
        result: CapabilityResultMetadata,
    },
    Failed {
        operation_id: ToolOperationId,
        failure: ToolFailure,
    },
    ApprovalDenied,
    GrantRevoked {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    TimedOut {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    Superseded {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
        current_generation: SessionGeneration,
    },
}

impl CapabilityRequestStatus {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Denied { .. }
                | Self::Succeeded { .. }
                | Self::Failed { .. }
                | Self::ApprovalDenied
                | Self::GrantRevoked { .. }
                | Self::TimedOut { .. }
                | Self::Superseded { .. }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilityRequestSnapshot {
    pub id: CapabilityRequestId,
    pub accepted_sequence: u64,
    pub accepted_at_tick: u64,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub consumer_id: ConsumerId,
    pub actor_id: ToolActorId,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
    pub approval_summary: String,
    pub deadline_tick: u64,
    pub payload_bytes: usize,
    pub policy_decision: PolicyDecision,
    pub status: CapabilityRequestStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolAuditSubject {
    pub request_id: CapabilityRequestId,
    pub accepted_sequence: u64,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub consumer_id: ConsumerId,
    pub actor_id: ToolActorId,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationIgnoredReason {
    UnknownRequest,
    StaleGeneration,
    InstanceMismatch,
    ProviderMismatch,
    RequestNotDispatched,
    OperationMismatch,
    DeadlineElapsed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAuditEventKind {
    ProviderRegistered {
        provider_id: ToolProviderId,
        owner: CapabilityOwner,
        capability_count: usize,
    },
    GrantSet {
        grant: PolicyGrant,
    },
    GrantRevoked {
        key: PolicyKey,
    },
    GenerationAdvanced {
        instance_id: AgentInstanceId,
        previous: Option<SessionGeneration>,
        current: SessionGeneration,
        purged_grant_count: usize,
    },
    RequestEvaluated {
        decision: PolicyDecision,
        payload_bytes: usize,
    },
    ApprovalResolved {
        decision: ApprovalDecision,
    },
    RequestGrantRevoked {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    InvocationDispatched {
        operation_id: ToolOperationId,
    },
    InvocationSucceeded {
        operation_id: ToolOperationId,
        result_bytes: u64,
        truncated: bool,
    },
    InvocationFailed {
        operation_id: ToolOperationId,
        failure_kind: ToolFailureKind,
    },
    RequestTimedOut {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    RequestSuperseded {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
        current_generation: SessionGeneration,
    },
    ObservationIgnored {
        operation_id: ToolOperationId,
        reason: ObservationIgnoredReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolAuditEvent {
    pub sequence: u64,
    pub tick: u64,
    pub subject: Option<ToolAuditSubject>,
    pub event: ToolAuditEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolEngineSnapshot {
    pub revision: u64,
    pub current_tick: u64,
    pub generations: Vec<(AgentInstanceId, SessionGeneration)>,
    pub providers: Vec<CapabilityProviderDescriptor>,
    pub grants: Vec<PolicyGrant>,
    pub requests: Vec<CapabilityRequestSnapshot>,
    pub audit_events: Vec<ToolAuditEvent>,
    pub dropped_audit_events: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolValidationError {
    Required {
        field: &'static str,
    },
    InvalidIdentifier {
        field: &'static str,
    },
    ControlCharacter {
        field: &'static str,
    },
    CapabilityOutsideAdmission {
        capability_id: ToolCapabilityId,
    },
    DuplicateIdentifier {
        field: &'static str,
    },
    ZeroIdentifier {
        field: &'static str,
    },
    TooLarge {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    TooMany {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    ResultTooLarge {
        max: u64,
        actual: u64,
    },
    ResultLengthMismatch {
        declared: u64,
        actual: usize,
    },
    DeadlineElapsed {
        current_tick: u64,
        deadline_tick: u64,
    },
}

impl fmt::Display for ToolValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid tool contract: {self:?}")
    }
}

impl std::error::Error for ToolValidationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolEngineError {
    Validation(ToolValidationError),
    DuplicateProvider {
        provider_id: ToolProviderId,
    },
    DuplicateRequest {
        request_id: CapabilityRequestId,
    },
    ProviderCapacityExceeded,
    PolicyCapacityExceeded,
    RequestCapacityExceeded,
    EffectCapacityExceeded,
    CompletionCapacityExceeded,
    UnknownPolicyProvider {
        provider_id: ToolProviderId,
    },
    UnknownPolicyCapability {
        provider_id: ToolProviderId,
        capability_id: ToolCapabilityId,
    },
    ProviderOwnerMismatch {
        provider_id: ToolProviderId,
        owner: ConsumerId,
        requested_consumer: ConsumerId,
    },
    UnknownPolicyInstance {
        instance_id: AgentInstanceId,
    },
    PolicyGenerationMismatch {
        instance_id: AgentInstanceId,
        current: SessionGeneration,
        requested: SessionGeneration,
    },
    UnknownRequest {
        request_id: CapabilityRequestId,
    },
    RequestNotAwaitingApproval {
        request_id: CapabilityRequestId,
    },
    ApprovalScopeMismatch {
        request_id: CapabilityRequestId,
    },
    ApprovalNonceMismatch {
        request_id: CapabilityRequestId,
        expected: u64,
        actual: u64,
    },
    ApprovalGenerationStale {
        current: SessionGeneration,
        actual: SessionGeneration,
    },
    ApprovalDeadlineElapsed {
        request_id: CapabilityRequestId,
    },
    ClockRegressed {
        current_tick: u64,
        requested_tick: u64,
    },
    GenerationRegressed {
        instance_id: AgentInstanceId,
        current: SessionGeneration,
        requested: SessionGeneration,
    },
    CounterExhausted {
        counter: &'static str,
    },
}

impl From<ToolValidationError> for ToolEngineError {
    fn from(error: ToolValidationError) -> Self {
        Self::Validation(error)
    }
}

impl fmt::Display for ToolEngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tool engine rejected transition: {self:?}")
    }
}

impl std::error::Error for ToolEngineError {}

fn validate_identifier(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ToolValidationError> {
    if value.trim().is_empty() {
        return Err(ToolValidationError::Required { field });
    }
    if value.len() > max {
        return Err(ToolValidationError::TooLarge {
            field,
            max,
            actual: value.len(),
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(ToolValidationError::InvalidIdentifier { field });
    }
    Ok(())
}

fn validate_required_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ToolValidationError> {
    if value.trim().is_empty() {
        return Err(ToolValidationError::Required { field });
    }
    validate_text(field, value, max)
}

fn validate_optional_text(
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> Result<(), ToolValidationError> {
    match value {
        Some(value) => validate_text(field, value, max),
        None => Ok(()),
    }
}

fn validate_text(field: &'static str, value: &str, max: usize) -> Result<(), ToolValidationError> {
    if value.len() > max {
        return Err(ToolValidationError::TooLarge {
            field,
            max,
            actual: value.len(),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ToolValidationError::ControlCharacter { field });
    }
    Ok(())
}
