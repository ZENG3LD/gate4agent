use gate4agent_c2_client::{
    connect_local, C2ControlError, C2ControlHandle, C2EventReceiver, C2PendingRequest,
};
use gate4agent_c2_protocol::{
    C2ManagedSessionRecord, C2NodeEventEnvelope, C2NodeResponse, C2NodeSnapshot,
    C2ObservationSupport, C2Topology, NodeRoute, NodeTransportState, RoutedNodeEvent,
};
use gate4agent_harness_delivery::CompiledDeliveryBundleV2;
use gate4agent_harness_api::{
    HarnessNativeSessionCatalogEntryV1, HarnessNativeSessionCatalogPageV1,
    HarnessNativeSessionCatalogScopeV1, HarnessNativeSessionCatalogSummaryV1,
    HarnessNativeSessionCatalogWindowV1, HarnessNativeSessionExternalGroupKindV1,
    HarnessNativeSessionExternalGroupV1, HarnessNativeSessionPreviewMessageV1,
    HarnessNativeSessionPreviewRoleV1, HarnessNativeSessionPreviewV1,
    HarnessNativeSessionPreviewedV1, HarnessNativeSessionRouteV1,
    HarnessNativeSessionSelectionV1, HarnessNativeSessionsCatalogedV1,
    HarnessNativeSessionsPagedV1, HarnessOperatorRequestV1, HarnessOperatorResponseV1,
};
use gate4agent_node_protocol::{
    DeliveryBlobChunkHexV1, DeliveryBlobDigestV1, DeliveryBundleManifestV2,
    DeliveryCommitReceiptV1, DeliveryStageId, HarnessMcpActivationDigest,
    HarnessMcpCallId, HarnessMcpRejectReasonV1, HarnessMcpReplyChunkHexV1,
    HarnessMcpReservationId, NodeFailureCode, NodeId, NodeRequest,
    ResolvedBundleReceipt, ResolvedContextPackReceipt, ResolvedHarnessMcpProxyReceiptV1,
    ResolvedSpawnReceipt, ResolvedSpawnSpec,
    SessionAddress, SessionMode,
    SessionRecordId, SpawnFieldProvenance, SpawnOverride, SpawnProfileRevision,
    SpawnProfileId,
    SpawnPromptMetadata, SpawnResolutionProvenance, SpawnSpec, WorkspaceId,
    NativeSessionCatalogRoute, NativeSessionSelection,
    MAX_DELIVERY_CHUNK_RAW_BYTES,
};
use gate4agent_harness_protocol::{
    HarnessContinuationRef, HarnessContinuationStateV1, HarnessIdempotencyRef,
    HarnessOperationId, HarnessRequestDigest, HarnessRunId, HarnessSelectorV1,
};
use gate4agent_node_wire::local_hmac_sha256;
use gate4agent_types::AgentId;
use thiserror::Error;
use std::{sync::Arc, time::{Duration, Instant}};

const NATIVE_HISTORY_TIMEOUT_FLOOR: Duration = Duration::from_secs(34);

#[derive(Clone)]
pub struct HarnessC2Adapter {
    control: C2ControlHandle,
}

/// The sole authenticated C2 event stream owned by the harness connection.
///
/// It is returned to the caller instead of being internally drained so the
/// observation service can consume every routed event without a black hole.
pub struct HarnessC2EventReceiver {
    inner: C2EventReceiver,
}

/// Sealed topology watch for observation route health.
pub struct HarnessC2TopologyReceiver {
    inner: tokio::sync::watch::Receiver<Arc<C2Topology>>,
}

impl HarnessC2TopologyReceiver {
    pub fn current(&self) -> Vec<HarnessObservationRoute> {
        observation_topology(&self.inner.borrow())
    }

    pub async fn changed(&mut self) -> Result<Vec<HarnessObservationRoute>, HarnessC2Error> {
        self.inner.changed().await.map_err(|_| HarnessC2Error::TopologyClosed)?;
        Ok(self.current())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessObservationRoute {
    route: NodeRoute,
    support: Option<C2ObservationSupport>,
}

impl HarnessObservationRoute {
    pub fn route(&self) -> &NodeRoute { &self.route }

    pub fn support(&self) -> Option<C2ObservationSupport> { self.support }
}

impl HarnessC2EventReceiver {
    pub async fn recv(&mut self) -> Option<RoutedNodeEvent> {
        self.inner.recv().await
    }
}

impl HarnessC2Adapter {
    /// Connects the harness as the authenticated C2 operator.
    ///
    /// The returned adapter deliberately exposes no generic request handle. Its
    /// command surface is restricted to inventory reads, typed ContextPack
    /// export, and SpawnSpec dispatch.
    /// Candidate inventory is deliberately not a reconciliation authority.
    pub async fn connect(
        endpoint: &str,
        token: &str,
    ) -> Result<(Self, HarnessC2EventReceiver), HarnessC2Error> {
        let (control, events) = connect_local(endpoint, token)
            .await
            .map_err(HarnessC2Error::Connect)?;
        Ok((Self { control }, HarnessC2EventReceiver { inner: events }))
    }

    pub fn exact_route(&self, node_id: &NodeId) -> Result<NodeRoute, HarnessC2Error> {
        let topology = self.control.current_topology();
        let node = topology
            .nodes
            .iter()
            .find(|node| &node.node_id == node_id)
            .ok_or_else(|| HarnessC2Error::UnknownNode(node_id.clone()))?;
        if node.transport != NodeTransportState::Online {
            return Err(HarnessC2Error::NodeOffline(node_id.clone()));
        }
        let expected_incarnation_id = node
            .current_incarnation_id
            .ok_or_else(|| HarnessC2Error::MissingIncarnation(node_id.clone()))?;
        Ok(NodeRoute {
            node_id: node_id.clone(),
            expected_incarnation_id,
        })
    }

    pub(crate) fn validate_current_staged_delivery_proof(
        &self,
        proof: &StagedDeliveryProof,
    ) -> Result<(), HarnessC2Error> {
        let current = self.exact_route(&proof.route.node_id)?;
        if current != proof.route {
            return Err(HarnessC2Error::DeliveryRouteMismatch);
        }
        Ok(())
    }

    pub(crate) fn start_prepared_continuation_spawn(
        &self,
        prepared: PreparedContinuationSpawnDispatch,
        profile: SpawnProfileRevisionProof,
    ) -> Result<PendingContinuationSpawnDispatch, HarnessC2Error> {
        self.start_prepared_spawn(prepared.inner, profile)
            .map(|inner| PendingContinuationSpawnDispatch { inner })
    }

    pub(crate) fn start_prepared_spawn(
        &self,
        prepared: PreparedSpawnDispatch,
        profile: SpawnProfileRevisionProof,
    ) -> Result<PendingSpawnDispatch, HarnessC2Error> {
        validate_prepared_spawn_profile(&prepared, &profile)?;
        validate_prepared_spawn(&prepared)?;
        self.ensure_current_incarnation(&prepared.route)?;
        let PreparedSpawnDispatch {
            route,
            operation_id,
            idempotency_ref,
            spec,
            fingerprint,
            expected_bundle,
            expected_context,
            harness_mcp,
        } = prepared;
        let mut cleared_spec = spec.clone();
        cleared_spec.overrides.bundle_id = SpawnOverride::Clear;
        cleared_spec.overrides.context_id = SpawnOverride::Clear;
        let request = match &harness_mcp {
            Some(mcp) => NodeRequest::SpawnSpecWithHarnessMcp {
                reservation_id: mcp.reservation_id.clone(),
                activation_digest: mcp.activation_digest.clone(),
                spec,
                deadline_unix_ms: mcp.deadline_unix_ms,
            },
            None => NodeRequest::SpawnSpec { spec },
        };
        let pending = self.control.start_request(route.clone(), request)
            .map_err(HarnessC2Error::SpawnEnqueue)?;
        Ok(PendingSpawnDispatch {
            correlation: SpawnResponseCorrelation {
                route,
                operation_id,
                idempotency_ref,
                cleared_spec,
                profile_revision: profile.profile_revision,
                fingerprint,
                expected_bundle,
                expected_context,
                expected_proxy: harness_mcp.map(|mcp| (
                    mcp.reservation_id,
                    mcp.activation_digest,
                    mcp.proxy_receipt,
                )),
            },
            pending: Some(pending),
        })
    }

    pub(crate) async fn preflight_spawn_profile(
        &self,
        route: &NodeRoute,
        profile_id: &SpawnProfileId,
    ) -> Result<SpawnProfileRevisionProof, HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        let snapshot = self.snapshot(route).await?;
        let profile_revision = snapshot.launch_inventory.as_ref()
            .and_then(|inventory| inventory.spawn_profiles.as_ref())
            .and_then(|profiles| profiles.iter().find(|profile| &profile.id == profile_id))
            .map(|profile| profile.revision.clone())
            .ok_or_else(|| HarnessC2Error::SpawnProfileUnavailable(profile_id.clone()))?;
        Ok(SpawnProfileRevisionProof {
            route: route.clone(),
            profile_id: profile_id.clone(),
            profile_revision,
        })
    }

    /// Synchronously enqueues the single request authorized by durable
    /// Exporting state. A restored Exporting record cannot create this proof.
    pub(crate) fn start_context_pack_export(
        &self,
        prepared: crate::PreparedContinuationExport,
    ) -> Result<ContextPackExportStart, HarnessC2Error> {
        let authority = prepared.continuation().clone();
        if authority.state != HarnessContinuationStateV1::Exporting {
            return Err(HarnessC2Error::ContinuationAuthorityMismatch);
        }
        let route = NodeRoute {
            node_id: NodeId::new(authority.node_id.as_str())
                .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
            expected_incarnation_id: authority.node_incarnation.as_str().parse()
                .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
        };
        if let Err(error) = self.ensure_current_incarnation(&route) {
            return Ok(ContextPackExportStart::NotEnqueued(
                ExportContextPackOutcome::ExpiredBeforeSend {
                    prepared,
                    reason: context_export_expiry_reason(&error),
                },
            ));
        }
        let source_session = harness_binding_session(&authority.source_binding)?;
        let source_provider = AgentId::new(authority.source_provider.as_str())
            .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?;
        let pending = match self.control.start_request(
            route.clone(),
            NodeRequest::ExportContextPack { session: source_session.clone() },
        ) {
            Ok(pending) => pending,
            Err(_) => return Ok(ContextPackExportStart::NotEnqueued(
                ExportContextPackOutcome::ExpiredBeforeSend {
                    prepared,
                    reason: ContextExportExpiryReason::QueueUnavailable,
                },
            )),
        };
        Ok(ContextPackExportStart::Enqueued(PendingContextPackExport {
            prepared,
            authority,
            route,
            source_session,
            source_provider,
            pending: Some(pending),
        }))
    }

    /// Compatibility wrapper. Production actor code must call the synchronous
    /// start method in the same turn that commits the durable lease.
    pub(crate) async fn export_context_pack(
        &self,
        prepared: crate::PreparedContinuationExport,
    ) -> Result<ExportContextPackOutcome, HarnessC2Error> {
        match self.start_context_pack_export(prepared)? {
            ContextPackExportStart::Enqueued(pending) => pending.finish().await,
            ContextPackExportStart::NotEnqueued(outcome) => Ok(outcome),
        }
    }

    /// Returns the exact current incarnations for every online Node.
    ///
    /// Unsupported observation routes are included so the observation host can
    /// persist an explicit unsupported state instead of silently omitting them.
    pub fn observation_routes(&self) -> Vec<NodeRoute> {
        observation_topology(&self.control.current_topology())
            .into_iter()
            .map(|route| route.route)
            .collect()
    }

    pub fn topology_receiver(&self) -> HarnessC2TopologyReceiver {
        HarnessC2TopologyReceiver { inner: self.control.subscribe_topology() }
    }

    pub(crate) fn start_arm_harness_mcp_reservation(
        &self,
        service: &crate::HarnessService,
        route: &NodeRoute,
        prepared: crate::PreparedHarnessMcpReservation,
        spawn_spec: &SpawnSpec,
    ) -> Result<PendingHarnessMcpArm, HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        let durable = service.harness_mcp_reservation(prepared.reservation_id())
            .ok_or(HarnessC2Error::HarnessMcpAuthorityMismatch)?;
        if durable != &prepared.reservation
            || durable.state != crate::HarnessMcpReservationStateV1::Prepared
            || durable.node_id.as_str() != route.node_id.as_str()
            || durable.node_incarnation_id.as_str()
                != route.expected_incarnation_id.to_string()
            || spawn_spec_fingerprint(spawn_spec)? != durable.spawn_spec_fingerprint
        {
            return Err(HarnessC2Error::HarnessMcpAuthorityMismatch);
        }
        let reservation = durable.clone();
        let pending = self.control.start_request(route.clone(), NodeRequest::ArmHarnessMcpReservation {
            reservation_id: durable.reservation_id.clone(),
            activation_digest: durable.activation_digest.clone(),
            spawn_spec: spawn_spec.clone(),
            expires_at_unix_ms: durable.expires_at_unix_ms,
        }).map_err(HarnessC2Error::HarnessMcpArmEnqueue)?;
        Ok(PendingHarnessMcpArm {
            route: route.clone(),
            reservation,
            pending: Some(pending),
        })
    }

    pub(crate) fn start_native_history_request(
        &self,
        request: HarnessOperatorRequestV1,
    ) -> Result<PendingNativeHistoryRequest, HarnessC2Error> {
        let (route, wire_request) = native_history_wire_request(&request)?;
        self.ensure_current_incarnation(&route)?;
        let pending = self.control.start_request(route.clone(), wire_request)
            .map_err(HarnessC2Error::NativeHistoryEnqueue)?;
        Ok(PendingNativeHistoryRequest {
            route,
            request,
            started_at: Instant::now(),
            pending: Some(pending),
        })
    }

