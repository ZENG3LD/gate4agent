//! Deterministic policy and lifecycle engine for Gate-owned tool capabilities.
//!
//! This crate owns no executor. It releases an invocation only as a typed
//! effect after an exact grant (and, when configured, an approval). Provider
//! shells return correlated observations through the same generation fence.

mod engine;
mod model;

pub use engine::ToolEngine;
pub use model::{
    ApprovalDecision, ApprovalResolution, CancellationDisposition, CapabilityClass,
    CapabilityCompletionEnvelope, CapabilityDescriptor, CapabilityEffect, CapabilityEffectEnvelope,
    CapabilityObservation, CapabilityObservationEnvelope, CapabilityOwner,
    CapabilityProviderDescriptor, CapabilityRequest, CapabilityRequestId,
    CapabilityRequestSnapshot, CapabilityRequestStatus, CapabilityResult, CapabilityResultDelivery,
    CapabilityResultMetadata, ConsumerId, GrantMode, InvocationCancelReason,
    ObservationIgnoredReason, PolicyDecision, PolicyDenial, PolicyGrant, PolicyKey,
    ResourceScopeId, ToolActorId, ToolAuditEvent, ToolAuditEventKind, ToolAuditSubject,
    ToolCapabilityId, ToolEngineError, ToolEngineSnapshot, ToolFailure, ToolFailureKind,
    ToolOperationId, ToolProviderId, ToolValidationError, TOOL_ACTOR_ID_MAX_BYTES,
    TOOL_APPROVAL_SUMMARY_MAX_BYTES, TOOL_AUDIT_EVENTS_MAX, TOOL_CAPABILITIES_PER_PROVIDER_MAX,
    TOOL_CAPABILITY_DESCRIPTION_MAX_BYTES, TOOL_CAPABILITY_ID_MAX_BYTES, TOOL_COMPLETIONS_MAX,
    TOOL_CONSUMER_ID_MAX_BYTES, TOOL_EFFECTS_MAX, TOOL_FAILURE_MESSAGE_MAX_BYTES,
    TOOL_INLINE_RESULT_MAX_BYTES, TOOL_MEDIA_TYPE_MAX_BYTES, TOOL_PAYLOAD_MAX_BYTES,
    TOOL_POLICIES_MAX, TOOL_PROVIDERS_MAX, TOOL_PROVIDER_ID_MAX_BYTES, TOOL_REQUESTS_MAX,
    TOOL_RESOURCE_SCOPE_ID_MAX_BYTES, TOOL_RESULT_MAX_BYTES, TOOL_RESULT_REFERENCE_MAX_BYTES,
    TOOL_RESULT_SUMMARY_MAX_BYTES,
};
