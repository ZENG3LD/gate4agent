use gate4agent_node_protocol::{
    NodeCursor, NodeId, NodeIncarnationId, SessionRecordId, WorkspaceId,
};
use gate4agent_observation_api::{
    AgentInstanceId, ManagedRecordLink, ManagedSessionKey, ObservationApiError,
    ObservationIngressEnvelope,
    ObservationIngressPayload, ObservationRecordInventory, ObservationResyncBatch, ObservationTarget,
    ObservationTransport, ProjectionAvailability, ProjectionFreshness, RuntimeSessionKey,
    SessionGeneration,
};
use gate4agent_observation_engine::{ApplyOutcome, ObservationEngineError};
use gate4agent_observation_protocol::{
    ObservationEvidenceV1, ObservationKindV1, ObservationV1,
};
use gate4agent_observation_service::{
    ObservationService, ObservationServiceError, ObservationStoreLimits,
};
use gate4agent_observation_store::ObservationStoreError;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn database_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "gate4agent-observation-{label}-{}-{}.sqlite3",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed),
    ))
}

fn node() -> NodeId {
    NodeId::new("node-observation-a").expect("node ID")
}

fn incarnation() -> NodeIncarnationId {
    NodeIncarnationId::from_bytes([7; 16])
}

fn runtime_target() -> ObservationTarget {
    ObservationTarget::Runtime {
        key: RuntimeSessionKey {
            node_id: node(),
            incarnation_id: incarnation(),
            workspace_id: WorkspaceId::new("workspace-observation-a").expect("workspace ID"),
            instance_id: AgentInstanceId(41),
            generation: SessionGeneration(0),
        },
    }
}

fn managed_key() -> ManagedSessionKey {
    ManagedSessionKey {
        node_id: node(),
        incarnation_id: incarnation(),
        record_id: SessionRecordId::new("record-observation-a").expect("record ID"),
    }
}

fn managed_target() -> ObservationTarget {
    ObservationTarget::Managed { key: managed_key() }
}

fn observation(sequence: u64, kind: ObservationKindV1) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node(),
        cursor: NodeCursor { incarnation_id: incarnation(), sequence },
        received_at_ms: 20_000 + sequence,
        transport: ObservationTransport::DirectNode,
        payload: ObservationIngressPayload::Observation {
            address: runtime_target(),
            observation: ObservationV1 {
                source_sequence: sequence,
                observed_at_unix_ms: Some(19_000 + sequence),
                evidence: ObservationEvidenceV1::StructuredProvider,
                kind,
                truncated: false,
            },
        },
    }
}

fn transport_gap(sequence: u64) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node(),
        cursor: NodeCursor { incarnation_id: incarnation(), sequence },
        received_at_ms: 20_000 + sequence,
        transport: ObservationTransport::DirectNode,
        payload: ObservationIngressPayload::ResyncRequired {
            oldest: NodeCursor { incarnation_id: incarnation(), sequence },
        },
    }
}

fn inventory() -> ObservationRecordInventory {
    ObservationRecordInventory {
        node_id: node(),
        incarnation_id: incarnation(),
        records: vec![ManagedRecordLink { managed: managed_key(), runtime: None }],
        complete: true,
    }
}

fn limits(operations: usize) -> ObservationStoreLimits {
    ObservationStoreLimits { tail_operations: operations, tail_bytes: 4 * 1024 * 1024 }
}

