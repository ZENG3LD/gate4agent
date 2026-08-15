use gate4agent_node_protocol::{
    NodeCursor, NodeId, NodeIncarnationId, SessionRecordId, WorkspaceId,
};
use gate4agent_observation_api::{
    AgentInstanceId, ManagedRecordLink, ManagedSessionKey, ObservationApiError, ObservationGap,
    ObservationIngressEnvelope, ObservationIngressPayload, ObservationResyncBatch,
    ObservationTarget, ObservationTransport, ProjectionAvailability, ProjectionFreshness,
    RuntimeSessionKey, SessionGeneration,
};
use gate4agent_observation_engine::{
    ApplyOutcome, ContextOccupancyProvenance, CorrelationState, ObservationEngine,
    ObservationEngineError, UsageTotals,
};
use gate4agent_observation_protocol::{
    ObservationCapabilitiesV1, ObservationEvidenceV1, ObservationKindV1,
    ObservationSourceFamilyV1, ObservationTodoItemV1, ObservationTodoStateV1, ObservationV1,
};

fn node() -> NodeId {
    NodeId::new("node-a").expect("node ID")
}

fn incarnation(byte: u8) -> NodeIncarnationId {
    NodeIncarnationId::from_bytes([byte; 16])
}

fn runtime(incarnation_id: NodeIncarnationId) -> RuntimeSessionKey {
    runtime_generation(incarnation_id, 0)
}

fn runtime_generation(
    incarnation_id: NodeIncarnationId,
    generation: u64,
) -> RuntimeSessionKey {
    RuntimeSessionKey {
        node_id: node(),
        incarnation_id,
        workspace_id: WorkspaceId::new("workspace-a").expect("workspace ID"),
        instance_id: AgentInstanceId(7),
        generation: SessionGeneration(generation),
    }
}

fn target(incarnation_id: NodeIncarnationId) -> ObservationTarget {
    ObservationTarget::Runtime {
        key: runtime(incarnation_id),
    }
}

fn managed_key(incarnation_id: NodeIncarnationId) -> ManagedSessionKey {
    ManagedSessionKey {
        node_id: node(),
        incarnation_id,
        record_id: SessionRecordId::new("record-inline-a").expect("record ID"),
    }
}

fn managed_target(incarnation_id: NodeIncarnationId) -> ObservationTarget {
    ObservationTarget::Managed {
        key: managed_key(incarnation_id),
    }
}

fn managed_link(
    incarnation_id: NodeIncarnationId,
    generation: Option<u64>,
) -> ManagedRecordLink {
    ManagedRecordLink {
        managed: managed_key(incarnation_id),
        runtime: generation.map(|generation| runtime_generation(incarnation_id, generation)),
    }
}

fn envelope(
    incarnation_id: NodeIncarnationId,
    sequence: u64,
    kind: ObservationKindV1,
) -> ObservationIngressEnvelope {
    envelope_with_evidence(
        incarnation_id,
        sequence,
        ObservationEvidenceV1::StructuredProvider,
        kind,
    )
}

fn envelope_with_evidence(
    incarnation_id: NodeIncarnationId,
    sequence: u64,
    evidence: ObservationEvidenceV1,
    kind: ObservationKindV1,
) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node(),
        cursor: NodeCursor {
            incarnation_id,
            sequence,
        },
        received_at_ms: 10_000 + sequence,
        transport: ObservationTransport::DirectNode,
        payload: ObservationIngressPayload::Observation {
            address: target(incarnation_id),
            observation: ObservationV1 {
                source_sequence: sequence,
                observed_at_unix_ms: Some(9_000 + sequence),
                evidence,
                kind,
                truncated: false,
            },
        },
    }
}

fn managed_envelope(
    incarnation_id: NodeIncarnationId,
    sequence: u64,
    kind: ObservationKindV1,
) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node(),
        cursor: NodeCursor {
            incarnation_id,
            sequence,
        },
        received_at_ms: 10_000 + sequence,
        transport: ObservationTransport::DirectNode,
        payload: ObservationIngressPayload::Observation {
            address: managed_target(incarnation_id),
            observation: ObservationV1 {
                source_sequence: sequence,
                observed_at_unix_ms: Some(9_000 + sequence),
                evidence: ObservationEvidenceV1::StructuredProvider,
                kind,
                truncated: false,
            },
        },
    }
}

fn transport_gap_envelope(
    incarnation_id: NodeIncarnationId,
    sequence: u64,
) -> ObservationIngressEnvelope {
    ObservationIngressEnvelope {
        node_id: node(),
        cursor: NodeCursor {
            incarnation_id,
            sequence,
        },
        received_at_ms: 10_000 + sequence,
        transport: ObservationTransport::DirectNode,
        payload: ObservationIngressPayload::ResyncRequired {
            oldest: NodeCursor {
                incarnation_id,
                sequence,
            },
        },
    }
}

