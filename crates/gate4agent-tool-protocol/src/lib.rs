//! Dependency-light, versioned contracts for the Gate4Agent capability plane.
//!
//! These types perform no I/O and grant no authority by themselves. The tool
//! engine validates every deserialized envelope again at its trusted ingress.

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
pub const TOOL_ACTIVE_REQUESTS_PER_CLIENT_MAX: usize = 32;
pub const TOOL_EFFECTS_MAX: usize = TOOL_REQUESTS_MAX * 2;
pub const TOOL_COMPLETIONS_MAX: usize = 128;
pub const TOOL_AUDIT_EVENTS_MAX: usize = 4_096;
pub const CAPABILITY_PROTOCOL_VERSION: u16 = 1;

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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityOwner {
    Gate,
    Consumer(ConsumerId),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityClass {
    Browser,
    ConsumerState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

    pub fn validate_admission(&self) -> Result<(), ToolValidationError> {
        let normalized = self.id.as_str().to_ascii_lowercase();
        let has_forbidden_namespace = has_forbidden_capability_namespace(&normalized);
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityProviderDescriptor {
    pub id: ToolProviderId,
    pub owner: CapabilityOwner,
    pub capabilities: Vec<CapabilityDescriptor>,
}

impl CapabilityProviderDescriptor {
    pub fn validate(&self) -> Result<(), ToolValidationError> {
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

    pub fn has_capability(&self, capability_id: &ToolCapabilityId) -> bool {
        self.capabilities
            .iter()
            .any(|capability| &capability.id == capability_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PolicyKey {
    pub consumer_id: ConsumerId,
    pub actor_id: ToolActorId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantMode {
    Allow,
    RequireApproval,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyGrant {
    pub key: PolicyKey,
    pub mode: GrantMode,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CapabilityRequestKey {
    pub consumer_id: ConsumerId,
    pub actor_id: ToolActorId,
    pub local_id: CapabilityRequestId,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequestInput {
    pub local_id: CapabilityRequestId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
    pub approval_summary: String,
    pub deadline_tick: u64,
    pub payload: Vec<u8>,
}

impl fmt::Debug for CapabilityRequestInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityRequestInput")
            .field("local_id", &self.local_id)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("capability_id", &self.capability_id)
            .field("resource_scope_id", &self.resource_scope_id)
            .field("approval_summary", &self.approval_summary)
            .field("deadline_tick", &self.deadline_tick)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

impl CapabilityRequestInput {
    pub fn validate(&self, current_tick: u64) -> Result<(), ToolValidationError> {
        if self.local_id.0 == 0 {
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

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConsumerBoundCapabilityRequest {
    pub protocol_version: u16,
    pub consumer_id: ConsumerId,
    pub actor_id: ToolActorId,
    pub request: CapabilityRequestInput,
}

impl ConsumerBoundCapabilityRequest {
    pub fn new(
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
        request: CapabilityRequestInput,
    ) -> Self {
        Self {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            consumer_id,
            actor_id,
            request,
        }
    }

    pub fn key(&self) -> CapabilityRequestKey {
        CapabilityRequestKey {
            consumer_id: self.consumer_id.clone(),
            actor_id: self.actor_id.clone(),
            local_id: self.request.local_id,
        }
    }

    pub fn validate(&self, current_tick: u64) -> Result<(), ToolValidationError> {
        validate_protocol_version(self.protocol_version)?;
        self.request.validate(current_tick)
    }
}

impl fmt::Debug for ConsumerBoundCapabilityRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConsumerBoundCapabilityRequest")
            .field("protocol_version", &self.protocol_version)
            .field("key", &self.key())
            .field("request", &self.request)
            .finish()
    }
}

pub type CapabilityRequest = ConsumerBoundCapabilityRequest;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDenial {
    UnknownInstance,
    InactiveInstance,
    StaleGeneration { current: SessionGeneration },
    UnknownProvider,
    UnknownCapability,
    ProviderOwnerMismatch,
    MissingGrant,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyDecision {
    Deny(PolicyDenial),
    RequireApproval,
    Allow,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalDecision {
    ApproveOnce,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalResolution {
    pub request_key: CapabilityRequestKey,
    pub accepted_sequence: u64,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub decision: ApprovalDecision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolAuthorityEnvelope {
    pub protocol_version: u16,
    pub sequence: u64,
    pub command: ToolAuthorityCommand,
}

impl ToolAuthorityEnvelope {
    pub fn validate(&self) -> Result<(), ToolValidationError> {
        validate_protocol_version(self.protocol_version)?;
        if self.sequence == 0 {
            return Err(ToolValidationError::ZeroIdentifier {
                field: "tool authority sequence",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAuthorityCommand {
    SetGrant {
        grant: PolicyGrant,
    },
    RevokeGrant {
        key: PolicyKey,
    },
    ResolveApproval {
        resolution: ApprovalResolution,
    },
    CloseClient {
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAuthorityOutcome {
    GrantSet,
    GrantRevoked {
        existed: bool,
    },
    ApprovalResolved,
    ApprovalExpired {
        request_key: CapabilityRequestKey,
        accepted_sequence: u64,
    },
    ClientClosed {
        purged_grant_count: usize,
        closed_request_count: usize,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolInstanceState {
    Active,
    Inactive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationCancelReason {
    DeadlineElapsed,
    GenerationSuperseded,
    GrantRevoked,
    InstanceClosed,
    ClientClosed,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityEffectEnvelope {
    pub protocol_version: u16,
    pub sequence: u64,
    pub operation_id: ToolOperationId,
    pub request_key: CapabilityRequestKey,
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
            .field("protocol_version", &self.protocol_version)
            .field("sequence", &self.sequence)
            .field("operation_id", &self.operation_id)
            .field("request_key", &self.request_key)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("deadline_tick", &self.deadline_tick)
            .field("effect", &self.effect)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityResultMetadata {
    pub byte_len: u64,
    pub media_type: Option<String>,
    pub truncated: bool,
    pub redacted_summary: Option<String>,
}

impl CapabilityResultMetadata {
    pub fn validate(&self) -> Result<(), ToolValidationError> {
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
/// a [`CapabilityCompletionBatch`]. Neither variant is retained in
/// snapshots or audit events. An opaque reference is meaningful only to the
/// provider identified by its completion envelope; the core never resolves or
/// interprets it.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
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
    pub fn validate(&self) -> Result<(), ToolValidationError> {
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

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityTerminalOutcome {
    Succeeded {
        result: CapabilityResult,
    },
    Failed {
        failure: ToolFailure,
    },
    PolicyDenied {
        reason: PolicyDenial,
    },
    ApprovalDenied,
    GrantRevoked {
        cancellation: CancellationDisposition,
    },
    TimedOut {
        cancellation: CancellationDisposition,
    },
    Superseded {
        current_generation: SessionGeneration,
        cancellation: CancellationDisposition,
    },
    InstanceClosed {
        cancellation: CancellationDisposition,
    },
    ClientClosed {
        cancellation: CancellationDisposition,
    },
}

impl fmt::Debug for CapabilityTerminalOutcome {
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
            Self::PolicyDenied { reason } => formatter
                .debug_struct("PolicyDenied")
                .field("reason", reason)
                .finish(),
            Self::ApprovalDenied => formatter.write_str("ApprovalDenied"),
            Self::GrantRevoked { cancellation } => formatter
                .debug_struct("GrantRevoked")
                .field("cancellation", cancellation)
                .finish(),
            Self::TimedOut { cancellation } => formatter
                .debug_struct("TimedOut")
                .field("cancellation", cancellation)
                .finish(),
            Self::Superseded {
                current_generation,
                cancellation,
            } => formatter
                .debug_struct("Superseded")
                .field("current_generation", current_generation)
                .field("cancellation", cancellation)
                .finish(),
            Self::InstanceClosed { cancellation } => formatter
                .debug_struct("InstanceClosed")
                .field("cancellation", cancellation)
                .finish(),
            Self::ClientClosed { cancellation } => formatter
                .debug_struct("ClientClosed")
                .field("cancellation", cancellation)
                .finish(),
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityCompletionEnvelope {
    pub protocol_version: u16,
    pub sequence: u64,
    pub accepted_sequence: u64,
    pub operation_id: Option<ToolOperationId>,
    pub request_key: CapabilityRequestKey,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub outcome: CapabilityTerminalOutcome,
}

impl fmt::Debug for CapabilityCompletionEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityCompletionEnvelope")
            .field("protocol_version", &self.protocol_version)
            .field("sequence", &self.sequence)
            .field("accepted_sequence", &self.accepted_sequence)
            .field("operation_id", &self.operation_id)
            .field("request_key", &self.request_key)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("outcome", &self.outcome)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityCompletionBatch {
    pub completions: Vec<CapabilityCompletionEnvelope>,
    pub dropped_since_last_drain: u64,
    pub total_dropped: u64,
    pub next_sequence: u64,
    pub sequence_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolFailureKind {
    Rejected,
    Unavailable,
    InvalidInput,
    Execution,
    Cancelled,
    ProviderContractViolation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolFailure {
    pub kind: ToolFailureKind,
    pub redacted_message: Option<String>,
}

impl ToolFailure {
    pub fn validate(&self) -> Result<(), ToolValidationError> {
        validate_optional_text(
            "tool failure redacted message",
            self.redacted_message.as_deref(),
            TOOL_FAILURE_MESSAGE_MAX_BYTES,
        )
    }

    pub fn provider_contract_violation() -> Self {
        Self {
            kind: ToolFailureKind::ProviderContractViolation,
            redacted_message: None,
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
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
    pub fn validate(&self) -> Result<(), ToolValidationError> {
        match self {
            Self::Succeeded { result } => result.validate(),
            Self::Failed { failure } => failure.validate(),
        }
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityObservationEnvelope {
    pub protocol_version: u16,
    pub operation_id: ToolOperationId,
    pub request_key: CapabilityRequestKey,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub observation: CapabilityObservation,
}

impl fmt::Debug for CapabilityObservationEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityObservationEnvelope")
            .field("protocol_version", &self.protocol_version)
            .field("operation_id", &self.operation_id)
            .field("request_key", &self.request_key)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("provider_id", &self.provider_id)
            .field("observation", &self.observation)
            .finish()
    }
}

impl CapabilityObservationEnvelope {
    pub fn validate(&self) -> Result<(), ToolValidationError> {
        validate_protocol_version(self.protocol_version)?;
        if self.operation_id.0 == 0 {
            return Err(ToolValidationError::ZeroIdentifier {
                field: "tool operation id",
            });
        }
        self.observation.validate()
    }
}

/// Cancellation remains unconfirmed after a cancel effect is queued. Only
/// `QueuedInvokeRemoved` proves the invocation never left the engine.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CancellationDisposition {
    NotRequired,
    QueuedInvokeRemoved,
    CancelQueuedUnconfirmed,
    DroppedQueueFull,
    DroppedSequenceExhausted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    InstanceClosed {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    ClientClosed {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
}

impl CapabilityRequestStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Denied { .. }
                | Self::Succeeded { .. }
                | Self::Failed { .. }
                | Self::ApprovalDenied
                | Self::GrantRevoked { .. }
                | Self::TimedOut { .. }
                | Self::Superseded { .. }
                | Self::InstanceClosed { .. }
                | Self::ClientClosed { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityRequestSnapshot {
    pub key: CapabilityRequestKey,
    pub accepted_sequence: u64,
    pub accepted_at_tick: u64,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
    /// Bounded, intentionally displayable text for a trusted approval surface.
    pub approval_summary: String,
    pub approval_summary_bytes: usize,
    pub deadline_tick: u64,
    pub payload_bytes: usize,
    pub policy_decision: PolicyDecision,
    pub status: CapabilityRequestStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolAuditSubject {
    pub request_key: CapabilityRequestKey,
    pub accepted_sequence: u64,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
    pub provider_id: ToolProviderId,
    pub capability_id: ToolCapabilityId,
    pub resource_scope_id: ResourceScopeId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    InstanceStateChanged {
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        state: ToolInstanceState,
        purged_grant_count: usize,
    },
    InstanceRemoved {
        instance_id: AgentInstanceId,
        previous_generation: SessionGeneration,
        purged_grant_count: usize,
    },
    ClientClosed {
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
        purged_grant_count: usize,
        closed_request_count: usize,
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
    RequestInstanceClosed {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    RequestClientClosed {
        operation_id: Option<ToolOperationId>,
        cancellation: CancellationDisposition,
    },
    CompletionDropped {
        completion_sequence: Option<u64>,
        reason: CompletionDropReason,
    },
    ObservationIgnored {
        operation_id: ToolOperationId,
        reason: ObservationIgnoredReason,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolAuditEvent {
    pub sequence: u64,
    pub tick: u64,
    pub subject: Option<ToolAuditSubject>,
    pub event: ToolAuditEventKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolEngineSnapshot {
    pub protocol_version: u16,
    pub revision: u64,
    pub current_tick: u64,
    pub generations: Vec<(AgentInstanceId, SessionGeneration)>,
    pub instance_states: Vec<(AgentInstanceId, ToolInstanceState)>,
    pub providers: Vec<CapabilityProviderDescriptor>,
    pub grants: Vec<PolicyGrant>,
    pub requests: Vec<CapabilityRequestSnapshot>,
    pub audit_events: Vec<ToolAuditEvent>,
    pub dropped_audit_events: u64,
    pub revision_overflow_count: u64,
    pub next_completion_sequence: u64,
    pub dropped_completions: u64,
    pub effect_sequence_exhausted: bool,
    pub completion_sequence_exhausted: bool,
    pub audit_sequence_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionDropReason {
    QueueFull,
    SequenceExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolValidationError {
    UnsupportedProtocolVersion {
        expected: u16,
        actual: u16,
    },
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

fn has_forbidden_capability_namespace(value: &str) -> bool {
    const SEPARATORS: [char; 5] = ['.', ':', '/', '-', '_'];

    let compact = value
        .chars()
        .filter(|character| !SEPARATORS.contains(character))
        .collect::<String>();
    if ["shell", "filesystem", "mcp"]
        .iter()
        .any(|forbidden| compact.contains(forbidden))
    {
        return true;
    }

    let segments = value
        .split(SEPARATORS)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    segments.iter().enumerate().any(|(start, _)| {
        let mut candidate = String::new();
        segments.iter().skip(start).any(|segment| {
            if candidate.len() >= 2 {
                return false;
            }
            candidate.push_str(segment);
            candidate == "fs"
        })
    })
}

pub fn validate_protocol_version(actual: u16) -> Result<(), ToolValidationError> {
    if actual == CAPABILITY_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ToolValidationError::UnsupportedProtocolVersion {
            expected: CAPABILITY_PROTOCOL_VERSION,
            actual,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ConsumerBoundCapabilityRequest {
        ConsumerBoundCapabilityRequest::new(
            ConsumerId::new("station.test").unwrap(),
            ToolActorId::new("agent.primary").unwrap(),
            CapabilityRequestInput {
                local_id: CapabilityRequestId(7),
                instance_id: AgentInstanceId(11),
                generation: SessionGeneration(3),
                provider_id: ToolProviderId::new("gate.browser").unwrap(),
                capability_id: ToolCapabilityId::new("browser.page.snapshot").unwrap(),
                resource_scope_id: ResourceScopeId::new("workspace:test/page:active").unwrap(),
                approval_summary: "Read active page state".to_owned(),
                deadline_tick: 20,
                payload: b"secret payload".to_vec(),
            },
        )
    }

    #[test]
    fn request_wire_round_trip_preserves_scoped_key_and_validates() {
        let request = request();
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: ConsumerBoundCapabilityRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            decoded.key(),
            CapabilityRequestKey {
                consumer_id: ConsumerId::new("station.test").unwrap(),
                actor_id: ToolActorId::new("agent.primary").unwrap(),
                local_id: CapabilityRequestId(7),
            }
        );
        decoded.validate(1).unwrap();
    }

    #[test]
    fn unsupported_wire_version_and_unbounded_id_fail_validation() {
        let mut request = request();
        request.protocol_version = CAPABILITY_PROTOCOL_VERSION + 1;
        assert!(matches!(
            request.validate(1),
            Err(ToolValidationError::UnsupportedProtocolVersion { .. })
        ));
        let invalid = serde_json::to_string(
            &String::from_utf8(vec![b'x'; TOOL_ACTOR_ID_MAX_BYTES + 1]).unwrap(),
        )
        .unwrap();
        assert!(serde_json::from_str::<ToolActorId>(&invalid).is_err());
    }

    #[test]
    fn raw_request_debug_is_redacted() {
        let request = request();
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("secret payload"));
        assert!(rendered.contains("payload_bytes"));
    }

    #[test]
    fn capability_admission_rejects_separator_obfuscation_without_rejecting_browser_terms() {
        for id in [
            "browser.file-system.read",
            "browser.f-i-l-e-s-y-s-t-e-m.read",
            "browser.s-h-e-l-l.exec",
            "browser.m-c-p.call",
            "browser.f-s.read",
        ] {
            assert!(matches!(
                CapabilityDescriptor::new(
                    ToolCapabilityId::new(id).unwrap(),
                    CapabilityClass::Browser,
                    "unsafe capability",
                ),
                Err(ToolValidationError::CapabilityOutsideAdmission { .. })
            ));
        }

        for id in [
            "browser.page.snapshot",
            "browser.frame.offset.read",
            "browser.dom.forms.inspect",
        ] {
            assert!(CapabilityDescriptor::new(
                ToolCapabilityId::new(id).unwrap(),
                CapabilityClass::Browser,
                "bounded browser capability",
            )
            .is_ok());
        }
    }

    #[test]
    fn output_contracts_round_trip_for_service_and_wasm_consumers() {
        let outcome = ToolAuthorityOutcome::ClientClosed {
            purged_grant_count: 2,
            closed_request_count: 1,
        };
        let encoded = serde_json::to_string(&outcome).unwrap();
        assert_eq!(
            serde_json::from_str::<ToolAuthorityOutcome>(&encoded).unwrap(),
            outcome
        );

        let snapshot = ToolEngineSnapshot {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            revision: 4,
            current_tick: 9,
            generations: vec![(AgentInstanceId(11), SessionGeneration(3))],
            instance_states: vec![(AgentInstanceId(11), ToolInstanceState::Active)],
            providers: Vec::new(),
            grants: Vec::new(),
            requests: Vec::new(),
            audit_events: vec![ToolAuditEvent {
                sequence: 1,
                tick: 9,
                subject: None,
                event: ToolAuditEventKind::ClientClosed {
                    consumer_id: ConsumerId::new("station.test").unwrap(),
                    actor_id: ToolActorId::new("agent.primary").unwrap(),
                    purged_grant_count: 2,
                    closed_request_count: 1,
                },
            }],
            dropped_audit_events: 0,
            revision_overflow_count: 0,
            next_completion_sequence: 2,
            dropped_completions: 0,
            effect_sequence_exhausted: false,
            completion_sequence_exhausted: false,
            audit_sequence_exhausted: false,
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let decoded = serde_json::from_str::<ToolEngineSnapshot>(&encoded).unwrap();
        assert_eq!(decoded.protocol_version, CAPABILITY_PROTOCOL_VERSION);
        assert_eq!(decoded, snapshot);
    }
}
