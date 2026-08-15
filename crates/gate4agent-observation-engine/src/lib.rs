//! Bounded in-memory reducer for read-only session observations.

use gate4agent_observation_api::{
    ManagedRecordLink, ManagedSessionKey, NodeCursor, NodeId, NodeIncarnationId,
    ObservationApiError, ObservationGap, ObservationIngressEnvelope, ObservationIngressPayload,
    ObservationRecordInventory, ObservationResyncBatch, ObservationTarget,
    ProjectionAvailability, ProjectionFreshness,
    RuntimeSessionKey, CURSOR_JOURNAL_MAX, FILES_PER_SESSION_MAX, INGRESS_BATCH_MAX,
    INTERACTIONS_PER_SESSION_MAX, NODE_ROUTES_MAX, OWNED_PROCESSES_PER_SESSION_MAX,
    PROJECTIONS_MAX, RETIRED_INCARNATIONS_MAX, SUBAGENTS_PER_SESSION_MAX,
    TIMELINE_PER_SESSION_MAX, TOOLS_PER_SESSION_MAX,
};
use gate4agent_observation_protocol::{
    ObservationCapabilitiesV1, ObservationEvidenceV1, ObservationInteractionOutcomeV1,
    ObservationKindV1, ObservationSourceFamilyV1, ObservationTodoItemV1,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CorrelationState {
    Pending,
    Completed { success: Option<bool> },
    Resolved { outcome: ObservationInteractionOutcomeV1 },
    UnknownAfterGap,
    OrphanCompletion { success: Option<bool> },
    OrphanResolution { outcome: ObservationInteractionOutcomeV1 },
}

impl CorrelationState {
    fn is_active(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CorrelationProjection {
    pub correlation_id: String,
    pub class: Option<String>,
    pub evidence: ObservationEvidenceV1,
    pub state: CorrelationState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileProjection {
    pub path: Option<String>,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageTotals {
    fn checked_delta(self, previous: Self) -> Option<Self> {
        Some(Self {
            input_tokens: self.input_tokens.checked_sub(previous.input_tokens)?,
            output_tokens: self.output_tokens.checked_sub(previous.output_tokens)?,
            cache_read_tokens: self.cache_read_tokens.checked_sub(previous.cache_read_tokens)?,
            cache_write_tokens: self.cache_write_tokens.checked_sub(previous.cache_write_tokens)?,
            reasoning_tokens: self.reasoning_tokens.checked_sub(previous.reasoning_tokens)?,
        })
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_tokens: self.cache_read_tokens.saturating_add(other.cache_read_tokens),
            cache_write_tokens: self.cache_write_tokens.saturating_add(other.cache_write_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextOccupancyProvenance {
    CumulativeUsage,
    PerTurnUsageSynthesis,
    ExactCurrentWindow,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextOccupancySnapshot {
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub unattributed_tokens: u64,
    pub used_tokens: u64,
    pub context_window: Option<u64>,
    pub evidence: ObservationEvidenceV1,
    pub provenance: ContextOccupancyProvenance,
}

impl ContextOccupancySnapshot {
    pub fn occupied_tokens(self) -> u64 {
        self.used_tokens
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageProjection {
    pub observed_delta: UsageTotals,
    pub last_cumulative: Option<UsageTotals>,
    pub cumulative_evidence: Option<ObservationEvidenceV1>,
    pub context_window: Option<u64>,
    #[serde(default)]
    pub context_occupancy: Option<ContextOccupancySnapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TodoRevisionProjection {
    pub revision: u64,
    pub items: Vec<ObservationTodoItemV1>,
    pub complete: bool,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TodoProjection {
    pub current: Option<TodoRevisionProjection>,
    pub previous: Option<TodoRevisionProjection>,
    pub conflict: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimelineEntry {
    pub cursor: NodeCursor,
    pub received_at_ms: u64,
    pub evidence: ObservationEvidenceV1,
    pub kind: ObservationKindV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceCapabilitiesProjection {
    pub evidence: ObservationEvidenceV1,
    pub source_family: ObservationSourceFamilyV1,
    pub source_adapter: String,
    pub capabilities: ObservationCapabilitiesV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryProjection {
    pub evidence: ObservationEvidenceV1,
    pub message_count: u64,
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionProjection {
    pub target: ObservationTarget,
    pub availability: ProjectionAvailability,
    pub freshness: ProjectionFreshness,
    pub transport_incomplete: bool,
    pub incomplete_evidence: Vec<ObservationEvidenceV1>,
    pub stale_evidence: Vec<ObservationEvidenceV1>,
    pub source_capabilities: Vec<SourceCapabilitiesProjection>,
    pub history: Option<HistoryProjection>,
    pub todos: TodoProjection,
    pub tools: Vec<CorrelationProjection>,
    pub subagents: Vec<CorrelationProjection>,
    pub interactions: Vec<CorrelationProjection>,
    pub owned_processes: Vec<CorrelationProjection>,
    pub files: VecDeque<FileProjection>,
    pub usage: UsageProjection,
    pub timeline: VecDeque<TimelineEntry>,
}

impl SessionProjection {
    fn new(target: ObservationTarget) -> Self {
        Self {
            target,
            availability: ProjectionAvailability::Unknown,
            freshness: ProjectionFreshness::Unavailable,
            transport_incomplete: false,
            incomplete_evidence: Vec::new(),
            stale_evidence: Vec::new(),
            source_capabilities: Vec::new(),
            history: None,
            todos: TodoProjection::default(),
            tools: Vec::new(),
            subagents: Vec::new(),
            interactions: Vec::new(),
            owned_processes: Vec::new(),
            files: VecDeque::new(),
            usage: UsageProjection::default(),
            timeline: VecDeque::new(),
        }
    }

    fn mark_observed(&mut self) {
        if matches!(
            self.availability,
            ProjectionAvailability::Unknown | ProjectionAvailability::NotObserved
        ) || (self.availability == ProjectionAvailability::Frozen
            && self.freshness == ProjectionFreshness::Unavailable)
        {
            self.availability = ProjectionAvailability::Current;
        }
        if self.availability != ProjectionAvailability::Frozen
            && !self.transport_incomplete
            && self.incomplete_evidence.is_empty()
            && self.stale_evidence.is_empty()
        {
            self.freshness = ProjectionFreshness::Live;
        }
    }

    fn mark_inventory_present(&mut self, linked: bool) {
        if self.availability == ProjectionAvailability::Frozen
            && self.freshness == ProjectionFreshness::ReplacedIncarnation
        {
            return;
        }
        if self.transport_incomplete || !self.incomplete_evidence.is_empty() {
            self.availability = ProjectionAvailability::Partial;
            self.freshness = ProjectionFreshness::IncompleteAfterGap;
            return;
        }

        let observed = !self.timeline.is_empty() || !self.source_capabilities.is_empty();
        if !observed {
            self.availability = ProjectionAvailability::NotObserved;
            self.freshness = ProjectionFreshness::LastKnown;
        } else if !self.stale_evidence.is_empty() {
            self.availability = ProjectionAvailability::Current;
            self.freshness = ProjectionFreshness::Stale;
        } else {
            self.availability = ProjectionAvailability::Current;
            if !linked
                || matches!(
                    self.freshness,
                    ProjectionFreshness::LastKnown
                        | ProjectionFreshness::ReplacedIncarnation
                        | ProjectionFreshness::Unavailable
                )
            {
                self.freshness = ProjectionFreshness::LastKnown;
            }
        }
    }

    fn mark_inventory_absent(&mut self) {
        self.availability = ProjectionAvailability::Frozen;
        self.freshness = ProjectionFreshness::Unavailable;
    }

    fn refresh_integrity(&mut self) {
        if self.availability == ProjectionAvailability::Frozen {
            return;
        }
        if self.transport_incomplete || !self.incomplete_evidence.is_empty() {
            self.availability = ProjectionAvailability::Partial;
            self.freshness = ProjectionFreshness::IncompleteAfterGap;
        } else if !self.stale_evidence.is_empty() {
            if self.availability == ProjectionAvailability::Partial {
                self.availability = ProjectionAvailability::Current;
            }
            self.freshness = ProjectionFreshness::Stale;
        } else {
            if self.availability == ProjectionAvailability::Partial {
                self.availability = ProjectionAvailability::Current;
            }
            if !matches!(
                self.availability,
                ProjectionAvailability::Unknown | ProjectionAvailability::NotObserved
            ) {
                self.freshness = ProjectionFreshness::Live;
            }
        }
    }

    fn mark_transport_gap(&mut self) {
        self.transport_incomplete = true;
        self.availability = ProjectionAvailability::Partial;
        self.freshness = ProjectionFreshness::IncompleteAfterGap;
        self.mark_pending_unknown();
    }

    fn mark_source_incomplete(&mut self, evidence: ObservationEvidenceV1, unknown_pending: bool) {
        push_unique_evidence(&mut self.incomplete_evidence, evidence);
        self.availability = ProjectionAvailability::Partial;
        self.freshness = ProjectionFreshness::IncompleteAfterGap;
        if unknown_pending {
            self.mark_pending_unknown();
        }
    }

    fn mark_source_stale(&mut self, evidence: ObservationEvidenceV1) {
        push_unique_evidence(&mut self.stale_evidence, evidence);
        self.refresh_integrity();
    }

    fn mark_pending_unknown(&mut self) {
        mark_pending_unknown(&mut self.tools);
        mark_pending_unknown(&mut self.subagents);
        mark_pending_unknown(&mut self.interactions);
        mark_pending_unknown(&mut self.owned_processes);
    }

    fn source_reset(&mut self, evidence: ObservationEvidenceV1) {
        self.source_capabilities.retain(|entry| entry.evidence != evidence);
        if self.history.as_ref().map(|history| history.evidence) == Some(evidence) {
            self.history = None;
        }
        self.tools.retain(|entry| entry.evidence != evidence);
        self.subagents.retain(|entry| entry.evidence != evidence);
        self.interactions.retain(|entry| entry.evidence != evidence);
        self.owned_processes.retain(|entry| entry.evidence != evidence);
        self.files.retain(|entry| entry.evidence != evidence);
        if self.todos.current.as_ref().map(|todo| todo.evidence) == Some(evidence) {
            self.todos.current = None;
            self.todos.previous = None;
            self.todos.conflict = false;
        }
        if self.usage.cumulative_evidence == Some(evidence) {
            self.usage.last_cumulative = None;
            self.usage.cumulative_evidence = None;
        }
        if self.usage.context_occupancy.map(|snapshot| snapshot.evidence) == Some(evidence) {
            self.usage.context_occupancy = None;
        }
        self.incomplete_evidence.retain(|current| *current != evidence);
        self.stale_evidence.retain(|current| *current != evidence);
        self.refresh_integrity();
    }

    fn push_timeline(&mut self, entry: TimelineEntry) {
        if self.timeline.len() == TIMELINE_PER_SESSION_MAX {
            self.timeline.pop_front();
        }
        self.timeline.push_back(entry);
    }
}

fn push_unique_evidence(
    entries: &mut Vec<ObservationEvidenceV1>,
    evidence: ObservationEvidenceV1,
) {
    if !entries.contains(&evidence) {
        entries.push(evidence);
    }
}

fn mark_pending_unknown(entries: &mut [CorrelationProjection]) {
    for entry in entries {
        if entry.state == CorrelationState::Pending {
            entry.state = CorrelationState::UnknownAfterGap;
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CursorKey {
    node_id: NodeId,
    incarnation_id: NodeIncarnationId,
    sequence: u64,
}

pub const OBSERVATION_ENGINE_CHECKPOINT_VERSION_V1: u16 = 1;
pub const OBSERVATION_ROUTE_HISTORY_MAX: usize =
    RETIRED_INCARNATIONS_MAX + NODE_ROUTES_MAX;
// Keep enough exact canonical ingress to deduplicate the immediate alternate
// transport replay while leaving ample headroom before the hard journal bound.
const CURSOR_JOURNAL_CHECKPOINT_RETAINED_MAX: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCheckpointCursorV1 {
    pub node_id: NodeId,
    pub cursor: NodeCursor,
    pub payload: ObservationIngressPayload,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCheckpointRouteV1 {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationCheckpointRouteCursorV1 {
    pub node_id: NodeId,
    pub cursor: NodeCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationEngineCheckpointV1 {
    pub version: u16,
    pub projections: Vec<SessionProjection>,
    pub cursor_journal: Vec<ObservationCheckpointCursorV1>,
    pub current_incarnations: Vec<ObservationCheckpointRouteV1>,
    pub retired_incarnations: Vec<ObservationCheckpointRouteV1>,
    pub managed_links: Vec<ManagedRecordLink>,
    pub transport_gaps: Vec<ObservationCheckpointRouteV1>,
    pub retention_floors: Vec<ObservationCheckpointRouteCursorV1>,
    pub high_watermarks: Vec<ObservationCheckpointRouteCursorV1>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObservationEngine {
    projections: HashMap<ObservationTarget, SessionProjection>,
    journal: HashMap<CursorKey, ObservationIngressEnvelope>,
    current_incarnations: HashMap<NodeId, NodeIncarnationId>,
    retired_incarnations: HashSet<(NodeId, NodeIncarnationId)>,
    managed_links: HashMap<ManagedSessionKey, Option<RuntimeSessionKey>>,
    transport_gaps: HashSet<(NodeId, NodeIncarnationId)>,
    retention_floors: HashMap<(NodeId, NodeIncarnationId), u64>,
    high_watermarks: HashMap<(NodeId, NodeIncarnationId), u64>,
}

impl ObservationEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn projection(&self, target: &ObservationTarget) -> Option<&SessionProjection> {
        self.projections.get(target)
    }

    pub fn projection_count(&self) -> usize {
        self.projections.len()
    }

    pub fn projections(&self) -> impl Iterator<Item = (&ObservationTarget, &SessionProjection)> {
        self.projections.iter()
    }

    pub fn managed_runtime(&self, key: &ManagedSessionKey) -> Option<&RuntimeSessionKey> {
        self.managed_links.get(key).and_then(Option::as_ref)
    }

    pub fn durable_resume_cursors(&self) -> Vec<(NodeId, NodeCursor)> {
        let mut cursors = self
            .high_watermarks
            .iter()
            .map(|((node_id, incarnation_id), sequence)| {
                (node_id.clone(), NodeCursor {
                    incarnation_id: *incarnation_id,
                    sequence: *sequence,
                })
            })
            .collect::<Vec<_>>();
        cursors.sort_by(|left, right| {
            left.0.cmp(&right.0)
                .then_with(|| left.1.incarnation_id.cmp(&right.1.incarnation_id))
        });
        cursors
    }

    pub fn checkpoint(&self) -> ObservationEngineCheckpointV1 {
        self.to_checkpoint()
    }

    pub fn restore(
        checkpoint: ObservationEngineCheckpointV1,
    ) -> Result<Self, ObservationEngineError> {
        checkpoint.validate()?;
        let mut engine = Self::new();
        for projection in checkpoint.projections {
            engine.projections.insert(projection.target.clone(), projection);
        }
        for entry in checkpoint.cursor_journal {
            let key = CursorKey {
                node_id: entry.node_id.clone(),
                incarnation_id: entry.cursor.incarnation_id,
                sequence: entry.cursor.sequence,
            };
            engine.journal.insert(key, ObservationIngressEnvelope {
                node_id: entry.node_id,
                cursor: entry.cursor,
                received_at_ms: 1,
                transport: gate4agent_observation_api::ObservationTransport::DirectNode,
                payload: entry.payload,
            });
        }
        for route in checkpoint.current_incarnations {
            engine.current_incarnations.insert(route.node_id, route.incarnation_id);
        }
        for route in checkpoint.retired_incarnations {
            engine.retired_incarnations.insert((route.node_id, route.incarnation_id));
        }
        for link in checkpoint.managed_links {
            engine.managed_links.insert(link.managed, link.runtime);
        }
        for route in checkpoint.transport_gaps {
            engine.transport_gaps.insert((route.node_id, route.incarnation_id));
        }
        for route in checkpoint.retention_floors {
            engine.retention_floors.insert(
                (route.node_id, route.cursor.incarnation_id),
                route.cursor.sequence,
            );
        }
        for route in checkpoint.high_watermarks {
            engine.high_watermarks.insert(
                (route.node_id, route.cursor.incarnation_id),
                route.cursor.sequence,
            );
        }
        Ok(engine)
    }

    pub fn prepare(
        &self,
        envelope: ObservationIngressEnvelope,
    ) -> Result<PreparedBatch, ObservationEngineError> {
        self.prepare_batch(&[envelope])
    }

    pub fn prepare_batch(
        &self,
        envelopes: &[ObservationIngressEnvelope],
    ) -> Result<PreparedBatch, ObservationEngineError> {
        if envelopes.len() > INGRESS_BATCH_MAX {
            return Err(ObservationEngineError::CapacityExhausted {
                collection: "ingress batch",
                max: INGRESS_BATCH_MAX,
            });
        }
        let mut next = self.clone();
        let mut outcomes = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            outcomes.push(next.apply_envelope(envelope.clone())?);
        }
        Ok(PreparedBatch { next, outcomes })
    }

    pub fn prepare_resync(
        &self,
        batch: &ObservationResyncBatch,
    ) -> Result<PreparedBatch, ObservationEngineError> {
        batch.validate()?;
        let mut next = self.clone();
        next.observe_incarnation(&batch.node_id, batch.incarnation_id)?;
        next.record_high_watermark(
            &batch.node_id,
            batch.incarnation_id,
            batch.high_watermark.sequence,
        );
        next.apply_resync_records(batch)?;
        let proves_no_eviction = batch.requested_after.saturating_add(1)
            >= batch.oldest_available_sequence;
        if proves_no_eviction {
            next.clear_node_transport_gap(&batch.node_id, batch.incarnation_id);
        } else {
            next.mark_node_gap(&batch.node_id, batch.incarnation_id);
        }
        let mut outcomes = Vec::with_capacity(batch.events.len());
        for envelope in &batch.events {
            outcomes.push(next.apply_envelope(envelope.clone())?);
        }
        Ok(PreparedBatch { next, outcomes })
    }

    /// Atomically applies a bounded inventory of managed records for one exact
    /// Node incarnation.
    ///
    /// Inventory is record/link state, not transport recovery evidence. This
    /// operation deliberately never clears or creates an observation transport
    /// gap.
    pub fn prepare_record_inventory(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
        records: &[ManagedRecordLink],
        complete: bool,
    ) -> Result<PreparedBatch, ObservationEngineError> {
        let inventory = ObservationRecordInventory {
            node_id: node_id.clone(),
            incarnation_id,
            records: records.to_vec(),
            complete,
        };
        inventory.validate()?;

        let mut next = self.clone();
        next.observe_incarnation(node_id, incarnation_id)?;
        next.apply_record_inventory(node_id, incarnation_id, records, complete)?;
        Ok(PreparedBatch {
            next,
            outcomes: Vec::new(),
        })
    }

    /// Commits an already validated and fully reduced state without failure.
    pub fn accept(&mut self, prepared: PreparedBatch) {
        *self = prepared.next;
    }

    fn apply_envelope(
        &mut self,
        envelope: ObservationIngressEnvelope,
    ) -> Result<ApplyOutcome, ObservationEngineError> {
        envelope.validate()?;
        let cursor_key = CursorKey {
            node_id: envelope.node_id.clone(),
            incarnation_id: envelope.cursor.incarnation_id,
            sequence: envelope.cursor.sequence,
        };
        if let Some(existing) = self.journal.get(&cursor_key) {
            if existing.canonical_eq(&envelope) {
                return Ok(ApplyOutcome::Duplicate);
            }
            return Err(ObservationEngineError::CursorCollision {
                node_id: envelope.node_id,
                cursor: envelope.cursor,
            });
        }
        if self
            .retention_floors
            .get(&(envelope.node_id.clone(), envelope.cursor.incarnation_id))
            .is_some_and(|floor| envelope.cursor.sequence <= *floor)
        {
            return Err(ObservationEngineError::BelowRetentionFloor {
                node_id: envelope.node_id,
                cursor: envelope.cursor,
            });
        }
        if self.journal.len() >= CURSOR_JOURNAL_MAX {
            self.compact_cursor_journal();
        }

        self.observe_incarnation(&envelope.node_id, envelope.cursor.incarnation_id)?;
        self.apply_payload(&envelope)?;
        self.record_high_watermark(
            &envelope.node_id,
            envelope.cursor.incarnation_id,
            envelope.cursor.sequence,
        );
        self.journal.insert(cursor_key, envelope);
        Ok(ApplyOutcome::Applied)
    }

    fn observe_incarnation(
        &mut self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Result<(), ObservationEngineError> {
        match self.current_incarnations.get(node_id).copied() {
            None => {
                if self.current_incarnations.len() == NODE_ROUTES_MAX {
                    return Err(ObservationEngineError::CapacityExhausted {
                        collection: "node routes",
                        max: NODE_ROUTES_MAX,
                    });
                }
                self.current_incarnations.insert(node_id.clone(), incarnation_id);
            }
            Some(current) if current == incarnation_id => {}
            Some(_) if self.retired_incarnations.contains(&(node_id.clone(), incarnation_id)) => {
                return Err(ObservationEngineError::ReplacedIncarnation {
                    node_id: node_id.clone(),
                    incarnation_id,
                });
            }
            Some(current) => {
                if self.retired_incarnations.len() == RETIRED_INCARNATIONS_MAX {
                    return Err(ObservationEngineError::CapacityExhausted {
                        collection: "retired incarnations",
                        max: RETIRED_INCARNATIONS_MAX,
                    });
                }
                self.retired_incarnations.insert((node_id.clone(), current));
                for projection in self.projections.values_mut() {
                    let replaced = match &projection.target {
                        ObservationTarget::Runtime { key } => {
                            &key.node_id == node_id && key.incarnation_id == current
                        }
                        ObservationTarget::Managed { key } => {
                            &key.node_id == node_id && key.incarnation_id == current
                        }
                    };
                    if replaced {
                        projection.availability = ProjectionAvailability::Frozen;
                        projection.freshness = ProjectionFreshness::ReplacedIncarnation;
                    }
                }
                for (key, runtime) in self.managed_links.iter_mut() {
                    if &key.node_id == node_id && key.incarnation_id == current {
                        *runtime = None;
                    }
                }
                self.current_incarnations.insert(node_id.clone(), incarnation_id);
            }
        }
        Ok(())
    }

    fn apply_payload(
        &mut self,
        envelope: &ObservationIngressEnvelope,
    ) -> Result<(), ObservationEngineError> {
        match &envelope.payload {
            ObservationIngressPayload::Observation {
                address,
                observation,
            } => {
                let projection = self.ensure_projection(address.clone())?;
                projection.mark_observed();
                apply_observation(projection, envelope.cursor, envelope.received_at_ms, observation)?;
            }
            ObservationIngressPayload::ManagedRecordUpserted { link } => {
                self.upsert_managed_link(link.clone())?;
            }
            ObservationIngressPayload::ManagedRecordRemoved { key } => {
                self.remove_managed_link(key);
            }
            ObservationIngressPayload::ResyncRequired { .. } => {
                self.mark_node_gap(&envelope.node_id, envelope.cursor.incarnation_id);
            }
            ObservationIngressPayload::CursorOnly => {}
        }
        Ok(())
    }

    fn ensure_projection(
        &mut self,
        target: ObservationTarget,
    ) -> Result<&mut SessionProjection, ObservationEngineError> {
        if !self.projections.contains_key(&target) {
            if self.projections.len() == PROJECTIONS_MAX {
                return Err(ObservationEngineError::CapacityExhausted {
                    collection: "session projections",
                    max: PROJECTIONS_MAX,
                });
            }
            let mut projection = SessionProjection::new(target.clone());
            if self.transport_gaps.contains(&(
                target.node_id().clone(),
                target.incarnation_id(),
            )) {
                projection.mark_transport_gap();
            }
            self.projections.insert(target.clone(), projection);
        }
        Ok(self.projections.get_mut(&target).expect("projection was inserted"))
    }

    fn upsert_managed_link(
        &mut self,
        link: ManagedRecordLink,
    ) -> Result<(), ObservationEngineError> {
        let target = ObservationTarget::Managed {
            key: link.managed.clone(),
        };
        let projection = self.ensure_projection(target)?;
        projection.mark_inventory_present(link.runtime.is_some());
        self.managed_links.insert(link.managed, link.runtime);
        Ok(())
    }

    fn remove_managed_link(&mut self, key: &ManagedSessionKey) {
        self.managed_links.remove(key);
        let target = ObservationTarget::Managed { key: key.clone() };
        if let Some(projection) = self.projections.get_mut(&target) {
            projection.mark_inventory_absent();
        }
    }

    fn apply_resync_records(
        &mut self,
        batch: &ObservationResyncBatch,
    ) -> Result<(), ObservationEngineError> {
        self.apply_record_inventory(
            &batch.node_id,
            batch.incarnation_id,
            &batch.records,
            batch.records_complete,
        )
    }

    fn apply_record_inventory(
        &mut self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
        records: &[ManagedRecordLink],
        complete: bool,
    ) -> Result<(), ObservationEngineError> {
        let present: HashSet<_> = records.iter().map(|link| link.managed.clone()).collect();
        if complete {
            let absent: Vec<_> = self
                .projections
                .keys()
                .filter_map(|target| match target {
                    ObservationTarget::Managed { key }
                        if &key.node_id == node_id
                            && key.incarnation_id == incarnation_id
                            && !present.contains(key) => Some(key.clone()),
                    _ => None,
                })
                .collect();
            for key in absent {
                self.managed_links.remove(&key);
                let target = ObservationTarget::Managed { key: key.clone() };
                if let Some(projection) = self.projections.get_mut(&target) {
                    projection.mark_inventory_absent();
                }
            }
        }
        for link in records {
            self.upsert_managed_link(link.clone())?;
        }
        Ok(())
    }

    fn mark_node_gap(&mut self, node_id: &NodeId, incarnation_id: NodeIncarnationId) {
        self.transport_gaps.insert((node_id.clone(), incarnation_id));
        for projection in self.projections.values_mut() {
            let belongs = match &projection.target {
                ObservationTarget::Runtime { key } => {
                    &key.node_id == node_id && key.incarnation_id == incarnation_id
                }
                ObservationTarget::Managed { key } => {
                    &key.node_id == node_id
                        && key.incarnation_id == incarnation_id
                }
            };
            if belongs && projection.availability != ProjectionAvailability::Frozen {
                projection.mark_transport_gap();
            }
        }
    }

    fn clear_node_transport_gap(&mut self, node_id: &NodeId, incarnation_id: NodeIncarnationId) {
        self.transport_gaps.remove(&(node_id.clone(), incarnation_id));
        for projection in self.projections.values_mut() {
            let belongs = match &projection.target {
                ObservationTarget::Runtime { key } => {
                    &key.node_id == node_id && key.incarnation_id == incarnation_id
                }
                ObservationTarget::Managed { key } => {
                    &key.node_id == node_id
                        && key.incarnation_id == incarnation_id
                }
            };
            if belongs {
                projection.transport_incomplete = false;
                projection.refresh_integrity();
            }
        }
    }

    fn record_high_watermark(
        &mut self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
        sequence: u64,
    ) {
        let high = self
            .high_watermarks
            .entry((node_id.clone(), incarnation_id))
            .or_insert(0);
        *high = (*high).max(sequence);
    }

    fn compact_cursor_journal(&mut self) {
        if self.journal.len() <= CURSOR_JOURNAL_CHECKPOINT_RETAINED_MAX {
            return;
        }
        let evicted_count = self.journal.len() - CURSOR_JOURNAL_CHECKPOINT_RETAINED_MAX;
        let mut keys = self.journal.keys().cloned().collect::<Vec<_>>();
        keys.sort_by(|left, right| {
            left.sequence.cmp(&right.sequence)
                .then_with(|| left.node_id.cmp(&right.node_id))
                .then_with(|| left.incarnation_id.cmp(&right.incarnation_id))
        });
        for key in keys.into_iter().take(evicted_count) {
            let floor = self
                .retention_floors
                .entry((key.node_id.clone(), key.incarnation_id))
                .or_insert(0);
            *floor = (*floor).max(key.sequence);
            self.journal.remove(&key);
        }
    }

    fn to_checkpoint(&self) -> ObservationEngineCheckpointV1 {
        let mut projections = self.projections.values().cloned().collect::<Vec<_>>();
        projections.sort_by(|left, right| target_sort_key(&left.target).cmp(&target_sort_key(&right.target)));

        let mut cursor_journal = self.journal.values().map(|entry| {
            ObservationCheckpointCursorV1 {
                node_id: entry.node_id.clone(),
                cursor: entry.cursor,
                payload: entry.payload.clone(),
            }
        }).collect::<Vec<_>>();
        cursor_journal.sort_by(route_cursor_order);

        let mut current_incarnations = self.current_incarnations.iter().map(|(node_id, incarnation_id)| {
            ObservationCheckpointRouteV1 { node_id: node_id.clone(), incarnation_id: *incarnation_id }
        }).collect::<Vec<_>>();
        current_incarnations.sort_by(route_order);
        let mut retired_incarnations = self.retired_incarnations.iter().map(|(node_id, incarnation_id)| {
            ObservationCheckpointRouteV1 { node_id: node_id.clone(), incarnation_id: *incarnation_id }
        }).collect::<Vec<_>>();
        retired_incarnations.sort_by(route_order);
        let mut managed_links = self.managed_links.iter().map(|(managed, runtime)| ManagedRecordLink {
            managed: managed.clone(), runtime: runtime.clone(),
        }).collect::<Vec<_>>();
        managed_links.sort_by(|left, right| {
            left.managed.node_id.cmp(&right.managed.node_id)
                .then_with(|| left.managed.incarnation_id.cmp(&right.managed.incarnation_id))
                .then_with(|| left.managed.record_id.cmp(&right.managed.record_id))
        });
        let mut transport_gaps = self.transport_gaps.iter().map(|(node_id, incarnation_id)| {
            ObservationCheckpointRouteV1 { node_id: node_id.clone(), incarnation_id: *incarnation_id }
        }).collect::<Vec<_>>();
        transport_gaps.sort_by(route_order);
        let mut retention_floors = route_cursor_entries(&self.retention_floors);
        retention_floors.sort_by(route_checkpoint_cursor_order);
        let mut high_watermarks = route_cursor_entries(&self.high_watermarks);
        high_watermarks.sort_by(route_checkpoint_cursor_order);

        ObservationEngineCheckpointV1 {
            version: OBSERVATION_ENGINE_CHECKPOINT_VERSION_V1,
            projections,
            cursor_journal,
            current_incarnations,
            retired_incarnations,
            managed_links,
            transport_gaps,
            retention_floors,
            high_watermarks,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedBatch {
    next: ObservationEngine,
    outcomes: Vec<ApplyOutcome>,
}

impl PreparedBatch {
    pub fn outcomes(&self) -> &[ApplyOutcome] {
        &self.outcomes
    }

    pub fn compact_for_checkpoint(&mut self) -> ObservationEngineCheckpointV1 {
        self.next.compact_cursor_journal();
        self.next.to_checkpoint()
    }

    pub fn checkpoint(&self) -> ObservationEngineCheckpointV1 {
        self.next.to_checkpoint()
    }
}

impl ObservationEngineCheckpointV1 {
    pub fn validate(&self) -> Result<(), ObservationEngineError> {
        if self.version != OBSERVATION_ENGINE_CHECKPOINT_VERSION_V1 {
            return Err(ObservationEngineError::InvalidCheckpoint {
                detail: "unsupported checkpoint version",
            });
        }
        if self.projections.len() > PROJECTIONS_MAX
            || self.cursor_journal.len() > CURSOR_JOURNAL_MAX
            || self.current_incarnations.len() > NODE_ROUTES_MAX
            || self.retired_incarnations.len() > RETIRED_INCARNATIONS_MAX
            || self.managed_links.len() > PROJECTIONS_MAX
            || self.transport_gaps.len() > OBSERVATION_ROUTE_HISTORY_MAX
            || self.retention_floors.len() > OBSERVATION_ROUTE_HISTORY_MAX
            || self.high_watermarks.len() > OBSERVATION_ROUTE_HISTORY_MAX
        {
            return Err(ObservationEngineError::InvalidCheckpoint {
                detail: "checkpoint collection exceeds its bound",
            });
        }

        let mut projection_targets = HashSet::new();
        for projection in &self.projections {
            projection.target.validate_route(
                projection.target.node_id(),
                projection.target.incarnation_id(),
            )?;
            if projection.timeline.len() > TIMELINE_PER_SESSION_MAX
                || projection.tools.len() > TOOLS_PER_SESSION_MAX
                || projection.subagents.len() > SUBAGENTS_PER_SESSION_MAX
                || projection.files.len() > FILES_PER_SESSION_MAX
                || projection.owned_processes.len() > OWNED_PROCESSES_PER_SESSION_MAX
                || projection.interactions.len() > INTERACTIONS_PER_SESSION_MAX
                || !projection_targets.insert(projection.target.clone())
            {
                return Err(ObservationEngineError::InvalidCheckpoint {
                    detail: "invalid or duplicate session projection",
                });
            }
        }

        let mut journal_keys = HashSet::new();
        for entry in &self.cursor_journal {
            if entry.cursor.sequence == 0 {
                return Err(ObservationEngineError::InvalidCheckpoint {
                    detail: "zero checkpoint cursor",
                });
            }
            entry.payload.validate_route(&entry.node_id, entry.cursor)?;
            let key = (entry.node_id.clone(), entry.cursor.incarnation_id, entry.cursor.sequence);
            if !journal_keys.insert(key) {
                return Err(ObservationEngineError::InvalidCheckpoint {
                    detail: "duplicate checkpoint cursor",
                });
            }
        }

        validate_unique_routes(&self.current_incarnations)?;
        validate_unique_routes(&self.retired_incarnations)?;
        validate_unique_routes(&self.transport_gaps)?;
        validate_unique_route_cursors(&self.retention_floors, false)?;
        validate_unique_route_cursors(&self.high_watermarks, true)?;
        let mut current_nodes = HashSet::new();
        if self.current_incarnations.iter().any(|route| {
            !current_nodes.insert(route.node_id.clone())
                || self.retired_incarnations.iter().any(|retired| {
                    retired.node_id == route.node_id
                        && retired.incarnation_id == route.incarnation_id
                })
        }) {
            return Err(ObservationEngineError::InvalidCheckpoint {
                detail: "current incarnation routes conflict",
            });
        }

        let mut managed_keys = HashSet::new();
        for link in &self.managed_links {
            link.validate_route(&link.managed.node_id, link.managed.incarnation_id)?;
            if !managed_keys.insert(link.managed.clone()) {
                return Err(ObservationEngineError::InvalidCheckpoint {
                    detail: "duplicate managed link",
                });
            }
        }

        for floor in &self.retention_floors {
            let high = self.high_watermarks.iter().find(|high| {
                high.node_id == floor.node_id
                    && high.cursor.incarnation_id == floor.cursor.incarnation_id
            });
            if floor.cursor.sequence == 0
                || high.is_none_or(|high| floor.cursor.sequence > high.cursor.sequence)
            {
                return Err(ObservationEngineError::InvalidCheckpoint {
                    detail: "retention floor exceeds durable high watermark",
                });
            }
        }
        for entry in &self.cursor_journal {
            let high = self.high_watermarks.iter().find(|high| {
                high.node_id == entry.node_id
                    && high.cursor.incarnation_id == entry.cursor.incarnation_id
            });
            let floor = self.retention_floors.iter().find(|floor| {
                floor.node_id == entry.node_id
                    && floor.cursor.incarnation_id == entry.cursor.incarnation_id
            });
            if high.is_none_or(|high| entry.cursor.sequence > high.cursor.sequence)
                || floor.is_some_and(|floor| entry.cursor.sequence <= floor.cursor.sequence)
            {
                return Err(ObservationEngineError::InvalidCheckpoint {
                    detail: "cursor journal is outside durable route bounds",
                });
            }
        }
        Ok(())
    }
}

fn validate_unique_routes(
    routes: &[ObservationCheckpointRouteV1],
) -> Result<(), ObservationEngineError> {
    let mut unique = HashSet::new();
    if routes.iter().any(|route| !unique.insert((route.node_id.clone(), route.incarnation_id))) {
        return Err(ObservationEngineError::InvalidCheckpoint {
            detail: "duplicate checkpoint route",
        });
    }
    Ok(())
}

fn validate_unique_route_cursors(
    routes: &[ObservationCheckpointRouteCursorV1],
    allow_zero: bool,
) -> Result<(), ObservationEngineError> {
    let mut unique = HashSet::new();
    if routes.iter().any(|route| {
        (!allow_zero && route.cursor.sequence == 0)
            || !unique.insert((route.node_id.clone(), route.cursor.incarnation_id))
    }) {
        return Err(ObservationEngineError::InvalidCheckpoint {
            detail: "invalid or duplicate checkpoint route cursor",
        });
    }
    Ok(())
}

fn target_sort_key(target: &ObservationTarget) -> (NodeId, NodeIncarnationId, String) {
    match target {
        ObservationTarget::Runtime { key } => (
            key.node_id.clone(),
            key.incarnation_id,
            format!("runtime:{}:{}:{}", key.workspace_id, key.instance_id.0, key.generation.0),
        ),
        ObservationTarget::Managed { key } => (
            key.node_id.clone(),
            key.incarnation_id,
            format!("managed:{}", key.record_id),
        ),
    }
}

fn route_order(
    left: &ObservationCheckpointRouteV1,
    right: &ObservationCheckpointRouteV1,
) -> std::cmp::Ordering {
    left.node_id.cmp(&right.node_id)
        .then_with(|| left.incarnation_id.cmp(&right.incarnation_id))
}

fn route_cursor_order(
    left: &ObservationCheckpointCursorV1,
    right: &ObservationCheckpointCursorV1,
) -> std::cmp::Ordering {
    left.node_id.cmp(&right.node_id)
        .then_with(|| left.cursor.incarnation_id.cmp(&right.cursor.incarnation_id))
        .then_with(|| left.cursor.sequence.cmp(&right.cursor.sequence))
}

fn route_checkpoint_cursor_order(
    left: &ObservationCheckpointRouteCursorV1,
    right: &ObservationCheckpointRouteCursorV1,
) -> std::cmp::Ordering {
    left.node_id.cmp(&right.node_id)
        .then_with(|| left.cursor.incarnation_id.cmp(&right.cursor.incarnation_id))
}

fn route_cursor_entries(
    entries: &HashMap<(NodeId, NodeIncarnationId), u64>,
) -> Vec<ObservationCheckpointRouteCursorV1> {
    entries.iter().map(|((node_id, incarnation_id), sequence)| {
        ObservationCheckpointRouteCursorV1 {
            node_id: node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: *incarnation_id,
                sequence: *sequence,
            },
        }
    }).collect()
}

fn apply_observation(
    projection: &mut SessionProjection,
    cursor: NodeCursor,
    received_at_ms: u64,
    observation: &gate4agent_observation_protocol::ObservationV1,
) -> Result<(), ObservationEngineError> {
    let evidence = observation.evidence;
    match &observation.kind {
        ObservationKindV1::SourceCapabilities {
            source_family,
            source_adapter,
            capabilities,
        } => {
            if let Some(current) = projection.source_capabilities.iter_mut().find(|current| {
                current.evidence == evidence
                    && current.source_family == *source_family
                    && current.source_adapter == *source_adapter
            }) {
                current.capabilities = *capabilities;
            } else {
                if projection.source_capabilities.len() == gate4agent_observation_protocol::OBSERVATION_COLLECTION_MAX {
                    return Err(ObservationEngineError::CapacityExhausted {
                        collection: "source capabilities",
                        max: gate4agent_observation_protocol::OBSERVATION_COLLECTION_MAX,
                    });
                }
                projection.source_capabilities.push(SourceCapabilitiesProjection {
                    evidence,
                    source_family: *source_family,
                    source_adapter: source_adapter.clone(),
                    capabilities: *capabilities,
                });
            }
        }
        ObservationKindV1::Stopped | ObservationKindV1::Exited { .. } => {
            projection.freshness = ProjectionFreshness::LastKnown;
        }
        ObservationKindV1::Stale => projection.mark_source_stale(evidence),
        ObservationKindV1::Error { .. }
            if evidence == ObservationEvidenceV1::HistoryProjection =>
        {
            projection.mark_source_stale(evidence)
        }
        ObservationKindV1::ToolStarted {
            correlation_id,
            class,
        } => start_correlation(
            &mut projection.tools,
            correlation_id,
            Some(class),
            evidence,
            TOOLS_PER_SESSION_MAX,
            "tools",
        )?,
        ObservationKindV1::ToolCompleted {
            correlation_id,
            class,
            success,
            ..
        } => complete_correlation(
            &mut projection.tools,
            correlation_id,
            Some(class),
            evidence,
            Some(*success),
            TOOLS_PER_SESSION_MAX,
            "tools",
        )?,
        ObservationKindV1::ApprovalRequested {
            correlation_id,
            tool_class,
        }
        | ObservationKindV1::QuestionRequested {
            correlation_id,
            tool_class,
        } => start_correlation(
            &mut projection.interactions,
            correlation_id,
            Some(tool_class),
            evidence,
            INTERACTIONS_PER_SESSION_MAX,
            "interactions",
        )?,
        ObservationKindV1::ApprovalResolved {
            correlation_id,
            outcome,
        }
        | ObservationKindV1::QuestionResolved {
            correlation_id,
            outcome,
        }
        | ObservationKindV1::InteractionResolved {
            correlation_id,
            outcome,
        } => resolve_correlation(
            &mut projection.interactions,
            correlation_id,
            evidence,
            *outcome,
            INTERACTIONS_PER_SESSION_MAX,
            "interactions",
        )?,
        ObservationKindV1::SubagentStarted {
            correlation_id,
            class,
        } => start_correlation(
            &mut projection.subagents,
            correlation_id,
            Some(class),
            evidence,
            SUBAGENTS_PER_SESSION_MAX,
            "subagents",
        )?,
        ObservationKindV1::SubagentProgress { correlation_id } => {
            start_correlation(
                &mut projection.subagents,
                correlation_id,
                None,
                evidence,
                SUBAGENTS_PER_SESSION_MAX,
                "subagents",
            )?;
        }
        ObservationKindV1::SubagentCompleted {
            correlation_id,
            success,
        } => complete_correlation(
            &mut projection.subagents,
            correlation_id,
            None,
            evidence,
            *success,
            SUBAGENTS_PER_SESSION_MAX,
            "subagents",
        )?,
        ObservationKindV1::OwnedProcessStarted {
            correlation_id,
            class,
        } => start_correlation(
            &mut projection.owned_processes,
            correlation_id,
            Some(class),
            evidence,
            OWNED_PROCESSES_PER_SESSION_MAX,
            "owned processes",
        )?,
        ObservationKindV1::OwnedProcessExited {
            correlation_id,
            success,
            ..
        } => complete_correlation(
            &mut projection.owned_processes,
            correlation_id,
            None,
            evidence,
            *success,
            OWNED_PROCESSES_PER_SESSION_MAX,
            "owned processes",
        )?,
        ObservationKindV1::TodoSnapshot {
            revision,
            items,
            complete,
        } => apply_todo(projection, *revision, items, *complete, evidence),
        ObservationKindV1::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            context_window,
            is_cumulative,
        } => apply_usage(
            &mut projection.usage,
            UsageTotals {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cache_read_tokens: *cache_read_tokens,
                cache_write_tokens: *cache_write_tokens,
                reasoning_tokens: *reasoning_tokens,
            },
            *context_window,
            *is_cumulative,
            evidence,
        ),
        ObservationKindV1::ContextWindowUsage {
            uncached_input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            unattributed_tokens,
            used_tokens,
            capacity_tokens,
        } => apply_context_window_usage(
            &mut projection.usage,
            *uncached_input_tokens,
            *cache_read_tokens,
            *cache_write_tokens,
            *output_tokens,
            *unattributed_tokens,
            *used_tokens,
            *capacity_tokens,
            evidence,
        ),
        ObservationKindV1::FileChanged { path } => {
            if projection.files.len() == FILES_PER_SESSION_MAX {
                projection.files.pop_front();
            }
            projection.files.push_back(FileProjection {
                path: path.clone(),
                evidence,
            });
        }
        ObservationKindV1::HistorySnapshot {
            message_count,
            message_count_exact,
            completed_turn_count,
            total_tokens,
        } => {
            projection.history = Some(HistoryProjection {
                evidence,
                message_count: *message_count,
                message_count_exact: *message_count_exact,
                completed_turn_count: *completed_turn_count,
                total_tokens: *total_tokens,
            });
            projection.stale_evidence.retain(|current| *current != evidence);
            projection.refresh_integrity();
        }
        ObservationKindV1::Gap { .. } => projection.mark_source_incomplete(evidence, true),
        ObservationKindV1::SourceReset => projection.source_reset(evidence),
        _ => {}
    }
    if observation.truncated {
        projection.mark_source_incomplete(evidence, false);
    }
    if !matches!(observation.kind, ObservationKindV1::SourceCapabilities { .. }) {
        projection.push_timeline(TimelineEntry {
            cursor,
            received_at_ms,
            evidence,
            kind: observation.kind.clone(),
        });
    }
    Ok(())
}

fn start_correlation(
    entries: &mut Vec<CorrelationProjection>,
    correlation_id: &str,
    class: Option<&String>,
    evidence: ObservationEvidenceV1,
    max: usize,
    collection: &'static str,
) -> Result<(), ObservationEngineError> {
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.correlation_id == correlation_id)
    {
        if existing.state == CorrelationState::Pending && existing.class.is_none() {
            existing.class = class.cloned();
        }
        return Ok(());
    }
    ensure_correlation_capacity(entries, max, collection)?;
    entries.push(CorrelationProjection {
        correlation_id: correlation_id.to_owned(),
        class: class.cloned(),
        evidence,
        state: CorrelationState::Pending,
    });
    Ok(())
}

fn complete_correlation(
    entries: &mut Vec<CorrelationProjection>,
    correlation_id: &str,
    class: Option<&String>,
    evidence: ObservationEvidenceV1,
    success: Option<bool>,
    max: usize,
    collection: &'static str,
) -> Result<(), ObservationEngineError> {
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.correlation_id == correlation_id)
    {
        if existing.state == CorrelationState::Pending {
            existing.state = CorrelationState::Completed { success };
            if existing.class.is_none() {
                existing.class = class.cloned();
            }
        }
        return Ok(());
    }
    ensure_correlation_capacity(entries, max, collection)?;
    entries.push(CorrelationProjection {
        correlation_id: correlation_id.to_owned(),
        class: class.cloned(),
        evidence,
        state: CorrelationState::OrphanCompletion { success },
    });
    Ok(())
}

fn resolve_correlation(
    entries: &mut Vec<CorrelationProjection>,
    correlation_id: &str,
    evidence: ObservationEvidenceV1,
    outcome: ObservationInteractionOutcomeV1,
    max: usize,
    collection: &'static str,
) -> Result<(), ObservationEngineError> {
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| entry.correlation_id == correlation_id)
    {
        if existing.state == CorrelationState::Pending {
            existing.state = CorrelationState::Resolved { outcome };
        }
        return Ok(());
    }
    ensure_correlation_capacity(entries, max, collection)?;
    entries.push(CorrelationProjection {
        correlation_id: correlation_id.to_owned(),
        class: None,
        evidence,
        state: CorrelationState::OrphanResolution { outcome },
    });
    Ok(())
}

fn ensure_correlation_capacity(
    entries: &mut Vec<CorrelationProjection>,
    max: usize,
    collection: &'static str,
) -> Result<(), ObservationEngineError> {
    if entries.len() < max {
        return Ok(());
    }
    if let Some(index) = entries.iter().position(|entry| !entry.state.is_active()) {
        entries.remove(index);
        return Ok(());
    }
    Err(ObservationEngineError::CapacityExhausted { collection, max })
}

fn apply_todo(
    projection: &mut SessionProjection,
    revision: u64,
    items: &[ObservationTodoItemV1],
    complete: bool,
    evidence: ObservationEvidenceV1,
) {
    let incoming = TodoRevisionProjection {
        revision,
        items: items.to_vec(),
        complete,
        evidence,
    };
    match projection.todos.current.as_ref() {
        None => projection.todos.current = Some(incoming),
        Some(current) if revision > current.revision => {
            projection.todos.previous = projection.todos.current.replace(incoming);
            projection.todos.conflict = false;
        }
        Some(current) if revision == current.revision && current == &incoming => {}
        Some(current) if revision == current.revision => {
            projection.todos.conflict = true;
            projection.mark_source_incomplete(evidence, false);
        }
        Some(_) => {
            projection.todos.conflict = true;
            projection.mark_source_incomplete(evidence, false);
        }
    }
}

fn apply_usage(
    projection: &mut UsageProjection,
    incoming: UsageTotals,
    context_window: Option<u64>,
    is_cumulative: bool,
    evidence: ObservationEvidenceV1,
) {
    projection.context_window = context_window.or(projection.context_window);
    let previous = projection
        .context_occupancy
        .filter(|snapshot| snapshot.evidence == evidence);
    let occupancy_totals = if is_cumulative {
        incoming
    } else {
        UsageTotals {
            input_tokens: incoming.input_tokens,
            output_tokens: previous
                .map(|snapshot| snapshot.output_tokens)
                .unwrap_or(0)
                .saturating_add(incoming.output_tokens),
            cache_read_tokens: incoming.cache_read_tokens,
            cache_write_tokens: incoming.cache_write_tokens,
            reasoning_tokens: incoming.reasoning_tokens,
        }
    };
    projection.context_occupancy = Some(ContextOccupancySnapshot {
        uncached_input_tokens: occupancy_totals.input_tokens,
        output_tokens: occupancy_totals.output_tokens,
        cache_read_tokens: occupancy_totals.cache_read_tokens,
        cache_write_tokens: occupancy_totals.cache_write_tokens,
        reasoning_tokens: Some(occupancy_totals.reasoning_tokens),
        unattributed_tokens: 0,
        used_tokens: occupancy_totals
            .input_tokens
            .saturating_add(occupancy_totals.cache_read_tokens)
            .saturating_add(occupancy_totals.cache_write_tokens)
            .saturating_add(occupancy_totals.output_tokens),
        context_window: context_window.or_else(|| previous.and_then(|snapshot| snapshot.context_window)),
        evidence,
        provenance: if is_cumulative {
            ContextOccupancyProvenance::CumulativeUsage
        } else {
            ContextOccupancyProvenance::PerTurnUsageSynthesis
        },
    });
    if !is_cumulative {
        projection.observed_delta = projection.observed_delta.saturating_add(incoming);
        return;
    }
    if projection.cumulative_evidence == Some(evidence) {
        if let Some(previous) = projection.last_cumulative {
            if let Some(delta) = incoming.checked_delta(previous) {
                projection.observed_delta = projection.observed_delta.saturating_add(delta);
            }
        }
    }
    projection.last_cumulative = Some(incoming);
    projection.cumulative_evidence = Some(evidence);
}

fn apply_context_window_usage(
    projection: &mut UsageProjection,
    uncached_input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    unattributed_tokens: u64,
    used_tokens: u64,
    capacity_tokens: u64,
    evidence: ObservationEvidenceV1,
) {
    if evidence == ObservationEvidenceV1::StructuredProvider {
        projection.context_occupancy = Some(ContextOccupancySnapshot {
            uncached_input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens: None,
            unattributed_tokens,
            used_tokens,
            context_window: Some(capacity_tokens),
            evidence,
            provenance: ContextOccupancyProvenance::ExactCurrentWindow,
        });
    } else if projection.context_occupancy.is_some_and(|snapshot| {
        snapshot.provenance == ContextOccupancyProvenance::ExactCurrentWindow
    }) {
        projection.context_occupancy = None;
    }
}

#[cfg(test)]
mod context_occupancy_internal_tests {
    use super::*;

    fn apply_exact(projection: &mut UsageProjection, evidence: ObservationEvidenceV1) {
        apply_context_window_usage(
            projection,
            10,
            20,
            5,
            15,
            10,
            60,
            100,
            evidence,
        );
    }

    #[test]
    fn non_structured_exact_context_cannot_activate_or_survive_in_reducer() {
        let mut projection = UsageProjection {
            observed_delta: UsageTotals {
                input_tokens: 3,
                output_tokens: 2,
                ..UsageTotals::default()
            },
            ..UsageProjection::default()
        };
        let observed_delta = projection.observed_delta;
        for evidence in [
            ObservationEvidenceV1::ManagedHook,
            ObservationEvidenceV1::NodeLifecycle,
            ObservationEvidenceV1::WorkspaceObservation,
            ObservationEvidenceV1::HistoryProjection,
            ObservationEvidenceV1::PtyHint,
        ] {
            apply_exact(&mut projection, evidence);
            assert!(projection.context_occupancy.is_none(), "{evidence:?}");
            assert_eq!(projection.observed_delta, observed_delta, "{evidence:?}");

            apply_exact(&mut projection, ObservationEvidenceV1::StructuredProvider);
            assert!(projection.context_occupancy.is_some());
            apply_exact(&mut projection, evidence);
            assert!(projection.context_occupancy.is_none(), "{evidence:?}");
            assert_eq!(projection.observed_delta, observed_delta, "{evidence:?}");
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ObservationEngineError {
    #[error(transparent)]
    Api(#[from] ObservationApiError),
    #[error("node {node_id} cursor {cursor:?} was reused with different canonical ingress")]
    CursorCollision { node_id: NodeId, cursor: NodeCursor },
    #[error("node {node_id} cursor {cursor:?} is at or below the durable retention floor")]
    BelowRetentionFloor { node_id: NodeId, cursor: NodeCursor },
    #[error("node {node_id} incarnation {incarnation_id} was already replaced")]
    ReplacedIncarnation {
        node_id: NodeId,
        incarnation_id: NodeIncarnationId,
    },
    #[error("active {collection} capacity {max} is exhausted")]
    CapacityExhausted {
        collection: &'static str,
        max: usize,
    },
    #[error("invalid observation engine checkpoint: {detail}")]
    InvalidCheckpoint { detail: &'static str },
}

pub fn gap(first_sequence: u64, last_sequence: u64) -> ObservationGap {
    ObservationGap {
        first_sequence,
        last_sequence,
    }
}