fn accept(engine: &mut ObservationEngine, event: ObservationIngressEnvelope) -> ApplyOutcome {
    let prepared = engine.prepare(event).expect("prepare ingress");
    let outcome = prepared.outcomes()[0];
    engine.accept(prepared);
    outcome
}

fn accept_resync(
    engine: &mut ObservationEngine,
    incarnation_id: NodeIncarnationId,
    requested_after: u64,
    high_watermark: u64,
    gaps: Vec<ObservationGap>,
    events: Vec<ObservationIngressEnvelope>,
) {
    let oldest_available_sequence = gaps.first()
        .map(|gap| gap.last_sequence.saturating_add(1))
        .unwrap_or_else(|| requested_after.saturating_add(1));
    let prepared = engine
        .prepare_resync(&ObservationResyncBatch {
            node_id: node(),
            incarnation_id,
            requested_after,
            high_watermark: NodeCursor {
                incarnation_id,
                sequence: high_watermark,
            },
            oldest_available_sequence,
            records: Vec::new(),
            records_complete: false,
            gaps,
            events,
        })
        .expect("prepare observation resync");
    engine.accept(prepared);
}

fn accept_record_inventory(
    engine: &mut ObservationEngine,
    incarnation_id: NodeIncarnationId,
    records: &[ManagedRecordLink],
    complete: bool,
) {
    let prepared = engine
        .prepare_record_inventory(&node(), incarnation_id, records, complete)
        .expect("prepare managed record inventory");
    assert!(prepared.outcomes().is_empty());
    engine.accept(prepared);
}

#[test]
fn duplicate_direct_and_c2_observation_is_idempotent_by_exact_node_cursor() {
    let incarnation_id = incarnation(1);
    let direct = envelope(incarnation_id, 1, ObservationKindV1::Working);
    let mut c2 = direct.clone();
    c2.transport = ObservationTransport::C2;
    c2.received_at_ms = c2.received_at_ms.saturating_add(5_000);

    let mut engine = ObservationEngine::new();
    assert_eq!(accept(&mut engine, direct), ApplyOutcome::Applied);
    assert_eq!(accept(&mut engine, c2), ApplyOutcome::Duplicate);
    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.timeline.len(), 1);
}

#[test]
fn same_cursor_with_different_observation_fails_closed() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(incarnation_id, 1, ObservationKindV1::Working),
    );
    let before = engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .clone();

    let error = engine
        .prepare(envelope(incarnation_id, 1, ObservationKindV1::Ready))
        .expect_err("cursor collision");
    assert!(matches!(error, ObservationEngineError::CursorCollision { .. }));
    assert_eq!(
        engine.projection(&target(incarnation_id)).expect("projection"),
        &before
    );
}

#[test]
fn noncontiguous_visible_node_sequences_do_not_invent_gap() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            ObservationKindV1::ToolStarted {
                correlation_id: "tool-a".to_owned(),
                class: "command".to_owned(),
            },
        ),
    );
    accept(
        &mut engine,
        envelope(incarnation_id, 3, ObservationKindV1::Working),
    );

    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.availability, ProjectionAvailability::Current);
    assert_eq!(projection.freshness, ProjectionFreshness::Live);
    assert_eq!(projection.tools[0].state, CorrelationState::Pending);
}

#[test]
fn node_incarnation_change_freezes_old_session_without_rebinding() {
    let old_incarnation = incarnation(1);
    let new_incarnation = incarnation(2);
    let old_event = envelope(old_incarnation, 1, ObservationKindV1::Working);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, old_event.clone());
    accept(
        &mut engine,
        envelope(new_incarnation, 1, ObservationKindV1::Ready),
    );

    let old = engine.projection(&target(old_incarnation)).expect("old projection");
    assert_eq!(old.availability, ProjectionAvailability::Frozen);
    assert_eq!(old.freshness, ProjectionFreshness::ReplacedIncarnation);
    assert_eq!(accept(&mut engine, old_event), ApplyOutcome::Duplicate);
    assert!(matches!(
        engine.prepare(envelope(old_incarnation, 2, ObservationKindV1::Ready)),
        Err(ObservationEngineError::ReplacedIncarnation { .. })
    ));
    assert_eq!(
        engine
            .projection(&target(old_incarnation))
            .expect("old projection")
            .timeline
            .len(),
        1
    );
}

#[test]
fn explicit_gap_marks_pending_correlations_unknown() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            ObservationKindV1::ToolStarted {
                correlation_id: "tool-a".to_owned(),
                class: "command".to_owned(),
            },
        ),
    );
    accept(
        &mut engine,
        envelope(incarnation_id, 2, ObservationKindV1::Gap { missed: 4 }),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            3,
            ObservationKindV1::ToolCompleted {
                correlation_id: "tool-a".to_owned(),
                class: "command".to_owned(),
                success: true,
                duration_ms: Some(5),
            },
        ),
    );

    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.availability, ProjectionAvailability::Partial);
    assert_eq!(projection.tools[0].state, CorrelationState::UnknownAfterGap);
}