    /// Reads the authoritative Node replay floor, high watermark, exact
    /// inventory snapshot, and observation envelopes after a durable cursor.
    ///
    /// This issues only `NodeRequest::Resync`; it does not reduce or persist any
    /// observation. Sequence holes above the replay floor are allowed because
    /// the Node sequence is global across observation and non-observation events.
    pub async fn observation_resync(
        &self,
        route: &NodeRoute,
        after_sequence: u64,
    ) -> Result<HarnessObservationResync, HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        let routed = self.control
            .request(route.clone(), NodeRequest::Resync { after_sequence })
            .await
            .map_err(HarnessC2Error::ObservationTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::IncarnationChanged { node_id: route.node_id.clone() });
        }
        match routed.response {
            Ok(C2NodeResponse::Resync {
                event_sequence,
                oldest_available_sequence,
                snapshot,
                events,
            }) => build_observation_resync(
                route,
                after_sequence,
                event_sequence,
                oldest_available_sequence,
                snapshot,
                events,
            ),
            Err(failure) => Err(HarnessC2Error::ObservationRejected { code: failure.code }),
            Ok(_) => Err(HarnessC2Error::UnexpectedObservationResponse),
        }
    }

    pub async fn snapshot(&self, route: &NodeRoute) -> Result<C2NodeSnapshot, HarnessC2Error> {
        let routed = self
            .control
            .request(route.clone(), gate4agent_node_protocol::NodeRequest::Snapshot)
            .await
            .map_err(HarnessC2Error::InventoryTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::IncarnationChanged {
                node_id: route.node_id.clone(),
            });
        }
        match routed.response {
            Ok(C2NodeResponse::Snapshot { snapshot, .. }) => Ok(snapshot),
            Err(failure) => Err(HarnessC2Error::InventoryRejected {
                code: failure.code,
            }),
            Ok(_) => Err(HarnessC2Error::UnexpectedInventoryResponse),
        }
    }

    pub async fn spawn_baseline(
        &self,
        route: &NodeRoute,
    ) -> Result<Vec<SessionRecordId>, HarnessC2Error> {
        let mut record_ids = self
            .snapshot(route)
            .await?
            .session_records
            .into_iter()
            .map(|record| record.record_id)
            .collect::<Vec<_>>();
        record_ids.sort();
        record_ids.dedup();
        Ok(record_ids)
    }

    /// Stages one reviewed compiled bundle. Each mutation is sent at most once;
    /// an uncertain result stops the sequence without replay or implicit abort.
    pub(crate) async fn stage_compiled_delivery(
        &self,
        lease: PreparedDeliveryStageLease,
    ) -> Result<StagedDeliveryProof, HarnessC2Error> {
        let PreparedDeliveryStageLease {
            route,
            operation_id,
            run_id,
            workspace_id,
            selector,
            compiled,
        } = lease;
        operation_id.validate().map_err(HarnessC2Error::OperationIdentity)?;
        run_id.validate().map_err(HarnessC2Error::OperationIdentity)?;
        selector.validate().map_err(HarnessC2Error::OperationIdentity)?;
        compiled.verify().map_err(|error| {
            HarnessC2Error::InvalidCompiledDelivery(error.to_string())
        })?;
        let stage = self.begin_delivery_stage(
            &route,
            operation_id,
            run_id,
            workspace_id,
            selector,
            compiled.manifest().clone(),
        ).await?;
        for blob_digest in stage.missing_blobs.clone() {
            let blob = compiled.blobs().iter().find(|blob| {
                blob.receipt().digest == blob_digest
            }).ok_or(HarnessC2Error::DeliveryCorrelationMismatch)?;
            for (chunk_index, bytes) in blob.bytes()
                .chunks(MAX_DELIVERY_CHUNK_RAW_BYTES)
                .enumerate()
            {
                let offset = (chunk_index * MAX_DELIVERY_CHUNK_RAW_BYTES) as u64;
                let chunk_hex = DeliveryBlobChunkHexV1::new(lower_hex(bytes))
                    .map_err(|_| HarnessC2Error::DeliveryCorrelationMismatch)?;
                self.put_delivery_blob_chunk(
                    &stage,
                    blob_digest.clone(),
                    offset,
                    chunk_hex,
                ).await?;
            }
        }
        self.commit_delivery_stage(stage).await
    }

    pub(crate) async fn begin_delivery_stage(
        &self,
        route: &NodeRoute,
        operation_id: HarnessOperationId,
        run_id: HarnessRunId,
        workspace_id: WorkspaceId,
        selector: HarnessSelectorV1,
        manifest: DeliveryBundleManifestV2,
    ) -> Result<DeliveryUploadStage, HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        manifest.validate().map_err(|error| {
            HarnessC2Error::InvalidDeliveryManifest(error.to_string())
        })?;
        let response = self.delivery_request(
            route,
            NodeRequest::BeginDeliveryStage { manifest: manifest.clone() },
        ).await?;
        let C2NodeResponse::DeliveryStageBegun {
            stage_id,
            manifest_digest,
            missing_blobs,
        } = response else {
            return Err(HarnessC2Error::UnexpectedDeliveryResponse);
        };
        if !delivery_stage_begin_matches(&manifest, &manifest_digest, &missing_blobs) {
            return Err(HarnessC2Error::DeliveryCorrelationMismatch);
        }
        Ok(DeliveryUploadStage {
            route: route.clone(),
            operation_id,
            run_id,
            workspace_id,
            selector,
            stage_id,
            manifest,
            missing_blobs,
        })
    }

    pub(crate) async fn put_delivery_blob_chunk(
        &self,
        stage: &DeliveryUploadStage,
        blob_digest: DeliveryBlobDigestV1,
        offset: u64,
        chunk_hex: DeliveryBlobChunkHexV1,
    ) -> Result<u64, HarnessC2Error> {
        self.ensure_current_incarnation(&stage.route)?;
        if !stage.missing_blobs.contains(&blob_digest) {
            return Err(HarnessC2Error::DeliveryCorrelationMismatch);
        }
        let expected_next_offset = offset.checked_add(chunk_hex.raw_len() as u64)
            .ok_or(HarnessC2Error::DeliveryCorrelationMismatch)?;
        let response = self.delivery_request(
            &stage.route,
            NodeRequest::PutDeliveryBlobChunk {
                stage_id: stage.stage_id.clone(),
                blob_digest: blob_digest.clone(),
                offset,
                chunk_hex,
            },
        ).await?;
        match response {
            C2NodeResponse::DeliveryBlobChunkAccepted {
                stage_id,
                blob_digest: accepted_digest,
                next_offset,
            } if delivery_chunk_reply_matches(
                &stage.stage_id,
                &blob_digest,
                expected_next_offset,
                &stage_id,
                &accepted_digest,
                next_offset,
            ) => Ok(next_offset),
            C2NodeResponse::DeliveryBlobChunkAccepted { .. } => {
                Err(HarnessC2Error::DeliveryCorrelationMismatch)
            }
            _ => Err(HarnessC2Error::UnexpectedDeliveryResponse),
        }
    }

    pub(crate) async fn commit_delivery_stage(
        &self,
        stage: DeliveryUploadStage,
    ) -> Result<StagedDeliveryProof, HarnessC2Error> {
        self.ensure_current_incarnation(&stage.route)?;
        let response = self.delivery_request(
            &stage.route,
            NodeRequest::CommitDeliveryStage { stage_id: stage.stage_id.clone() },
        ).await?;
        let C2NodeResponse::DeliveryCommitted { receipt } = response else {
            return Err(HarnessC2Error::UnexpectedDeliveryResponse);
        };
        if !delivery_receipt_matches_manifest(&receipt, &stage.manifest) {
            return Err(HarnessC2Error::DeliveryCorrelationMismatch);
        }
        Ok(StagedDeliveryProof {
            route: stage.route,
            operation_id: stage.operation_id,
            run_id: stage.run_id,
            workspace_id: stage.workspace_id,
            selector: stage.selector,
            stage_id: stage.stage_id,
            manifest: stage.manifest,
            receipt,
        })
    }

    async fn delivery_request(
        &self,
        route: &NodeRoute,
        request: NodeRequest,
    ) -> Result<C2NodeResponse, HarnessC2Error> {
        let routed = self.control.request(route.clone(), request)
            .await
            .map_err(HarnessC2Error::DeliveryTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::DeliveryRouteMismatch);
        }
        match routed.response {
            Ok(response) => Ok(response),
            Err(failure) => Err(HarnessC2Error::DeliveryRejected {
                code: failure.code,
                category: delivery_failure_category(failure.code),
            }),
        }
    }

    /// Returns non-authoritative inventory candidates without requiring a
    /// SpawnSpec and without dispatching any replay.
    ///
    /// Even a single candidate cannot bind an OutcomeUnknown operation because
    /// current H1 inventory has no durable operation/idempotency proof. H4 adds
    /// that authoritative Node lookup.
    pub async fn spawn_inventory_candidates(
        &self,
        route: &NodeRoute,
        baseline_record_ids: &[SessionRecordId],
        workspace_id: &WorkspaceId,
        expected_provider: &AgentId,
        expected_mode: SessionMode,
    ) -> Result<SpawnInventoryCandidates, HarnessC2Error> {
        validate_canonical_baseline(baseline_record_ids)?;
        self.ensure_current_incarnation(route)?;
        let snapshot = self.snapshot(route).await?;
        let candidates = matching_records(
            &snapshot,
            baseline_record_ids,
            workspace_id,
            expected_provider,
            expected_mode,
        );
        Ok(SpawnInventoryCandidates { candidates })
    }

    /// Resolves a managed record only from an already-known authoritative spawn
    /// receipt. This is not available to lost-receipt reconciliation.
    pub async fn resolve_accepted_receipt(
        &self,
        route: &NodeRoute,
        accepted: &AuthoritativeSpawnReceipt,
    ) -> Result<AcceptedSpawnBindingProof, HarnessC2Error> {
        let receipt = &accepted.receipt;
        if receipt.incarnation_id != route.expected_incarnation_id
            || receipt.target.node_id != route.node_id
        {
            return Err(HarnessC2Error::AcceptedReceiptRouteMismatch);
        }
        self.ensure_current_incarnation(route)?;
        let snapshot = self.snapshot(route).await?;
        let mut records = snapshot.session_records.iter().filter(|record| {
            record.active_session.as_ref() == Some(&receipt.session)
                && record.workspace_id == receipt.target.workspace_id
                && record.provider == receipt.provider
                && record.mode == receipt.mode
        });
        let record = records.next().ok_or(HarnessC2Error::AcceptedReceiptRecordMissing)?;
        if records.next().is_some() {
            return Err(HarnessC2Error::AcceptedReceiptRecordAmbiguous);
        }
        Ok(AcceptedSpawnBindingProof {
            operation_id: accepted.operation_id.clone(),
            spawn_spec_fingerprint: accepted.spawn_spec_fingerprint.clone(),
            idempotency_ref: accepted.idempotency_ref.clone(),
            node_id: receipt.target.node_id.clone(),
            incarnation_id: receipt.incarnation_id,
            workspace_id: receipt.target.workspace_id.clone(),
            provider: receipt.provider.clone(),
            mode: receipt.mode,
            record_id: record.record_id.clone(),
            session: receipt.session.clone(),
            bundle: receipt.bundle.clone(),
            context: receipt.context.clone(),
            harness_mcp_proxy: accepted.harness_mcp_proxy.clone(),
        })
    }

    pub(crate) async fn activate_harness_mcp_reservation(
        &self,
        route: &NodeRoute,
        reservation: crate::HarnessMcpReservationV1,
        record_id: SessionRecordId,
        session: SessionAddress,
    ) -> Result<ActivatedHarnessMcpReservationProof, HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        if reservation.node_id.as_str() != route.node_id.as_str()
            || reservation.node_incarnation_id.as_str()
                != route.expected_incarnation_id.to_string()
        {
            return Err(HarnessC2Error::HarnessMcpAuthorityMismatch);
        }
        let routed = self.control.request(
            route.clone(),
            NodeRequest::ActivateHarnessMcpReservation {
                reservation_id: reservation.reservation_id.clone(),
                activation_digest: reservation.activation_digest.clone(),
                record_id: record_id.clone(),
                session: session.clone(),
            },
        ).await.map_err(HarnessC2Error::HarnessMcpTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::HarnessMcpRouteMismatch);
        }
        match routed.response {
            Ok(C2NodeResponse::Activated {
                reservation_id,
                activation_digest,
                record_id: echoed_record,
                session: echoed_session,
            }) if reservation_id == reservation.reservation_id
                && activation_digest == reservation.activation_digest
                && echoed_record == record_id
                && echoed_session == session => Ok(ActivatedHarnessMcpReservationProof {
                    route: route.clone(),
                    reservation,
                    record_id,
                    session,
                }),
            Ok(C2NodeResponse::Activated { .. }) => {
                Err(HarnessC2Error::HarnessMcpCorrelationMismatch)
            }
            Err(failure) => Err(HarnessC2Error::HarnessMcpRejected { code: failure.code }),
            Ok(_) => Err(HarnessC2Error::UnexpectedHarnessMcpResponse),
        }
    }

    pub(crate) async fn abort_harness_mcp_reservation(
        &self,
        route: &NodeRoute,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
    ) -> Result<(), HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        let routed = self.control.request(
            route.clone(),
            NodeRequest::AbortHarnessMcpReservation {
                reservation_id: reservation_id.clone(),
                activation_digest: activation_digest.clone(),
            },
        ).await.map_err(HarnessC2Error::HarnessMcpTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::HarnessMcpRouteMismatch);
        }
        match routed.response {
            Ok(C2NodeResponse::Aborted {
                reservation_id: echoed_reservation,
                activation_digest: echoed_digest,
            }) if &echoed_reservation == reservation_id
                && &echoed_digest == activation_digest => Ok(()),
            Ok(C2NodeResponse::Aborted { .. }) => {
                Err(HarnessC2Error::HarnessMcpCorrelationMismatch)
            }
            Err(failure) => Err(HarnessC2Error::HarnessMcpRejected { code: failure.code }),
            Ok(_) => Err(HarnessC2Error::UnexpectedHarnessMcpResponse),
        }
    }

    pub(crate) async fn put_harness_mcp_reply_chunk(
        &self,
        route: &NodeRoute,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        record_id: &SessionRecordId,
        session: &SessionAddress,
        call_id: &HarnessMcpCallId,
        offset: u32,
        final_chunk: bool,
        chunk_hex: HarnessMcpReplyChunkHexV1,
    ) -> Result<u32, HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        let expected_next = offset.checked_add(chunk_hex.raw_len() as u32)
            .ok_or(HarnessC2Error::HarnessMcpCorrelationMismatch)?;
        let routed = self.control.request(route.clone(), NodeRequest::PutHarnessMcpReplyChunk {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
            record_id: record_id.clone(),
            session: session.clone(),
            call_id: call_id.clone(),
            offset,
            final_chunk,
            chunk_hex,
        }).await.map_err(HarnessC2Error::HarnessMcpTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::HarnessMcpRouteMismatch);
        }
        match routed.response {
            Ok(C2NodeResponse::ReplyChunkAccepted {
                reservation_id: echoed_reservation,
                activation_digest: echoed_digest,
                record_id: echoed_record,
                session: echoed_session,
                call_id: echoed_call,
                next_offset,
                completed,
            }) if &echoed_reservation == reservation_id
                && &echoed_digest == activation_digest
                && &echoed_record == record_id
                && &echoed_session == session
                && &echoed_call == call_id
                && next_offset == expected_next
                && completed == final_chunk => Ok(next_offset),
            Ok(C2NodeResponse::ReplyChunkAccepted { .. }) => {
                Err(HarnessC2Error::HarnessMcpCorrelationMismatch)
            }
            Err(failure) => Err(HarnessC2Error::HarnessMcpRejected { code: failure.code }),
            Ok(_) => Err(HarnessC2Error::UnexpectedHarnessMcpResponse),
        }
    }

    pub(crate) async fn reject_harness_mcp_call(
        &self,
        route: &NodeRoute,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        record_id: &SessionRecordId,
        session: &SessionAddress,
        call_id: &HarnessMcpCallId,
        reason: HarnessMcpRejectReasonV1,
    ) -> Result<(), HarnessC2Error> {
        self.ensure_current_incarnation(route)?;
        let routed = self.control.request(route.clone(), NodeRequest::RejectHarnessMcpCall {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
            record_id: record_id.clone(),
            session: session.clone(),
            call_id: call_id.clone(),
            reason,
        }).await.map_err(HarnessC2Error::HarnessMcpTransport)?;
        if routed.node_id != route.node_id
            || routed.incarnation_id != route.expected_incarnation_id
        {
            return Err(HarnessC2Error::HarnessMcpRouteMismatch);
        }
        match routed.response {
            Ok(C2NodeResponse::CallRejected {
                reservation_id: echoed_reservation,
                activation_digest: echoed_digest,
                record_id: echoed_record,
                session: echoed_session,
                call_id: echoed_call,
            }) if &echoed_reservation == reservation_id
                && &echoed_digest == activation_digest
                && &echoed_record == record_id
                && &echoed_session == session
                && &echoed_call == call_id => Ok(()),
            Ok(C2NodeResponse::CallRejected { .. }) => {
                Err(HarnessC2Error::HarnessMcpCorrelationMismatch)
            }
            Err(failure) => Err(HarnessC2Error::HarnessMcpRejected { code: failure.code }),
            Ok(_) => Err(HarnessC2Error::UnexpectedHarnessMcpResponse),
        }
    }

    fn ensure_current_incarnation(&self, route: &NodeRoute) -> Result<(), HarnessC2Error> {
        let current = self.exact_route(&route.node_id)?;
        if current.expected_incarnation_id != route.expected_incarnation_id {
            return Err(HarnessC2Error::IncarnationChanged {
                node_id: route.node_id.clone(),
            });
        }
        Ok(())
    }
}

pub(crate) struct DeliveryUploadStage {
    route: NodeRoute,
    operation_id: HarnessOperationId,
    run_id: HarnessRunId,
    workspace_id: WorkspaceId,
    selector: HarnessSelectorV1,
    stage_id: DeliveryStageId,
    manifest: DeliveryBundleManifestV2,
    missing_blobs: Vec<DeliveryBlobDigestV1>,
}

/// Non-cloneable authority for one exact content-addressed delivery upload.
/// It is constructed only by the service after a current grant check.
pub(crate) struct PreparedDeliveryStageLease {
    route: NodeRoute,
    operation_id: HarnessOperationId,
    run_id: HarnessRunId,
    workspace_id: WorkspaceId,
    selector: HarnessSelectorV1,
    compiled: CompiledDeliveryBundleV2,
}

