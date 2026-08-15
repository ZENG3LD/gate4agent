//! Read-only monitoring contracts built from bounded observation facts.
//!
//! This crate intentionally contains no provider transport, terminal data,
//! persistence, or provider-specific identifiers.

use gate4agent_observation_protocol::{ObservationKindV1, ObservationV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use gate4agent_node_protocol::{
    NodeCursor, NodeId, NodeIncarnationId, SessionRecordId, WorkspaceId,
};
pub use gate4agent_types::{AgentInstanceId, SessionGeneration};

pub const PROJECTIONS_MAX: usize = 8_192;
pub const TIMELINE_PER_SESSION_MAX: usize = 512;
pub const TOOLS_PER_SESSION_MAX: usize = 128;
pub const SUBAGENTS_PER_SESSION_MAX: usize = 128;
pub const FILES_PER_SESSION_MAX: usize = 128;
pub const OWNED_PROCESSES_PER_SESSION_MAX: usize = 128;
pub const INTERACTIONS_PER_SESSION_MAX: usize = 64;
pub const INGRESS_BATCH_MAX: usize = 8_192;
pub const CURSOR_JOURNAL_MAX: usize = 65_536;
pub const NODE_ROUTES_MAX: usize = PROJECTIONS_MAX;
pub const RETIRED_INCARNATIONS_MAX: usize = CURSOR_JOURNAL_MAX;
pub const RESYNC_RECORDS_MAX: usize = PROJECTIONS_MAX;
pub const RESYNC_GAPS_MAX: usize = 512;
pub const RESYNC_EVENTS_MAX: usize = PROJECTIONS_MAX;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionKey {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
    pub workspace_id: WorkspaceId,
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

impl RuntimeSessionKey {
    pub fn validate(&self) -> Result<(), ObservationApiError> {
        if self.instance_id.0 == 0 {
            return Err(ObservationApiError::ZeroAgentInstanceId);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionKey {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
    pub record_id: SessionRecordId,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "target", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ObservationTarget {
    Runtime { key: RuntimeSessionKey },
    Managed { key: ManagedSessionKey },
}

impl ObservationTarget {
    pub fn node_id(&self) -> &NodeId {
        match self {
            Self::Runtime { key } => &key.node_id,
            Self::Managed { key } => &key.node_id,
        }
    }

    pub fn runtime_key(&self) -> Option<&RuntimeSessionKey> {
        match self {
            Self::Runtime { key } => Some(key),
            Self::Managed { .. } => None,
        }
    }

    pub fn incarnation_id(&self) -> NodeIncarnationId {
        match self {
            Self::Runtime { key } => key.incarnation_id,
            Self::Managed { key } => key.incarnation_id,
        }
    }

    pub fn validate_route(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Result<(), ObservationApiError> {
        if self.node_id() != node_id {
            return Err(ObservationApiError::RouteNodeMismatch);
        }
        if self.incarnation_id() != incarnation_id {
            return Err(ObservationApiError::RouteIncarnationMismatch);
        }
        if let Some(runtime) = self.runtime_key() {
            runtime.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedRecordLink {
    pub managed: ManagedSessionKey,
    pub runtime: Option<RuntimeSessionKey>,
}

impl ManagedRecordLink {
    pub fn validate_route(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Result<(), ObservationApiError> {
        if &self.managed.node_id != node_id {
            return Err(ObservationApiError::RouteNodeMismatch);
        }
        if self.managed.incarnation_id != incarnation_id {
            return Err(ObservationApiError::RouteIncarnationMismatch);
        }
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.validate()?;
            if &runtime.node_id != node_id {
                return Err(ObservationApiError::RouteNodeMismatch);
            }
            if runtime.incarnation_id != incarnation_id {
                return Err(ObservationApiError::RouteIncarnationMismatch);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationTransport {
    DirectNode,
    C2,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ObservationIngressPayload {
    Observation {
        address: ObservationTarget,
        observation: ObservationV1,
    },
    ManagedRecordUpserted {
        link: ManagedRecordLink,
    },
    ManagedRecordRemoved {
        key: ManagedSessionKey,
    },
    ResyncRequired {
        oldest: NodeCursor,
    },
    CursorOnly,
}

impl ObservationIngressPayload {
    pub fn validate_route(
        &self,
        node_id: &NodeId,
        cursor: NodeCursor,
    ) -> Result<(), ObservationApiError> {
        match self {
            Self::Observation {
                address,
                observation,
            } => {
                address.validate_route(node_id, cursor.incarnation_id)?;
                observation.validate().map_err(ObservationApiError::Observation)?;
                if let ObservationKindV1::Error { detail } = &observation.kind {
                    validate_error_category(detail)?;
                }
                Ok(())
            }
            Self::ManagedRecordUpserted { link } => {
                link.validate_route(node_id, cursor.incarnation_id)
            }
            Self::ManagedRecordRemoved { key } => {
                if &key.node_id != node_id {
                    return Err(ObservationApiError::RouteNodeMismatch);
                }
                if key.incarnation_id != cursor.incarnation_id {
                    return Err(ObservationApiError::RouteIncarnationMismatch);
                }
                Ok(())
            }
            Self::ResyncRequired { oldest } => {
                if oldest.incarnation_id != cursor.incarnation_id {
                    return Err(ObservationApiError::RouteIncarnationMismatch);
                }
                if oldest.sequence == 0 || oldest.sequence > cursor.sequence {
                    return Err(ObservationApiError::InvalidOldestCursor);
                }
                Ok(())
            }
            Self::CursorOnly => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationIngressEnvelope {
    pub node_id: NodeId,
    pub cursor: NodeCursor,
    pub received_at_ms: u64,
    pub transport: ObservationTransport,
    pub payload: ObservationIngressPayload,
}

impl ObservationIngressEnvelope {
    pub fn validate(&self) -> Result<(), ObservationApiError> {
        if self.cursor.sequence == 0 {
            return Err(ObservationApiError::ZeroEventSequence);
        }
        if self.received_at_ms == 0 {
            return Err(ObservationApiError::ZeroReceivedAt);
        }
        self.payload.validate_route(&self.node_id, self.cursor)
    }

    /// Equality of the Node-authoritative ingress fact.
    ///
    /// Transport and local receive time are delivery metadata. They may differ
    /// when the same exact Node cursor is observed directly and through C2.
    pub fn canonical_eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.cursor == other.cursor
            && self.payload == other.payload
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationGap {
    pub first_sequence: u64,
    pub last_sequence: u64,
}

impl ObservationGap {
    pub fn validate(&self, requested_after: u64, high_watermark: u64) -> Result<(), ObservationApiError> {
        if self.first_sequence == 0
            || self.first_sequence > self.last_sequence
            || self.first_sequence <= requested_after
            || self.last_sequence > high_watermark
        {
            return Err(ObservationApiError::InvalidGap);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationResyncBatch {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
    pub requested_after: u64,
    pub high_watermark: NodeCursor,
    /// Node-authoritative replay floor. `high_watermark.sequence + 1` means
    /// that no replay event is available through the high watermark.
    pub oldest_available_sequence: u64,
    pub records: Vec<ManagedRecordLink>,
    pub records_complete: bool,
    /// The exact retention-evicted prefix between `requested_after` and
    /// `oldest_available_sequence`; sparse non-observation sequences are not gaps.
    pub gaps: Vec<ObservationGap>,
    pub events: Vec<ObservationIngressEnvelope>,
}

/// One exact managed-record inventory operation for a Node incarnation.
///
/// This is inventory state only. Applying it must not imply that an
/// observation transport gap has been repaired.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecordInventory {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
    pub records: Vec<ManagedRecordLink>,
    pub complete: bool,
}

impl ObservationRecordInventory {
    pub fn validate(&self) -> Result<(), ObservationApiError> {
        validate_count("managed record inventory", self.records.len(), RESYNC_RECORDS_MAX)?;
        for record in &self.records {
            record.validate_route(&self.node_id, self.incarnation_id)?;
        }
        Ok(())
    }
}

impl ObservationResyncBatch {
    pub fn validate(&self) -> Result<(), ObservationApiError> {
        if self.high_watermark.incarnation_id != self.incarnation_id
            || self.high_watermark.sequence < self.requested_after
        {
            return Err(ObservationApiError::InvalidHighWatermark);
        }
        validate_count("resync records", self.records.len(), RESYNC_RECORDS_MAX)?;
        validate_count("resync gaps", self.gaps.len(), RESYNC_GAPS_MAX)?;
        validate_count("resync events", self.events.len(), RESYNC_EVENTS_MAX)?;

        let maximum_floor = self
            .high_watermark
            .sequence
            .checked_add(1)
            .unwrap_or(u64::MAX);
        let requested_next = self.requested_after.checked_add(1).unwrap_or(u64::MAX);
        if self.oldest_available_sequence == 0
            || self.oldest_available_sequence > maximum_floor
        {
            return Err(ObservationApiError::InvalidReplayFloor);
        }
        let expected_gap = (requested_next < self.oldest_available_sequence).then(|| {
            ObservationGap {
                first_sequence: requested_next,
                last_sequence: self.oldest_available_sequence - 1,
            }
        });
        if self.gaps.as_slice() != expected_gap.as_slice() {
            return Err(ObservationApiError::InvalidReplayGapProof);
        }

        for record in &self.records {
            record.validate_route(&self.node_id, self.incarnation_id)?;
        }
        for gap in &self.gaps {
            gap.validate(self.requested_after, self.high_watermark.sequence)?;
        }
        for event in &self.events {
            event.validate()?;
            if event.node_id != self.node_id
                || event.cursor.incarnation_id != self.incarnation_id
                || event.cursor.sequence <= self.requested_after
                || event.cursor.sequence < self.oldest_available_sequence
                || event.cursor.sequence > self.high_watermark.sequence
            {
                return Err(ObservationApiError::EventOutsideResyncRoute);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionAvailability {
    Unknown,
    NotObserved,
    Current,
    Partial,
    Frozen,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionFreshness {
    Live,
    LastKnown,
    Stale,
    IncompleteAfterGap,
    ReplacedIncarnation,
    Unavailable,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ObservationApiError {
    #[error("agent instance ID must be non-zero")]
    ZeroAgentInstanceId,
    #[error("node event sequence must be non-zero")]
    ZeroEventSequence,
    #[error("received_at_ms must be non-zero")]
    ZeroReceivedAt,
    #[error("observation route node does not match its envelope")]
    RouteNodeMismatch,
    #[error("observation route incarnation does not match its cursor")]
    RouteIncarnationMismatch,
    #[error("observation failed protocol validation: {0}")]
    Observation(#[source] gate4agent_observation_protocol::ObservationValidationError),
    #[error("resync-required oldest cursor is invalid")]
    InvalidOldestCursor,
    #[error("resync high watermark is invalid")]
    InvalidHighWatermark,
    #[error("resync oldest available sequence is invalid")]
    InvalidReplayFloor,
    #[error("resync gaps do not exactly prove the authoritative replay floor")]
    InvalidReplayGapProof,
    #[error("resync gap is outside the requested cursor range")]
    InvalidGap,
    #[error("resync event is outside the batch route or cursor range")]
    EventOutsideResyncRoute,
    #[error("error detail must be a lowercase categorical slug")]
    InvalidErrorDetail,
    #[error("{field} count {actual} exceeds maximum {max}")]
    TooMany {
        field: &'static str,
        max: usize,
        actual: usize,
    },
}

fn validate_error_category(detail: &str) -> Result<(), ObservationApiError> {
    let mut segments = detail.split('-');
    if segments.any(|segment| {
        segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
    {
        return Err(ObservationApiError::InvalidErrorDetail);
    }
    Ok(())
}

fn validate_count(
    field: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), ObservationApiError> {
    if actual > max {
        return Err(ObservationApiError::TooMany { field, max, actual });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_observation_protocol::{
        ObservationEvidenceV1, ObservationKindV1,
    };

    fn node(value: &str) -> NodeId {
        NodeId::new(value).expect("node ID")
    }

    fn incarnation(byte: u8) -> NodeIncarnationId {
        NodeIncarnationId::from_bytes([byte; 16])
    }

    fn runtime(node_id: NodeId, incarnation_id: NodeIncarnationId) -> RuntimeSessionKey {
        RuntimeSessionKey {
            node_id,
            incarnation_id,
            workspace_id: WorkspaceId::new("workspace-a").expect("workspace ID"),
            instance_id: AgentInstanceId(1),
            generation: SessionGeneration(0),
        }
    }

    fn event(node_id: NodeId, incarnation_id: NodeIncarnationId) -> ObservationIngressEnvelope {
        ObservationIngressEnvelope {
            node_id: node_id.clone(),
            cursor: NodeCursor {
                incarnation_id,
                sequence: 1,
            },
            received_at_ms: 10,
            transport: ObservationTransport::DirectNode,
            payload: ObservationIngressPayload::Observation {
                address: ObservationTarget::Runtime {
                    key: runtime(node_id, incarnation_id),
                },
                observation: ObservationV1 {
                    source_sequence: 1,
                    observed_at_unix_ms: Some(9),
                    evidence: ObservationEvidenceV1::NodeLifecycle,
                    kind: ObservationKindV1::Ready,
                    truncated: false,
                },
            },
        }
    }

    #[test]
    fn ingress_validation_and_resync_bounds_are_enforced() {
        let node_a = node("node-a");
        let node_b = node("node-b");
        let incarnation_id = incarnation(1);
        let valid = event(node_a.clone(), incarnation_id);
        valid.validate().expect("valid ingress");

        let mut wrong_route = valid.clone();
        wrong_route.node_id = node_b;
        assert_eq!(
            wrong_route.validate(),
            Err(ObservationApiError::RouteNodeMismatch)
        );

        let mut zero_received = valid.clone();
        zero_received.received_at_ms = 0;
        assert_eq!(
            zero_received.validate(),
            Err(ObservationApiError::ZeroReceivedAt)
        );

        let batch = ObservationResyncBatch {
            node_id: node_a,
            incarnation_id,
            requested_after: 0,
            high_watermark: NodeCursor {
                incarnation_id,
                sequence: 1,
            },
            oldest_available_sequence: 1,
            records: Vec::new(),
            records_complete: true,
            gaps: vec![ObservationGap {
                first_sequence: 1,
                last_sequence: 1,
            }; RESYNC_GAPS_MAX + 1],
            events: vec![valid],
        };
        assert!(matches!(
            batch.validate(),
            Err(ObservationApiError::TooMany {
                field: "resync gaps",
                ..
            })
        ));
    }

    #[test]
    fn managed_route_is_exactly_incarnation_scoped() {
        let node_id = node("node-a");
        let first_incarnation = incarnation(1);
        let second_incarnation = incarnation(2);
        let key = ManagedSessionKey {
            node_id: node_id.clone(),
            incarnation_id: first_incarnation,
            record_id: SessionRecordId::new("record-inline-a").expect("record ID"),
        };
        let target = ObservationTarget::Managed { key: key.clone() };
        target
            .validate_route(&node_id, first_incarnation)
            .expect("exact managed route");
        assert_eq!(
            target.validate_route(&node_id, second_incarnation),
            Err(ObservationApiError::RouteIncarnationMismatch)
        );

        let link = ManagedRecordLink {
            managed: key,
            runtime: None,
        };
        link.validate_route(&node_id, first_incarnation)
            .expect("exact managed link route");
        assert_eq!(
            link.validate_route(&node_id, second_incarnation),
            Err(ObservationApiError::RouteIncarnationMismatch)
        );
    }
}