#[test]
fn source_reset_starts_new_revision_epoch() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            todo(9, "old", ObservationTodoStateV1::InProgress, false),
        ),
    );
    accept(
        &mut engine,
        envelope(incarnation_id, 2, ObservationKindV1::SourceReset),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            3,
            todo(1, "new", ObservationTodoStateV1::Pending, false),
        ),
    );

    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.todos.current.as_ref().expect("current").revision, 1);
    assert!(projection.todos.previous.is_none());
    assert!(!projection.todos.conflict);
    assert_eq!(projection.timeline.len(), 3);
}

#[test]
fn source_capabilities_are_latest_per_source_and_reset_with_their_evidence() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    for (sequence, tools, usage) in [(1, true, false), (2, true, true)] {
        accept(
            &mut engine,
            envelope(
                incarnation_id,
                sequence,
                ObservationKindV1::SourceCapabilities {
                    source_family: ObservationSourceFamilyV1::Pipe,
                    source_adapter: "codex".to_owned(),
                    capabilities: ObservationCapabilitiesV1 {
                        tools,
                        usage,
                        ..ObservationCapabilitiesV1::default()
                    },
                },
            ),
        );
    }
    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.source_capabilities.len(), 1);
    assert!(projection.source_capabilities[0].capabilities.usage);
    assert!(projection.timeline.is_empty(), "capability metadata is not activity");

    accept(
        &mut engine,
        envelope(incarnation_id, 3, ObservationKindV1::SourceReset),
    );
    assert!(engine
        .projection(&target(incarnation_id))
        .unwrap()
        .source_capabilities
        .is_empty());
}

#[test]
fn history_source_reset_clears_only_history_evidence() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            1,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::History,
                source_adapter: "native-history".to_owned(),
                capabilities: ObservationCapabilitiesV1 {
                    history_summary: true,
                    ..ObservationCapabilitiesV1::default()
                },
            },
        ),
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            2,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 8,
                message_count_exact: true,
                completed_turn_count: Some(4),
                total_tokens: Some(21),
            },
        ),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            3,
            ObservationKindV1::ToolStarted {
                correlation_id: "tool-live".to_owned(),
                class: "command".to_owned(),
            },
        ),
    );

    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            4,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::SourceReset,
        ),
    );

    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.history, None);
    assert_eq!(projection.tools.len(), 1);
    assert_eq!(projection.tools[0].correlation_id, "tool-live");
    assert!(projection.source_capabilities.is_empty());
}

#[test]
fn failed_history_refresh_preserves_last_known_snapshot() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            1,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 8,
                message_count_exact: true,
                completed_turn_count: Some(4),
                total_tokens: None,
            },
        ),
    );
    let last_known = engine
        .projection(&target(incarnation_id))
        .unwrap()
        .history
        .clone();

    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            2,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::Stale,
        ),
    );
    assert_eq!(
        engine.projection(&target(incarnation_id)).unwrap().history,
        last_known
    );

    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            3,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::Error {
                detail: "bounded-refresh-failure".to_owned(),
            },
        ),
    );
    let failed = engine.projection(&target(incarnation_id)).unwrap();
    assert_eq!(failed.history, last_known);
    assert_eq!(failed.freshness, ProjectionFreshness::Stale);

    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            4,
            ObservationEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 10,
                message_count_exact: true,
                completed_turn_count: Some(5),
                total_tokens: Some(34),
            },
        ),
    );
    let refreshed = engine.projection(&target(incarnation_id)).unwrap();
    assert_eq!(refreshed.history.as_ref().unwrap().message_count, 10);
    assert_eq!(refreshed.history.as_ref().unwrap().total_tokens, Some(34));
    assert_eq!(refreshed.freshness, ProjectionFreshness::Live);
}

#[test]
fn subagent_completion_preserves_unknown_provider_outcome() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            ObservationKindV1::SubagentCompleted {
                correlation_id: "sub-0123456789abcdef".to_owned(),
                success: None,
            },
        ),
    );
    assert_eq!(
        engine.projection(&target(incarnation_id)).unwrap().subagents[0].state,
        CorrelationState::OrphanCompletion { success: None },
    );
}