impl PreparedDeliveryStageLease {
    pub(crate) fn new(
        route: NodeRoute,
        operation_id: HarnessOperationId,
        run_id: HarnessRunId,
        workspace_id: WorkspaceId,
        selector: HarnessSelectorV1,
        compiled: CompiledDeliveryBundleV2,
    ) -> Result<Self, HarnessC2Error> {
        operation_id.validate().map_err(HarnessC2Error::OperationIdentity)?;
        run_id.validate().map_err(HarnessC2Error::OperationIdentity)?;
        selector.validate().map_err(HarnessC2Error::OperationIdentity)?;
        compiled.verify().map_err(|error| {
            HarnessC2Error::InvalidCompiledDelivery(error.to_string())
        })?;
        Ok(Self {
            route,
            operation_id,
            run_id,
            workspace_id,
            selector,
            compiled,
        })
    }
}

/// Opaque authority created only from an exact correlated `DeliveryCommitted`
/// response on the same Node incarnation as the complete upload stage.
#[derive(Debug, Eq, PartialEq)]
pub struct StagedDeliveryProof {
    route: NodeRoute,
    operation_id: HarnessOperationId,
    run_id: HarnessRunId,
    workspace_id: WorkspaceId,
    selector: HarnessSelectorV1,
    stage_id: DeliveryStageId,
    manifest: DeliveryBundleManifestV2,
    receipt: DeliveryCommitReceiptV1,
}

/// Opaque single-use authority produced only by an exact Node `Armed` reply.
pub struct ArmedHarnessMcpReservationProof {
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
}

/// Non-cloneable waiter for the one Arm request synchronously accepted by C2.
/// A start error means C2 did not accept the request into its bounded queue.
pub(crate) struct PendingHarnessMcpArm {
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
    pending: Option<C2PendingRequest>,
}

impl PendingHarnessMcpArm {
    pub(crate) async fn finish(
        mut self,
    ) -> Result<ArmedHarnessMcpReservationProof, HarnessC2Error> {
        let pending = self.pending.take()
            .expect("pending harness MCP Arm owns exactly one C2 waiter");
        let routed = pending.finish().await.map_err(HarnessC2Error::HarnessMcpTransport)?;
        validate_harness_mcp_arm_response(
            &self.route,
            &self.reservation.reservation_id,
            &self.reservation.activation_digest,
            self.reservation.expires_at_unix_ms,
            routed,
        )?;
        Ok(ArmedHarnessMcpReservationProof {
            route: self.route,
            reservation: self.reservation,
        })
    }
}

fn validate_harness_mcp_arm_response(
    route: &NodeRoute,
    expected_reservation_id: &HarnessMcpReservationId,
    expected_activation_digest: &HarnessMcpActivationDigest,
    expected_expires_at_unix_ms: u64,
    routed: gate4agent_c2_protocol::RoutedNodeResponse,
) -> Result<(), HarnessC2Error> {
    if routed.node_id != route.node_id
        || routed.incarnation_id != route.expected_incarnation_id
    {
        return Err(HarnessC2Error::HarnessMcpRouteMismatch);
    }
    match routed.response {
        Ok(C2NodeResponse::Armed {
            reservation_id,
            activation_digest,
            expires_at_unix_ms,
        }) if &reservation_id == expected_reservation_id
            && &activation_digest == expected_activation_digest
            && expires_at_unix_ms == expected_expires_at_unix_ms => Ok(()),
        Ok(C2NodeResponse::Armed { .. }) => {
            Err(HarnessC2Error::HarnessMcpCorrelationMismatch)
        }
        Err(failure) => Err(HarnessC2Error::HarnessMcpRejected { code: failure.code }),
        Ok(_) => Err(HarnessC2Error::UnexpectedHarnessMcpResponse),
    }
}

pub struct ActivatedHarnessMcpReservationProof {
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
    record_id: SessionRecordId,
    session: SessionAddress,
}

impl ActivatedHarnessMcpReservationProof {
    pub fn reservation_id(&self) -> &HarnessMcpReservationId {
        &self.reservation.reservation_id
    }

    pub(crate) fn validate_record(
        &self,
        record: &crate::HarnessMcpReservationV1,
    ) -> Result<(), crate::HarnessServiceError> {
        let mut normalized = record.clone();
        normalized.revision = self.reservation.revision;
        normalized.state = self.reservation.state;
        normalized.updated_at_unix_ms = self.reservation.updated_at_unix_ms;
        if self.reservation != normalized
            || self.route.node_id.as_str() != record.node_id.as_str()
            || self.route.expected_incarnation_id.to_string()
                != record.node_incarnation_id.as_str()
            || record.record_id.as_ref().is_none_or(|value| {
                value.as_str() != self.record_id.as_str()
            })
            || record.instance_id != Some(self.session.session.instance_id.0)
            || record.generation != Some(self.session.session.generation.0)
        {
            return Err(crate::HarnessServiceError::HarnessMcpProofMismatch);
        }
        Ok(())
    }
}

impl ArmedHarnessMcpReservationProof {
    pub fn reservation_id(&self) -> &HarnessMcpReservationId {
        &self.reservation.reservation_id
    }

    pub fn activation_digest(&self) -> &HarnessMcpActivationDigest {
        &self.reservation.activation_digest
    }

    pub(crate) fn validate_record(
        &self,
        record: &crate::HarnessMcpReservationV1,
    ) -> Result<(), crate::HarnessServiceError> {
        let mut normalized = record.clone();
        normalized.revision = self.reservation.revision;
        normalized.state = self.reservation.state;
        normalized.updated_at_unix_ms = self.reservation.updated_at_unix_ms;
        if self.reservation != normalized
            || self.route.node_id.as_str() != record.node_id.as_str()
            || self.route.expected_incarnation_id.to_string()
                != record.node_incarnation_id.as_str()
        {
            return Err(crate::HarnessServiceError::HarnessMcpProofMismatch);
        }
        Ok(())
    }
}

impl StagedDeliveryProof {
    pub(crate) fn operation_id(&self) -> &HarnessOperationId { &self.operation_id }

    pub(crate) fn run_id(&self) -> &HarnessRunId { &self.run_id }

    pub(crate) fn node_id(&self) -> &NodeId { &self.route.node_id }

    pub(crate) fn incarnation_id(&self) -> gate4agent_node_protocol::NodeIncarnationId {
        self.route.expected_incarnation_id
    }

    pub(crate) fn workspace_id(&self) -> &WorkspaceId { &self.workspace_id }

    pub(crate) fn selector(&self) -> &HarnessSelectorV1 { &self.selector }

    pub(crate) fn manifest(&self) -> &DeliveryBundleManifestV2 { &self.manifest }

    pub(crate) fn receipt(&self) -> &DeliveryCommitReceiptV1 { &self.receipt }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryFailureCategory {
    Validation,
    StageConflict,
    Integrity,
    Storage,
    Other,
}

fn delivery_failure_category(code: NodeFailureCode) -> DeliveryFailureCategory {
    match code {
        NodeFailureCode::DeliveryManifestInvalid
        | NodeFailureCode::DeliveryBlobUnexpected => DeliveryFailureCategory::Validation,
        NodeFailureCode::UnknownDeliveryStage
        | NodeFailureCode::DeliveryStageConflict
        | NodeFailureCode::DeliveryChunkOutOfOrder
        | NodeFailureCode::DeliveryStageIncomplete => DeliveryFailureCategory::StageConflict,
        NodeFailureCode::DeliveryBlobDigestMismatch
        | NodeFailureCode::DeliveryBundleDigestMismatch => DeliveryFailureCategory::Integrity,
        NodeFailureCode::DeliveryStageStorageFailed => DeliveryFailureCategory::Storage,
        _ => DeliveryFailureCategory::Other,
    }
}

fn delivery_stage_begin_matches(
    manifest: &DeliveryBundleManifestV2,
    manifest_digest: &gate4agent_node_protocol::DeliveryManifestDigestV2,
    missing_blobs: &[DeliveryBlobDigestV1],
) -> bool {
    manifest_digest == &manifest.manifest_digest
        && !missing_blobs.windows(2).any(|pair| pair[0] >= pair[1])
        && missing_blobs.iter().all(|digest| {
            manifest.components.iter().any(|component| {
                &component.blob.digest == digest
            })
        })
}

fn delivery_chunk_reply_matches(
    expected_stage_id: &DeliveryStageId,
    expected_blob_digest: &DeliveryBlobDigestV1,
    expected_next_offset: u64,
    actual_stage_id: &DeliveryStageId,
    actual_blob_digest: &DeliveryBlobDigestV1,
    actual_next_offset: u64,
) -> bool {
    actual_stage_id == expected_stage_id
        && actual_blob_digest == expected_blob_digest
        && actual_next_offset == expected_next_offset
}

fn delivery_receipt_matches_manifest(
    receipt: &DeliveryCommitReceiptV1,
    manifest: &DeliveryBundleManifestV2,
) -> bool {
    if receipt.bundle_id != manifest.bundle_id
        || receipt.revision != manifest.revision
        || receipt.bundle_digest != manifest.bundle_digest
        || receipt.manifest_digest != manifest.manifest_digest
    {
        return false;
    }
    let mut expected = std::collections::BTreeMap::new();
    for component in &manifest.components {
        match expected.insert(component.blob.digest.clone(), component.blob.byte_len) {
            Some(byte_len) if byte_len != component.blob.byte_len => return false,
            _ => {}
        }
    }
    receipt.blobs.len() == expected.len()
        && receipt.blobs.iter().zip(expected).all(|(actual, (digest, byte_len))| {
            actual.digest == digest && actual.byte_len == byte_len
        })
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn observation_topology(topology: &C2Topology) -> Vec<HarnessObservationRoute> {
    let mut routes = topology.nodes.iter().filter_map(|node| {
        if node.transport != NodeTransportState::Online { return None; }
        node.current_incarnation_id.map(|expected_incarnation_id| HarnessObservationRoute {
            route: NodeRoute {
                node_id: node.node_id.clone(),
                expected_incarnation_id,
            },
            support: node.observation_support,
        })
    }).collect::<Vec<_>>();
    routes.sort_by(|left, right| left.route.node_id.as_str().cmp(right.route.node_id.as_str()));
    routes.dedup_by(|left, right| left.route.node_id == right.route.node_id);
    routes
}

const HARNESS_OBSERVATION_RESYNC_EVENTS_MAX: usize = 4_096;

/// Sealed Node resync authority for the observation host.
///
/// Private fields prevent callers from fabricating replay floors, high
/// watermarks, or complete managed inventory. Accessors expose only the exact
/// values validated from a routed Node response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessObservationResync {
    route: NodeRoute,
    requested_after_sequence: u64,
    event_sequence: u64,
    oldest_available_sequence: u64,
    snapshot: C2NodeSnapshot,
    lifecycle_control_events: Vec<C2NodeEventEnvelope>,
    events: Vec<C2NodeEventEnvelope>,
}

impl HarnessObservationResync {
    pub fn route(&self) -> &NodeRoute { &self.route }

    pub fn requested_after_sequence(&self) -> u64 { self.requested_after_sequence }

    pub fn event_sequence(&self) -> u64 { self.event_sequence }

    pub fn oldest_available_sequence(&self) -> u64 { self.oldest_available_sequence }

    pub fn has_eviction_gap(&self) -> bool {
        self.requested_after_sequence.saturating_add(1) < self.oldest_available_sequence
    }

    pub fn snapshot(&self) -> &C2NodeSnapshot { &self.snapshot }

    pub fn observation_support(&self) -> Option<C2ObservationSupport> {
        self.snapshot.observation_support
    }

    pub fn managed_inventory(&self) -> &[C2ManagedSessionRecord] {
        &self.snapshot.session_records
    }

    pub fn lifecycle_control_events(&self) -> &[C2NodeEventEnvelope] {
        &self.lifecycle_control_events
    }

    pub fn observation_events(&self) -> &[C2NodeEventEnvelope] { &self.events }