fn cleanup(path: &PathBuf) {
    for candidate in [
        path.clone(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        let _ = fs::remove_file(candidate);
    }
}

#[test]
fn restart_rebuild_equals_pre_restart_projection() {
    let path = database_path("restart");
    let mut service = ObservationService::open_with_limits(&path, limits(32)).expect("open");
    service.apply_ingress(observation(1, ObservationKindV1::Working)).expect("working");
    service.apply_record_inventory(inventory()).expect("inventory");
    service.apply_resync(ObservationResyncBatch {
        node_id: node(),
        incarnation_id: incarnation(),
        requested_after: 1,
        high_watermark: NodeCursor { incarnation_id: incarnation(), sequence: 2 },
        oldest_available_sequence: 2,
        records: Vec::new(),
        records_complete: false,
        gaps: Vec::new(),
        events: vec![observation(2, ObservationKindV1::Ready)],
    }).expect("resync");
    let before = service.committed_snapshot();
    service.close().expect("close");

    let reopened = ObservationService::open_with_limits(&path, limits(32)).expect("reopen");
    assert_eq!(reopened.committed_snapshot(), before);
    assert_eq!(reopened.durable_resume_cursors(), vec![(node(), NodeCursor {
        incarnation_id: incarnation(), sequence: 2,
    })]);
    reopened.close().expect("close reopened");
    cleanup(&path);
}

#[test]
fn direct_and_c2_duplicate_is_canonical_across_restart() {
    let path = database_path("duplicate");
    let direct = observation(1, ObservationKindV1::Working);
    let mut service = ObservationService::open_with_limits(&path, limits(32)).expect("open");
    assert_eq!(service.apply_ingress(direct.clone()).expect("direct"), ApplyOutcome::Applied);
    service.close().expect("close");

    let mut c2 = direct;
    c2.transport = ObservationTransport::C2;
    c2.received_at_ms += 9_000;
    let mut reopened = ObservationService::open_with_limits(&path, limits(32)).expect("reopen");
    assert_eq!(reopened.apply_ingress(c2).expect("C2 duplicate"), ApplyOutcome::Duplicate);
    assert_eq!(reopened.projection(&runtime_target()).unwrap().timeline.len(), 1);
    reopened.close().expect("close reopened");
    cleanup(&path);
}

#[test]
fn checkpoint_preserves_immediate_cross_transport_duplicates_in_both_directions() {
    for (label, initial_transport, duplicate_transport) in [
        ("direct-c2", ObservationTransport::DirectNode, ObservationTransport::C2),
        ("c2-direct", ObservationTransport::C2, ObservationTransport::DirectNode),
    ] {
        let path = database_path(label);
        let mut initial = observation(1, ObservationKindV1::Working);
        initial.transport = initial_transport;
        let mut service = ObservationService::open_with_limits(&path, limits(2)).expect("open");
        assert_eq!(service.apply_ingress(initial.clone()).expect("initial"), ApplyOutcome::Applied);
        service.apply_record_inventory(inventory()).expect("checkpoint");

        let mut duplicate = initial;
        duplicate.transport = duplicate_transport;
        duplicate.received_at_ms += 9_000;
        assert_eq!(service.apply_ingress(duplicate).expect("duplicate"), ApplyOutcome::Duplicate);
        assert_eq!(service.projection(&runtime_target()).unwrap().timeline.len(), 1);
        service.close().expect("close");
        cleanup(&path);
    }
}

#[test]
fn checkpoint_restart_preserves_recent_duplicate_and_rejects_forgotten_cursor() {
    let path = database_path("retained-restart");
    let mut service = ObservationService::open_with_limits(&path, limits(258)).expect("open");
    for sequence in 1..=257 {
        service.apply_ingress(observation(sequence, ObservationKindV1::Working))
            .expect("journal observation");
    }
    service.apply_record_inventory(inventory()).expect("checkpoint");
    service.close().expect("close");

    let mut reopened = ObservationService::open_with_limits(&path, limits(258)).expect("reopen");
    let old = reopened.apply_ingress(observation(1, ObservationKindV1::Working))
        .expect_err("forgotten cursor must remain below the floor");
    assert!(matches!(
        old,
        ObservationServiceError::Engine(ObservationEngineError::BelowRetentionFloor { .. })
    ));
    let mut recent = observation(257, ObservationKindV1::Working);
    recent.transport = ObservationTransport::C2;
    recent.received_at_ms += 7_000;
    assert_eq!(reopened.apply_ingress(recent).expect("recent duplicate"), ApplyOutcome::Duplicate);
    reopened.close().expect("close reopened");
    cleanup(&path);
}

#[test]
fn cursor_collision_fails_closed_without_poisoning_service() {
    let collision_path = database_path("collision");
    let mut service = ObservationService::open_with_limits(&collision_path, limits(32)).expect("open");
    service.apply_ingress(observation(1, ObservationKindV1::Working)).expect("working");
    let error = service.apply_ingress(observation(1, ObservationKindV1::Ready)).expect_err("collision");
    assert!(matches!(error, ObservationServiceError::Engine(ObservationEngineError::CursorCollision { .. })));
    assert!(!service.is_poisoned());
    service.close().expect("close");
    cleanup(&collision_path);
}

#[test]
fn compaction_preserves_projection_gap_and_managed_inventory() {
    let path = database_path("compaction");
    let mut service = ObservationService::open_with_limits(&path, limits(2)).expect("open");
    service.apply_ingress(transport_gap(1)).expect("gap before projection");
    service.apply_record_inventory(inventory()).expect("inventory and checkpoint");
    service.apply_ingress(observation(2, ObservationKindV1::Working)).expect("projection");
    let before = service.committed_snapshot();
    let managed = service.projection(&managed_target()).expect("managed projection");
    assert!(managed.transport_incomplete);
    assert_eq!(managed.availability, ProjectionAvailability::Partial);
    service.close().expect("close");

    let reopened = ObservationService::open_with_limits(&path, limits(2)).expect("reopen");
    assert_eq!(reopened.committed_snapshot(), before);
    let runtime = reopened.projection(&runtime_target()).expect("runtime projection");
    assert!(runtime.transport_incomplete);
    assert_eq!(runtime.freshness, ProjectionFreshness::IncompleteAfterGap);
    assert!(reopened.projection(&managed_target()).is_some());
    reopened.close().expect("close reopened");
    cleanup(&path);
}

#[test]
fn failed_transaction_never_mutates_engine_and_poison_requires_reopen() {
    let path = database_path("failed-transaction");
    let mut service = ObservationService::open_with_limits(&path, limits(32)).expect("open");
    service.apply_ingress(observation(1, ObservationKindV1::Working)).expect("baseline");
    let before = service.engine().clone();

    let blocker = rusqlite::Connection::open(&path).expect("blocker connection");
    blocker.execute_batch("BEGIN IMMEDIATE;").expect("hold writer lock");
    let error = service.apply_ingress(observation(2, ObservationKindV1::Ready)).expect_err("locked transaction");
    assert!(matches!(error, ObservationServiceError::Store(_)));
    assert!(service.is_poisoned());
    assert_eq!(service.engine(), &before);
    assert!(matches!(
        service.apply_ingress(observation(3, ObservationKindV1::Working)),
        Err(ObservationServiceError::Poisoned)
    ));
    blocker.execute_batch("ROLLBACK;").expect("release writer lock");
    drop(blocker);
    service.close().expect("close poisoned service");

    let reopened = ObservationService::open_with_limits(&path, limits(32)).expect("reopen");
    assert_eq!(reopened.engine(), &before);
    reopened.close().expect("close reopened");
    cleanup(&path);
}

#[test]
fn database_bytes_lack_privacy_sentinels() {
    let path = database_path("privacy");
    let attempted_sentinel = "PROMPT_PRIVACY_SENTINEL include the full private prompt";
    let mut service = ObservationService::open_with_limits(&path, limits(2)).expect("open");
    let rejected = service.apply_ingress(observation(
        1,
        ObservationKindV1::Error {
            detail: attempted_sentinel.to_owned(),
        },
    )).expect_err("prompt-like error detail must be rejected");
    assert!(matches!(
        rejected,
        ObservationServiceError::Engine(ObservationEngineError::Api(
            ObservationApiError::InvalidErrorDetail
        ))
    ));
    service.apply_ingress(observation(1, ObservationKindV1::Working)).expect("observation");
    service.apply_record_inventory(inventory()).expect("checkpoint");

    for candidate in [
        path.clone(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        assert!(candidate.exists(), "expected SQLite durability file {candidate:?}");
        let bytes = fs::read(&candidate).expect("durable observation bytes");
        assert!(!bytes.windows(attempted_sentinel.len())
            .any(|window| window == attempted_sentinel.as_bytes()));
    }
    service.close().expect("close");
    cleanup(&path);
}

#[test]
fn stored_checkpoint_version_must_match_decoded_version() {
    let path = database_path("checkpoint-version-mismatch");
    let mut service = ObservationService::open_with_limits(&path, limits(2)).expect("open");
    service.apply_ingress(observation(1, ObservationKindV1::Working)).expect("observation");
    service.apply_record_inventory(inventory()).expect("checkpoint");
    service.close().expect("close");

    let connection = rusqlite::Connection::open(&path).expect("corruptor connection");
    connection.execute(
        "UPDATE observation_checkpoint SET checkpoint_version = checkpoint_version + 1",
        [],
    ).expect("corrupt stored checkpoint version");
    drop(connection);
    assert!(matches!(
        ObservationService::open_with_limits(&path, limits(2)),
        Err(ObservationServiceError::Store(ObservationStoreError::Corrupt(
            "stored checkpoint version does not match decoded checkpoint"
        )))
    ));
    cleanup(&path);
}

#[test]
fn restart_restores_projection_gap_managed_inventory_and_history() {
    let path = database_path("restart-complete-read-model");
    let mut service = ObservationService::open_with_limits(&path, limits(32)).expect("open");
    service.apply_ingress(transport_gap(1)).expect("gap");
    service.apply_record_inventory(inventory()).expect("inventory");
    service.apply_ingress(observation(2, ObservationKindV1::Working)).expect("working");
    let mut history = observation(3, ObservationKindV1::HistorySnapshot {
        message_count: 17,
        message_count_exact: true,
        completed_turn_count: Some(6),
        total_tokens: Some(4_096),
    });
    let ObservationIngressPayload::Observation { observation, .. } = &mut history.payload else {
        unreachable!("history observation payload")
    };
    observation.evidence = ObservationEvidenceV1::HistoryProjection;
    service.apply_ingress(history).expect("history");
    service.close().expect("close");

    let reopened = ObservationService::open_with_limits(&path, limits(32)).expect("reopen");
    let runtime = reopened.projection(&runtime_target()).expect("runtime projection");
    assert!(runtime.transport_incomplete);
    assert_eq!(runtime.freshness, ProjectionFreshness::IncompleteAfterGap);
    let history = runtime.history.as_ref().expect("history projection");
    assert_eq!(history.message_count, 17);
    assert_eq!(history.completed_turn_count, Some(6));
    assert!(reopened.projection(&managed_target()).is_some());
    assert_eq!(reopened.durable_resume_cursors(), vec![(node(), NodeCursor {
        incarnation_id: incarnation(),
        sequence: 3,
    })]);
    reopened.close().expect("close reopened");
    cleanup(&path);
}