#[test]
fn source_reset_clears_only_its_source_gap_and_stale_state() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            1,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::Working,
        ),
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            2,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::Gap { missed: 2 },
        ),
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            3,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::SourceReset,
        ),
    );
    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.availability, ProjectionAvailability::Current);
    assert_eq!(projection.freshness, ProjectionFreshness::Live);

    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            4,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::Stale,
        ),
    );
    assert_eq!(
        engine.projection(&target(incarnation_id)).expect("projection").freshness,
        ProjectionFreshness::Stale
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            5,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::SourceReset,
        ),
    );
    assert_eq!(
        engine.projection(&target(incarnation_id)).expect("projection").freshness,
        ProjectionFreshness::Live
    );
}

#[test]
fn source_reset_does_not_hide_other_source_or_transport_gap() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            1,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::Working,
        ),
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            2,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::Gap { missed: 1 },
        ),
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            3,
            ObservationEvidenceV1::ManagedHook,
            ObservationKindV1::Gap { missed: 1 },
        ),
    );
    accept(
        &mut engine,
        envelope_with_evidence(
            incarnation_id,
            4,
            ObservationEvidenceV1::StructuredProvider,
            ObservationKindV1::SourceReset,
        ),
    );
    let projection = engine.projection(&target(incarnation_id)).expect("projection");
    assert_eq!(projection.availability, ProjectionAvailability::Partial);
    assert_eq!(projection.freshness, ProjectionFreshness::IncompleteAfterGap);

    let transport_incarnation = incarnation(2);
    let mut transport_engine = ObservationEngine::new();
    accept(
        &mut transport_engine,
        envelope(transport_incarnation, 1, ObservationKindV1::Working),
    );
    accept(
        &mut transport_engine,
        transport_gap_envelope(transport_incarnation, 2),
    );
    accept(
        &mut transport_engine,
        envelope(
            transport_incarnation,
            3,
            ObservationKindV1::SourceReset,
        ),
    );
    let projection = transport_engine
        .projection(&target(transport_incarnation))
        .expect("transport projection");
    assert_eq!(projection.availability, ProjectionAvailability::Partial);
    assert_eq!(projection.freshness, ProjectionFreshness::IncompleteAfterGap);
}

#[test]
fn complete_resync_clears_transport_gap_while_partial_resync_preserves_it() {
    let incarnation_id = incarnation(9);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, envelope(incarnation_id, 1, ObservationKindV1::Working));
    accept(&mut engine, transport_gap_envelope(incarnation_id, 4));
    assert!(engine
        .projection(&target(incarnation_id))
        .expect("gapped projection")
        .transport_incomplete);

    accept_resync(
        &mut engine,
        incarnation_id,
        1,
        4,
        vec![ObservationGap {
            first_sequence: 2,
            last_sequence: 2,
        }],
        vec![envelope(
            incarnation_id,
            3,
            ObservationKindV1::TurnCompleted,
        )],
    );
    assert!(engine
        .projection(&target(incarnation_id))
        .expect("partial projection")
        .transport_incomplete);

    accept_resync(&mut engine, incarnation_id, 1, 4, Vec::new(), Vec::new());
    let projection = engine
        .projection(&target(incarnation_id))
        .expect("recovered projection");
    assert!(!projection.transport_incomplete);
    assert_ne!(projection.freshness, ProjectionFreshness::IncompleteAfterGap);
}

#[test]
fn forged_replay_gap_proof_is_rejected_and_cannot_clear_transport_gap() {
    let incarnation_id = incarnation(16);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, envelope(incarnation_id, 1, ObservationKindV1::Working));
    accept(&mut engine, transport_gap_envelope(incarnation_id, 4));

    let forged = ObservationResyncBatch {
        node_id: node(),
        incarnation_id,
        requested_after: 1,
        high_watermark: NodeCursor {
            incarnation_id,
            sequence: 4,
        },
        oldest_available_sequence: 3,
        records: Vec::new(),
        records_complete: false,
        gaps: Vec::new(),
        events: vec![envelope(
            incarnation_id,
            3,
            ObservationKindV1::TurnCompleted,
        )],
    };
    assert_eq!(
        engine.prepare_resync(&forged).expect_err("forged gap proof must fail"),
        ObservationEngineError::Api(ObservationApiError::InvalidReplayGapProof)
    );
    assert!(engine
        .projection(&target(incarnation_id))
        .expect("gapped projection")
        .transport_incomplete);

    accept_resync(&mut engine, incarnation_id, 1, 4, Vec::new(), Vec::new());
    assert!(!engine
        .projection(&target(incarnation_id))
        .expect("authoritatively recovered projection")
        .transport_incomplete);
}