    #[cfg(test)]
    pub(crate) fn test_fixture(
        route: NodeRoute,
        event_sequence: u64,
        snapshot: C2NodeSnapshot,
    ) -> Self {
        build_observation_resync(&route, 0, event_sequence, 1, snapshot, Vec::new())
            .expect("valid observation resync fixture")
    }
}

fn build_observation_resync(
    route: &NodeRoute,
    requested_after_sequence: u64,
    event_sequence: u64,
    oldest_available_sequence: u64,
    snapshot: C2NodeSnapshot,
    events: Vec<C2NodeEventEnvelope>,
) -> Result<HarnessObservationResync, HarnessC2Error> {
    if snapshot.node_id != route.node_id
        || requested_after_sequence > event_sequence
        || oldest_available_sequence == 0
        || oldest_available_sequence > event_sequence.saturating_add(1)
        || events.len() > HARNESS_OBSERVATION_RESYNC_EVENTS_MAX
        || events.windows(2).any(|pair| pair[0].sequence >= pair[1].sequence)
        || events.iter().any(|event| {
            event.sequence <= requested_after_sequence
                || event.sequence < oldest_available_sequence
                || event.sequence > event_sequence
        })
    {
        return Err(HarnessC2Error::InvalidObservationResync);
    }
    if snapshot.observation_support.is_some_and(|support| !support.is_valid()) {
        return Err(HarnessC2Error::InvalidObservationResync);
    }
    let lifecycle_control_events = events.iter()
        .filter(|event| matches!(event.event, gate4agent_c2_protocol::C2NodeEvent::Control { .. }))
        .cloned()
        .collect();
    let events = events.into_iter()
        .filter(|event| event.event.requires_observation_events_capability())
        .collect();
    Ok(HarnessObservationResync {
        route: route.clone(),
        requested_after_sequence,
        event_sequence,
        oldest_available_sequence,
        snapshot,
        lifecycle_control_events,
        events,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnDispatchOutcome {
    Accepted(AuthoritativeSpawnReceipt),
    Rejected {
        code: gate4agent_node_protocol::NodeFailureCode,
    },
    OutcomeUnknown {
        reason: SpawnOutcomeUnknownReason,
    },
}

#[derive(Debug)]
pub(crate) struct PreparedSpawnDispatch {
    route: NodeRoute,
    operation_id: HarnessOperationId,
    idempotency_ref: HarnessIdempotencyRef,
    spec: SpawnSpec,
    fingerprint: HarnessRequestDigest,
    expected_bundle: Option<ResolvedBundleReceipt>,
    expected_context: Option<ResolvedContextPackReceipt>,
    harness_mcp: Option<PreparedSpawnHarnessMcp>,
}

impl PreparedSpawnDispatch {
    pub(crate) fn new(
        route: NodeRoute,
        operation_id: HarnessOperationId,
        idempotency_ref: HarnessIdempotencyRef,
        spec: SpawnSpec,
        fingerprint: HarnessRequestDigest,
    ) -> Result<Self, HarnessC2Error> {
        validate_spawn_route(&route, &spec)?;
        validate_prepared_spawn_overrides(&spec)?;
        operation_id.validate().map_err(HarnessC2Error::OperationIdentity)?;
        idempotency_ref.validate().map_err(HarnessC2Error::OperationIdentity)?;
        Ok(Self {
            route,
            operation_id,
            idempotency_ref,
            spec,
            fingerprint,
            expected_bundle: None,
            expected_context: None,
            harness_mcp: None,
        })
    }

    pub(crate) fn with_expected_bundle(
        mut self,
        expected_bundle: ResolvedBundleReceipt,
    ) -> Result<Self, HarnessC2Error> {
        match &self.spec.overrides.bundle_id {
            SpawnOverride::Set { value } if value == &expected_bundle.id => {}
            _ => return Err(HarnessC2Error::NonAuthoritativeBundleOverride),
        }
        self.expected_bundle = Some(expected_bundle);
        Ok(self)
    }

    pub(crate) fn with_expected_context(
        mut self,
        expected_context: ResolvedContextPackReceipt,
    ) -> Result<Self, HarnessC2Error> {
        match &self.spec.overrides.context_id {
            SpawnOverride::Set { value } if value == &expected_context.id => {}
            _ => return Err(HarnessC2Error::NonAuthoritativeContextOverride),
        }
        self.expected_context = Some(expected_context);
        Ok(self)
    }

    pub(crate) fn with_harness_mcp(
        mut self,
        reservation: &crate::HarnessMcpReservationV1,
        deadline_unix_ms: u64,
    ) -> Self {
        self.harness_mcp = Some(PreparedSpawnHarnessMcp {
            reservation_id: reservation.reservation_id.clone(),
            activation_digest: reservation.activation_digest.clone(),
            proxy_receipt: reservation.proxy_receipt(),
            deadline_unix_ms,
        });
        self
    }
}

pub(crate) struct PendingSpawnDispatch {
    correlation: SpawnResponseCorrelation,
    pending: Option<C2PendingRequest>,
}

pub(crate) struct PendingNativeHistoryRequest {
    route: NodeRoute,
    request: HarnessOperatorRequestV1,
    started_at: Instant,
    pending: Option<C2PendingRequest>,
}

impl PendingNativeHistoryRequest {
    pub(crate) async fn finish(
        mut self,
    ) -> Result<HarnessOperatorResponseV1, HarnessC2Error> {
        let pending = self.pending.take()
            .expect("pending native history request owns exactly one C2 waiter");
        let routed = match pending.finish().await {
            Err(C2ControlError::Closed)
                if self.started_at.elapsed() >= NATIVE_HISTORY_TIMEOUT_FLOOR => {
                    return Err(HarnessC2Error::NativeHistoryDeadline);
                }
            Err(error) => return Err(HarnessC2Error::NativeHistoryTransport(error)),
            Ok(routed) => routed,
        };
        if routed.node_id != self.route.node_id
            || routed.incarnation_id != self.route.expected_incarnation_id
        {
            return Err(HarnessC2Error::NativeHistoryRouteMismatch);
        }
        match routed.response {
            Err(failure) => Err(HarnessC2Error::NativeHistoryRejected { code: failure.code }),
            Ok(response) => correlate_native_history_response(self.request, response),
        }
    }
}

fn native_history_wire_request(
    request: &HarnessOperatorRequestV1,
) -> Result<(NodeRoute, NodeRequest), HarnessC2Error> {
    let api_route = match request {
        HarnessOperatorRequestV1::CatalogNativeSessions { route, .. }
        | HarnessOperatorRequestV1::PageNativeSessions { route, .. } => route,
        HarnessOperatorRequestV1::PreviewNativeSession { selection, .. } => &selection.route,
        _ => return Err(HarnessC2Error::InvalidNativeHistoryRequest),
    };
    let route = NodeRoute {
        node_id: NodeId::new(api_route.node_id.as_str())
            .map_err(|_| HarnessC2Error::InvalidNativeHistoryRequest)?,
        expected_incarnation_id: api_route.incarnation_id.parse()
            .map_err(|_| HarnessC2Error::InvalidNativeHistoryRequest)?,
    };
    let wire_request = match request {
        HarnessOperatorRequestV1::CatalogNativeSessions { route, limit } => {
            NodeRequest::CatalogNativeSessions {
                route: native_history_wire_route(route)?,
                limit: *limit,
            }
        }
        HarnessOperatorRequestV1::PageNativeSessions {
            route,
            window,
            catalog_revision,
            recent_cutoff_unix_ms,
            after_selection_id,
            limit,
        } => NodeRequest::PageNativeSessions {
            route: native_history_wire_route(route)?,
            window: native_history_wire_window(*window),
            catalog_revision: *catalog_revision,
            recent_cutoff_unix_ms: *recent_cutoff_unix_ms,
            after_selection_id: after_selection_id.clone(),
            limit: *limit,
        },
        HarnessOperatorRequestV1::PreviewNativeSession { selection, message_limit } => {
            NodeRequest::PreviewNativeSession {
                selection: native_history_wire_selection(selection)?,
                message_limit: *message_limit,
            }
        }
        _ => return Err(HarnessC2Error::InvalidNativeHistoryRequest),
    };
    Ok((route, wire_request))
}

fn native_history_wire_route(
    route: &HarnessNativeSessionRouteV1,
) -> Result<NativeSessionCatalogRoute, HarnessC2Error> {
    let provider = AgentId::new(route.provider.as_str())
        .map_err(|_| HarnessC2Error::InvalidNativeHistoryRequest)?;
    match route.scope {
        HarnessNativeSessionCatalogScopeV1::Workspace => {
            let workspace_id = route.workspace_id.as_deref()
                .ok_or(HarnessC2Error::InvalidNativeHistoryRequest)?;
            Ok(NativeSessionCatalogRoute::workspace(
                WorkspaceId::new(workspace_id)
                    .map_err(|_| HarnessC2Error::InvalidNativeHistoryRequest)?,
                provider,
            ))
        }
        HarnessNativeSessionCatalogScopeV1::Unregistered => {
            if route.workspace_id.is_some() {
                return Err(HarnessC2Error::InvalidNativeHistoryRequest);
            }
            Ok(NativeSessionCatalogRoute::unregistered(provider))
        }
    }
}

fn native_history_wire_selection(
    selection: &HarnessNativeSessionSelectionV1,
) -> Result<NativeSessionSelection, HarnessC2Error> {
    Ok(NativeSessionSelection {
        route: native_history_wire_route(&selection.route)?,
        catalog_revision: selection.catalog_revision,
        recent_cutoff_unix_ms: selection.recent_cutoff_unix_ms,
        selection_id: selection.selection_id.clone(),
    })
}

fn native_history_wire_window(
    window: HarnessNativeSessionCatalogWindowV1,
) -> gate4agent_node_protocol::NativeSessionCatalogWindow {
    match window {
        HarnessNativeSessionCatalogWindowV1::Recent => {
            gate4agent_node_protocol::NativeSessionCatalogWindow::Recent
        }
        HarnessNativeSessionCatalogWindowV1::Older => {
            gate4agent_node_protocol::NativeSessionCatalogWindow::Older
        }
    }
}

fn correlate_native_history_response(
    request: HarnessOperatorRequestV1,
    response: C2NodeResponse,
) -> Result<HarnessOperatorResponseV1, HarnessC2Error> {
    match (request, response) {
        (
            HarnessOperatorRequestV1::CatalogNativeSessions { route, .. },
            C2NodeResponse::NativeSessionsCataloged {
                route: echoed_route,
                entries,
                summary,
            },
        ) if native_history_wire_route(&route).ok().as_ref() == Some(&echoed_route) => {
            Ok(HarnessOperatorResponseV1::NativeSessionsCataloged(
                HarnessNativeSessionsCatalogedV1 {
                    route,
                    entries: entries.into_iter().map(project_native_history_entry).collect(),
                    summary: summary.map(project_native_history_summary),
                },
            ))
        }
        (
            HarnessOperatorRequestV1::PageNativeSessions {
                route,
                window,
                catalog_revision,
                recent_cutoff_unix_ms: _,
                ..
            },
            C2NodeResponse::NativeSessionsPaged {
                route: echoed_route,
                page,
            },
        ) if native_history_wire_route(&route).ok().as_ref() == Some(&echoed_route)
            && page.window == native_history_wire_window(window)
            && page.revision == catalog_revision => {
            Ok(HarnessOperatorResponseV1::NativeSessionsPaged(
                HarnessNativeSessionsPagedV1 {
                    route,
                    page: project_native_history_page(page),
                },
            ))
        }
        (
            HarnessOperatorRequestV1::PreviewNativeSession { selection, .. },
            C2NodeResponse::NativeSessionPreviewed {
                selection: echoed_selection,
                preview,
            },
        ) if native_history_wire_selection(&selection).ok().as_ref()
            == Some(&echoed_selection) => {
            Ok(HarnessOperatorResponseV1::NativeSessionPreviewed(
                HarnessNativeSessionPreviewedV1 {
                    selection,
                    preview: project_native_history_preview(preview),
                },
            ))
        }
        _ => Err(HarnessC2Error::NativeHistoryCorrelationMismatch),
    }
}

fn project_native_history_entry(
    entry: gate4agent_node_protocol::NativeSessionCatalogEntry,
) -> HarnessNativeSessionCatalogEntryV1 {
    HarnessNativeSessionCatalogEntryV1 {
        selection_id: entry.selection_id,
        title: entry.title,
        modified_at_unix_ms: entry.modified_at_unix_ms,
        model: entry.model,
        message_count: entry.message_count,
        completed_turn_count: entry.completed_turn_count,
        external_group: entry.external_group.map(|group| HarnessNativeSessionExternalGroupV1 {
            group_id: group.group_id,
            kind: match group.kind {
                gate4agent_node_protocol::NativeSessionExternalGroupKind::Project => {
                    HarnessNativeSessionExternalGroupKindV1::Project
                }
                gate4agent_node_protocol::NativeSessionExternalGroupKind::Global => {
                    HarnessNativeSessionExternalGroupKindV1::Global
                }
            },
            display_name: group.display_name,
        }),
        record_id: entry.record_id.map(|record_id| record_id.as_str().to_owned()),
    }
}

fn project_native_history_summary(
    summary: gate4agent_node_protocol::NativeSessionCatalogSummary,
) -> HarnessNativeSessionCatalogSummaryV1 {
    HarnessNativeSessionCatalogSummaryV1 {
        catalog_revision: summary.catalog_revision,
        recent_cutoff_unix_ms: summary.recent_cutoff_unix_ms,
        recent_total_count: summary.recent_total_count,
        older_total_count: summary.older_total_count,
        recent_next_after_selection_id: summary.recent_next_after_selection_id,
        recent_has_more: summary.recent_has_more,
    }
}

fn project_native_history_page(
    page: gate4agent_node_protocol::NativeSessionCatalogPage,
) -> HarnessNativeSessionCatalogPageV1 {
    HarnessNativeSessionCatalogPageV1 {
        window: match page.window {
            gate4agent_node_protocol::NativeSessionCatalogWindow::Recent => {
                HarnessNativeSessionCatalogWindowV1::Recent
            }
            gate4agent_node_protocol::NativeSessionCatalogWindow::Older => {
                HarnessNativeSessionCatalogWindowV1::Older
            }
        },
        revision: page.revision,
        entries: page.entries.into_iter().map(project_native_history_entry).collect(),
        next_after_selection_id: page.next_after_selection_id,
        remaining_count: page.remaining_count,
        has_more: page.has_more,
    }
}

fn project_native_history_preview(
    preview: gate4agent_node_protocol::NativeSessionPreview,
) -> HarnessNativeSessionPreviewV1 {
    HarnessNativeSessionPreviewV1 {
        title: preview.title,
        modified_at_unix_ms: preview.modified_at_unix_ms,
        model: preview.model,
        message_count: preview.message_count,
        message_count_exact: preview.message_count_exact,
        completed_turn_count: preview.completed_turn_count,
        total_tokens: preview.total_tokens,
        truncated: preview.truncated,
        messages: preview.messages.into_iter().map(|message| {
            HarnessNativeSessionPreviewMessageV1 {
                role: match message.role {
                    gate4agent_types::HistoryMessageRole::User => {
                        HarnessNativeSessionPreviewRoleV1::User
                    }
                    gate4agent_types::HistoryMessageRole::Assistant => {
                        HarnessNativeSessionPreviewRoleV1::Assistant
                    }
                },
                text: message.text,
            }
        }).collect(),
    }
}

struct SpawnResponseCorrelation {
    route: NodeRoute,
    operation_id: HarnessOperationId,
    idempotency_ref: HarnessIdempotencyRef,
    cleared_spec: SpawnSpec,
    profile_revision: SpawnProfileRevision,
    fingerprint: HarnessRequestDigest,
    expected_bundle: Option<ResolvedBundleReceipt>,
    expected_context: Option<ResolvedContextPackReceipt>,
    expected_proxy: Option<(
        HarnessMcpReservationId,
        HarnessMcpActivationDigest,
        ResolvedHarnessMcpProxyReceiptV1,
    )>,
}

impl PendingSpawnDispatch {
    pub(crate) async fn finish(mut self) -> Result<SpawnDispatchOutcome, HarnessC2Error> {
        let pending = self.pending.take().expect("pending spawn owns exactly one C2 waiter");
        correlate_spawn_response(self.correlation, pending.finish().await)
    }
}

fn correlate_spawn_response(
    correlation: SpawnResponseCorrelation,
    routed: Result<gate4agent_c2_protocol::RoutedNodeResponse, C2ControlError>,
) -> Result<SpawnDispatchOutcome, HarnessC2Error> {
        let routed = match routed {
            Ok(response) => response,
            Err(error) => return Ok(SpawnDispatchOutcome::OutcomeUnknown {
                reason: unknown_reason(&error),
            }),
        };
        if routed.node_id != correlation.route.node_id
            || routed.incarnation_id != correlation.route.expected_incarnation_id
        {
            return Ok(SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::RoutedIdentityMismatch,
            });
        }
        let receipt = match (routed.response, correlation.expected_proxy.as_ref()) {
            (Ok(C2NodeResponse::SpawnSpecAccepted { receipt }), None) => receipt,
            (Ok(C2NodeResponse::Spawned {
                reservation_id,
                activation_digest,
                receipt,
            }), Some((expected_reservation, expected_digest, expected_proxy)))
                if &reservation_id == expected_reservation
                    && &activation_digest == expected_digest
                    && receipt.harness_mcp_proxy.as_ref() == Some(expected_proxy) => receipt,
            (Err(failure), _) => return Ok(SpawnDispatchOutcome::Rejected {
                code: failure.code,
            }),
            _ => return Ok(SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::UnexpectedResponse,
            }),
        };
        let mut expected = explicit_spawn_resolution(
            &correlation.cleared_spec,
            correlation.profile_revision,
        )?;
        if let Some(bundle) = &correlation.expected_bundle {
            expected.bundle_id = Some(bundle.id.clone());
            expected.provenance.bundle_id = SpawnFieldProvenance::Override;
        }
        if let Some(context) = &correlation.expected_context {
            expected.context_id = Some(context.id.clone());
            expected.provenance.context_id = SpawnFieldProvenance::Override;
        }
        let mut ordinary = receipt.clone();
        ordinary.harness_mcp_proxy = None;
        if !validate_spawn_receipt(
            &correlation.route,
            &expected,
            correlation.expected_bundle.as_ref(),
            correlation.expected_context.as_ref(),
            &ordinary,
        ) {
            return Ok(SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::ReceiptMismatch,
            });
        }
        Ok(SpawnDispatchOutcome::Accepted(AuthoritativeSpawnReceipt {
            operation_id: correlation.operation_id,
            spawn_spec_fingerprint: correlation.fingerprint,
            idempotency_ref: correlation.idempotency_ref,
            harness_mcp_proxy: receipt.harness_mcp_proxy.clone(),
            receipt,
        }))
}

/// Non-cloneable evidence of one exact typed C2 ContextPack export reply.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ExportedContextPackProof {
    continuation_ref: HarnessContinuationRef,
    route: NodeRoute,
    source_session: SessionAddress,
    source_provider: AgentId,
    context: ResolvedContextPackReceipt,
}

/// Owned, non-cloneable pre-send authority. It contains only exact safe
/// receipts and request metadata, so no HarnessService borrow crosses await.
#[derive(Debug)]
pub(crate) struct PreparedContinuationSpawnDispatch {
    inner: PreparedSpawnDispatch,
}

pub(crate) struct PendingContinuationSpawnDispatch {
    inner: PendingSpawnDispatch,
}

/// Sealed read-only inventory evidence captured before durable lease issuance.
pub(crate) struct SpawnProfileRevisionProof {
    route: NodeRoute,
    profile_id: SpawnProfileId,
    profile_revision: SpawnProfileRevision,
}

impl SpawnProfileRevisionProof {
    pub(crate) fn route(&self) -> &NodeRoute { &self.route }

    pub(crate) fn revision(&self) -> &SpawnProfileRevision { &self.profile_revision }

    pub(crate) fn bind_spec(
        &self,
        spec: SpawnSpec,
    ) -> Result<SpawnSpec, HarnessC2Error> {
        if spec.target.node_id != self.route.node_id || spec.profile_id != self.profile_id {
            return Err(HarnessC2Error::SpawnProfileAuthorityMismatch);
        }
        if spec.expected_profile_revision != self.profile_revision {
            return Err(HarnessC2Error::SpawnProfileAuthorityMismatch);
        }
        Ok(spec)
    }
}

fn validate_prepared_spawn_profile(
    prepared: &PreparedSpawnDispatch,
    profile: &SpawnProfileRevisionProof,
) -> Result<(), HarnessC2Error> {
    if profile.route != prepared.route
        || profile.profile_id != prepared.spec.profile_id
        || profile.profile_revision != prepared.spec.expected_profile_revision
    {
        return Err(HarnessC2Error::SpawnProfileAuthorityMismatch);
    }
    Ok(())
}

impl PendingContinuationSpawnDispatch {
    pub(crate) async fn finish(self) -> Result<SpawnDispatchOutcome, HarnessC2Error> {
        self.inner.finish().await
    }
}

pub(crate) enum ContextPackExportStart {
    Enqueued(PendingContextPackExport),
    NotEnqueued(ExportContextPackOutcome),
}

