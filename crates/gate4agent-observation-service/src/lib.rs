//! Single-writer durable authority for monitoring-only observations.

use gate4agent_observation_api::{
    ManagedRecordLink, NodeCursor, NodeId, NodeIncarnationId, ObservationIngressEnvelope,
    ObservationRecordInventory, ObservationResyncBatch, ObservationTarget,
};
use gate4agent_observation_engine::{
    ApplyOutcome, ObservationEngine, ObservationEngineError, PreparedBatch, SessionProjection,
};
use gate4agent_observation_store::{
    ObservationStore, ObservationStoreError, StoredObservationOperation,
};
use std::path::Path;
use thiserror::Error;

pub use gate4agent_observation_store::ObservationStoreLimits;

pub struct ObservationService {
    store: Option<ObservationStore>,
    engine: ObservationEngine,
    poisoned: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationCommittedSnapshot {
    pub projections: Vec<SessionProjection>,
    pub durable_resume_cursors: Vec<(NodeId, NodeCursor)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationCommittedRoute {
    pub node_id: NodeId,
    pub incarnation_id: NodeIncarnationId,
    pub projections: Vec<SessionProjection>,
    pub durable_resume_cursor: Option<NodeCursor>,
}

impl ObservationService {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ObservationServiceError> {
        Self::open_with_limits(path, ObservationStoreLimits::default())
    }

    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: ObservationStoreLimits,
    ) -> Result<Self, ObservationServiceError> {
        let store = ObservationStore::open_with_limits(path, limits)?;
        let (checkpoint, operations) = store.load()?;
        let mut engine = match checkpoint {
            Some(checkpoint) => ObservationEngine::restore(checkpoint)?,
            None => ObservationEngine::new(),
        };
        for operation in operations {
            let prepared = prepare_operation(&engine, &operation)?;
            engine.accept(prepared);
        }
        Ok(Self { store: Some(store), engine, poisoned: false })
    }

    pub fn apply_ingress(
        &mut self,
        envelope: ObservationIngressEnvelope,
    ) -> Result<ApplyOutcome, ObservationServiceError> {
        self.ensure_healthy()?;
        let operation = StoredObservationOperation::Ingress { envelope: envelope.clone() };
        let prepared = self.engine.prepare(envelope)?;
        let outcome = prepared.outcomes()[0];
        self.commit_prepared(operation, prepared)?;
        Ok(outcome)
    }

    pub fn apply_resync(
        &mut self,
        batch: ObservationResyncBatch,
    ) -> Result<Vec<ApplyOutcome>, ObservationServiceError> {
        self.ensure_healthy()?;
        let operation = StoredObservationOperation::Resync { batch: batch.clone() };
        let prepared = self.engine.prepare_resync(&batch)?;
        let outcomes = prepared.outcomes().to_vec();
        self.commit_prepared(operation, prepared)?;
        Ok(outcomes)
    }

    pub fn apply_record_inventory(
        &mut self,
        inventory: ObservationRecordInventory,
    ) -> Result<(), ObservationServiceError> {
        self.ensure_healthy()?;
        inventory.validate()?;
        let prepared = self.engine.prepare_record_inventory(
            &inventory.node_id,
            inventory.incarnation_id,
            &inventory.records,
            inventory.complete,
        )?;
        let operation = StoredObservationOperation::RecordInventory { inventory };
        self.commit_prepared(operation, prepared)
    }

    pub fn apply_record_inventory_parts(
        &mut self,
        node_id: NodeId,
        incarnation_id: NodeIncarnationId,
        records: Vec<ManagedRecordLink>,
        complete: bool,
    ) -> Result<(), ObservationServiceError> {
        self.apply_record_inventory(ObservationRecordInventory {
            node_id,
            incarnation_id,
            records,
            complete,
        })
    }

    pub fn engine(&self) -> &ObservationEngine {
        &self.engine
    }

    pub fn projection(&self, target: &ObservationTarget) -> Option<&SessionProjection> {
        self.engine.projection(target)
    }

    pub fn durable_resume_cursors(&self) -> Vec<(NodeId, NodeCursor)> {
        self.engine.durable_resume_cursors()
    }

    pub fn committed_snapshot(&self) -> ObservationCommittedSnapshot {
        let mut projections = self.engine.projections()
            .map(|(_, projection)| projection.clone())
            .collect::<Vec<_>>();
        projections.sort_by(|left, right| {
            format!("{:?}", left.target).cmp(&format!("{:?}", right.target))
        });
        ObservationCommittedSnapshot {
            projections,
            durable_resume_cursors: self.durable_resume_cursors(),
        }
    }

    pub fn committed_route(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> ObservationCommittedRoute {
        let mut projections = self.engine.projections()
            .filter(|(target, _)| target.node_id() == node_id)
            .map(|(_, projection)| projection.clone())
            .collect::<Vec<_>>();
        projections.sort_by(|left, right| {
            format!("{:?}", left.target).cmp(&format!("{:?}", right.target))
        });
        let durable_resume_cursor = self.durable_resume_cursors()
            .into_iter()
            .find_map(|(candidate, cursor)| {
                (candidate == *node_id && cursor.incarnation_id == incarnation_id)
                    .then_some(cursor)
            });
        ObservationCommittedRoute {
            node_id: node_id.clone(),
            incarnation_id,
            projections,
            durable_resume_cursor,
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn flush(&mut self) -> Result<(), ObservationServiceError> {
        self.ensure_healthy()?;
        self.store.as_mut().expect("open observation store").flush()?;
        Ok(())
    }

    pub fn close(mut self) -> Result<(), ObservationServiceError> {
        if let Some(store) = self.store.take() {
            store.close()?;
        }
        Ok(())
    }

    fn ensure_healthy(&self) -> Result<(), ObservationServiceError> {
        if self.poisoned {
            Err(ObservationServiceError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn commit_prepared(
        &mut self,
        operation: StoredObservationOperation,
        mut prepared: PreparedBatch,
    ) -> Result<(), ObservationServiceError> {
        let encoded = operation.encode()?;
        let should_checkpoint = self.store.as_ref()
            .expect("open observation store")
            .should_checkpoint_after(encoded.len());
        let checkpoint = should_checkpoint.then(|| prepared.compact_for_checkpoint());
        if let Err(error) = self.store.as_mut()
            .expect("open observation store")
            .commit_operation(&operation, checkpoint.as_ref())
        {
            self.poisoned = true;
            return Err(error.into());
        }
        self.engine.accept(prepared);
        Ok(())
    }
}

fn prepare_operation(
    engine: &ObservationEngine,
    operation: &StoredObservationOperation,
) -> Result<PreparedBatch, ObservationEngineError> {
    match operation {
        StoredObservationOperation::Ingress { envelope } => engine.prepare(envelope.clone()),
        StoredObservationOperation::Resync { batch } => engine.prepare_resync(batch),
        StoredObservationOperation::RecordInventory { inventory } => engine.prepare_record_inventory(
            &inventory.node_id,
            inventory.incarnation_id,
            &inventory.records,
            inventory.complete,
        ),
    }
}

#[derive(Debug, Error)]
pub enum ObservationServiceError {
    #[error("observation service is poisoned; reopen it to replay durable state")]
    Poisoned,
    #[error(transparent)]
    Store(#[from] ObservationStoreError),
    #[error(transparent)]
    Engine(#[from] ObservationEngineError),
    #[error(transparent)]
    Api(#[from] gate4agent_observation_api::ObservationApiError),
}