#[test]
fn managed_record_inventory_does_not_clear_transport_gap() {
    let incarnation_id = incarnation(10);
    let link = managed_link(incarnation_id, Some(0));
    let mut engine = ObservationEngine::new();
    accept_record_inventory(&mut engine, incarnation_id, &[link.clone()], true);
    accept(
        &mut engine,
        managed_envelope(incarnation_id, 1, ObservationKindV1::Working),
    );
    accept(&mut engine, transport_gap_envelope(incarnation_id, 2));

    let before = engine
        .projection(&managed_target(incarnation_id))
        .expect("gapped managed projection");
    assert!(before.transport_incomplete);
    assert_eq!(before.availability, ProjectionAvailability::Partial);

    accept_record_inventory(&mut engine, incarnation_id, &[link], true);
    let after = engine
        .projection(&managed_target(incarnation_id))
        .expect("inventory-managed projection");
    assert!(after.transport_incomplete);
    assert_eq!(after.availability, ProjectionAvailability::Partial);
    assert_eq!(after.freshness, ProjectionFreshness::IncompleteAfterGap);
}

#[test]
fn gap_before_first_projection_is_retained_and_inventory_never_clears_it() {
    let incarnation_id = incarnation(15);
    let link = managed_link(incarnation_id, None);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, transport_gap_envelope(incarnation_id, 1));

    accept_record_inventory(&mut engine, incarnation_id, &[link.clone()], true);
    let inventory_projection = engine
        .projection(&managed_target(incarnation_id))
        .expect("inventory creates managed projection");
    assert!(inventory_projection.transport_incomplete);
    assert_eq!(inventory_projection.availability, ProjectionAvailability::Partial);

    accept(
        &mut engine,
        envelope(incarnation_id, 2, ObservationKindV1::Working),
    );
    let runtime_projection = engine
        .projection(&target(incarnation_id))
        .expect("ordinary observation creates runtime projection");
    assert!(runtime_projection.transport_incomplete);
    assert_eq!(runtime_projection.freshness, ProjectionFreshness::IncompleteAfterGap);

    accept_record_inventory(&mut engine, incarnation_id, &[link], true);
    assert!(engine
        .projection(&managed_target(incarnation_id))
        .expect("managed projection")
        .transport_incomplete);
    assert!(engine
        .projection(&target(incarnation_id))
        .expect("runtime projection")
        .transport_incomplete);
}

#[test]
fn managed_inline_projection_survives_runtime_exit_and_unlink() {
    let incarnation_id = incarnation(11);
    let mut engine = ObservationEngine::new();
    accept_record_inventory(
        &mut engine,
        incarnation_id,
        &[managed_link(incarnation_id, Some(0))],
        true,
    );
    accept(
        &mut engine,
        managed_envelope(
            incarnation_id,
            1,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::OneShot,
                source_adapter: "inline-session".to_owned(),
                capabilities: ObservationCapabilitiesV1 {
                    tools: true,
                    usage: true,
                    ..ObservationCapabilitiesV1::default()
                },
            },
        ),
    );
    accept(
        &mut engine,
        managed_envelope(
            incarnation_id,
            2,
            todo(1, "provider fact", ObservationTodoStateV1::InProgress, false),
        ),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            3,
            ObservationKindV1::Exited {
                success: Some(true),
            },
        ),
    );

    accept_record_inventory(
        &mut engine,
        incarnation_id,
        &[managed_link(incarnation_id, None)],
        true,
    );
    let unlinked = engine
        .projection(&managed_target(incarnation_id))
        .expect("unlinked managed projection")
        .clone();
    assert_eq!(unlinked.source_capabilities.len(), 1);
    assert_eq!(unlinked.todos.current.as_ref().unwrap().revision, 1);
    assert_eq!(unlinked.availability, ProjectionAvailability::Current);
    assert_eq!(unlinked.freshness, ProjectionFreshness::LastKnown);
    assert!(engine.managed_runtime(&managed_key(incarnation_id)).is_none());

    accept_record_inventory(
        &mut engine,
        incarnation_id,
        &[managed_link(incarnation_id, Some(1))],
        true,
    );
    let restarted = engine
        .projection(&managed_target(incarnation_id))
        .expect("restarted managed projection");
    assert_eq!(restarted.source_capabilities, unlinked.source_capabilities);
    assert_eq!(restarted.todos, unlinked.todos);
    assert_eq!(restarted.freshness, ProjectionFreshness::LastKnown);
    assert_eq!(
        engine
            .managed_runtime(&managed_key(incarnation_id))
            .expect("relinked runtime")
            .generation,
        SessionGeneration(1)
    );
}