pub(crate) struct PendingContextPackExport {
    prepared: crate::PreparedContinuationExport,
    authority: gate4agent_harness_protocol::HarnessContinuationV1,
    route: NodeRoute,
    source_session: SessionAddress,
    source_provider: AgentId,
    pending: Option<C2PendingRequest>,
}

impl PendingContextPackExport {
    pub(crate) async fn finish(
        mut self,
    ) -> Result<ExportContextPackOutcome, HarnessC2Error> {
        let pending = self.pending.take()
            .expect("pending ContextPack export owns exactly one C2 waiter");
        let routed = match pending.finish().await {
            Ok(routed) => routed,
            Err(_) => return Ok(ExportContextPackOutcome::OutcomeUnknown {
                prepared: self.prepared,
                reason: ContextExportOutcomeUnknownReason::Transport,
            }),
        };
        if routed.node_id != self.route.node_id
            || routed.incarnation_id != self.route.expected_incarnation_id
        {
            return Ok(ExportContextPackOutcome::OutcomeUnknown {
                prepared: self.prepared,
                reason: ContextExportOutcomeUnknownReason::RouteMismatch,
            });
        }
        match routed.response {
            Ok(C2NodeResponse::ContextPackExported { context })
                if context_export_receipt_matches(
                    &self.route,
                    &self.source_session,
                    &self.source_provider,
                    &context,
                ) => Ok(ExportContextPackOutcome::Exported {
                    prepared: self.prepared,
                    proof: ExportedContextPackProof {
                        continuation_ref: self.authority.continuation_ref,
                        route: self.route,
                        source_session: self.source_session,
                        source_provider: self.source_provider,
                        context,
                    },
                }),
            Ok(C2NodeResponse::ContextPackExported { .. }) => {
                Ok(ExportContextPackOutcome::OutcomeUnknown {
                    prepared: self.prepared,
                    reason: ContextExportOutcomeUnknownReason::ReceiptMismatch,
                })
            }
            Err(failure) => Ok(ExportContextPackOutcome::Rejected {
                prepared: self.prepared,
                code: failure.code,
            }),
            Ok(_) => Ok(ExportContextPackOutcome::OutcomeUnknown {
                prepared: self.prepared,
                reason: ContextExportOutcomeUnknownReason::UnexpectedResponse,
            }),
        }
    }
}

#[derive(Debug)]
struct PreparedSpawnHarnessMcp {
    reservation_id: HarnessMcpReservationId,
    activation_digest: HarnessMcpActivationDigest,
    proxy_receipt: ResolvedHarnessMcpProxyReceiptV1,
    deadline_unix_ms: u64,
}

impl PreparedContinuationSpawnDispatch {
    pub(crate) fn new(
        route: NodeRoute,
        operation_id: HarnessOperationId,
        idempotency_ref: HarnessIdempotencyRef,
        spec: SpawnSpec,
        fingerprint: HarnessRequestDigest,
        delivery: &gate4agent_harness_protocol::HarnessDeliveryV1,
        continuation: &gate4agent_harness_protocol::HarnessContinuationV1,
    ) -> Result<Self, HarnessC2Error> {
        let expected_bundle = ResolvedBundleReceipt {
                id: gate4agent_node_protocol::SpawnBundleId::new(
                    delivery.bundle.bundle_id.as_str(),
                ).map_err(|_| HarnessC2Error::StagedDeliveryAuthorityMismatch)?,
                revision: gate4agent_node_protocol::SpawnBundleRevision::new(
                    delivery.bundle.revision.as_str(),
                ).map_err(|_| HarnessC2Error::StagedDeliveryAuthorityMismatch)?,
                digest: gate4agent_node_protocol::SpawnBundleDigest::new(
                    delivery.bundle.digest.as_str(),
                ).map_err(|_| HarnessC2Error::StagedDeliveryAuthorityMismatch)?,
            };
        let expected_context = harness_context_to_node(
                continuation.context.as_ref()
                    .ok_or(HarnessC2Error::ContinuationAuthorityMismatch)?,
            )?;
        let inner = PreparedSpawnDispatch::new(
            route,
            operation_id,
            idempotency_ref,
            spec,
            fingerprint,
        )?.with_expected_bundle(expected_bundle)?
            .with_expected_context(expected_context)?;
        Ok(Self { inner })
    }


    pub(crate) fn continuation_only(
        route: NodeRoute,
        operation_id: HarnessOperationId,
        idempotency_ref: HarnessIdempotencyRef,
        spec: SpawnSpec,
        fingerprint: HarnessRequestDigest,
        continuation: &gate4agent_harness_protocol::HarnessContinuationV1,
    ) -> Result<Self, HarnessC2Error> {
        let expected_context = harness_context_to_node(
                continuation.context.as_ref()
                    .ok_or(HarnessC2Error::ContinuationAuthorityMismatch)?,
            )?;
        let inner = PreparedSpawnDispatch::new(
            route,
            operation_id,
            idempotency_ref,
            spec,
            fingerprint,
        )?.with_expected_context(expected_context)?;
        Ok(Self { inner })
    }