#[test]
fn managed_record_inventory_rejects_cross_node_or_incarnation() {
    let first_incarnation = incarnation(12);
    let second_incarnation = incarnation(13);
    let other_node = NodeId::new("node-b").expect("other node ID");
    let mut engine = ObservationEngine::new();

    let wrong_node = ManagedRecordLink {
        managed: ManagedSessionKey {
            node_id: other_node,
            incarnation_id: first_incarnation,
            record_id: SessionRecordId::new("record-inline-b").expect("record ID"),
        },
        runtime: None,
    };
    assert_eq!(
        engine
            .prepare_record_inventory(&node(), first_incarnation, &[wrong_node], true)
            .expect_err("cross-node inventory must fail"),
        ObservationEngineError::Api(ObservationApiError::RouteNodeMismatch)
    );

    let wrong_managed_incarnation = managed_link(second_incarnation, None);
    assert_eq!(
        engine
            .prepare_record_inventory(
                &node(),
                first_incarnation,
                &[wrong_managed_incarnation],
                true,
            )
            .expect_err("cross-incarnation managed record must fail"),
        ObservationEngineError::Api(ObservationApiError::RouteIncarnationMismatch)
    );
    let wrong_runtime_incarnation = ManagedRecordLink {
        managed: managed_key(first_incarnation),
        runtime: Some(runtime(second_incarnation)),
    };
    assert_eq!(
        engine
            .prepare_record_inventory(
                &node(),
                first_incarnation,
                &[wrong_runtime_incarnation],
                true,
            )
            .expect_err("cross-incarnation runtime link must fail"),
        ObservationEngineError::Api(ObservationApiError::RouteIncarnationMismatch)
    );

    accept_record_inventory(
        &mut engine,
        first_incarnation,
        &[managed_link(first_incarnation, Some(0))],
        true,
    );
    accept(
        &mut engine,
        managed_envelope(first_incarnation, 1, ObservationKindV1::Working),
    );
    accept_record_inventory(
        &mut engine,
        second_incarnation,
        &[managed_link(second_incarnation, None)],
        true,
    );
    accept(
        &mut engine,
        managed_envelope(second_incarnation, 1, ObservationKindV1::Working),
    );
    assert!(matches!(
        engine.prepare_record_inventory(
            &node(),
            first_incarnation,
            &[managed_link(first_incarnation, None)],
            true,
        ),
        Err(ObservationEngineError::ReplacedIncarnation { incarnation_id, .. })
            if incarnation_id == first_incarnation
    ));
    let frozen = engine
        .projection(&managed_target(first_incarnation))
        .expect("old managed projection");
    assert_eq!(frozen.availability, ProjectionAvailability::Frozen);
    assert_eq!(
        frozen.freshness,
        ProjectionFreshness::ReplacedIncarnation
    );
    let current = engine
        .projection(&managed_target(second_incarnation))
        .expect("new managed projection");
    assert_eq!(current.availability, ProjectionAvailability::Current);
    assert_eq!(current.freshness, ProjectionFreshness::Live);
    assert!(engine
        .managed_runtime(&managed_key(first_incarnation))
        .is_none());
}

#[test]
fn managed_record_inventory_retains_source_facts() {
    let incarnation_id = incarnation(14);
    let link = managed_link(incarnation_id, Some(0));
    let mut engine = ObservationEngine::new();
    accept_record_inventory(&mut engine, incarnation_id, &[link.clone()], true);
    accept(
        &mut engine,
        managed_envelope(
            incarnation_id,
            1,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::ManagedHook,
                source_adapter: "managed-inline-hook".to_owned(),
                capabilities: ObservationCapabilitiesV1 {
                    tools: true,
                    todo: true,
                    file_changes: true,
                    ..ObservationCapabilitiesV1::default()
                },
            },
        ),
    );
    accept(
        &mut engine,
        managed_envelope(
            incarnation_id,
            2,
            ObservationKindV1::FileChanged {
                path: Some("src/lib.rs".to_owned()),
            },
        ),
    );

    let observed = engine
        .projection(&managed_target(incarnation_id))
        .expect("observed managed projection")
        .clone();
    accept_record_inventory(&mut engine, incarnation_id, &[], false);
    let partial_inventory = engine
        .projection(&managed_target(incarnation_id))
        .expect("partial inventory managed projection");
    assert_eq!(partial_inventory.availability, ProjectionAvailability::Current);
    assert_eq!(partial_inventory.source_capabilities, observed.source_capabilities);
    assert_eq!(partial_inventory.files, observed.files);

    accept_record_inventory(&mut engine, incarnation_id, &[], true);
    let absent = engine
        .projection(&managed_target(incarnation_id))
        .expect("absent managed projection");
    assert_eq!(absent.availability, ProjectionAvailability::Frozen);
    assert_eq!(absent.freshness, ProjectionFreshness::Unavailable);
    assert_eq!(absent.source_capabilities, observed.source_capabilities);
    assert_eq!(absent.files, observed.files);
    assert_eq!(absent.timeline, observed.timeline);

    accept_record_inventory(
        &mut engine,
        incarnation_id,
        &[ManagedRecordLink {
            managed: managed_key(incarnation_id),
            runtime: None,
        }],
        true,
    );
    let restored = engine
        .projection(&managed_target(incarnation_id))
        .expect("restored managed projection");
    assert_eq!(restored.availability, ProjectionAvailability::Current);
    assert_eq!(restored.freshness, ProjectionFreshness::LastKnown);
    assert_eq!(restored.source_capabilities, observed.source_capabilities);
    assert_eq!(restored.files, observed.files);
    assert_eq!(restored.timeline, observed.timeline);
}

#[test]
fn todo_revisions_preserve_current_and_previous_without_inferred_completion() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            todo(1, "first", ObservationTodoStateV1::InProgress, false),
        ),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            2,
            todo(2, "second", ObservationTodoStateV1::Pending, false),
        ),
    );

    let todos = &engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .todos;
    let current = todos.current.as_ref().expect("current");
    let previous = todos.previous.as_ref().expect("previous");
    assert_eq!(current.revision, 2);
    assert_eq!(current.items[0].state, ObservationTodoStateV1::Pending);
    assert!(!current.complete);
    assert_eq!(previous.revision, 1);
    assert_eq!(previous.items[0].state, ObservationTodoStateV1::InProgress);
    assert!(!previous.complete);
}

#[test]
fn orphan_tool_completion_remains_orphan() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            ObservationKindV1::ToolCompleted {
                correlation_id: "tool-a".to_owned(),
                class: "command".to_owned(),
                success: false,
                duration_ms: None,
            },
        ),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            2,
            ObservationKindV1::ToolStarted {
                correlation_id: "tool-a".to_owned(),
                class: "command".to_owned(),
            },
        ),
    );

    assert_eq!(
        engine
            .projection(&target(incarnation_id))
            .expect("projection")
            .tools[0]
            .state,
        CorrelationState::OrphanCompletion {
            success: Some(false)
        }
    );
}

#[test]
fn cumulative_usage_regression_does_not_invent_delta() {
    let incarnation_id = incarnation(1);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, envelope(incarnation_id, 1, usage(100, 50)));
    accept(&mut engine, envelope(incarnation_id, 2, usage(80, 40)));
    assert_eq!(
        engine
            .projection(&target(incarnation_id))
            .expect("projection")
            .usage
            .observed_delta,
        UsageTotals::default()
    );

    accept(&mut engine, envelope(incarnation_id, 3, usage(90, 45)));
    assert_eq!(
        engine
            .projection(&target(incarnation_id))
            .expect("projection")
            .usage
            .observed_delta,
        UsageTotals {
            input_tokens: 10,
            output_tokens: 5,
            ..UsageTotals::default()
        }
    );
}

#[test]
fn context_occupancy_cumulative_replaces_every_counter_and_excludes_reasoning() {
    let incarnation_id = incarnation(17);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            usage_snapshot(10, 4, 20, 3, 90, Some(100), false),
        ),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            2,
            usage_snapshot(5, 6, 7, 8, 999, Some(200), true),
        ),
    );

    let occupancy = engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .usage
        .context_occupancy
        .expect("context occupancy");
    assert_eq!(occupancy.uncached_input_tokens, 5);
    assert_eq!(occupancy.output_tokens, 6);
    assert_eq!(occupancy.cache_read_tokens, 7);
    assert_eq!(occupancy.cache_write_tokens, 8);
    assert_eq!(occupancy.reasoning_tokens, Some(999));
    assert_eq!(occupancy.context_window, Some(200));
    assert_eq!(occupancy.provenance, ContextOccupancyProvenance::CumulativeUsage);
    assert_eq!(occupancy.occupied_tokens(), 26);
}

#[test]
fn context_occupancy_per_turn_replaces_prompt_and_reasoning_but_accumulates_output() {
    let incarnation_id = incarnation(18);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            usage_snapshot(10, 4, 20, 3, 2, Some(100), false),
        ),
    );
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            2,
            usage_snapshot(5, 6, 7, 8, 9, None, false),
        ),
    );

    let usage_projection = &engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .usage;
    let occupancy = usage_projection.context_occupancy.expect("context occupancy");
    assert_eq!(occupancy.uncached_input_tokens, 5);
    assert_eq!(occupancy.output_tokens, 10);
    assert_eq!(occupancy.cache_read_tokens, 7);
    assert_eq!(occupancy.cache_write_tokens, 8);
    assert_eq!(occupancy.reasoning_tokens, Some(9));
    assert_eq!(occupancy.context_window, Some(100));
    assert_eq!(
        occupancy.provenance,
        ContextOccupancyProvenance::PerTurnUsageSynthesis,
    );
    assert_eq!(occupancy.occupied_tokens(), 30);
    assert_eq!(usage_projection.observed_delta, UsageTotals {
        input_tokens: 15,
        output_tokens: 10,
        cache_read_tokens: 27,
        cache_write_tokens: 11,
        reasoning_tokens: 11,
    });
}