    pub(crate) fn with_harness_mcp(
        mut self,
        reservation: &crate::HarnessMcpReservationV1,
        deadline_unix_ms: u64,
    ) -> Self {
        self.inner = self.inner.with_harness_mcp(reservation, deadline_unix_ms);
        self
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ExportContextPackOutcome {
    Exported {
        prepared: crate::PreparedContinuationExport,
        proof: ExportedContextPackProof,
    },
    Rejected {
        prepared: crate::PreparedContinuationExport,
        code: NodeFailureCode,
    },
    OutcomeUnknown {
        prepared: crate::PreparedContinuationExport,
        reason: ContextExportOutcomeUnknownReason,
    },
    ExpiredBeforeSend {
        prepared: crate::PreparedContinuationExport,
        reason: ContextExportExpiryReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContextExportOutcomeUnknownReason {
    Transport,
    RouteMismatch,
    ReceiptMismatch,
    UnexpectedResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContextExportExpiryReason {
    NodeUnavailable,
    IncarnationChanged,
    QueueUnavailable,
}

fn context_export_expiry_reason(error: &HarnessC2Error) -> ContextExportExpiryReason {
    if matches!(error, HarnessC2Error::IncarnationChanged { .. }) {
        ContextExportExpiryReason::IncarnationChanged
    } else {
        ContextExportExpiryReason::NodeUnavailable
    }
}

impl ExportedContextPackProof {
    pub(crate) fn continuation_ref(&self) -> &HarnessContinuationRef {
        &self.continuation_ref
    }
    pub(crate) fn route(&self) -> &NodeRoute { &self.route }
    pub(crate) fn source_session(&self) -> &SessionAddress { &self.source_session }
    pub(crate) fn source_provider(&self) -> &AgentId { &self.source_provider }
    pub(crate) fn context(&self) -> &ResolvedContextPackReceipt { &self.context }
}

/// An opaque proof that this adapter received and fully validated Node's exact
/// accepted SpawnSpec receipt.
///
/// Callers can inspect the resulting session but cannot construct or substitute
/// a raw `ResolvedSpawnReceipt` as H1 authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthoritativeSpawnReceipt {
    operation_id: HarnessOperationId,
    spawn_spec_fingerprint: HarnessRequestDigest,
    idempotency_ref: HarnessIdempotencyRef,
    receipt: ResolvedSpawnReceipt,
    harness_mcp_proxy: Option<ResolvedHarnessMcpProxyReceiptV1>,
}

impl AuthoritativeSpawnReceipt {
    pub fn session(&self) -> &SessionAddress {
        &self.receipt.session
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnOutcomeUnknownReason {
    Transport,
    Protocol,
    Relay,
    RoutedIdentityMismatch,
    ReceiptMismatch,
    UnexpectedResponse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnInventoryCandidates {
    pub candidates: Vec<InventorySpawnCandidate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventorySpawnCandidate {
    pub record_id: SessionRecordId,
    pub session: Option<SessionAddress>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedSpawnBindingProof {
    operation_id: HarnessOperationId,
    spawn_spec_fingerprint: HarnessRequestDigest,
    idempotency_ref: HarnessIdempotencyRef,
    node_id: NodeId,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    workspace_id: WorkspaceId,
    provider: AgentId,
    mode: SessionMode,
    record_id: SessionRecordId,
    session: SessionAddress,
    bundle: Option<ResolvedBundleReceipt>,
    context: Option<ResolvedContextPackReceipt>,
    harness_mcp_proxy: Option<ResolvedHarnessMcpProxyReceiptV1>,
}

impl AcceptedSpawnBindingProof {
    pub fn record_id(&self) -> &SessionRecordId {
        &self.record_id
    }

    pub fn session(&self) -> &SessionAddress {
        &self.session
    }

    pub(crate) fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    pub(crate) fn operation_id(&self) -> &HarnessOperationId {
        &self.operation_id
    }

    pub(crate) fn spawn_spec_fingerprint(&self) -> &HarnessRequestDigest {
        &self.spawn_spec_fingerprint
    }

    pub(crate) fn idempotency_ref(&self) -> &HarnessIdempotencyRef {
        &self.idempotency_ref
    }

    pub(crate) fn incarnation_id(&self) -> gate4agent_node_protocol::NodeIncarnationId {
        self.incarnation_id
    }

    pub(crate) fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub(crate) fn provider(&self) -> &AgentId {
        &self.provider
    }

    pub(crate) fn mode(&self) -> SessionMode {
        self.mode
    }

    pub(crate) fn bundle(&self) -> Option<&ResolvedBundleReceipt> {
        self.bundle.as_ref()
    }

    pub(crate) fn context(&self) -> Option<&ResolvedContextPackReceipt> {
        self.context.as_ref()
    }

    pub(crate) fn harness_mcp_proxy(&self) -> Option<&ResolvedHarnessMcpProxyReceiptV1> {
        self.harness_mcp_proxy.as_ref()
    }

    pub(crate) fn runtime_identity(&self) -> (u64, u64) {
        (
            self.session.session.instance_id.0,
            self.session.session.generation.0,
        )
    }
}

#[cfg(test)]
pub(crate) fn accepted_spawn_binding_proof_for_test(
    operation_id: HarnessOperationId,
    spawn_spec_fingerprint: HarnessRequestDigest,
    idempotency_ref: HarnessIdempotencyRef,
    node_id: NodeId,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    workspace_id: WorkspaceId,
    provider: AgentId,
    mode: SessionMode,
    record_id: SessionRecordId,
    session: SessionAddress,
    bundle: Option<ResolvedBundleReceipt>,
    context: Option<ResolvedContextPackReceipt>,
) -> AcceptedSpawnBindingProof {
    AcceptedSpawnBindingProof {
        operation_id,
        spawn_spec_fingerprint,
        idempotency_ref,
        node_id,
        incarnation_id,
        workspace_id,
        provider,
        mode,
        record_id,
        session,
        bundle,
        context,
        harness_mcp_proxy: None,
    }
}

#[cfg(test)]
pub(crate) fn armed_harness_mcp_reservation_proof_for_test(
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
) -> ArmedHarnessMcpReservationProof {
    ArmedHarnessMcpReservationProof { route, reservation }
}

#[cfg(test)]
pub(crate) fn accepted_harness_mcp_spawn_binding_proof_for_test(
    operation_id: HarnessOperationId,
    spawn_spec_fingerprint: HarnessRequestDigest,
    idempotency_ref: HarnessIdempotencyRef,
    node_id: NodeId,
    incarnation_id: gate4agent_node_protocol::NodeIncarnationId,
    workspace_id: WorkspaceId,
    provider: AgentId,
    mode: SessionMode,
    record_id: SessionRecordId,
    session: SessionAddress,
    bundle: Option<ResolvedBundleReceipt>,
    context: Option<ResolvedContextPackReceipt>,
    harness_mcp_proxy: ResolvedHarnessMcpProxyReceiptV1,
) -> AcceptedSpawnBindingProof {
    AcceptedSpawnBindingProof {
        operation_id,
        spawn_spec_fingerprint,
        idempotency_ref,
        node_id,
        incarnation_id,
        workspace_id,
        provider,
        mode,
        record_id,
        session,
        bundle,
        context,
        harness_mcp_proxy: Some(harness_mcp_proxy),
    }
}

fn matching_records(
    snapshot: &C2NodeSnapshot,
    baseline_record_ids: &[SessionRecordId],
    workspace_id: &WorkspaceId,
    provider: &AgentId,
    mode: SessionMode,
) -> Vec<InventorySpawnCandidate> {
    snapshot
        .session_records
        .iter()
        .filter(|record| {
            baseline_record_ids.binary_search(&record.record_id).is_err()
                && &record.workspace_id == workspace_id
                && &record.provider == provider
                && record.mode == mode
        })
        .map(|record| InventorySpawnCandidate {
            record_id: record.record_id.clone(),
            session: record.active_session.clone(),
        })
        .collect()
}

pub fn spawn_spec_fingerprint(spec: &SpawnSpec) -> Result<HarnessRequestDigest, HarnessC2Error> {
    const DOMAIN: &[u8] = b"gate4agent-harness-spawn-spec-fingerprint-v1";
    let encoded = serde_json::to_vec(spec).map_err(HarnessC2Error::SerializeSpawnSpec)?;
    let digest = local_hmac_sha256(DOMAIN, &encoded)
        .map_err(HarnessC2Error::Fingerprint)?;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    HarnessRequestDigest::new(hex).map_err(HarnessC2Error::FingerprintValidation)
}

fn validate_canonical_baseline(
    baseline_record_ids: &[SessionRecordId],
) -> Result<(), HarnessC2Error> {
    if baseline_record_ids
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
    {
        return Err(HarnessC2Error::NonCanonicalBaseline);
    }
    Ok(())
}

fn validate_spawn_route(route: &NodeRoute, spec: &SpawnSpec) -> Result<(), HarnessC2Error> {
    if spec.target.node_id != route.node_id {
        return Err(HarnessC2Error::SpawnTargetMismatch);
    }
    Ok(())
}

fn validate_prepared_spawn(prepared: &PreparedSpawnDispatch) -> Result<(), HarnessC2Error> {
    validate_spawn_route(&prepared.route, &prepared.spec)?;
    prepared.operation_id.validate().map_err(HarnessC2Error::OperationIdentity)?;
    prepared.idempotency_ref.validate().map_err(HarnessC2Error::OperationIdentity)?;
    let mut cleared = prepared.spec.clone();
    match (&prepared.expected_bundle, &prepared.spec.overrides.bundle_id) {
        (Some(expected), SpawnOverride::Set { value }) if value == &expected.id => {
            cleared.overrides.bundle_id = SpawnOverride::Clear;
        }
        (None, SpawnOverride::Clear) => {}
        _ => return Err(HarnessC2Error::NonAuthoritativeBundleOverride),
    }
    match (&prepared.expected_context, &prepared.spec.overrides.context_id) {
        (Some(expected), SpawnOverride::Set { value }) if value == &expected.id => {
            cleared.overrides.context_id = SpawnOverride::Clear;
        }
        (None, SpawnOverride::Clear) => {}
        _ => return Err(HarnessC2Error::NonAuthoritativeContextOverride),
    }
    validate_authoritative_overrides(&cleared)
}

fn validate_authoritative_overrides(spec: &SpawnSpec) -> Result<(), HarnessC2Error> {
    if !matches!(spec.overrides.provider, SpawnOverride::Set { .. }) {
        return Err(HarnessC2Error::NonAuthoritativeProviderOverride);
    }
    if !matches!(spec.overrides.mode, SpawnOverride::Set { .. }) {
        return Err(HarnessC2Error::NonAuthoritativeModeOverride);
    }
    if !matches!(spec.overrides.terminal_size, SpawnOverride::Set { .. }) {
        return Err(HarnessC2Error::NonAuthoritativeTerminalSizeOverride);
    }
    if matches!(spec.overrides.prompt, SpawnOverride::Inherit) {
        return Err(HarnessC2Error::NonAuthoritativePromptOverride);
    }
    if !matches!(spec.overrides.bundle_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeBundleOverride);
    }
    if !matches!(spec.overrides.context_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeContextOverride);
    }
    if !matches!(spec.overrides.environment_profile_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeEnvironmentOverride);
    }
    Ok(())
}

fn validate_prepared_spawn_overrides(spec: &SpawnSpec) -> Result<(), HarnessC2Error> {
    if !matches!(spec.overrides.provider, SpawnOverride::Set { .. }) {
        return Err(HarnessC2Error::NonAuthoritativeProviderOverride);
    }
    if !matches!(spec.overrides.mode, SpawnOverride::Set { .. }) {
        return Err(HarnessC2Error::NonAuthoritativeModeOverride);
    }
    if !matches!(spec.overrides.terminal_size, SpawnOverride::Set { .. }) {
        return Err(HarnessC2Error::NonAuthoritativeTerminalSizeOverride);
    }
    if matches!(spec.overrides.prompt, SpawnOverride::Inherit) {
        return Err(HarnessC2Error::NonAuthoritativePromptOverride);
    }
    if matches!(spec.overrides.bundle_id, SpawnOverride::Inherit) {
        return Err(HarnessC2Error::NonAuthoritativeBundleOverride);
    }
    if matches!(spec.overrides.context_id, SpawnOverride::Inherit) {
        return Err(HarnessC2Error::NonAuthoritativeContextOverride);
    }
    if !matches!(spec.overrides.environment_profile_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeEnvironmentOverride);
    }
    Ok(())
}

fn explicit_spawn_resolution(
    spec: &SpawnSpec,
    profile_revision: SpawnProfileRevision,
) -> Result<ResolvedSpawnSpec, HarnessC2Error> {
    let provider = match &spec.overrides.provider {
        SpawnOverride::Set { value } => value.clone(),
        SpawnOverride::Inherit | SpawnOverride::Clear => {
            return Err(HarnessC2Error::NonAuthoritativeProviderOverride);
        }
    };
    let mode = match &spec.overrides.mode {
        SpawnOverride::Set { value } => *value,
        SpawnOverride::Inherit | SpawnOverride::Clear => {
            return Err(HarnessC2Error::NonAuthoritativeModeOverride);
        }
    };
    let terminal_size = match &spec.overrides.terminal_size {
        SpawnOverride::Set { value } => *value,
        SpawnOverride::Inherit | SpawnOverride::Clear => {
            return Err(HarnessC2Error::NonAuthoritativeTerminalSizeOverride);
        }
    };
    let (prompt, prompt_provenance) = match &spec.overrides.prompt {
        SpawnOverride::Set { value } => (Some(value.clone()), SpawnFieldProvenance::Override),
        SpawnOverride::Clear => (None, SpawnFieldProvenance::Cleared),
        SpawnOverride::Inherit => {
            return Err(HarnessC2Error::NonAuthoritativePromptOverride);
        }
    };
    if !matches!(spec.overrides.bundle_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeBundleOverride);
    }
    if !matches!(spec.overrides.context_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeContextOverride);
    }
    if !matches!(spec.overrides.environment_profile_id, SpawnOverride::Clear) {
        return Err(HarnessC2Error::NonAuthoritativeEnvironmentOverride);
    }
    Ok(ResolvedSpawnSpec {
        target: spec.target.clone(),
        profile_id: spec.profile_id.clone(),
        profile_revision,
        provider,
        mode,
        terminal_size,
        prompt,
        bundle_id: None,
        context_id: None,
        environment_profile_id: None,
        deadline_ms: spec.deadline_ms,
        idempotency_key: spec.idempotency_key.clone(),
        required_capabilities: spec.required_capabilities.clone(),
        provenance: SpawnResolutionProvenance {
            provider: SpawnFieldProvenance::Override,
            mode: SpawnFieldProvenance::Override,
            terminal_size: SpawnFieldProvenance::Override,
            prompt: prompt_provenance,
            bundle_id: SpawnFieldProvenance::Cleared,
            context_id: SpawnFieldProvenance::Cleared,
            environment_profile_id: SpawnFieldProvenance::Cleared,
        },
    })
}

fn validate_spawn_receipt(
    route: &NodeRoute,
    expected: &ResolvedSpawnSpec,
    expected_bundle: Option<&ResolvedBundleReceipt>,
    expected_context: Option<&ResolvedContextPackReceipt>,
    receipt: &ResolvedSpawnReceipt,
) -> bool {
    receipt.harness_mcp_proxy.is_none()
        && receipt.incarnation_id == route.expected_incarnation_id
        && receipt.target == expected.target
        && receipt.profile_id == expected.profile_id
        && receipt.profile_revision == expected.profile_revision
        && receipt.provider == expected.provider
        && receipt.mode == expected.mode
        && receipt.terminal_size == expected.terminal_size
        && receipt.prompt == SpawnPromptMetadata::from_prompt(expected.prompt.as_ref())
        && receipt.bundle_id == expected.bundle_id
        && receipt.bundle.as_ref() == expected_bundle
        && receipt.context_id == expected.context_id
        && receipt.context.as_ref() == expected_context
        && expected.environment_profile_id.is_none()
        && receipt.environment_profile.is_none()
        && receipt.deadline_ms == expected.deadline_ms
        && receipt.idempotency_key == expected.idempotency_key
        && receipt.required_capabilities == expected.required_capabilities
        && receipt.provenance == expected.provenance
        && receipt.context_binding_is_valid()
}

fn harness_binding_session(
    binding: &gate4agent_harness_protocol::HarnessSessionBindingV1,
) -> Result<SessionAddress, HarnessC2Error> {
    let active = match &binding.session {
        gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
            active_session: Some(active),
            ..
        } => active,
        _ => return Err(HarnessC2Error::ContinuationAuthorityMismatch),
    };
    Ok(SessionAddress {
        workspace_id: WorkspaceId::new(binding.workspace_id.as_str())
            .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
        session: gate4agent_node_protocol::SessionKey {
            instance_id: gate4agent_types::AgentInstanceId(active.instance_id),
            generation: gate4agent_types::SessionGeneration(active.generation),
        },
    })
}

fn context_export_receipt_matches(
    route: &NodeRoute,
    source_session: &SessionAddress,
    source_provider: &AgentId,
    context: &ResolvedContextPackReceipt,
) -> bool {
    context.is_valid()
        && context.lineage.source_node_id == route.node_id
        && &context.lineage.source_session == source_session
        && &context.lineage.source_provider == source_provider
}

pub(crate) fn harness_context_to_node(
    context: &gate4agent_harness_protocol::HarnessResolvedContextPackReceiptV1,
) -> Result<ResolvedContextPackReceipt, HarnessC2Error> {
    Ok(ResolvedContextPackReceipt {
        id: gate4agent_node_protocol::SpawnContextId::new(context.id.as_str())
            .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
        digest: gate4agent_node_protocol::SpawnContextDigest::new(&context.digest)
            .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
        lineage: gate4agent_node_protocol::ContextPackLineageReceipt {
            source_node_id: NodeId::new(context.lineage.source_node_id.as_str())
                .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
            source_session: SessionAddress {
                workspace_id: WorkspaceId::new(context.lineage.source_workspace_id.as_str())
                    .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
                session: gate4agent_node_protocol::SessionKey {
                    instance_id: gate4agent_types::AgentInstanceId(
                        context.lineage.source_instance_id,
                    ),
                    generation: gate4agent_types::SessionGeneration(
                        context.lineage.source_generation,
                    ),
                },
            },
            source_provider: AgentId::new(context.lineage.source_provider.as_str())
                .map_err(|_| HarnessC2Error::ContinuationAuthorityMismatch)?,
        },
        source_message_count: context.source_message_count,
        retained_message_count: context.retained_message_count,
        byte_len: context.byte_len,
        truncated: context.truncated,
    })
}

fn unknown_reason(error: &C2ControlError) -> SpawnOutcomeUnknownReason {
    match error {
        C2ControlError::Protocol(_) => SpawnOutcomeUnknownReason::Protocol,
        C2ControlError::Relay(_) => SpawnOutcomeUnknownReason::Relay,
        _ => SpawnOutcomeUnknownReason::Transport,
    }
}

#[derive(Debug, Error)]
pub enum HarnessC2Error {
    #[error("C2 connection failed: {0}")]
    Connect(C2ControlError),
    #[error("C2 inventory transport failed: {0}")]
    InventoryTransport(C2ControlError),
    #[error("C2 delivery outcome is unknown after transport, protocol, or relay failure: {0}")]
    DeliveryTransport(C2ControlError),
    #[error("compiled delivery bundle is invalid: {0}")]
    InvalidCompiledDelivery(String),
    #[error("delivery manifest is invalid: {0}")]
    InvalidDeliveryManifest(String),
    #[error("C2 delivery was rejected with {code:?} ({category:?})")]
    DeliveryRejected {
        code: NodeFailureCode,
        category: DeliveryFailureCategory,
    },
    #[error("C2 returned an unexpected delivery response")]
    UnexpectedDeliveryResponse,
    #[error("C2 delivery reply route or incarnation does not match the request")]
    DeliveryRouteMismatch,
    #[error("C2 delivery response does not exactly correlate with the request")]
    DeliveryCorrelationMismatch,
    #[error("unknown C2 node {0}")]
    UnknownNode(NodeId),
    #[error("C2 node {0} is not online")]
    NodeOffline(NodeId),
    #[error("C2 node {0} has no current incarnation")]
    MissingIncarnation(NodeId),
    #[error("C2 node {node_id} changed incarnation")]
    IncarnationChanged { node_id: NodeId },
    #[error("C2 inventory request was rejected with {code:?}")]
    InventoryRejected {
        code: gate4agent_node_protocol::NodeFailureCode,
    },
    #[error("C2 returned an unexpected inventory response")]
    UnexpectedInventoryResponse,
    #[error("C2 observation resync transport failed: {0}")]
    ObservationTransport(C2ControlError),
    #[error("C2 observation resync was rejected with {code:?}")]
    ObservationRejected { code: gate4agent_node_protocol::NodeFailureCode },
    #[error("C2 returned an unexpected observation resync response")]
    UnexpectedObservationResponse,
    #[error("C2 returned an invalid observation resync authority")]
    InvalidObservationResync,
    #[error("C2 topology watch closed")]
    TopologyClosed,
    #[error("SpawnSpec target does not match its exact C2 route")]
    SpawnTargetMismatch,
    #[error("SpawnSpec was not enqueued on the bounded C2 control queue: {0}")]
    SpawnEnqueue(C2ControlError),
    #[error("SpawnSpec provider override is not an explicit authoritative value")]
    NonAuthoritativeProviderOverride,
    #[error("SpawnSpec mode override is not an explicit authoritative value")]
    NonAuthoritativeModeOverride,
    #[error("SpawnSpec terminal-size override is not an explicit authoritative value")]
    NonAuthoritativeTerminalSizeOverride,
    #[error("SpawnSpec prompt override is not an explicit authoritative value")]
    NonAuthoritativePromptOverride,
    #[error("SpawnSpec bundle override cannot produce exact H1 authority")]
    NonAuthoritativeBundleOverride,
    #[error("SpawnSpec context override cannot produce exact H1 authority")]
    NonAuthoritativeContextOverride,
    #[error("durable continuation authority does not match the exact request")]
    ContinuationAuthorityMismatch,
    #[error("unable to persist the exact continuation export outcome: {0}")]
    ContinuationPersistence(crate::HarnessServiceError),
    #[error("ContextPack export transport failed after typed C2 invocation: {0}")]
    ContextExportTransport(C2ControlError),
    #[error("ContextPack export reply route or incarnation does not match")]
    ContextExportRouteMismatch,
    #[error("ContextPack export receipt lineage does not match source session/provider")]
    ContextExportLineageMismatch,
    #[error("Node rejected ContextPack export with {code:?}")]
    ContextExportRejected { code: NodeFailureCode },
    #[error("C2 returned an unexpected ContextPack export response")]
    UnexpectedContextExportResponse,
    #[error("SpawnSpec environment override cannot produce exact H1 authority")]
    NonAuthoritativeEnvironmentOverride,
    #[error("SpawnSpec is not authorized by the exact durable staged delivery and dispatch context")]
    StagedDeliveryAuthorityMismatch,
    #[error("SpawnSpec profile {0} is unavailable in exact Node launch inventory")]
    SpawnProfileUnavailable(gate4agent_node_protocol::SpawnProfileId),
    #[error("SpawnSpec profile preflight does not match the exact leased request")]
    SpawnProfileAuthorityMismatch,
    #[error("invalid durable operation identity: {0}")]
    OperationIdentity(gate4agent_harness_protocol::HarnessValidationError),
    #[error("SpawnSpec serialization failed: {0}")]
    SerializeSpawnSpec(serde_json::Error),
    #[error("SpawnSpec fingerprint failed: {0}")]
    Fingerprint(String),
    #[error("SpawnSpec fingerprint validation failed: {0}")]
    FingerprintValidation(gate4agent_harness_protocol::HarnessValidationError),
    #[error("spawn baseline record IDs are not strictly sorted and duplicate-free")]
    NonCanonicalBaseline,
    #[error("accepted spawn receipt does not match the exact route")]
    AcceptedReceiptRouteMismatch,
    #[error("accepted spawn receipt has no exact managed record")]
    AcceptedReceiptRecordMissing,
    #[error("accepted spawn receipt matches multiple managed records")]
    AcceptedReceiptRecordAmbiguous,
    #[error("harness MCP durable authority does not match the exact request")]
    HarnessMcpAuthorityMismatch,
    #[error("harness MCP Arm was not enqueued on the bounded C2 control queue: {0}")]
    HarnessMcpArmEnqueue(C2ControlError),
    #[error("harness MCP request transport failed: {0}")]
    HarnessMcpTransport(C2ControlError),
    #[error("harness MCP response route does not match the exact request route")]
    HarnessMcpRouteMismatch,
    #[error("harness MCP response did not exactly correlate with the request")]
    HarnessMcpCorrelationMismatch,
    #[error("C2 returned an unexpected harness MCP response")]
    UnexpectedHarnessMcpResponse,
    #[error("Node rejected the harness MCP request with {code:?}")]
    HarnessMcpRejected { code: NodeFailureCode },
    #[error("native history request is invalid")]
    InvalidNativeHistoryRequest,
    #[error("native history request was not enqueued: {0}")]
    NativeHistoryEnqueue(C2ControlError),
    #[error("native history transport failed: {0}")]
    NativeHistoryTransport(C2ControlError),
    #[error("native history request deadline elapsed")]
    NativeHistoryDeadline,
    #[error("native history response route or incarnation does not match")]
    NativeHistoryRouteMismatch,
    #[error("native history response does not exactly correlate with the request")]
    NativeHistoryCorrelationMismatch,
    #[error("Node rejected native history request with {code:?}")]
    NativeHistoryRejected { code: NodeFailureCode },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum C2StartFailureCategory {
    QueueFullNotSent,
    ClosedNotSent,
    RejectedBeforeSend,
}

impl HarnessC2Error {
    pub(crate) fn start_failure_category(&self) -> Option<C2StartFailureCategory> {
        let error = match self {
            Self::SpawnEnqueue(error) | Self::HarnessMcpArmEnqueue(error) => error,
            _ => return None,
        };
        Some(match error {
            C2ControlError::QueueFull => C2StartFailureCategory::QueueFullNotSent,
            C2ControlError::Closed => C2StartFailureCategory::ClosedNotSent,
            _ => C2StartFailureCategory::RejectedBeforeSend,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_c2_protocol::C2TopologyNode;
    use gate4agent_node_protocol::{
        CapabilityId, ContextPackLineageReceipt, NodeIncarnationId,
        ResolvedBundleReceipt, ResolvedContextPackReceipt,
        ManagedSessionState, ResolvedEnvironmentProfileReceipt, SessionKey, SpawnBundleDigest,
        SpawnBundleId, SpawnBundleRevision, SpawnContextDigest, SpawnContextId,
        SpawnDeadlineMs, SpawnEnvironmentProfileId, SpawnEnvironmentProfileRevision,
        SpawnIdempotencyKey, SpawnOverrides, SpawnProfileId, SpawnProfileRevision,
        SpawnPromptMetadata, SpawnRequiredCapabilities, SpawnTarget,
    };
    use gate4agent_types::{AgentInstanceId, SessionGeneration, TerminalSize};

    fn observation_snapshot(node_id: &NodeId) -> C2NodeSnapshot {
        C2NodeSnapshot {
            node_id: node_id.clone(),
            enabled_providers: Vec::new(),
            provider_runtime_statuses: Default::default(),
            workspaces: Vec::new(),
            session_records: Vec::new(),
            agent_progress: Vec::new(),
            managed_worktrees: Vec::new(),
            launch_inventory: None,
            observation_support: Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: true,
            }),
        }
    }

    fn topology_node(
        node_id: &str,
        transport: NodeTransportState,
        incarnation: Option<NodeIncarnationId>,
        support: Option<C2ObservationSupport>,
    ) -> C2TopologyNode {
        C2TopologyNode {
            node_id: NodeId::new(node_id).unwrap(),
            endpoint: format!(r"\\.\pipe\{node_id}"),
            relay_route: gate4agent_c2_protocol::C2RelayRoute::LocalIpc,
            transport,
            current_incarnation_id: incarnation,
            provider_contracts: Vec::new(),
            provider_adapter_contracts: Vec::new(),
            provider_runtime_statuses: Default::default(),
            observation_support: support,
        }
    }

    fn explicit_spec() -> SpawnSpec {
        SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("primary").unwrap(),
                worktree_id: Some(WorkspaceId::new("review-tree").unwrap()),
            },
            profile_id: SpawnProfileId::new("claude").unwrap(),
            expected_profile_revision: SpawnProfileRevision::new("r1").unwrap(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Set {
                    value: AgentId::new("claude").unwrap(),
                },
                mode: SpawnOverride::Set { value: SessionMode::Pty },
                terminal_size: SpawnOverride::Set {
                    value: TerminalSize { rows: 24, columns: 80 },
                },
                prompt: SpawnOverride::Clear,
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Clear,
                environment_profile_id: SpawnOverride::Clear,
            },
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("h1-proof").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
        }
    }

    fn accepted_receipt(spec: &SpawnSpec) -> ResolvedSpawnReceipt {
        explicit_spawn_resolution(spec, SpawnProfileRevision::new("r1").unwrap())
        .unwrap()
        .receipt(
            NodeIncarnationId::from_bytes([7; 16]),
            SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
        )
    }

    fn specialized_spawn_case(
        with_bundle: bool,
        with_context: bool,
        with_harness_mcp: bool,
    ) -> (SpawnResponseCorrelation, gate4agent_c2_protocol::RoutedNodeResponse) {
        let mut spec = explicit_spec();
        let bundle = with_bundle.then(|| ResolvedBundleReceipt {
            id: SpawnBundleId::new("bundle-specialized").unwrap(),
            revision: SpawnBundleRevision::new("r1").unwrap(),
            digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
        });
        let context = with_context.then(|| ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-specialized").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("source-node").unwrap(),
                source_session: SessionAddress {
                    workspace_id: WorkspaceId::new("source-workspace").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(9),
                        generation: SessionGeneration(2),
                    },
                },
                source_provider: AgentId::new("codex").unwrap(),
            },
            source_message_count: 3,
            retained_message_count: 2,
            byte_len: 64,
            truncated: true,
        });
        if let Some(bundle) = &bundle {
            spec.overrides.bundle_id = SpawnOverride::Set { value: bundle.id.clone() };
        }
        if let Some(context) = &context {
            spec.overrides.context_id = SpawnOverride::Set { value: context.id.clone() };
        }
        let route = NodeRoute {
            node_id: spec.target.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let mut cleared_spec = spec;
        cleared_spec.overrides.bundle_id = SpawnOverride::Clear;
        cleared_spec.overrides.context_id = SpawnOverride::Clear;
        let mut expected = explicit_spawn_resolution(
            &cleared_spec,
            SpawnProfileRevision::new("r1").unwrap(),
        ).unwrap();
        if let Some(bundle) = &bundle {
            expected.bundle_id = Some(bundle.id.clone());
            expected.provenance.bundle_id = SpawnFieldProvenance::Override;
        }
        if let Some(context) = &context {
            expected.context_id = Some(context.id.clone());
            expected.provenance.context_id = SpawnFieldProvenance::Override;
        }
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "c".repeat(24),
        )).unwrap();
        let activation_digest = HarnessMcpActivationDigest::new(format!(
            "sha256:{}",
            "d".repeat(64),
        )).unwrap();
        let proxy = ResolvedHarnessMcpProxyReceiptV1 {
            reservation_id: reservation_id.clone(),
            activation_digest: activation_digest.clone(),
        };
        let mut receipt = expected.receipt_with_materialization(
            route.expected_incarnation_id,
            SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
            None,
            bundle.clone(),
            context.clone(),
        );
        let expected_proxy = with_harness_mcp.then(|| {
            receipt.harness_mcp_proxy = Some(proxy.clone());
            (reservation_id.clone(), activation_digest.clone(), proxy)
        });
        let response = if with_harness_mcp {
            C2NodeResponse::Spawned {
                reservation_id,
                activation_digest,
                receipt,
            }
        } else {
            C2NodeResponse::SpawnSpecAccepted { receipt }
        };
        (
            SpawnResponseCorrelation {
                route: route.clone(),
                operation_id: HarnessOperationId::new(format!(
                    "hop_{}",
                    "e".repeat(24),
                )).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}",
                    "f".repeat(24),
                )).unwrap(),
                cleared_spec,
                profile_revision: SpawnProfileRevision::new("r1").unwrap(),
                fingerprint: HarnessRequestDigest::new("1".repeat(64)).unwrap(),
                expected_bundle: bundle,
                expected_context: context,
                expected_proxy,
            },
            gate4agent_c2_protocol::RoutedNodeResponse {
                node_id: route.node_id,
                incarnation_id: route.expected_incarnation_id,
                response: Ok(response),
            },
        )
    }

    #[test]
    fn specialized_c2_spawn_matrix_is_exact_and_one_shot() {
        for with_bundle in [false, true] {
            for with_context in [false, true] {
                for with_harness_mcp in [false, true] {
                    let (correlation, routed) = specialized_spawn_case(
                        with_bundle,
                        with_context,
                        with_harness_mcp,
                    );
                    assert!(matches!(
                        correlate_spawn_response(correlation, Ok(routed)).unwrap(),
                        SpawnDispatchOutcome::Accepted(_),
                    ));
                }
            }
        }

        let (correlation, mut wrong_route) = specialized_spawn_case(true, true, true);
        wrong_route.node_id = NodeId::new("wrong-node").unwrap();
        assert_eq!(
            correlate_spawn_response(correlation, Ok(wrong_route)).unwrap(),
            SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::RoutedIdentityMismatch,
            },
        );

        let (correlation, mut wrong_receipt) = specialized_spawn_case(true, true, true);
        let Ok(C2NodeResponse::Spawned { receipt, .. }) = &mut wrong_receipt.response else {
            panic!("H3B fixture must use Spawned");
        };
        receipt.context.as_mut().unwrap().byte_len += 1;
        assert_eq!(
            correlate_spawn_response(correlation, Ok(wrong_receipt)).unwrap(),
            SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::ReceiptMismatch,
            },
        );

        let (correlation, mut wrong_structure) = specialized_spawn_case(false, false, true);
        let Ok(C2NodeResponse::Spawned { receipt, .. }) = wrong_structure.response else {
            panic!("H3B fixture must use Spawned");
        };
        wrong_structure.response = Ok(C2NodeResponse::SpawnSpecAccepted { receipt });
        assert_eq!(
            correlate_spawn_response(correlation, Ok(wrong_structure)).unwrap(),
            SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::UnexpectedResponse,
            },
        );
    }

    #[test]
    fn prepared_spawn_rejects_non_authoritative_route_and_overrides_before_enqueue() {
        let mut spec = explicit_spec();
        let operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "a".repeat(24),
        )).unwrap();
        let idempotency_ref = HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            "b".repeat(24),
        )).unwrap();
        let fingerprint = HarnessRequestDigest::new("c".repeat(64)).unwrap();
        let wrong_route = NodeRoute {
            node_id: NodeId::new("node-b").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        assert!(matches!(
            PreparedSpawnDispatch::new(
                wrong_route,
                operation_id.clone(),
                idempotency_ref.clone(),
                spec.clone(),
                fingerprint.clone(),
            ),
            Err(HarnessC2Error::SpawnTargetMismatch),
        ));
        let route = NodeRoute {
            node_id: spec.target.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let mut inherited_provider = spec.clone();
        inherited_provider.overrides.provider = SpawnOverride::Inherit;
        assert!(matches!(
            PreparedSpawnDispatch::new(
                route.clone(),
                operation_id.clone(),
                idempotency_ref.clone(),
                inherited_provider,
                fingerprint.clone(),
            ),
            Err(HarnessC2Error::NonAuthoritativeProviderOverride),
        ));
        let mut cleared_mode = spec.clone();
        cleared_mode.overrides.mode = SpawnOverride::Clear;
        assert!(matches!(
            PreparedSpawnDispatch::new(
                route.clone(),
                operation_id.clone(),
                idempotency_ref.clone(),
                cleared_mode,
                fingerprint.clone(),
            ),
            Err(HarnessC2Error::NonAuthoritativeModeOverride),
        ));
        let expected = ResolvedBundleReceipt {
            id: SpawnBundleId::new("bundle-specialized").unwrap(),
            revision: SpawnBundleRevision::new("r1").unwrap(),
            digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
        };
        spec.overrides.bundle_id = SpawnOverride::Set {
            value: expected.id.clone(),
        };
        let prepared = PreparedSpawnDispatch::new(
            route.clone(),
            operation_id.clone(),
            idempotency_ref.clone(),
            spec.clone(),
            fingerprint.clone(),
        ).unwrap();
        assert!(matches!(
            validate_prepared_spawn(&prepared),
            Err(HarnessC2Error::NonAuthoritativeBundleOverride),
        ));
        let exact = PreparedSpawnDispatch::new(
            route,
            operation_id,
            idempotency_ref,
            spec,
            fingerprint,
        ).unwrap().with_expected_bundle(expected).unwrap();
        assert!(validate_prepared_spawn(&exact).is_ok());
    }

    #[test]
    fn preflight_r1_prepared_spawn_cannot_send_or_accept_after_r2() {
        let spec = explicit_spec();
        let route = NodeRoute {
            node_id: spec.target.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let r1 = SpawnProfileRevisionProof {
            route: route.clone(),
            profile_id: spec.profile_id.clone(),
            profile_revision: SpawnProfileRevision::new("r1").unwrap(),
        };
        let bound = r1.bind_spec(spec).unwrap();
        let fingerprint = spawn_spec_fingerprint(&bound).unwrap();
        let prepared = PreparedSpawnDispatch::new(
            route.clone(),
            HarnessOperationId::new(format!("hop_{}", "7".repeat(24))).unwrap(),
            HarnessIdempotencyRef::new(format!("hidem_{}", "8".repeat(24))).unwrap(),
            bound.clone(),
            fingerprint.clone(),
        ).unwrap();
        let r2 = SpawnProfileRevisionProof {
            route: route.clone(),
            profile_id: bound.profile_id.clone(),
            profile_revision: SpawnProfileRevision::new("r2").unwrap(),
        };
        assert!(matches!(
            validate_prepared_spawn_profile(&prepared, &r2),
            Err(HarnessC2Error::SpawnProfileAuthorityMismatch),
        ));

        let mut receipt = accepted_receipt(&bound);
        receipt.profile_revision = SpawnProfileRevision::new("r2").unwrap();
        let correlation = SpawnResponseCorrelation {
            route: route.clone(),
            operation_id: HarnessOperationId::new(format!("hop_{}", "7".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(
                format!("hidem_{}", "8".repeat(24)),
            ).unwrap(),
            cleared_spec: bound,
            profile_revision: SpawnProfileRevision::new("r1").unwrap(),
            fingerprint,
            expected_bundle: None,
            expected_context: None,
            expected_proxy: None,
        };
        let routed = gate4agent_c2_protocol::RoutedNodeResponse {
            node_id: route.node_id,
            incarnation_id: route.expected_incarnation_id,
            response: Ok(C2NodeResponse::SpawnSpecAccepted { receipt }),
        };
        assert_eq!(
            correlate_spawn_response(correlation, Ok(routed)).unwrap(),
            SpawnDispatchOutcome::OutcomeUnknown {
                reason: SpawnOutcomeUnknownReason::ReceiptMismatch,
            },
        );
    }

    #[test]
    fn h3b_arm_correlation_and_bounded_start_failures_are_exact() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "a".repeat(24),
        )).unwrap();
        let activation_digest = HarnessMcpActivationDigest::new(format!(
            "sha256:{}",
            "b".repeat(64),
        )).unwrap();
        let exact = gate4agent_c2_protocol::RoutedNodeResponse {
            node_id: route.node_id.clone(),
            incarnation_id: route.expected_incarnation_id,
            response: Ok(C2NodeResponse::Armed {
                reservation_id: reservation_id.clone(),
                activation_digest: activation_digest.clone(),
                expires_at_unix_ms: 55,
            }),
        };
        assert!(validate_harness_mcp_arm_response(
            &route,
            &reservation_id,
            &activation_digest,
            55,
            exact.clone(),
        ).is_ok());
        assert!(matches!(
            validate_harness_mcp_arm_response(
                &route,
                &reservation_id,
                &activation_digest,
                56,
                exact,
            ),
            Err(HarnessC2Error::HarnessMcpCorrelationMismatch),
        ));
        assert_eq!(
            HarnessC2Error::SpawnEnqueue(C2ControlError::QueueFull)
                .start_failure_category(),
            Some(C2StartFailureCategory::QueueFullNotSent),
        );
        assert_eq!(
            HarnessC2Error::HarnessMcpArmEnqueue(C2ControlError::Closed)
                .start_failure_category(),
            Some(C2StartFailureCategory::ClosedNotSent),
        );
        assert_eq!(
            HarnessC2Error::HarnessMcpArmEnqueue(C2ControlError::Protocol(
                "capability rejected before enqueue".to_owned(),
            )).start_failure_category(),
            Some(C2StartFailureCategory::RejectedBeforeSend),
        );
        assert_eq!(
            HarnessC2Error::HarnessMcpTransport(C2ControlError::Closed)
                .start_failure_category(),
            None,
        );
    }

    #[test]
    fn h3a_delivery_response_correlation_and_failure_categories_are_exact() {
        use gate4agent_node_protocol::{
            DeliveryBlobReceiptV1, DeliveryComponentKindV2, DeliveryComponentV2,
            DeliveryManifestDigestV2, DeliveryRelativePathV2, DeliveryScopeV2,
            SpawnBundleDigest, SpawnBundleId, SpawnBundleRevision,
        };

        let blob_digest = DeliveryBlobDigestV1::new(format!(
            "sha256:{}",
            "a".repeat(64),
        )).unwrap();
        let other_blob_digest = DeliveryBlobDigestV1::new(format!(
            "sha256:{}",
            "b".repeat(64),
        )).unwrap();
        let manifest_digest = DeliveryManifestDigestV2::new(format!(
            "sha256:{}",
            "c".repeat(64),
        )).unwrap();
        let manifest = DeliveryBundleManifestV2 {
            bundle_id: SpawnBundleId::new("bundle.h3a").unwrap(),
            revision: SpawnBundleRevision::new("revision-1").unwrap(),
            bundle_digest: SpawnBundleDigest::new(format!(
                "sha256:{}",
                "d".repeat(64),
            )).unwrap(),
            manifest_digest: manifest_digest.clone(),
            components: vec![DeliveryComponentV2 {
                kind: DeliveryComponentKindV2::File,
                scope: DeliveryScopeV2::Workspace,
                relative_path: DeliveryRelativePathV2::new("reviewed/payload.bin").unwrap(),
                blob: DeliveryBlobReceiptV1::new(blob_digest.clone(), 7).unwrap(),
            }],
        };
        assert!(delivery_stage_begin_matches(
            &manifest,
            &manifest_digest,
            std::slice::from_ref(&blob_digest),
        ));
        assert!(!delivery_stage_begin_matches(
            &manifest,
            &DeliveryManifestDigestV2::new(format!("sha256:{}", "e".repeat(64))).unwrap(),
            std::slice::from_ref(&blob_digest),
        ));
        assert!(!delivery_stage_begin_matches(
            &manifest,
            &manifest_digest,
            std::slice::from_ref(&other_blob_digest),
        ));

        let stage_id = DeliveryStageId::from_nonce([1; 16]);
        let other_stage_id = DeliveryStageId::from_nonce([2; 16]);
        assert!(delivery_chunk_reply_matches(
            &stage_id,
            &blob_digest,
            55,
            &stage_id,
            &blob_digest,
            55,
        ));
        assert!(!delivery_chunk_reply_matches(
            &stage_id,
            &blob_digest,
            55,
            &other_stage_id,
            &blob_digest,
            55,
        ));
        assert!(!delivery_chunk_reply_matches(
            &stage_id,
            &blob_digest,
            55,
            &stage_id,
            &other_blob_digest,
            55,
        ));
        assert!(!delivery_chunk_reply_matches(
            &stage_id,
            &blob_digest,
            55,
            &stage_id,
            &blob_digest,
            54,
        ));

        let receipt = DeliveryCommitReceiptV1 {
            bundle_id: manifest.bundle_id.clone(),
            revision: manifest.revision.clone(),
            bundle_digest: manifest.bundle_digest.clone(),
            manifest_digest: manifest.manifest_digest.clone(),
            blobs: vec![DeliveryBlobReceiptV1::new(blob_digest.clone(), 7).unwrap()],
        };
        assert!(delivery_receipt_matches_manifest(&receipt, &manifest));
        let mut changed = receipt;
        changed.blobs[0].byte_len = 8;
        assert!(!delivery_receipt_matches_manifest(&changed, &manifest));

        let cases = [
            (NodeFailureCode::DeliveryManifestInvalid, DeliveryFailureCategory::Validation),
            (NodeFailureCode::DeliveryBlobUnexpected, DeliveryFailureCategory::Validation),
            (NodeFailureCode::UnknownDeliveryStage, DeliveryFailureCategory::StageConflict),
            (NodeFailureCode::DeliveryStageConflict, DeliveryFailureCategory::StageConflict),
            (NodeFailureCode::DeliveryChunkOutOfOrder, DeliveryFailureCategory::StageConflict),
            (NodeFailureCode::DeliveryStageIncomplete, DeliveryFailureCategory::StageConflict),
            (NodeFailureCode::DeliveryBlobDigestMismatch, DeliveryFailureCategory::Integrity),
            (NodeFailureCode::DeliveryBundleDigestMismatch, DeliveryFailureCategory::Integrity),
            (NodeFailureCode::DeliveryStageStorageFailed, DeliveryFailureCategory::Storage),
        ];
        for (code, category) in cases {
            assert_eq!(delivery_failure_category(code), category);
        }
        assert_eq!(
            delivery_failure_category(NodeFailureCode::UnsupportedCapability),
            DeliveryFailureCategory::Other,
        );
    }

    #[test]
    fn authoritative_receipt_rejects_wrong_provider_mode_and_full_target() {
        let spec = explicit_spec();
        let route = NodeRoute {
            node_id: spec.target.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let expected = explicit_spawn_resolution(
            &spec,
            SpawnProfileRevision::new("r1").unwrap(),
        )
        .unwrap();
        let receipt = accepted_receipt(&spec);
        assert!(validate_spawn_receipt(
            &route,
            &expected,
            None,
            None,
            &receipt,
        ));
        let rejects = |candidate: &ResolvedSpawnReceipt| {
            assert!(!validate_spawn_receipt(
                &route,
                &expected,
                None,
                None,
                candidate,
            ));
        };

        let mut wrong_incarnation = receipt.clone();
        wrong_incarnation.incarnation_id = NodeIncarnationId::from_bytes([8; 16]);
        rejects(&wrong_incarnation);

        let mut wrong_provider = receipt.clone();
        wrong_provider.provider = AgentId::new("codex").unwrap();
        rejects(&wrong_provider);

        let mut wrong_mode = receipt.clone();
        wrong_mode.mode = SessionMode::Inline;
        rejects(&wrong_mode);

        let mut wrong_node = receipt.clone();
        wrong_node.target.node_id = NodeId::new("node-b").unwrap();
        rejects(&wrong_node);

        let mut wrong_workspace = receipt.clone();
        wrong_workspace.target.workspace_id = WorkspaceId::new("secondary").unwrap();
        rejects(&wrong_workspace);

        let mut wrong_worktree = receipt.clone();
        wrong_worktree.target.worktree_id = None;
        rejects(&wrong_worktree);

        let mut wrong_profile = receipt.clone();
        wrong_profile.profile_id = SpawnProfileId::new("codex").unwrap();
        rejects(&wrong_profile);

        let mut wrong_profile_revision = receipt.clone();
        wrong_profile_revision.profile_revision = SpawnProfileRevision::new("r2").unwrap();
        rejects(&wrong_profile_revision);

        let mut wrong_terminal = receipt.clone();
        wrong_terminal.terminal_size = TerminalSize { rows: 25, columns: 80 };
        rejects(&wrong_terminal);

        let mut wrong_prompt = receipt.clone();
        wrong_prompt.prompt = SpawnPromptMetadata { present: true, byte_len: 1 };
        rejects(&wrong_prompt);

        let mut wrong_bundle_id = receipt.clone();
        wrong_bundle_id.bundle_id = Some(SpawnBundleId::new("bundle-a").unwrap());
        rejects(&wrong_bundle_id);

        let mut wrong_bundle = receipt.clone();
        wrong_bundle.bundle = Some(ResolvedBundleReceipt {
            id: SpawnBundleId::new("bundle-a").unwrap(),
            revision: SpawnBundleRevision::new("r1").unwrap(),
            digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
        });
        rejects(&wrong_bundle);

        let mut wrong_context_id = receipt.clone();
        wrong_context_id.context_id = Some(SpawnContextId::new("context-a").unwrap());
        rejects(&wrong_context_id);

        let mut wrong_context = receipt.clone();
        wrong_context.context = Some(ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-a").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("source-node").unwrap(),
                source_session: receipt.session.clone(),
                source_provider: AgentId::new("claude").unwrap(),
            },
            source_message_count: 1,
            retained_message_count: 1,
            byte_len: 1,
            truncated: false,
        });
        rejects(&wrong_context);

        let mut wrong_environment = receipt.clone();
        wrong_environment.environment_profile = Some(ResolvedEnvironmentProfileReceipt {
            profile_id: SpawnEnvironmentProfileId::new("environment-a").unwrap(),
            profile_revision: SpawnEnvironmentProfileRevision::new("r1").unwrap(),
        });
        rejects(&wrong_environment);

        let mut wrong_deadline = receipt.clone();
        wrong_deadline.deadline_ms = SpawnDeadlineMs::new(6_000).unwrap();
        rejects(&wrong_deadline);

        let mut wrong_idempotency = receipt.clone();
        wrong_idempotency.idempotency_key = SpawnIdempotencyKey::new("other-key").unwrap();
        rejects(&wrong_idempotency);

        let mut wrong_capabilities = receipt.clone();
        wrong_capabilities.required_capabilities = SpawnRequiredCapabilities::new([
            CapabilityId::new("fixture-capability").unwrap(),
        ])
        .unwrap();
        rejects(&wrong_capabilities);

        let mut wrong_provenance = receipt;
        wrong_provenance.provenance.provider = SpawnFieldProvenance::Profile;
        rejects(&wrong_provenance);
    }

    #[test]
    fn spawn_inventory_candidates_are_read_only_and_never_create_binding_authority() {
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let session = |instance_id| SessionAddress {
            workspace_id: workspace_id.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(instance_id),
                generation: SessionGeneration(1),
            },
        };
        let record = |record_id: &str, provider: &str, instance_id| {
            C2ManagedSessionRecord {
                record_id: SessionRecordId::new(record_id).unwrap(),
                display_name: record_id.to_owned(),
                provider: AgentId::new(provider).unwrap(),
                mode: SessionMode::Pty,
                state: ManagedSessionState::Live,
                workspace_id: workspace_id.clone(),
                active_session: Some(session(instance_id)),
                environment_profile: None,
                bundle: None,
                context_id: None,
                context: None,
                task_binding: None,
                provider_identity_present: true,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 2,
            }
        };
        let mut snapshot = observation_snapshot(&NodeId::new("node-a").unwrap());
        snapshot.session_records = vec![
            record("record-baseline", "claude", 1),
            record("record-candidate", "claude", 2),
            record("record-other-provider", "codex", 3),
        ];
        let before = snapshot.clone();
        let baseline = vec![SessionRecordId::new("record-baseline").unwrap()];
        let candidates = matching_records(
            &snapshot,
            &baseline,
            &workspace_id,
            &AgentId::new("claude").unwrap(),
            SessionMode::Pty,
        );
        assert_eq!(candidates, vec![InventorySpawnCandidate {
            record_id: SessionRecordId::new("record-candidate").unwrap(),
            session: Some(session(2)),
        }]);
        assert_eq!(snapshot, before);
        assert!(validate_canonical_baseline(&baseline).is_ok());
        assert!(matches!(
            validate_canonical_baseline(&[
                SessionRecordId::new("record-candidate").unwrap(),
                SessionRecordId::new("record-baseline").unwrap(),
            ]),
            Err(HarnessC2Error::NonCanonicalBaseline),
        ));
    }

    #[test]
    fn continuation_export_lineage_rejects_wrong_node_session_and_provider() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let source_session = SessionAddress {
            workspace_id: WorkspaceId::new("primary").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(1),
            },
        };
        let source_provider = AgentId::new("claude").unwrap();
        let receipt = ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-a").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: route.node_id.clone(),
                source_session: source_session.clone(),
                source_provider: source_provider.clone(),
            },
            source_message_count: 2,
            retained_message_count: 2,
            byte_len: 16,
            truncated: false,
        };
        assert!(context_export_receipt_matches(
            &route,
            &source_session,
            &source_provider,
            &receipt,
        ));

        let mut wrong_node = receipt.clone();
        wrong_node.lineage.source_node_id = NodeId::new("node-b").unwrap();
        assert!(!context_export_receipt_matches(
            &route,
            &source_session,
            &source_provider,
            &wrong_node,
        ));
        let mut wrong_session = receipt.clone();
        wrong_session.lineage.source_session.session.generation = SessionGeneration(2);
        assert!(!context_export_receipt_matches(
            &route,
            &source_session,
            &source_provider,
            &wrong_session,
        ));
        let mut wrong_provider = receipt;
        wrong_provider.lineage.source_provider = AgentId::new("codex").unwrap();
        assert!(!context_export_receipt_matches(
            &route,
            &source_session,
            &source_provider,
            &wrong_provider,
        ));
    }

    #[test]
    fn observation_resync_seals_exact_floor_high_watermark_and_sparse_authority() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let resync = build_observation_resync(
            &route,
            1,
            10,
            5,
            observation_snapshot(&route.node_id),
            Vec::new(),
        )
        .expect("valid sparse resync");
        assert_eq!(resync.route(), &route);
        assert_eq!(resync.event_sequence(), 10);
        assert_eq!(resync.oldest_available_sequence(), 5);
        assert!(resync.has_eviction_gap());
        assert_eq!(resync.observation_events(), &[]);
        assert_eq!(resync.managed_inventory(), &[]);
        assert_eq!(
            resync.observation_support(),
            Some(C2ObservationSupport { events: true, managed_target: true, workflow_detail: true })
        );
    }

    #[test]
    fn observation_resync_preserves_ordered_lifecycle_controls_separately() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let control = |sequence, kind| C2NodeEventEnvelope {
            sequence,
            event: gate4agent_c2_protocol::C2NodeEvent::Control {
                address: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(7),
                        generation: SessionGeneration(1),
                    },
                },
                event: gate4agent_c2_protocol::C2ControlEvent {
                    protocol_version: gate4agent_types::CONTROL_PROTOCOL_VERSION,
                    sequence,
                    command_id: None,
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                    event: kind,
                },
            },
        };
        let resync = build_observation_resync(
            &route,
            4,
            7,
            5,
            observation_snapshot(&route.node_id),
            vec![
                control(5, gate4agent_c2_protocol::C2ControlEventKind::Running),
                C2NodeEventEnvelope {
                    sequence: 6,
                    event: gate4agent_c2_protocol::C2NodeEvent::WorkspaceRemoved {
                        workspace_id: WorkspaceId::new("old").unwrap(),
                    },
                },
                control(
                    7,
                    gate4agent_c2_protocol::C2ControlEventKind::Exited {
                        exit_code: Some(0),
                        forced: false,
                    },
                ),
            ],
        ).expect("valid lifecycle resync");
        assert_eq!(
            resync.lifecycle_control_events().iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![5, 7],
        );
        assert!(resync.observation_events().is_empty());
    }

    #[test]
    fn observation_resync_rejects_wrong_snapshot_identity_and_impossible_cursor() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let wrong_snapshot = observation_snapshot(&NodeId::new("node-b").unwrap());
        assert!(matches!(
            build_observation_resync(&route, 0, 0, 1, wrong_snapshot, Vec::new()),
            Err(HarnessC2Error::InvalidObservationResync)
        ));
        assert!(matches!(
            build_observation_resync(
                &route,
                11,
                10,
                5,
                observation_snapshot(&route.node_id),
                Vec::new(),
            ),
            Err(HarnessC2Error::InvalidObservationResync)
        ));
    }

    #[test]
    fn native_history_projection_is_exact_and_excludes_provider_session_identity() {
        let route = HarnessNativeSessionRouteV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            scope: HarnessNativeSessionCatalogScopeV1::Workspace,
            workspace_id: Some("workspace-a".to_owned()),
            provider: "codex".to_owned(),
        };
        let wire_route = NativeSessionCatalogRoute::workspace(
            WorkspaceId::new("workspace-a").unwrap(),
            AgentId::new("codex").unwrap(),
        );
        let request = HarnessOperatorRequestV1::CatalogNativeSessions {
            route: route.clone(),
            limit: 16,
        };
        let response = correlate_native_history_response(
            request.clone(),
            C2NodeResponse::NativeSessionsCataloged {
                route: wire_route.clone(),
                entries: vec![gate4agent_node_protocol::NativeSessionCatalogEntry {
                    selection_id: "selection-a".to_owned(),
                    title: Some("Recovered session".to_owned()),
                    modified_at_unix_ms: Some(10),
                    model: Some("model-a".to_owned()),
                    message_count: 4,
                    completed_turn_count: Some(2),
                    external_group: None,
                    record_id: Some(SessionRecordId::new("record-a").unwrap()),
                }],
                summary: Some(gate4agent_node_protocol::NativeSessionCatalogSummary {
                    catalog_revision: 7,
                    recent_cutoff_unix_ms: 9,
                    recent_total_count: 1,
                    older_total_count: 0,
                    recent_next_after_selection_id: None,
                    recent_has_more: false,
                }),
            },
        ).unwrap();
        response.validate().unwrap();
        let encoded = serde_json::to_string(&response).unwrap();
        for forbidden in ["provider_session", "session_id", "credential", "terminal"] {
            assert!(!encoded.contains(forbidden));
        }
        let wrong_route = NativeSessionCatalogRoute::workspace(
            WorkspaceId::new("workspace-b").unwrap(),
            AgentId::new("codex").unwrap(),
        );
        assert!(matches!(
            correlate_native_history_response(
                request,
                C2NodeResponse::NativeSessionsCataloged {
                    route: wrong_route,
                    entries: Vec::new(),
                    summary: None,
                },
            ),
            Err(HarnessC2Error::NativeHistoryCorrelationMismatch),
        ));
    }

    #[test]
    fn observation_topology_is_online_incarnation_exact_canonical_and_deduped() {
        let incarnation_a = NodeIncarnationId::from_bytes([1; 16]);
        let incarnation_b = NodeIncarnationId::from_bytes([2; 16]);
        let support_a = C2ObservationSupport {
            events: true,
            managed_target: false,
            workflow_detail: true,
        };
        let support_b = C2ObservationSupport {
            events: false,
            managed_target: true,
            workflow_detail: false,
        };
        let duplicate_b = topology_node(
            "node-b",
            NodeTransportState::Online,
            Some(incarnation_b),
            Some(support_b),
        );
        let topology = C2Topology {
            nodes: vec![
                duplicate_b.clone(),
                topology_node(
                    "node-offline",
                    NodeTransportState::Offline,
                    Some(NodeIncarnationId::from_bytes([3; 16])),
                    Some(support_a),
                ),
                topology_node(
                    "node-a",
                    NodeTransportState::Online,
                    Some(incarnation_a),
                    Some(support_a),
                ),
                topology_node(
                    "node-missing-incarnation",
                    NodeTransportState::Online,
                    None,
                    Some(support_a),
                ),
                topology_node(
                    "node-parked",
                    NodeTransportState::Parked,
                    Some(NodeIncarnationId::from_bytes([4; 16])),
                    None,
                ),
                duplicate_b,
            ],
        };

        let routes = observation_topology(&topology);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].route().node_id.as_str(), "node-a");
        assert_eq!(routes[0].route().expected_incarnation_id, incarnation_a);
        assert_eq!(routes[0].support(), Some(support_a));
        assert_eq!(routes[1].route().node_id.as_str(), "node-b");
        assert_eq!(routes[1].route().expected_incarnation_id, incarnation_b);
        assert_eq!(routes[1].support(), Some(support_b));
    }
}