#[test]
fn context_occupancy_same_source_reset_clears_snapshot() {
    let incarnation_id = incarnation(19);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, envelope(incarnation_id, 1, exact_context_usage()));
    accept(
        &mut engine,
        envelope(incarnation_id, 2, ObservationKindV1::SourceReset),
    );

    let usage_projection = &engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .usage;
    assert!(usage_projection.context_occupancy.is_none());
    assert!(usage_projection.last_cumulative.is_none());
}

#[test]
fn context_occupancy_survives_gap_but_projection_is_not_live() {
    let incarnation_id = incarnation(20);
    let mut engine = ObservationEngine::new();
    accept(&mut engine, envelope(incarnation_id, 1, exact_context_usage()));
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            2,
            ObservationKindV1::Gap { missed: 1 },
        ),
    );

    let projection = engine
        .projection(&target(incarnation_id))
        .expect("projection");
    assert!(projection.usage.context_occupancy.is_some());
    assert_eq!(projection.availability, ProjectionAvailability::Partial);
    assert_eq!(projection.freshness, ProjectionFreshness::IncompleteAfterGap);
}

#[test]
fn exact_context_window_usage_sets_authoritative_snapshot_without_changing_observed_delta() {
    let incarnation_id = incarnation(21);
    let mut engine = ObservationEngine::new();
    accept(
        &mut engine,
        envelope(
            incarnation_id,
            1,
            usage_snapshot(1, 2, 3, 4, 5, Some(100), false),
        ),
    );
    let observed_before = engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .usage
        .observed_delta;
    accept(
        &mut engine,
        envelope(incarnation_id, 2, exact_context_usage()),
    );

    let usage_projection = &engine
        .projection(&target(incarnation_id))
        .expect("projection")
        .usage;
    let occupancy = usage_projection.context_occupancy.expect("exact occupancy");
    assert_eq!(usage_projection.observed_delta, observed_before);
    assert_eq!(occupancy.provenance, ContextOccupancyProvenance::ExactCurrentWindow);
    assert_eq!(occupancy.uncached_input_tokens, 10);
    assert_eq!(occupancy.cache_read_tokens, 20);
    assert_eq!(occupancy.cache_write_tokens, 5);
    assert_eq!(occupancy.output_tokens, 15);
    assert_eq!(occupancy.reasoning_tokens, None);
    assert_eq!(occupancy.unattributed_tokens, 10);
    assert_eq!(occupancy.occupied_tokens(), 60);
    assert_eq!(occupancy.context_window, Some(100));
}

#[test]
fn exact_context_window_usage_rejects_or_clears_every_non_structured_evidence() {
    let incarnation_id = incarnation(22);
    for evidence in [
        ObservationEvidenceV1::ManagedHook,
        ObservationEvidenceV1::NodeLifecycle,
        ObservationEvidenceV1::WorkspaceObservation,
        ObservationEvidenceV1::HistoryProjection,
        ObservationEvidenceV1::PtyHint,
    ] {
        let engine = ObservationEngine::new();
        let event = envelope_with_evidence(
            incarnation_id,
            1,
            evidence,
            exact_context_usage(),
        );
        assert!(engine.prepare(event).is_err(), "{evidence:?}");
        assert!(engine.projection(&target(incarnation_id)).is_none());
    }
}

#[test]
fn context_occupancy_is_serde_default_for_older_usage_projection() {
    let projection: gate4agent_observation_engine::UsageProjection = serde_json::from_value(
        serde_json::json!({
            "observed_delta": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_tokens": 0,
                "cache_write_tokens": 0,
                "reasoning_tokens": 0
            },
            "last_cumulative": null,
            "cumulative_evidence": null,
            "context_window": null
        }),
    )
    .expect("older usage projection");
    assert!(projection.context_occupancy.is_none());
}

fn todo(
    revision: u64,
    text: &str,
    state: ObservationTodoStateV1,
    complete: bool,
) -> ObservationKindV1 {
    ObservationKindV1::TodoSnapshot {
        revision,
        items: vec![ObservationTodoItemV1 {
            id: Some(format!("todo-{revision}")),
            text: text.to_owned(),
            state,
        }],
        complete,
    }
}

fn usage(input_tokens: u64, output_tokens: u64) -> ObservationKindV1 {
    usage_snapshot(
        input_tokens,
        output_tokens,
        0,
        0,
        0,
        Some(200_000),
        true,
    )
}

fn usage_snapshot(
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    context_window: Option<u64>,
    is_cumulative: bool,
) -> ObservationKindV1 {
    ObservationKindV1::Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        context_window,
        is_cumulative,
    }
}

fn exact_context_usage() -> ObservationKindV1 {
    ObservationKindV1::ContextWindowUsage {
        uncached_input_tokens: 10,
        cache_read_tokens: 20,
        cache_write_tokens: 5,
        output_tokens: 15,
        unattributed_tokens: 10,
        used_tokens: 60,
        capacity_tokens: 100,
    }
}
